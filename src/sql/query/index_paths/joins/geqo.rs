//! PostgreSQL 18's genetic join search (`src/backend/optimizer/geqo`): the
//! steady-state genetic algorithm of `geqo_main.c` over a pool of join
//! orders (`geqo_pool.c`), chosen with a linear bias (`geqo_selection.c`)
//! and recombined by edge recombination (`geqo_erx.c`, PostgreSQL's
//! compiled choice), each order evaluated by building its joins through
//! the standard join path generation (`geqo_eval.c`). Its random numbers
//! come from `pg_prng` seeded by `geqo_seed` (`geqo_random.c`), so a
//! seed always gives the same plan.
//! <https://github.com/postgres/postgres/tree/REL_18_6/src/backend/optimizer/geqo>

use crate::mem::mutex::Mutex;
use core::cell::Cell;

use crate::sql::eval::SqlError;
use crate::sql::eval::sqlstate;
use crate::sql::guc::PrngState;

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_vec::FixedVec;

use super::{Relids, Search};

/// `Gene`: a relation's 1-based position among the problem's initial
/// relations.
type Gene = i32;

/// Individuals one pool holds: the largest pool the default sizing picks
/// (`50 * geqo_effort` at `geqo_effort = 10`). A larger `geqo_pool_size`
/// fails with a named error.
pub(crate) const MAX_POOL: usize = 500;

/// `Chromosome`: a pool slot's string and its fitness (`worth`).
#[derive(Clone, Copy)]
struct Chromosome {
    string: usize,
    worth: f64,
}

/// `Edge` of the edge recombination table.
#[derive(Clone, Copy)]
struct Edge {
    edge_list: [Gene; 4],
    total_edges: i32,
    unused_edges: i32,
}

const NO_EDGE: Edge = Edge {
    edge_list: [0; 4],
    total_edges: 0,
    unused_edges: 0,
};

/// The join search's startup reservation: the most relations one query
/// plans (`max_join_relations`) and the genetic search's pool, sized for
/// [`MAX_POOL`] individuals of that many relations. One genetic search
/// uses the pool at a time.
pub(crate) struct JoinSearchPool {
    max_relations: usize,
    memory: Mutex<PoolMemory>,
}

/// The pool's strings (each `stride` genes, with `alloc_chromo`'s spare),
/// the two parents' after them, and the edge table.
struct PoolMemory {
    stride: usize,
    strings: FixedVec<Gene>,
    data: [Chromosome; MAX_POOL],
    order: [u32; MAX_POOL],
    edges: FixedVec<Edge>,
}

const MOMMA: usize = MAX_POOL;
const DADDY: usize = MAX_POOL + 1;

impl PoolMemory {
    fn string(&self, at: usize) -> &[Gene] {
        &self.strings[at * self.stride..(at + 1) * self.stride]
    }

    fn string_mut(&mut self, at: usize) -> &mut [Gene] {
        &mut self.strings[at * self.stride..(at + 1) * self.stride]
    }

    /// Copies string `from` over string `to`.
    fn copy_string(&mut self, from: usize, to: usize) {
        let stride = self.stride;
        self.strings
            .copy_within(from * stride..(from + 1) * stride, to * stride);
    }
}

impl JoinSearchPool {
    /// Startup bytes of a pool for `max_relations` relations.
    pub(crate) fn budget_bytes(max_relations: usize) -> usize {
        let stride = max_relations + 1;
        (MAX_POOL + 2) * stride * size_of::<Gene>() + stride * size_of::<Edge>() + size_of::<Self>()
    }

    pub(crate) fn new(budget: &mut Budget, max_relations: usize) -> Result<Self, BudgetError> {
        budget.draw(size_of::<Self>(), "join search pool")?;
        let stride = max_relations + 1;
        let mut strings = FixedVec::new(budget, "join search pool", (MAX_POOL + 2) * stride)?;
        while strings.len() < strings.capacity() {
            strings.push(0).expect("a reserved gene");
        }
        let mut edges = FixedVec::new(budget, "join search pool", stride)?;
        while edges.len() < edges.capacity() {
            edges.push(NO_EDGE).expect("a reserved edge");
        }
        Ok(Self {
            max_relations,
            memory: Mutex::new(PoolMemory {
                stride,
                strings,
                data: [Chromosome {
                    string: 0,
                    worth: 0.0,
                }; MAX_POOL],
                order: [0; MAX_POOL],
                edges,
            }),
        })
    }

