//! The memory PostgreSQL 18.6's transition functions take for a group's
//! state in a hash aggregate's transition context, replayed from the values
//! each input row hands them.
//!
//! A state passed by value takes nothing; an initial value is copied when
//! the group starts (`initialize_aggregate`). The others allocate as their
//! input arrives:
//!
//! - a strict function returning one of its arguments (`min`, `max`) keeps
//!   a copy of its first input (`ExecAggInitGroup`), then copies each value
//!   it returns instead of the state and frees the old copy
//!   (`ExecAggCopyTransValue`);
//! - the integer and interval sums allocate their fixed state on their
//!   first call (`makeInt128AggState`, `makeIntervalAggState`);
//! - the numeric sums allocate a `NumericAggState`, whose digit buffers
//!   `accum_sum_rescale` replaces as the inputs' weights and scales grow;
//! - `string_agg` keeps a `StringInfo` of 1024 bytes that
//!   `enlargeStringInfo` doubles;
//! - `array_agg` keeps an `ArrayBuildState` of eight elements that
//!   `accumArrayResult` doubles, and a detoasted copy of each element passed
//!   by reference.
//!
//! Sources (REL_18_6): `executor/execExprInterp.c`, `executor/nodeAgg.c`,
//! `utils/adt/numeric.c`, `utils/adt/varlena.c`, `utils/adt/arrayfuncs.c`,
//! `utils/adt/array_userfuncs.c`, `common/stringinfo.c`.

use core::cell::Cell;
use core::cmp::Ordering;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::eval::{SqlError, arena_full, sqlstate};
use crate::sql::numeric::{Numeric, Sign};
use crate::sql::query::postgres_memory::{MemoryStep, stored, value_written_size};
use crate::sql::types::Datum;
use crate::sql_err;

/// How a copied value reaches the state: as the input presented it (a
/// function reading its arguments with `PG_GETARG_*_PP`), detoasted to a
/// four-byte header (`PG_GETARG_NUMERIC`), or as a fixed-length value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopiedForm {
    Packed,
    Detoasted,
    Fixed(u16),
}

/// What a transition state takes in the aggregate's memory per group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateMemory {
    /// Nothing besides the hash entry: a state passed by value.
    ByValue,
    /// The initial value's copy when the group starts, in bytes.
    Initial(u16),
    /// A fixed state the transition function allocates on its first call.
    FirstCall(u16),
    /// A copy of the input the strict function returns: `smaller` keeps the
    /// lesser value; `ties_replace` returns the input on equal values.
    Copied {
        smaller: bool,
        ties_replace: bool,
        form: CopiedForm,
    },
    /// `NumericAggState`, with the sum of squares when `squares`; `int8`
    /// reads `bigint` inputs.
    Numeric {
        squares: bool,
        int8: bool,
    },
    StringAgg,
    ArrayAgg,
}

