//! `pg_catalog` and `information_schema` as synthesized read-only tables.
//!
//! Drivers and ORMs introspect these to discover relations and columns.
//! Rather than store them, we materialize the rows on demand from the live
//! catalog into the statement arena and hand them to the normal query
//! pipeline as a synthetic table, so WHERE / projection / ORDER BY / LIMIT
//! and joins all work against them.

use crate::mem::arena::Arena;
use crate::util::StackStr;
use crate::{sql_err, stack_format};
use crate::storage::{ColumnMeta, SqlName, Storage, TableDef, MAX_COLUMNS};

use super::eval::{sqlstate, SqlError};
use super::types::{ColType, Datum};

/// A materialized catalog relation: its shape plus rows in the arena.
pub struct SynthTable<'a> {
    pub def: TableDef,
    pub rows: &'a [&'a [Datum<'a>]],
}

/// Stable per-name OIDs so a table's oid is consistent within a session.
/// User relations start above the reserved range.
const FIRST_USER_OID: i32 = 16384;
const PUBLIC_NS_OID: i32 = 2200;
const PG_CATALOG_NS_OID: i32 = 11;

pub fn is_catalog_relation(qualifier: Option<&str>, name: &str) -> bool {
    match qualifier {
        Some("pg_catalog") => true,
        Some("information_schema") => matches!(name, "tables" | "columns" | "schemata"),
        Some(_) => false,
        None => matches!(
            name,
            "pg_class"
                | "pg_attribute"
                | "pg_type"
                | "pg_namespace"
                | "pg_tables"
                | "pg_views"
                | "pg_roles"
                | "pg_database"
                | "pg_am"
                | "pg_index"
                | "pg_constraint"
                | "pg_attrdef"
                | "pg_collation"
                | "pg_policy"
                | "pg_rewrite"
                | "pg_trigger"
                | "pg_inherits"
                | "pg_statistic_ext"
                | "pg_publication"
                | "pg_publication_rel"
                | "pg_publication_namespace"
                | "pg_foreign_table"
                | "pg_foreign_server"
                | "pg_partitioned_table"
                | "pg_description"
                | "pg_enum"
                | "pg_range"
                | "pg_settings"
                | "pg_proc"
        ),
    }
}