    /// `max_join_relations`.
    pub(crate) fn max_relations(&self) -> usize {
        self.max_relations
    }
}

std::thread_local! {
    /// This thread holds its join search pool.
    static SEARCHING: Cell<bool> = const { Cell::new(false) };
}

/// Marks the thread's genetic search until dropped.
struct Searching;

impl Searching {
    fn enter() -> Self {
        SEARCHING.with(|searching| searching.set(true));
        Self
    }
}

impl Drop for Searching {
    fn drop(&mut self) {
        SEARCHING.with(|searching| searching.set(false));
    }
}

/// The GEQO settings a join search reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GeqoSettings {
    /// `geqo` and `geqo_threshold`.
    pub(crate) enabled: bool,
    pub(crate) threshold: i32,
    pub(crate) effort: i32,
    pub(crate) pool_size: i32,
    pub(crate) generations: i32,
    pub(crate) selection_bias: f64,
    pub(crate) seed: f64,
}

impl GeqoSettings {
    /// The session's settings.
    pub(crate) fn current() -> Self {
        let value = crate::sql::guc::planner_value;
        Self {
            enabled: value("geqo") != 0.0,
            threshold: value("geqo_threshold") as i32,
            effort: value("geqo_effort") as i32,
            pool_size: value("geqo_pool_size") as i32,
            generations: value("geqo_generations") as i32,
            selection_bias: value("geqo_selection_bias"),
            seed: value("geqo_seed"),
        }
    }

    /// `gimme_pool_size`.
    fn pool_size(&self, relations: usize) -> i64 {
        if self.pool_size >= 2 {
            return i64::from(self.pool_size);
        }
        let size = 2.0f64.powf(relations as f64 + 1.0);
        let maxsize = 50 * self.effort;
        if size > f64::from(maxsize) {
            return i64::from(maxsize);
        }
        let minsize = 10 * self.effort;
        if size < f64::from(minsize) {
            return i64::from(minsize);
        }
        size.ceil() as i64
    }

    /// `gimme_number_generations`.
    fn generations(&self, pool_size: i64) -> i64 {
        if self.generations > 0 {
            i64::from(self.generations)
        } else {
            pool_size
        }
    }
}

/// A clump of joined relations of `gimme_tree`.
#[derive(Clone, Copy)]
struct Clump {
    rel: Relids,
    size: usize,
}

/// `gimme_tree`'s clumps, larger ones first, in a slice of the search's
/// work space.
struct Clumps<'w> {
    items: &'w mut [Clump],
    len: usize,
}

impl Clumps<'_> {
    fn as_slice(&self) -> &[Clump] {
        &self.items[..self.len]
    }

    fn remove(&mut self, at: usize) {
        self.items.copy_within(at + 1..self.len, at);
        self.len -= 1;
    }

    fn insert(&mut self, at: usize, clump: Clump) {
        self.items.copy_within(at..self.len, at + 1);
        self.items[at] = clump;
        self.len += 1;
    }
}

/// `GeqoPrivateData`'s random state with `geqo_rand` and `geqo_randint`.
struct Random(PrngState);

impl Random {
    fn rand(&mut self) -> f64 {
        self.0.next_f64()
    }

    /// `geqo_randint(root, upper, lower)`.
    fn randint(&mut self, upper: i32, lower: i32) -> i32 {
        self.0.next_u64_range(lower as u64, upper as u64) as i32
    }
}