impl StateMemory {
    /// The memory of the state of transition function `function`, whose
    /// state `get_agg_clause_costs` charges `space` bytes; `None` for a
    /// state the model does not follow.
    pub(crate) fn of(function: i32, space: f64) -> Option<Self> {
        let copied = |smaller, ties_replace, form| Self::Copied {
            smaller,
            ties_replace,
            form,
        };
        Some(match function {
            // int2_avg_accum, int4_avg_accum: `{0,0}` as `int8[]`.
            1962 | 1963 => Self::Initial(40),
            // float4_accum, float8_accum: `{0,0,0}` as `float8[]`.
            208 | 222 => Self::Initial(48),
            // float8_regr_accum: six `float8` elements.
            2806 => Self::Initial(72),
            // int8_avg_accum, int2_accum, int4_accum: `Int128AggState`.
            2746 | 1834 | 1835 => Self::FirstCall(48),
            // interval_avg_accum: `IntervalAggState`.
            1843 => Self::FirstCall(40),
            // text_smaller, text_larger.
            459 => copied(true, true, CopiedForm::Packed),
            458 => copied(false, true, CopiedForm::Packed),
            // bpchar_smaller, bpchar_larger keep the state on a tie.
            1064 => copied(true, false, CopiedForm::Packed),
            1063 => copied(false, false, CopiedForm::Packed),
            // bytea_smaller, bytea_larger.
            6394 => copied(true, true, CopiedForm::Packed),
            6393 => copied(false, true, CopiedForm::Packed),
            // network_smaller, network_larger.
            3563 => copied(true, true, CopiedForm::Packed),
            3562 => copied(false, true, CopiedForm::Packed),
            // numeric_smaller, numeric_larger.
            1766 => copied(true, true, CopiedForm::Detoasted),
            1767 => copied(false, true, CopiedForm::Detoasted),
            // interval_smaller, interval_larger.
            1197 => copied(true, true, CopiedForm::Fixed(16)),
            1198 => copied(false, true, CopiedForm::Fixed(16)),
            // timetz_smaller, timetz_larger.
            1380 => copied(true, true, CopiedForm::Fixed(12)),
            1379 => copied(false, true, CopiedForm::Fixed(12)),
            // tidsmaller, tidlarger keep the state on a tie.
            2796 => copied(true, false, CopiedForm::Fixed(6)),
            2795 => copied(false, false, CopiedForm::Fixed(6)),
            // numeric_avg_accum, numeric_accum, int8_accum.
            2858 => Self::Numeric {
                squares: false,
                int8: false,
            },
            1833 => Self::Numeric {
                squares: true,
                int8: false,
            },
            1836 => Self::Numeric {
                squares: true,
                int8: true,
            },
            // string_agg_transfn, bytea_string_agg_transfn.
            3535 | 3543 => Self::StringAgg,
            // array_agg_transfn.
            2333 => Self::ArrayAgg,
            _ if space == 0.0 => Self::ByValue,
            _ => return None,
        })
    }

    /// Whether the state allocates as its input arrives.
    pub(crate) fn input_driven(self) -> bool {
        !matches!(self, Self::ByValue | Self::Initial(_))
    }
}

/// What an aggregate's latest update handed its transition: nothing when a
/// `FILTER` skipped the row, else its first argument, a second one
/// (`string_agg`'s delimiter) and, for `min` and `max`, how the input
/// compared with the value held (`None` before the first).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum Observed<'a> {
    #[default]
    NotCalled,
    Called {
        value: Datum<'a>,
        extra: Datum<'a>,
        ordering: Option<Ordering>,
    },
}

/// `NBASE`.
const NBASE: i32 = 10000;
/// `sizeof(NumericAggState)`.
const NUMERIC_AGG_STATE: u32 = 144;
/// `sizeof(StringInfoData)` and `STRINGINFO_DEFAULT_SIZE`.
const STRING_INFO: u32 = 24;
const STRING_INFO_DEFAULT: u32 = 1024;
/// `sizeof(ArrayBuildState)` and the elements `initArrayResult` starts
/// with outside a subcontext.
const ARRAY_BUILD_STATE: u32 = 48;
const ARRAY_INITIAL: u32 = 8;
/// `sizeof(Datum)`.
const DATUM: u32 = 8;

/// `NumericSumAccum`: base-`NBASE` digits added without carrying, the
/// positive and negative inputs apart.
#[derive(Clone, Copy, Debug)]
struct SumAccum<'a> {
    weight: i32,
    dscale: i32,
    uncarried: i32,
    carry_space: bool,
    positive: &'a [Cell<i32>],
    negative: &'a [Cell<i32>],
}

impl<'a> SumAccum<'a> {
    const EMPTY: Self = Self {
        weight: 0,
        dscale: 0,
        uncarried: 0,
        carry_space: false,
        positive: &[],
        negative: &[],
    };

    /// `accum_sum_carry`.
    fn carry(&mut self) {
        if self.uncarried == 0 {
            return;
        }
        for digits in [self.positive, self.negative] {
            let mut carry = 0;
            let mut digit = 0;
            for slot in digits.iter().rev() {
                digit = slot.get() + carry;
                if digit >= NBASE {
                    carry = digit / NBASE;
                    digit -= carry * NBASE;
                } else {
                    carry = 0;
                }
                slot.set(digit);
            }
            if digit > 0 {
                self.carry_space = false;
            }
        }
        self.uncarried = 0;
    }