/// Builds the requested catalog relation. `qualifier` is the schema (or
/// None). Allocates rows in `arena`.
pub fn synthesize<'a>(
    storage: &Storage,
    qualifier: Option<&str>,
    name: &'a str,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let info = qualifier == Some("information_schema");
    match (info, name) {
        (false, "pg_class") => pg_class(storage, arena),
        (false, "pg_attribute") => pg_attribute(storage, arena),
        (false, "pg_attrdef") => pg_attrdef(arena),
        (false, "pg_collation") => pg_collation(arena),
        (false, "pg_type") => pg_type(arena),
        (false, "pg_namespace") => pg_namespace(arena),
        (false, "pg_tables") => pg_tables(storage, arena),
        (false, "pg_am") => finish(
            def_of(
                "pg_am",
                &[
                    ("oid", ColType::Int4),
                    ("amname", ColType::Text),
                    ("amhandler", ColType::Int4),
                    ("amtype", ColType::Bpchar),
                ],
            ),
            &[
                row(&[Datum::Int4(403), text("btree", arena)?, Datum::Int4(0), text("i", arena)?], arena)?,
                row(&[Datum::Int4(405), text("hash", arena)?, Datum::Int4(0), text("i", arena)?], arena)?,
            ],
            arena,
        ),
        (false, "pg_constraint") => pg_constraint(storage, arena),
        (false, "pg_index") => pg_index(storage, arena),
        (false, "pg_policy") => finish(
            def_of(
                "pg_policy",
                &[
                    ("oid", ColType::Int4),
                    ("polname", ColType::Text),
                    ("polrelid", ColType::Int4),
                    ("polcmd", ColType::Bpchar),
                    ("polpermissive", ColType::Bool),
                    ("polroles", ColType::Array(super::types::ArrElem::Int4)),
                    ("polqual", ColType::Text),
                    ("polwithcheck", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_statistic_ext") => finish(
            def_of(
                "pg_statistic_ext",
                &[
                    ("oid", ColType::Int4),
                    ("stxrelid", ColType::Int4),
                    ("stxnamespace", ColType::Int4),
                    ("stxname", ColType::Text),
                    ("stxkind", ColType::Array(super::types::ArrElem::Text)),
                    ("stxstattarget", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_publication") => finish(
            def_of(
                "pg_publication",
                &[
                    ("oid", ColType::Int4),
                    ("pubname", ColType::Text),
                    ("puballtables", ColType::Bool),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_publication_namespace") => finish(
            def_of(
                "pg_publication_namespace",
                &[("pnpubid", ColType::Int4), ("pnnspid", ColType::Int4)],
            ),
            &[],
            arena,
        ),
        (false, "pg_publication_rel") => finish(
            def_of(
                "pg_publication_rel",
                &[
                    ("prpubid", ColType::Int4),
                    ("prrelid", ColType::Int4),
                    ("prqual", ColType::Text),
                    ("prattrs", ColType::Array(super::types::ArrElem::Int4)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_inherits") => finish(
            def_of(
                "pg_inherits",
                &[
                    ("inhrelid", ColType::Int4),
                    ("inhparent", ColType::Int4),
                    ("inhseqno", ColType::Int4),
                    ("inhdetachpending", ColType::Bool),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_rewrite") => finish(
            def_of(
                "pg_rewrite",
                &[
                    ("oid", ColType::Int4),
                    ("rulename", ColType::Text),
                    ("ev_class", ColType::Int4),
                    ("ev_type", ColType::Bpchar),
                    ("is_instead", ColType::Bool),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_trigger") => finish(
            def_of(
                "pg_trigger",
                &[
                    ("oid", ColType::Int4),
                    ("tgname", ColType::Text),
                    ("tgrelid", ColType::Int4),
                    ("tgenabled", ColType::Bpchar),
                    ("tgisinternal", ColType::Bool),
                    ("tgconstraint", ColType::Int4),
                    ("tgfoid", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_foreign_table") => finish(
            def_of(
                "pg_foreign_table",
                &[
                    ("ftrelid", ColType::Int4),
                    ("ftserver", ColType::Int4),
                    ("ftoptions", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_foreign_server") => finish(
            def_of(
                "pg_foreign_server",
                &[
                    ("oid", ColType::Int4),
                    ("srvname", ColType::Text),
                    ("srvoptions", ColType::Array(super::types::ArrElem::Text)),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_partitioned_table") => finish(
            def_of(
                "pg_partitioned_table",
                &[
                    ("partrelid", ColType::Int4),
                    ("partstrat", ColType::Bpchar),
                    ("partattrs", ColType::Text),
                    ("partexprs", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_settings") => finish(
            def_of("pg_settings", &[("name", ColType::Text), ("setting", ColType::Text)]),
            &[
                row(&[text("max_index_keys", arena)?, text("32", arena)?], arena)?,
                row(&[text("max_identifier_length", arena)?, text("63", arena)?], arena)?,
                row(&[text("server_version", arena)?, text("18.4", arena)?], arena)?,
                row(&[text("server_encoding", arena)?, text("UTF8", arena)?], arena)?,
                row(&[text("standard_conforming_strings", arena)?, text("on", arena)?], arena)?,
            ],
            arena,
        ),
        (false, "pg_proc") => finish(
            def_of(
                "pg_proc",
                &[
                    ("oid", ColType::Int4),
                    ("proname", ColType::Text),
                    ("pronamespace", ColType::Int4),
                    ("prorettype", ColType::Int4),
                    ("prokind", ColType::Bpchar),
                    ("proargtypes", ColType::Text),
                    ("prosrc", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_description") => finish(
            def_of(
                "pg_description",
                &[
                    ("objoid", ColType::Int4),
                    ("classoid", ColType::Int4),
                    ("objsubid", ColType::Int4),
                    ("description", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_enum") => finish(
            def_of(
                "pg_enum",
                &[
                    ("oid", ColType::Int4),
                    ("enumtypid", ColType::Int4),
                    ("enumsortorder", ColType::Float8),
                    ("enumlabel", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_range") => finish(
            def_of(
                "pg_range",
                &[("rngtypid", ColType::Int4), ("rngsubtype", ColType::Int4)],
            ),
            &[],
            arena,
        ),
        (false, "pg_views") | (false, "pg_roles") | (false, "pg_database") => {
            empty_like(name, storage, arena)
        }
        (true, "tables") => info_tables(storage, arena),
        (true, "columns") => info_columns(storage, arena),
        (true, "schemata") => info_schemata(arena),
        _ => Err(sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "catalog relation \"{}\" is not implemented",
            name
        )),
    }
}

/// Deterministic oid for a live table: slot index offset into the user
/// range (stable for a running process).
fn table_oid(_storage: &Storage, slot: usize) -> i32 {
    FIRST_USER_OID + slot as i32
}

/// Index relations get OIDs from a separate range so they never collide with
/// table OIDs; `pos` is the index's position within its table's index list.
const FIRST_INDEX_OID: i32 = 90_000;
const MAX_INDEXES_PER_TABLE: i32 = 64;
fn index_oid(slot: usize, pos: usize) -> i32 {
    FIRST_INDEX_OID + slot as i32 * MAX_INDEXES_PER_TABLE + pos as i32
}

/// One materialized index relation (implicit primary-key / unique index from a
/// constraint, or an explicit `CREATE INDEX`).
#[derive(Clone, Copy)]
struct IdxInfo {
    oid: i32,
    table_oid: i32,
    table_slot: usize,
    name: StackStr<64>,
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    n_cols: usize,
    is_primary: bool,
    is_unique: bool,
}

const MAX_SYNTH_INDEXES: usize = 256;

/// Enumerates every index relation psql `\d` would show: a single-column PK or
/// UNIQUE (from column flags), a multi-column PK/UNIQUE (from `uniques`), and
/// explicit `CREATE INDEX`es. OIDs are assigned by table slot + position so the
/// same index resolves identically here and in `pg_get_indexdef`.
fn collect_indexes(storage: &Storage, out: &mut [Option<IdxInfo>; MAX_SYNTH_INDEXES]) -> usize {
    let mut n = 0;
    let mut push = |info: IdxInfo, n: &mut usize| {
        if *n < MAX_SYNTH_INDEXES {
            out[*n] = Some(info);
            *n += 1;
        }
    };
    for (slot, table) in storage.live_tables() {
        let def = &table.def;
        let table_name = def.name.as_str();
        let toid = table_oid(storage, slot);
        let mut pos = 0usize;
        let mut mk = |columns: &[u16], is_primary: bool, is_unique: bool, name: StackStr<64>| {
            let mut c = [0u16; crate::storage::MAX_INDEX_COLS];
            c[..columns.len()].copy_from_slice(columns);
            let info = IdxInfo {
                oid: index_oid(slot, pos),
                table_oid: toid,
                table_slot: slot,
                name,
                columns: c,
                n_cols: columns.len(),
                is_primary,
                is_unique,
            };
            pos += 1;
            info
        };
        // Single-column PK / UNIQUE carried as column flags.
        for (ci, col) in def.columns().iter().enumerate() {
            if col.primary {
                let name = stack_str_64(stack_format!(64, "{}_pkey", table_name).as_str());
                push(mk(&[ci as u16], true, true, name), &mut n);
            } else if col.unique {
                let name =
                    stack_str_64(stack_format!(64, "{}_{}_key", table_name, col.name.as_str()).as_str());
                push(mk(&[ci as u16], false, true, name), &mut n);
            }
        }
        // Multi-column PK / UNIQUE constraints.
        for uk in def.uniques() {
            push(mk(uk.columns(), uk.is_primary, true, stack_str_64(uk.name.as_str())), &mut n);
        }
        // Explicit CREATE INDEX on this table.
        for index in storage.live_indexes().filter(|i| i.table.as_str() == table_name) {
            push(
                mk(&index.columns[..index.n_cols], false, index.unique, stack_str_64(index.name.as_str())),
                &mut n,
            );
        }
    }
    n
}

fn stack_str_64(s: &str) -> StackStr<64> {
    let mut out = StackStr::<64>::new();
    let _ = core::fmt::Write::write_str(&mut out, s);
    out
}

/// The relation name for an OID, used to render `oid::regclass`. Resolves both
/// ordinary tables and synthesized index relations.
pub fn relname_text<'a>(
    storage: &Storage,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    for (slot, table) in storage.live_tables() {
        if table_oid(storage, slot) == oid {
            let bytes =
                arena.alloc_slice_copy(table.def.name.as_str().as_bytes()).map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    let mut idxs = [None; MAX_SYNTH_INDEXES];
    let n = collect_indexes(storage, &mut idxs);
    for info in idxs[..n].iter().flatten() {
        if info.oid == oid {
            let bytes =
                arena.alloc_slice_copy(info.name.as_str().as_bytes()).map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    Ok(None)
}

/// The OID of the relation named `name`, for `'relname'::regclass`. Resolves
/// ordinary tables and synthesized index relations; `None` if no such relation.
pub fn reloid_of_name(storage: &Storage, name: &str) -> Option<i32> {
    for (slot, table) in storage.live_tables() {
        if table.def.name.as_str() == name {
            return Some(table_oid(storage, slot));
        }
    }
    let mut idxs = [None; MAX_SYNTH_INDEXES];
    let n = collect_indexes(storage, &mut idxs);
    idxs[..n]
        .iter()
        .flatten()
        .find(|info| info.name.as_str() == name)
        .map(|info| info.oid)
}

/// One materialized foreign-key constraint.
#[derive(Clone, Copy)]
struct FkInfo {
    oid: i32,
    conrelid: i32,
    confrelid: i32,
    child_slot: usize,
    fk_index: usize,
    name: StackStr<64>,
}

const FIRST_FK_OID: i32 = 200_000;

/// Enumerates every foreign-key constraint, resolving each child/parent table to
/// its OID. A child whose parent no longer exists is skipped (it cannot be
/// rendered), matching that a dropped parent leaves no referential row.
fn collect_fkeys(storage: &Storage, out: &mut [Option<FkInfo>; MAX_SYNTH_INDEXES]) -> usize {
    let mut n = 0;
    for (slot, table) in storage.live_tables() {
        let conrelid = table_oid(storage, slot);
        for (i, fk) in table.def.fkeys().iter().enumerate() {
            let Some(pslot) = storage.find_table(fk.parent.as_str()) else {
                continue;
            };
            if n == MAX_SYNTH_INDEXES {
                break;
            }
            out[n] = Some(FkInfo {
                oid: FIRST_FK_OID + slot as i32 * MAX_INDEXES_PER_TABLE + i as i32,
                conrelid,
                confrelid: table_oid(storage, pslot),
                child_slot: slot,
                fk_index: i,
                name: stack_str_64(fk.name.as_str()),
            });
            n += 1;
        }
    }
    n
}

/// The `FOREIGN KEY (...) REFERENCES parent(...)` definition psql prints from
/// `pg_get_constraintdef` for a foreign-key constraint OID.
pub fn constraint_def_text<'a>(
    storage: &Storage,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let mut fks = [None; MAX_SYNTH_INDEXES];
    let n = collect_fkeys(storage, &mut fks);
    for info in fks[..n].iter().flatten() {
        if info.oid != oid {
            continue;
        }
        let child = &storage.table(info.child_slot).def;
        let fk = &child.fkeys()[info.fk_index];
        let Some(pslot) = storage.find_table(fk.parent.as_str()) else {
            return Ok(None);
        };
        let parent = &storage.table(pslot).def;
        let mut s = StackStr::<256>::new();
        use core::fmt::Write as _;
        let _ = s.write_str("FOREIGN KEY (");
        for (k, &c) in fk.columns[..fk.n_cols].iter().enumerate() {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            let _ = s.write_str(child.columns()[c as usize].name.as_str());
        }
        let _ = write!(s, ") REFERENCES {}(", fk.parent.as_str());
        for (k, &c) in fk.parent_cols[..fk.n_parent_cols].iter().enumerate() {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            let _ = s.write_str(parent.columns()[c as usize].name.as_str());
        }
        let _ = s.write_str(")");
        let _ = s.write_str(fk_action_suffix(fk.on_delete, "DELETE"));
        let _ = s.write_str(fk_action_suffix(fk.on_update, "UPDATE"));
        let bytes = arena.alloc_slice_copy(s.as_str().as_bytes()).map_err(|_| arena_full())?;
        return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
    }
    Ok(None)
}

/// PostgreSQL omits the clause for the default NO ACTION and spells the others.
fn fk_action_suffix(a: crate::storage::FkAction, event: &str) -> &'static str {
    use crate::storage::FkAction::*;
    match (a, event) {
        (NoAction, _) => "",
        (Restrict, "DELETE") => " ON DELETE RESTRICT",
        (Restrict, _) => " ON UPDATE RESTRICT",
        (Cascade, "DELETE") => " ON DELETE CASCADE",
        (Cascade, _) => " ON UPDATE CASCADE",
        (SetNull, "DELETE") => " ON DELETE SET NULL",
        (SetNull, _) => " ON UPDATE SET NULL",
        (SetDefault, "DELETE") => " ON DELETE SET DEFAULT",
        (SetDefault, _) => " ON UPDATE SET DEFAULT",
    }
}

/// The `btree (col, ...)` index definition psql extracts from `pg_get_indexdef`
/// (it takes everything after `USING`, or the whole string when absent).
pub fn index_def_text<'a>(
    storage: &Storage,
    oid: i32,
    col: usize,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let mut idxs = [None; MAX_SYNTH_INDEXES];
    let n = collect_indexes(storage, &mut idxs);
    for info in idxs[..n].iter().flatten() {
        if info.oid != oid {
            continue;
        }
        let def = &storage.table(info.table_slot).def;
        let col_name = |ci: usize| def.columns()[info.columns[ci] as usize].name.as_str();
        // `col > 0`: just the name of that 1-based indexed column.
        if col > 0 {
            let name = if col <= info.n_cols { col_name(col - 1) } else { return Ok(None) };
            let bytes = arena.alloc_slice_copy(name.as_bytes()).map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
        let mut s = StackStr::<256>::new();
        use core::fmt::Write as _;
        let _ = s.write_str("btree (");
        for k in 0..info.n_cols {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            let _ = s.write_str(col_name(k));
        }
        let _ = s.write_str(")");
        let bytes = arena.alloc_slice_copy(s.as_str().as_bytes()).map_err(|_| arena_full())?;
        return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
    }
    Ok(None)
}

fn def_of(name: &str, columns: &[(&str, ColType)]) -> TableDef {
    let mut def = TableDef {
        name: SqlName::parse(name).expect("catalog name fits"),
        columns: [ColumnMeta {
            name: SqlName::parse("").unwrap(),
            ctype: ColType::Bool,
            type_mod: -1,
            not_null: false,
            unique: false,
            primary: false,
            auto_increment: false,
            default_value: None,
        }; MAX_COLUMNS],
        n_columns: columns.len(),
        ..TableDef::empty()
    };
    for (i, (n, t)) in columns.iter().enumerate() {
        def.columns[i].name = SqlName::parse(n).expect("catalog column fits");
        def.columns[i].ctype = *t;
    }
    def
}

/// Allocates `rows` (each a slice already in the arena) as a row slice.
fn finish<'a>(
    def: TableDef,
    rows: &[&'a [Datum<'a>]],
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let rows = arena
        .alloc_slice_copy(rows)
        .map_err(|_| arena_full())?;
    Ok(SynthTable { def, rows: &*rows })
}

fn row<'a>(vals: &[Datum<'a>], arena: &'a Arena) -> Result<&'a [Datum<'a>], SqlError> {
    arena.alloc_slice_copy(vals).map(|r| &*r).map_err(|_| arena_full())
}

fn text<'a>(s: &str, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    arena.alloc_str(s).map(Datum::Text).map_err(|_| arena_full())
}

fn pg_class<'a>(storage: &Storage, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_class",
        &[
            ("oid", ColType::Int4),
            ("relname", ColType::Text),
            ("relnamespace", ColType::Int4),
            ("relkind", ColType::Bpchar),
            ("relnatts", ColType::Int4),
            ("reltuples", ColType::Float8),
            ("relpages", ColType::Int4),
            ("relam", ColType::Int4),
            ("relowner", ColType::Int4),
            ("relchecks", ColType::Int2),
            ("relhasindex", ColType::Bool),
            ("relhasrules", ColType::Bool),
            ("relhastriggers", ColType::Bool),
            ("relrowsecurity", ColType::Bool),
            ("relforcerowsecurity", ColType::Bool),
            ("relispartition", ColType::Bool),
            ("reltablespace", ColType::Int4),
            ("reloftype", ColType::Int4),
            ("reltoastrelid", ColType::Int4),
            ("relpersistence", ColType::Bpchar),
            ("relreplident", ColType::Bpchar),
        ],
    );
    let mut indexes = [None; MAX_SYNTH_INDEXES];
    let n_idx = collect_indexes(storage, &mut indexes);
    let mut out: [&[Datum]; 512] = [&[]; 512];
    let mut n = 0;
    for (slot, table) in storage.live_tables() {
        if n == out.len() {
            break;
        }
        let toid = table_oid(storage, slot);
        let has_index = indexes[..n_idx].iter().flatten().any(|i| i.table_oid == toid);
        let n_checks = table.def.n_checks as i32;
        out[n] = row(
            &[
                Datum::Int4(toid),
                text(table.def.name.as_str(), arena)?,
                Datum::Int4(PUBLIC_NS_OID),
                text("r", arena)?, // relkind: ordinary table
                Datum::Int4(table.def.n_columns as i32),
                Datum::Float8(table.rows.len() as f64),
                Datum::Int4(0), // relpages
                Datum::Int4(0),  // relam
                Datum::Int4(10), // relowner (a role oid)
                Datum::Int4(n_checks), // relchecks
                Datum::Bool(has_index),
                Datum::Bool(false), // relhasrules
                Datum::Bool(false), // relhastriggers
                Datum::Bool(false), // relrowsecurity
                Datum::Bool(false), // relforcerowsecurity
                Datum::Bool(false), // relispartition
                Datum::Int4(0),     // reltablespace
                Datum::Int4(0),     // reloftype
                Datum::Int4(0),     // reltoastrelid
                text("p", arena)?,  // relpersistence: permanent
                text("d", arena)?,  // relreplident: default
            ],
            arena,
        )?;
        n += 1;
    }
    // Each index is itself a relation (relkind 'i'), so psql's `\d` join
    // (pg_index i JOIN pg_class c2 ON i.indexrelid = c2.oid) finds its name.
    for info in indexes[..n_idx].iter().flatten() {
        if n == out.len() {
            break;
        }
        out[n] = row(
            &[
                Datum::Int4(info.oid),
                text(info.name.as_str(), arena)?,
                Datum::Int4(PUBLIC_NS_OID),
                text("i", arena)?, // relkind: index
                Datum::Int4(info.n_cols as i32),
                Datum::Float8(0.0),
                Datum::Int4(0), // relpages
                Datum::Int4(403), // relam: btree
                Datum::Int4(10),
                Datum::Int4(0), // relchecks
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                text("p", arena)?,
                text("d", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_constraint<'a>(storage: &Storage, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_constraint",
        &[
            ("oid", ColType::Int4),
            ("conname", ColType::Text),
            ("conrelid", ColType::Int4),
            ("contype", ColType::Bpchar),
            ("conparentid", ColType::Int4),
            ("conindid", ColType::Int4),
            ("confrelid", ColType::Int4),
            ("condeferrable", ColType::Bool),
            ("condeferred", ColType::Bool),
            ("convalidated", ColType::Bool),
            ("conperiod", ColType::Bool),
            ("confupdtype", ColType::Bpchar),
            ("confdeltype", ColType::Bpchar),
            ("conkey", ColType::Array(super::types::ArrElem::Int4)),
            ("confkey", ColType::Array(super::types::ArrElem::Int4)),
        ],
    );
    let mut indexes = [None; MAX_SYNTH_INDEXES];
    let n_idx = collect_indexes(storage, &mut indexes);
    let mut out: [&[Datum]; MAX_SYNTH_INDEXES] = [&[]; MAX_SYNTH_INDEXES];
    let mut n = 0;
    // A PRIMARY KEY or UNIQUE constraint has a backing index; its `conindid`
    // links to that index so psql's `\d` labels a UNIQUE index as a constraint.
    for info in indexes[..n_idx].iter().flatten() {
        let contype = if info.is_primary {
            "p"
        } else if info.is_unique {
            "u"
        } else {
            continue;
        };
        out[n] = row(
            &[
                Datum::Int4(info.oid + 500_000), // constraint oid, distinct from the index's
                text(info.name.as_str(), arena)?,
                Datum::Int4(info.table_oid),
                text(contype, arena)?,
                Datum::Int4(0), // conparentid
                Datum::Int4(info.oid), // conindid -> the backing index
                Datum::Int4(0), // confrelid
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(true), // convalidated
                Datum::Bool(false), // conperiod
                text(" ", arena)?, // confupdtype (n/a for non-FK)
                text(" ", arena)?, // confdeltype
                attnum_array(&info.columns[..info.n_cols], arena)?,
                empty_int_array(arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    // Foreign-key constraints (contype 'f'): conrelid on the child, confrelid on
    // the referenced parent, so psql's "Foreign-key constraints" (child) and
    // "Referenced by" (parent) sections both resolve.
    let mut fks = [None; MAX_SYNTH_INDEXES];
    let n_fk = collect_fkeys(storage, &mut fks);
    for info in fks[..n_fk].iter().flatten() {
        if n == out.len() {
            break;
        }
        let fk = &storage.table(info.child_slot).def.fkeys()[info.fk_index];
        // conindid points at the parent's unique/PK index backing the referenced
        // columns, which JDBC joins to for foreign-key metadata.
        let conindid = indexes[..n_idx]
            .iter()
            .flatten()
            .find(|ix| {
                ix.table_oid == info.confrelid
                    && ix.n_cols == fk.n_parent_cols
                    && ix.columns[..ix.n_cols] == fk.parent_cols[..fk.n_parent_cols]
            })
            .map_or(0, |ix| ix.oid);
        out[n] = row(
            &[
                Datum::Int4(info.oid),
                text(info.name.as_str(), arena)?,
                Datum::Int4(info.conrelid),
                text("f", arena)?,
                Datum::Int4(0), // conparentid
                Datum::Int4(conindid),
                Datum::Int4(info.confrelid),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(false),
                text(fk_action_char(fk.on_update), arena)?,
                text(fk_action_char(fk.on_delete), arena)?,
                attnum_array(&fk.columns[..fk.n_cols], arena)?,
                attnum_array(&fk.parent_cols[..fk.n_parent_cols], arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

/// PostgreSQL's `confupdtype`/`confdeltype` code for a referential action.
fn fk_action_char(a: crate::storage::FkAction) -> &'static str {
    use crate::storage::FkAction::*;
    match a {
        NoAction => "a",
        Restrict => "r",
        Cascade => "c",
        SetNull => "n",
        SetDefault => "d",
    }
}

/// A `Datum::Array` of 1-based attribute numbers (column index + 1), the form
/// `conkey`/`confkey`/`indkey`-as-array take in `pg_constraint`.
fn attnum_array<'a>(columns: &[u16], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let mut vals = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (i, &c) in columns.iter().enumerate() {
        vals[i] = Datum::Int4(i32::from(c) + 1);
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&vals[..columns.len()], arena)?,
    })
}

fn empty_int_array<'a>(arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&[], arena)?,
    })
}

fn pg_index<'a>(storage: &Storage, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_index",
        &[
            ("indexrelid", ColType::Int4),
            ("indrelid", ColType::Int4),
            ("indisprimary", ColType::Bool),
            ("indisunique", ColType::Bool),
            ("indisclustered", ColType::Bool),
            ("indisvalid", ColType::Bool),
            ("indisreplident", ColType::Bool),
            ("indnatts", ColType::Int4),
            ("indkey", ColType::Array(super::types::ArrElem::Int4)),
            ("indoption", ColType::Array(super::types::ArrElem::Int4)),
        ],
    );
    let mut indexes = [None; MAX_SYNTH_INDEXES];
    let n_idx = collect_indexes(storage, &mut indexes);
    let mut out: [&[Datum]; MAX_SYNTH_INDEXES] = [&[]; MAX_SYNTH_INDEXES];
    let mut n = 0;
    for info in indexes[..n_idx].iter().flatten() {
        // indkey is the 1-based attribute numbers as an int2vector-like array;
        // indoption is one flag per column (0 = default ascending).
        let zeros = [0u16; crate::storage::MAX_INDEX_COLS];
        out[n] = row(
            &[
                Datum::Int4(info.oid),
                Datum::Int4(info.table_oid),
                Datum::Bool(info.is_primary),
                Datum::Bool(info.is_unique),
                Datum::Bool(false), // indisclustered
                Datum::Bool(true),  // indisvalid
                Datum::Bool(false), // indisreplident
                Datum::Int4(info.n_cols as i32),
                attnum_array(&info.columns[..info.n_cols], arena)?,
                option_array(&zeros[..info.n_cols], arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

/// An index-option array (one 0-flag per column) for `pg_index.indoption`.
fn option_array<'a>(columns: &[u16], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let mut vals = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (i, _) in columns.iter().enumerate() {
        vals[i] = Datum::Int4(0);
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&vals[..columns.len()], arena)?,
    })
}

fn pg_attribute<'a>(storage: &Storage, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_attribute",
        &[
            ("attrelid", ColType::Int4),
            ("attname", ColType::Text),
            ("atttypid", ColType::Int4),
            ("attnum", ColType::Int4),
            ("attnotnull", ColType::Bool),
            ("attlen", ColType::Int4),
            ("atttypmod", ColType::Int4),
            ("atthasdef", ColType::Bool),
            ("attcollation", ColType::Int4),
            ("attidentity", ColType::Bpchar),
            ("attgenerated", ColType::Bpchar),
            ("attisdropped", ColType::Bool),
            ("attnum_ord", ColType::Int4),
        ],
    );
    let mut out: [&[Datum]; 1024] = [&[]; 1024];
    let mut n = 0;
    for (slot, table) in storage.live_tables() {
        for (i, c) in table.def.columns().iter().enumerate() {
            if n == out.len() {
                break;
            }
            out[n] = row(
                &[
                    Datum::Int4(table_oid(storage, slot)),
                    text(c.name.as_str(), arena)?,
                    Datum::Int4(c.ctype.oid()),
                    Datum::Int4(i as i32 + 1),
                    Datum::Bool(c.not_null),
                    Datum::Int4(i32::from(c.ctype.typlen())),
                    Datum::Int4(c.type_mod),
                    Datum::Bool(c.default_value.is_some()),
                    Datum::Int4(0), // attcollation: default (0)
                    text("", arena)?, // attidentity
                    text("", arena)?, // attgenerated
                    Datum::Bool(false), // attisdropped
                    Datum::Int4(i as i32 + 1),
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn pg_attrdef<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    // We do not reconstruct default expressions for \d, so this stays empty.
    finish(
        def_of(
            "pg_attrdef",
            &[
                ("oid", ColType::Int4),
                ("adrelid", ColType::Int4),
                ("adnum", ColType::Int4),
                ("adbin", ColType::Text),
            ],
        ),
        &[],
        arena,
    )
}

fn pg_collation<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    finish(
        def_of(
            "pg_collation",
            &[("oid", ColType::Int4), ("collname", ColType::Text)],
        ),
        &[],
        arena,
    )
}

fn pg_type<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_type",
        &[
            ("oid", ColType::Int4),
            ("typname", ColType::Text),
            ("typlen", ColType::Int4),
            ("typcollation", ColType::Int4),
            ("typnamespace", ColType::Int4),
            ("typtype", ColType::Bpchar),   // 'b' = base type
            ("typcategory", ColType::Bpchar),
            ("typbasetype", ColType::Int4), // 0 unless a domain
            ("typelem", ColType::Int4),     // element type of an array, else 0
            ("typarray", ColType::Int4),    // the array type over this type, else 0
            ("typrelid", ColType::Int4),    // 0 unless a composite type
            ("typtypmod", ColType::Int4),
            ("typnotnull", ColType::Bool),
            ("typdefault", ColType::Text),
            ("typinput", ColType::Text),
            ("typoutput", ColType::Text),
        ],
    );
    let types = [
        ColType::Bool,
        ColType::Int4,
        ColType::Int8,
        ColType::Float8,
        ColType::Text,
        ColType::Date,
        ColType::Timestamp,
        ColType::Timestamptz,
        ColType::Uuid,
        ColType::Bytea,
        ColType::Numeric,
        ColType::Int2,
        ColType::Float4,
        ColType::Time,
        ColType::Interval,
    ];
    let category = |t: &ColType| match t {
        ColType::Bool => "B",
        ColType::Int2 | ColType::Int4 | ColType::Int8 | ColType::Float4 | ColType::Float8
        | ColType::Numeric => "N",
        ColType::Date | ColType::Time | ColType::Timestamp | ColType::Timestamptz => "D",
        ColType::Interval => "T",
        ColType::Uuid => "U",
        ColType::Bytea => "U",
        _ => "S",
    };
    // `int2`/`float4` report `int4`/`float8` on the wire (their `oid()`), but in
    // the catalog they need their own distinct OIDs so a join on `atttypid` does
    // not match two `pg_type` rows.
    let catalog_oid = |t: &ColType| match t {
        ColType::Int2 => super::types::oid::INT2,
        ColType::Float4 => super::types::oid::FLOAT4,
        _ => t.oid(),
    };
    let mut out: [&[Datum]; 32] = [&[]; 32];
    for (i, t) in types.iter().enumerate() {
        out[i] = row(
            &[
                Datum::Int4(catalog_oid(t)),
                text(t.internal_name(), arena)?,
                Datum::Int4(i32::from(t.typlen())),
                Datum::Int4(0), // typcollation: none
                Datum::Int4(PG_CATALOG_NS_OID),
                text("b", arena)?,
                text(category(t), arena)?,
                Datum::Int4(0), // typbasetype
                Datum::Int4(0), // typelem
                Datum::Int4(0), // typarray
                Datum::Int4(0), // typrelid
                Datum::Int4(-1), // typtypmod
                Datum::Bool(false),
                Datum::Null, // typdefault
                text("", arena)?,
                text("", arena)?,
            ],
            arena,
        )?;
    }
    finish(def, &out[..types.len()], arena)
}

fn pg_namespace<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_namespace",
        &[("oid", ColType::Int4), ("nspname", ColType::Text)],
    );
    let rows = [
        row(&[Datum::Int4(PG_CATALOG_NS_OID), text("pg_catalog", arena)?], arena)?,
        row(&[Datum::Int4(PUBLIC_NS_OID), text("public", arena)?], arena)?,
    ];
    finish(def, &rows, arena)
}

fn pg_tables<'a>(storage: &Storage, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_tables",
        &[
            ("schemaname", ColType::Text),
            ("tablename", ColType::Text),
            ("tableowner", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for (_, table) in storage.live_tables() {
        if n == out.len() {
            break;
        }
        out[n] = row(
            &[
                text("public", arena)?,
                text(table.def.name.as_str(), arena)?,
                text("pos3ql", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn info_tables<'a>(storage: &Storage, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "tables",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("table_type", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for (_, table) in storage.live_tables() {
        if n == out.len() {
            break;
        }
        out[n] = row(
            &[
                text("postgres", arena)?,
                text("public", arena)?,
                text(table.def.name.as_str(), arena)?,
                text("BASE TABLE", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn info_columns<'a>(storage: &Storage, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "columns",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("ordinal_position", ColType::Int4),
            ("is_nullable", ColType::Text),
            ("data_type", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 1024] = [&[]; 1024];
    let mut n = 0;
    for (_, table) in storage.live_tables() {
        for (i, c) in table.def.columns().iter().enumerate() {
            if n == out.len() {
                break;
            }
            out[n] = row(
                &[
                    text("postgres", arena)?,
                    text("public", arena)?,
                    text(table.def.name.as_str(), arena)?,
                    text(c.name.as_str(), arena)?,
                    Datum::Int4(i as i32 + 1),
                    text(if c.not_null { "NO" } else { "YES" }, arena)?,
                    text(c.ctype.name(), arena)?,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn info_schemata<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "schemata",
        &[
            ("catalog_name", ColType::Text),
            ("schema_name", ColType::Text),
        ],
    );
    let rows = [
        row(&[text("postgres", arena)?, text("public", arena)?], arena)?,
        row(&[text("postgres", arena)?, text("pg_catalog", arena)?], arena)?,
        row(
            &[text("postgres", arena)?, text("information_schema", arena)?],
            arena,
        )?,
    ];
    finish(def, &rows, arena)
}

fn empty_like<'a>(
    name: &str,
    _storage: &Storage,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    // A single-column empty relation is enough for existence probes.
    let def = def_of(name, &[("oid", ColType::Int4)]);
    finish(def, &[], arena)
}

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "catalog relation exceeds the statement arena"
    )
}