impl Search<'_, '_, '_> {
    /// `geqo`: the join of the current problem's initial relations
    /// (`levels[1]`) a genetic search finds, or `None` when path
    /// generation met a shape outside the model.
    pub(super) fn geqo(&mut self) -> Result<Option<Relids>, SqlError> {
        let settings = self.problem.settings.geqo;
        let number_of_rels = self.levels[1].len();
        let mut random = Random(PrngState::seeded(settings.seed));
        let pool_size = settings.pool_size(number_of_rels);
        let number_generations = settings.generations(pool_size);
        if pool_size > MAX_POOL as i64 {
            return Err(crate::sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "GEQO pool of {pool_size} individuals exceeds the {MAX_POOL} the server holds"
            ));
        }
        let pool_size = pool_size as usize;
        // A join search its own path generation started would wait for the
        // pool its caller holds.
        if SEARCHING.with(Cell::get) {
            return Err(crate::sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "GEQO pool is in use by an enclosing join search"
            ));
        }
        let _searching = Searching::enter();
        let length = number_of_rels;
        let arena = self.arena;
        let work = super::arena_slice(arena, 2 * length, |_| Clump {
            rel: Relids::EMPTY,
            size: 0,
        })?;
        let tour = super::arena_slice(arena, length, |_| 0)?;
        let mut guard = self
            .problem
            .pool
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let memory = &mut *guard;
        for (at, chromosome) in memory.data[..pool_size].iter_mut().enumerate() {
            chromosome.string = at;
        }
        // `random_init_pool`: invalid tours are discarded at once.
        let mut at = 0;
        let mut bad = 0;
        while at < pool_size {
            let string = memory.data[at].string;
            init_tour(&mut random, &mut memory.string_mut(string)[..length]);
            let worth = self.geqo_eval(&memory.string(string)[..length], work)?;
            if self.outside.is_some() {
                return Ok(None);
            }
            memory.data[at].worth = worth;
            if worth < f64::MAX {
                at += 1;
            } else {
                bad += 1;
                if at == 0 && bad >= 10000 {
                    return Err(failed());
                }
            }
        }
        sort_pool(memory, pool_size)?;
        for _ in 0..number_generations {
            // Selection with a linear bias.
            let first = linear_rand(&mut random, pool_size, settings.selection_bias);
            let mut second = linear_rand(&mut random, pool_size, settings.selection_bias);
            if pool_size > 1 {
                while first == second {
                    second = linear_rand(&mut random, pool_size, settings.selection_bias);
                }
            }
            let (momma, daddy) = (memory.data[first], memory.data[second]);
            memory.copy_string(momma.string, MOMMA);
            memory.copy_string(daddy.string, DADDY);
            // Edge recombination: the kid is momma's string.
            let stride = memory.stride;
            let PoolMemory { strings, edges, .. } = memory;
            let (_, parents) = strings.split_at_mut(MOMMA * stride);
            let (momma_string, daddy_string) = parents.split_at_mut(stride);
            gimme_edge_table(&momma_string[..length], &daddy_string[..length], edges);
            gimme_tour(&mut random, edges, &mut momma_string[..length])?;
            let worth = self.geqo_eval(&memory.string(MOMMA)[..length], work)?;
            if self.outside.is_some() {
                return Ok(None);
            }
            spread_chromo(memory, pool_size, length, worth);
        }
        let best = memory.data[0].string;
        tour.copy_from_slice(&memory.string(best)[..length]);
        drop(guard);
        let best = self.gimme_tree(tour, work)?;
        if self.outside.is_some() {
            return Ok(None);
        }
        best.map(Some).ok_or_else(failed)
    }

    /// `geqo_eval`: the cost of `tour`'s cheapest plan, `f64::MAX` when it
    /// joins nothing legal; the relations and paths it built are dropped
    /// (its memory context), the clauses it derived kept.
    fn geqo_eval(&mut self, tour: &[Gene], work: &mut [Clump]) -> Result<f64, SqlError> {
        let saved = self.save_joins();
        let joinrel = self.gimme_tree(tour, work)?;
        let fitness = match joinrel {
            Some(relids) if self.outside.is_none() => {
                self.path(self.rel(relids).cheapest_total).total
            }
            _ => f64::MAX,
        };
        self.restore_joins(saved)?;
        Ok(fitness)
    }

    /// `gimme_tree`: joins the tour's relations in its order, postponing
    /// joins without a join clause or order restriction, then forces the
    /// remaining clumps together.
    fn gimme_tree(
        &mut self,
        tour: &[Gene],
        work: &mut [Clump],
    ) -> Result<Option<Relids>, SqlError> {
        let (first, second) = work.split_at_mut(tour.len());
        let mut clumps = Clumps {
            items: first,
            len: 0,
        };
        for &gene in tour {
            let rel = self.levels[1].borrowed_slice()[gene as usize - 1];
            self.merge_clump(&mut clumps, Clump { rel, size: 1 }, false)?;
            if self.outside.is_some() {
                return Ok(None);
            }
        }
        if clumps.len > 1 {
            let mut forced = Clumps {
                items: second,
                len: 0,
            };
            for &clump in clumps.as_slice() {
                self.merge_clump(&mut forced, clump, true)?;
                if self.outside.is_some() {
                    return Ok(None);
                }
            }
            clumps = forced;
        }
        Ok((clumps.len == 1).then(|| clumps.items[0].rel))
    }

    /// `merge_clump`.
    fn merge_clump(
        &mut self,
        clumps: &mut Clumps<'_>,
        mut new: Clump,
        force: bool,
    ) -> Result<(), SqlError> {
        'merged: loop {
            for at in 0..clumps.len {
                let old = clumps.items[at];
                if !(force || self.desirable_join(old.rel, new.rel)?) {
                    continue;
                }
                let Some(joinrel) = self.make_join_rel(old.rel, new.rel, 0)? else {
                    if self.outside.is_some() {
                        return Ok(());
                    }
                    continue;
                };
                self.set_cheapest(joinrel)?;
                clumps.remove(at);
                new = Clump {
                    rel: joinrel,
                    size: old.size + new.size,
                };
                continue 'merged;
            }
            break;
        }
        if clumps.len == 0 || new.size == 1 {
            clumps.insert(clumps.len, new);
            return Ok(());
        }
        let at = clumps
            .as_slice()
            .iter()
            .position(|old| new.size > old.size)
            .unwrap_or(clumps.len);
        clumps.insert(at, new);
        Ok(())
    }

    /// `desirable_join`.
    fn desirable_join(&mut self, outer: Relids, inner: Relids) -> Result<bool, SqlError> {
        Ok(self.relevant(outer, inner) || self.order_restriction(outer, inner)?)
    }
}