    /// `accum_sum_add` of a value of base-`NBASE` `digits` (most significant
    /// first) of weight `weight`.
    fn add(
        &mut self,
        negative: bool,
        weight: i32,
        dscale: i32,
        digits: &[i16],
        arena: &'a Arena,
        steps: &mut Steps<'_, 'a>,
    ) -> Result<(), SqlError> {
        if self.uncarried == NBASE - 1 {
            self.carry();
        }
        // `accum_sum_rescale`.
        let (old_weight, old_ndigits) = (self.weight, self.positive.len() as i32);
        let (mut new_weight, mut new_ndigits) = (old_weight, old_ndigits);
        if weight >= new_weight {
            new_weight = weight + 1;
            new_ndigits += new_weight - old_weight;
        } else if !self.carry_space {
            new_weight += 1;
            new_ndigits += 1;
        }
        let accum_rscale = new_ndigits - new_weight - 1;
        let value_rscale = digits.len() as i32 - weight - 1;
        if value_rscale > accum_rscale {
            new_ndigits += value_rscale - accum_rscale;
        }
        if new_ndigits != old_ndigits || new_weight != old_weight {
            let shift = (new_weight - old_weight) as usize;
            let bytes = new_ndigits as u32 * 4;
            steps.push(MemoryStep::Alloc(bytes))?;
            steps.push(MemoryStep::Alloc(bytes))?;
            let resize = |old: &[Cell<i32>]| -> Result<&'a [Cell<i32>], SqlError> {
                let digits = arena
                    .alloc_persistent_slice_with(new_ndigits as usize, |at| {
                        at.checked_sub(shift)
                            .and_then(|from| old.get(from))
                            .map_or(0, Cell::get)
                    })
                    .map_err(|_| arena_full())?;
                Ok(Cell::from_mut(digits).as_slice_of_cells())
            };
            let (positive, negative) = (resize(self.positive)?, resize(self.negative)?);
            if old_ndigits > 0 {
                steps.push(MemoryStep::Free(old_ndigits as u32 * 4))?;
                steps.push(MemoryStep::Free(old_ndigits as u32 * 4))?;
            }
            self.positive = positive;
            self.negative = negative;
            self.weight = new_weight;
            self.carry_space = true;
        }
        self.dscale = self.dscale.max(dscale);
        let target = if negative {
            self.negative
        } else {
            self.positive
        };
        let at = (self.weight - weight) as usize;
        for (slot, &digit) in target[at..].iter().zip(digits) {
            slot.set(slot.get() + i32::from(digit));
        }
        self.uncarried += 1;
        Ok(())
    }
}

/// One group's transition state as far as its memory goes.
#[derive(Clone, Copy, Debug, Default)]
enum GroupMemory<'a> {
    /// No call allocated yet.
    #[default]
    Empty,
    /// A fixed state was allocated.
    Allocated,
    /// The copy held: its requested bytes, and whether it has a one-byte
    /// header, which `PG_GETARG_NUMERIC` detoasts into a new value.
    Copy {
        bytes: u32,
        short: bool,
    },
    Numeric {
        sum: SumAccum<'a>,
        squares: SumAccum<'a>,
    },
    String {
        len: u32,
        maxlen: u32,
    },
    Array {
        elements: u32,
        allocated: u32,
    },
}

/// Appends the steps of one row's transitions to the aggregate's log.
struct Steps<'l, 'a> {
    log: &'l mut ArenaList<'a, MemoryStep>,
}

impl Steps<'_, '_> {
    fn push(&mut self, step: MemoryStep) -> Result<(), SqlError> {
        self.log.push(step).map_err(|_| arena_full())
    }
}

/// A modelled state's value had no PostgreSQL size here.
fn unsized_value() -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "the memory of a hash aggregate's transition state over a value without a PostgreSQL storage size is not supported"
    )
}

/// The requested bytes of a size, which fit aset.c's `MaxAllocSize`.
fn bytes(size: usize) -> Result<u32, SqlError> {
    u32::try_from(size).map_err(|_| unsized_value())
}

/// One transition state of a hash aggregate, as its rows feed it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ModelledState {
    pub(crate) memory: StateMemory,
    /// The aggregate whose updates feed the state.
    pub(crate) aggregate: usize,
    /// The input's type, which sizes a copied element.
    pub(crate) input_type: i32,
    /// The input is an expression's result (a four-byte header), not a
    /// stored column's value.
    pub(crate) computed: bool,
}