fn failed() -> SqlError {
    crate::sql_err!(sqlstate::INTERNAL_ERROR, "geqo failed to make a valid plan")
}

/// `init_tour`: a random permutation of `1..=n` by the inside-out
/// Fisher-Yates shuffle.
fn init_tour(random: &mut Random, tour: &mut [Gene]) {
    if !tour.is_empty() {
        tour[0] = 1;
    }
    for i in 1..tour.len() {
        let j = random.randint(i as i32, 0) as usize;
        if i != j {
            tour[i] = tour[j];
        }
        tour[j] = i as Gene + 1;
    }
}

/// `sort_pool`: the pool by worth, cheapest first, as PostgreSQL's
/// `qsort` orders it.
fn sort_pool(memory: &mut PoolMemory, pool_size: usize) -> Result<(), SqlError> {
    let order = &mut memory.order[..pool_size];
    for (at, slot) in order.iter_mut().enumerate() {
        *slot = at as u32;
    }
    let data = memory.data;
    crate::sql::pg_qsort::pg_qsort(order, &mut |a, b| {
        let (first, second) = (data[a as usize].worth, data[b as usize].worth);
        Ok(if first == second {
            core::cmp::Ordering::Equal
        } else if first > second {
            core::cmp::Ordering::Greater
        } else {
            core::cmp::Ordering::Less
        })
    })?;
    for (at, &from) in order.iter().enumerate() {
        memory.data[at] = data[from as usize];
    }
    Ok(())
}

/// `linear_rand`: a pool index drawn with probability falling linearly
/// from `bias` times the median's at the best to `2 - bias` times at the
/// worst.
fn linear_rand(random: &mut Random, pool_size: usize, bias: f64) -> usize {
    let max = pool_size as f64;
    loop {
        let mut sqrtval = (bias * bias) - 4.0 * (bias - 1.0) * random.rand();
        if sqrtval > 0.0 {
            sqrtval = sqrtval.sqrt();
        }
        let index = max * (bias - sqrtval) / 2.0 / (bias - 1.0);
        if !(index < 0.0 || index >= max) {
            return index as usize;
        }
    }
}

/// `spread_chromo`: the kid (momma's string, of `worth`) replaces the
/// pool's worst individual and moves to its place by worth.
fn spread_chromo(memory: &mut PoolMemory, pool_size: usize, length: usize, worth: f64) {
    let data = &mut memory.data[..pool_size];
    if worth > data[pool_size - 1].worth {
        return;
    }
    let (mut top, mut mid, mut bot) = (0, pool_size / 2, pool_size - 1);
    let index = loop {
        if worth <= data[top].worth {
            break top;
        } else if worth == data[mid].worth {
            break mid;
        } else if worth == data[bot].worth || bot - top <= 1 {
            break bot;
        } else if worth < data[mid].worth {
            bot = mid;
            mid = top + ((bot - top) / 2);
        } else {
            top = mid;
            mid = top + ((bot - top) / 2);
        }
    };
    let worst = data[pool_size - 1].string;
    let stride = memory.stride;
    memory
        .strings
        .copy_within(MOMMA * stride..MOMMA * stride + length, worst * stride);
    let data = &mut memory.data[..pool_size];
    data[pool_size - 1].worth = worth;
    let mut swap = data[pool_size - 1];
    for slot in &mut data[index..] {
        core::mem::swap(slot, &mut swap);
    }
}

/// `gimme_edge_table`: the edges of both parents' circular tours, an edge
/// both share stored negated.
fn gimme_edge_table(tour1: &[Gene], tour2: &[Gene], edges: &mut [Edge]) {
    let num_gene = tour1.len();
    for edge in &mut edges[1..=num_gene] {
        edge.total_edges = 0;
        edge.unused_edges = 0;
    }
    for index1 in 0..num_gene {
        let index2 = (index1 + 1) % num_gene;
        gimme_edge(tour1[index1], tour1[index2], edges);
        gimme_edge(tour1[index2], tour1[index1], edges);
        gimme_edge(tour2[index1], tour2[index2], edges);
        gimme_edge(tour2[index2], tour2[index1], edges);
    }
}

/// `gimme_edge`.
fn gimme_edge(gene1: Gene, gene2: Gene, edges: &mut [Edge]) {
    let (city1, city2) = (gene1 as usize, gene2);
    let entry = &mut edges[city1];
    let count = entry.total_edges as usize;
    for listed in &mut entry.edge_list[..count] {
        if listed.abs() == city2 {
            *listed = -city2;
            return;
        }
    }
    entry.edge_list[count] = city2;
    entry.total_edges += 1;
    entry.unused_edges += 1;
}

/// `gimme_tour`: a new tour from the edge table, shared edges first.
fn gimme_tour(
    random: &mut Random,
    edges: &mut [Edge],
    new_gene: &mut [Gene],
) -> Result<(), SqlError> {
    let num_gene = new_gene.len();
    new_gene[0] = random.randint(num_gene as i32, 1);
    for i in 1..num_gene {
        let previous = new_gene[i - 1] as usize;
        remove_gene(new_gene[i - 1], edges[previous], edges);
        new_gene[i] = if edges[previous].unused_edges > 0 {
            gimme_gene(random, edges[previous], edges)?
        } else {
            edge_failure(random, new_gene, i - 1, edges)?
        };
        edges[previous].unused_edges = -1;
    }
    Ok(())
}

/// `remove_gene`: drops `gene` from the edge lists of its neighbors.
fn remove_gene(gene: Gene, edge: Edge, edges: &mut [Edge]) {
    for &listed in &edge.edge_list[..edge.unused_edges.max(0) as usize] {
        let possess_edge = listed.unsigned_abs() as usize;
        let genes_remaining = edges[possess_edge].unused_edges.max(0) as usize;
        for j in 0..genes_remaining {
            if edges[possess_edge].edge_list[j].abs() == gene {
                edges[possess_edge].unused_edges -= 1;
                edges[possess_edge].edge_list[j] =
                    edges[possess_edge].edge_list[genes_remaining - 1];
                break;
            }
        }
    }
}