/// The steps every input row's transitions take, in arrival order. The
/// steps and digit buffers grow from the arena's persistent tail: the scan
/// feeding the rows may rewind the arena's front after each row.
pub(crate) struct TransitionModel<'a> {
    arena: &'a Arena,
    states: &'a [ModelledState],
    groups: &'a mut [GroupMemory<'a>],
    steps: ArenaList<'a, MemoryStep>,
    starts: &'a mut [u32],
    rows: usize,
}

/// The steps of each input row, by arrival position.
#[derive(Clone, Copy)]
pub(crate) struct TransitionLog<'s> {
    starts: &'s [u32],
    steps: &'s [MemoryStep],
}

impl<'s> TransitionLog<'s> {
    /// The steps row `row`'s transitions took.
    pub(crate) fn row(&self, row: usize) -> &'s [MemoryStep] {
        &self.steps[self.starts[row] as usize..self.starts[row + 1] as usize]
    }
}

impl<'a> TransitionModel<'a> {
    /// A model of `states` over `groups` groups and `rows` input rows.
    pub(crate) fn new(
        arena: &'a Arena,
        states: &'a [ModelledState],
        groups: usize,
        rows: usize,
    ) -> Result<Self, SqlError> {
        Ok(Self {
            arena,
            states,
            groups: arena
                .alloc_slice_with(groups * states.len(), |_| GroupMemory::Empty)
                .map_err(|_| arena_full())?,
            steps: ArenaList::new_persistent(arena),
            starts: arena
                .alloc_slice_with(rows + 1, |_| 0u32)
                .map_err(|_| arena_full())?,
            rows: 0,
        })
    }

    /// Replays the next input row, of group `group`: `observe` gives what
    /// the aggregate feeding each state saw.
    pub(crate) fn row(
        &mut self,
        group: usize,
        mut observe: impl FnMut(usize) -> Observed<'a>,
    ) -> Result<(), SqlError> {
        let row = self.rows;
        self.starts[row] = u32::try_from(self.steps.len()).map_err(|_| arena_full())?;
        for (index, state) in self.states.iter().enumerate() {
            let Observed::Called {
                value,
                extra,
                ordering,
            } = observe(state.aggregate)
            else {
                continue;
            };
            let held = &mut self.groups[group * self.states.len() + index];
            let mut steps = Steps {
                log: &mut self.steps,
            };
            advance(state, held, value, extra, ordering, self.arena, &mut steps)?;
        }
        self.rows = row + 1;
        self.starts[row + 1] = u32::try_from(self.steps.len()).map_err(|_| arena_full())?;
        Ok(())
    }

    /// The steps of the rows replayed.
    pub(crate) fn log(&self) -> TransitionLog<'_> {
        TransitionLog {
            starts: &self.starts[..self.rows + 1],
            steps: self.steps.borrowed_slice(),
        }
    }
}

/// The digits of a numeric input as `init_var_from_num` reads them, into
/// `digits`: sign, weight, display scale and the digit count; `None` for NaN
/// or an infinity, which only count.
fn numeric_var(
    value: &Numeric<'_>,
    digits: &mut [i16],
) -> Result<Option<(bool, i32, i32, usize)>, SqlError> {
    if value.is_special() {
        return Ok(None);
    }
    let count = value.ndigits();
    let slots = digits.get_mut(..count).ok_or_else(unsized_value)?;
    for (at, slot) in slots.iter_mut().enumerate() {
        *slot = value.digit(at);
    }
    let weight = if count == 0 {
        0
    } else {
        i32::from(value.weight)
    };
    Ok(Some((
        value.sign == Sign::Neg,
        weight,
        i32::from(value.dscale),
        count,
    )))
}