/// `gimme_gene`: a shared edge, else a random one among the neighbors
/// with the fewest unused edges.
fn gimme_gene(random: &mut Random, edge: Edge, edges: &[Edge]) -> Result<Gene, SqlError> {
    let mut minimum_edges = 5;
    let mut minimum_count = -1;
    let used = edge.unused_edges.max(0) as usize;
    for &friend in &edge.edge_list[..used] {
        if friend < 0 {
            return Ok(friend.abs());
        }
        let unused = edges[friend as usize].unused_edges;
        if unused < minimum_edges {
            minimum_edges = unused;
            minimum_count = 1;
        } else if minimum_count == -1 {
            return Err(internal("minimum_count not set"));
        } else if unused == minimum_edges {
            minimum_count += 1;
        }
    }
    let rand_decision = random.randint(minimum_count - 1, 0);
    for &friend in &edge.edge_list[..used] {
        if edges[friend as usize].unused_edges == minimum_edges {
            minimum_count -= 1;
            if minimum_count == rand_decision {
                return Ok(friend);
            }
        }
    }
    Err(internal(
        "neither shared nor minimum number nor random edge found",
    ))
}

/// `edge_failure`: the next city when the last one has no unused edge.
fn edge_failure(
    random: &mut Random,
    gene: &[Gene],
    index: usize,
    edges: &[Edge],
) -> Result<Gene, SqlError> {
    let num_gene = gene.len();
    let fail_gene = gene[index];
    let mut remaining_edges = 0;
    let mut four_count = 0;
    let cities = || edges.iter().enumerate().take(num_gene + 1).skip(1);
    for (i, edge) in cities() {
        if edge.unused_edges != -1 && i as Gene != fail_gene {
            remaining_edges += 1;
            if edge.total_edges == 4 {
                four_count += 1;
            }
        }
    }
    if four_count != 0 {
        let rand_decision = random.randint(four_count - 1, 0);
        for (i, edge) in cities() {
            if i as Gene != fail_gene && edge.unused_edges != -1 && edge.total_edges == 4 {
                four_count -= 1;
                if rand_decision == four_count {
                    return Ok(i as Gene);
                }
            }
        }
    } else if remaining_edges != 0 {
        let rand_decision = random.randint(remaining_edges - 1, 0);
        for (i, edge) in cities() {
            if i as Gene != fail_gene && edge.unused_edges != -1 {
                remaining_edges -= 1;
                if rand_decision == remaining_edges {
                    return Ok(i as Gene);
                }
            }
        }
    } else {
        for (i, edge) in cities() {
            if edge.unused_edges >= 0 {
                return Ok(i as Gene);
            }
        }
    }
    Err(internal("no edge found"))
}

fn internal(message: &'static str) -> SqlError {
    crate::sql_err!(sqlstate::INTERNAL_ERROR, "{message}")
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    /// `init_tour` and `linear_rand` draw the numbers `geqo_seed` gives
    /// PostgreSQL's `pg_prng`: a permutation of every relation.
    #[test]
    fn tours_are_permutations() {
        let mut random = Random(PrngState::seeded(0.5));
        let mut tour = [0; 12];
        init_tour(&mut random, &mut tour);
        let mut sorted = tour;
        sorted.sort_unstable();
        assert_eq!(sorted, core::array::from_fn(|at| at as Gene + 1));
        for _ in 0..100 {
            assert!(linear_rand(&mut random, 10, 2.0) < 10);
        }
    }

    /// Edge recombination of two parents yields a permutation.
    #[test]
    fn edge_recombination_yields_a_permutation() {
        let mut random = Random(PrngState::seeded(0.0));
        let mut edges = [NO_EDGE; 21];
        let (mut one, mut two) = ([0; 20], [0; 20]);
        init_tour(&mut random, &mut one);
        init_tour(&mut random, &mut two);
        gimme_edge_table(&one, &two, &mut edges);
        let mut kid = [0; 20];
        gimme_tour(&mut random, &mut edges, &mut kid).unwrap();
        kid.sort_unstable();
        assert_eq!(kid, core::array::from_fn(|at| at as Gene + 1));
    }
}