fn advance<'a>(
    state: &ModelledState,
    held: &mut GroupMemory<'a>,
    value: Datum<'a>,
    extra: Datum<'a>,
    ordering: Option<Ordering>,
    arena: &'a Arena,
    steps: &mut Steps<'_, 'a>,
) -> Result<(), SqlError> {
    match state.memory {
        StateMemory::ByValue | StateMemory::Initial(_) => {}
        StateMemory::FirstCall(size) => {
            if matches!(held, GroupMemory::Empty) {
                steps.push(MemoryStep::Alloc(u32::from(size)))?;
                *held = GroupMemory::Allocated;
            }
        }
        StateMemory::Copied {
            smaller,
            ties_replace,
            form,
        } => {
            if value.is_null() {
                return Ok(());
            }
            let presented = |detoast: bool| -> Result<u32, SqlError> {
                match form {
                    CopiedForm::Fixed(size) => Ok(u32::from(size)),
                    _ => bytes(
                        value_written_size(None, &value, state.computed || detoast)
                            .ok_or_else(unsized_value)?,
                    ),
                }
            };
            match *held {
                GroupMemory::Copy { bytes: old, short } => {
                    let returned_input = match ordering {
                        Some(Ordering::Less) => smaller,
                        Some(Ordering::Greater) => !smaller,
                        Some(Ordering::Equal) => ties_replace,
                        None => true,
                    };
                    let detoasted = form == CopiedForm::Detoasted;
                    // `PG_GETARG_NUMERIC` of a state with a one-byte header
                    // returns a detoasted copy, which is never the state.
                    let new = if returned_input {
                        Some(presented(detoasted)?)
                    } else if detoasted && short {
                        Some(old + 3)
                    } else {
                        None
                    };
                    if let Some(new) = new {
                        steps.push(MemoryStep::Alloc(new))?;
                        steps.push(MemoryStep::Free(old))?;
                        *held = GroupMemory::Copy {
                            bytes: new,
                            short: false,
                        };
                    }
                }
                _ => {
                    let first = presented(false)?;
                    steps.push(MemoryStep::Alloc(first))?;
                    *held = GroupMemory::Copy {
                        bytes: first,
                        short: form == CopiedForm::Detoasted && first < presented(true)?,
                    };
                }
            }
        }
        StateMemory::Numeric { squares, int8 } => {
            let (mut sum, mut square) = match *held {
                GroupMemory::Numeric { sum, squares } => (sum, squares),
                _ => {
                    steps.push(MemoryStep::Alloc(NUMERIC_AGG_STATE))?;
                    (SumAccum::EMPTY, SumAccum::EMPTY)
                }
            };
            let mut stack = [0u8; 20];
            let input = match value {
                Datum::Null => None,
                Datum::Numeric(number) if !int8 => Some(number),
                Datum::Int8(number) if int8 => Some(Numeric::from_i64_stack(number, &mut stack)),
                _ => return Err(unsized_value()),
            };
            let mut digits = [0i16; 2 * crate::sql::numeric::MAX_NDIGITS];
            if let Some(number) = input
                && let Some((negative, weight, dscale, count)) = numeric_var(&number, &mut digits)?
            {
                sum.add(negative, weight, dscale, &digits[..count], arena, steps)?;
                if squares {
                    // `mul_var` of the value by itself at twice its scale:
                    // exact, its zero digits stripped.
                    let product = crate::sql::numeric::mul(&number, &number, arena)?;
                    if let Some((_, weight, _, count)) = numeric_var(&product, &mut digits)? {
                        square.add(false, weight, dscale * 2, &digits[..count], arena, steps)?;
                    }
                }
            }
            *held = GroupMemory::Numeric {
                sum,
                squares: square,
            };
        }
        StateMemory::StringAgg => {
            let length = |datum: &Datum<'_>| match datum {
                Datum::Text(text) => Some(text.len()),
                Datum::Bytea(data) => Some(data.len()),
                _ => None,
            };
            if value.is_null() {
                return Ok(());
            }
            let value = bytes(length(&value).ok_or_else(unsized_value)?)?;
            let (mut len, mut maxlen) = match *held {
                GroupMemory::String { len, maxlen } => (len, maxlen),
                _ => {
                    steps.push(MemoryStep::Alloc(STRING_INFO))?;
                    steps.push(MemoryStep::Alloc(STRING_INFO_DEFAULT))?;
                    (0, STRING_INFO_DEFAULT)
                }
            };
            let delimiter = if extra.is_null() {
                None
            } else {
                Some(bytes(length(&extra).ok_or_else(unsized_value)?)?)
            };
            for appended in delimiter.into_iter().chain([value]) {
                // `enlargeStringInfo` doubles the buffer until the bytes and
                // a terminator fit.
                let needed = len + appended + 1;
                if needed > maxlen {
                    let mut grown = maxlen * 2;
                    while needed > grown {
                        grown *= 2;
                    }
                    steps.push(MemoryStep::Realloc(maxlen, grown))?;
                    maxlen = grown;
                }
                len += appended;
            }
            *held = GroupMemory::String { len, maxlen };
        }
        StateMemory::ArrayAgg => {
            let (mut elements, mut allocated) = match *held {
                GroupMemory::Array {
                    elements,
                    allocated,
                } => (elements, allocated),
                _ => {
                    steps.push(MemoryStep::Alloc(ARRAY_BUILD_STATE))?;
                    steps.push(MemoryStep::Alloc(ARRAY_INITIAL * DATUM))?;
                    steps.push(MemoryStep::Alloc(ARRAY_INITIAL))?;
                    (0, ARRAY_INITIAL)
                }
            };
            if elements >= allocated {
                steps.push(MemoryStep::Realloc(
                    allocated * DATUM,
                    allocated * 2 * DATUM,
                ))?;
                steps.push(MemoryStep::Realloc(allocated, allocated * 2))?;
                allocated *= 2;
            }
            if !value.is_null() {
                let element =
                    stored(None, Some(state.input_type), &value).ok_or_else(unsized_value)?;
                if !element.layout.by_value {
                    // `PG_DETOAST_DATUM_COPY` gives a varlena a four-byte
                    // header; `datumCopy` copies a fixed-length value.
                    let size = if element.layout.length > 0 {
                        element.layout.length as usize
                    } else {
                        element.payload + 4
                    };
                    steps.push(MemoryStep::Alloc(bytes(size)?))?;
                }
            }
            elements += 1;
            *held = GroupMemory::Array {
                elements,
                allocated,
            };
        }
    }
    Ok(())
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;
    use crate::sql::query::postgres_memory::AllocSet;

    fn arena() -> Arena {
        let mut budget = crate::mem::budget::Budget::new(1 << 20);
        Arena::new(&mut budget, "transition memory test", 1 << 20).unwrap()
    }

    fn replay(
        memory: StateMemory,
        rows: &[(usize, Observed<'static>)],
        groups: usize,
        set: &mut AllocSet,
    ) {
        let arena = arena();
        let states = [ModelledState {
            memory,
            aggregate: 0,
            input_type: 25,
            computed: false,
        }];
        let states = arena.alloc_slice_copy(&states).unwrap();
        let mut model = TransitionModel::new(&arena, states, groups, rows.len()).unwrap();
        for &(group, observed) in rows {
            model.row(group, |_| observed).unwrap();
        }
        let log = model.log();
        for row in 0..rows.len() {
            for step in log.row(row) {
                match *step {
                    MemoryStep::Alloc(size) => set.alloc(size as usize),
                    MemoryStep::Free(size) => set.free(size as usize),
                    MemoryStep::Realloc(old, new) => set.realloc(old as usize, new as usize),
                }
            }
        }
    }

    fn called(value: Datum<'static>, extra: Datum<'static>) -> Observed<'static> {
        Observed::Called {
            value,
            extra,
            ordering: None,
        }
    }

    #[test]
    fn string_agg_doubles_its_buffer() {
        // Five groups taking 1000 values of four bytes and a delimiter
        // each, round robin, grow 1024 to 8192 bytes: with the default
        // chunk limit each buffer is a chunk (131072 bytes in five blocks,
        // measured on PostgreSQL 18.6), with `work_mem`'s 1024-byte limit
        // a block of its own (49392 bytes).
        let rows: [(usize, Observed<'static>); 5000] =
            core::array::from_fn(|row| (row % 5, called(Datum::Text("v123"), Datum::Text(","))));
        let mut set = AllocSet::new(8 << 20, 8192, 8192);
        replay(StateMemory::StringAgg, &rows, 5, &mut set);
        assert_eq!(set.allocated(), 131_072);
        let mut set = AllocSet::new(8192, 1024, 1024);
        replay(StateMemory::StringAgg, &rows, 5, &mut set);
        assert_eq!(set.allocated(), 49_392);
    }
}
