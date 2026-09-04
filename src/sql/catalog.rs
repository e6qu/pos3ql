//! `pg_catalog` and `information_schema` as synthesized read-only tables.
//!
//! Drivers and ORMs introspect these to discover relations and columns.
//! Rather than store them, we materialize the rows on demand from the live
//! catalog into the statement arena and hand them to the normal query
//! pipeline as a synthetic table, so WHERE / projection / ORDER BY / LIMIT
//! and joins all work against them.

use crate::mem::arena::Arena;
use crate::storage::{
    ColumnMeta, MAX_COLUMNS, MAX_ROUTINE_ARGUMENTS, OwnedDatum, PartitionBound,
    PartitionBoundValue, PartitionStrategy, PolicyCommandKind, SqlName, Storage, TableDef,
};
use crate::util::StackStr;
use crate::{sql_err, stack_format};

use super::eval::{
    ColumnLookup, SqlError, datum_to_text, ident_needs_quotes, quote_literal_str,
    resolved_expression_collation, sqlstate,
};
use super::types::{ColType, Datum, TypeMod};

/// A materialized catalog relation: its shape plus rows in the arena.
pub struct SynthTable<'a> {
    pub def: &'a TableDef,
    pub rows: &'a [&'a [Datum<'a>]],
}

/// Stable per-name OIDs so a table's oid is consistent within a session.
/// User relations start above the reserved range.
const FIRST_USER_OID: i32 = 16384;
const PUBLIC_NS_OID: i32 = 2200;
const PG_CATALOG_NS_OID: i32 = 11;
const PG_TOAST_NS_OID: i32 = 99;
/// Well-known catalog OIDs, for `pg_description.classoid`.
pub(crate) const PG_CLASS_OID: i32 = 1259;
pub(crate) const PG_NAMESPACE_OID: i32 = 2615;
pub(crate) const PG_TYPE_OID: i32 = 1247;
pub(crate) const PG_PROC_OID: i32 = 1255;
pub(crate) const PG_AM_OID: i32 = 2601;
pub(crate) const PG_LANGUAGE_OID: i32 = 2612;
const PG_AMOP_OID: i32 = 2602;
const PG_AMPROC_OID: i32 = 2603;
pub(crate) const PG_CAST_OID: i32 = 2605;
pub(crate) const PG_CONSTRAINT_OID: i32 = 2606;
pub(crate) const PG_CONVERSION_OID: i32 = 2607;
pub(crate) const PG_OPCLASS_OID: i32 = 2616;
pub(crate) const PG_OPERATOR_OID: i32 = 2617;
pub(crate) const PG_OPFAMILY_OID: i32 = 2753;
pub(crate) const PG_COLLATION_OID: i32 = 3456;
pub(crate) const PG_TS_DICT_OID: i32 = 3600;
pub(crate) const PG_TS_PARSER_OID: i32 = 3601;
pub(crate) const PG_TS_CONFIG_OID: i32 = 3602;
pub(crate) const PG_TS_TEMPLATE_OID: i32 = 3764;
pub(crate) const PG_REWRITE_OID: i32 = 2618;
pub(crate) const PG_LARGEOBJECT_METADATA_OID: i32 = 2995;
pub(crate) const PG_LARGEOBJECT_OID: i32 = 2613;
pub(crate) const PG_TRIGGER_OID: i32 = 2620;
pub(crate) const PG_TABLESPACE_OID: i32 = 1213;
pub(crate) const PG_POLICY_OID: i32 = 3256;
pub(crate) const PG_STATISTIC_EXT_OID: i32 = 3381;
pub(crate) const PG_EXTENSION_OID: i32 = 3079;
pub(crate) const PG_PUBLICATION_OID: i32 = 6104;
pub(crate) const PG_SUBSCRIPTION_OID: i32 = 6107;

const ACCESS_METHODS: [(&str, i32, i32, &str, &str); 7] = [
    ("heap", 2, 3, "heap_tableam_handler", "t"),
    ("btree", 403, 330, "bthandler", "i"),
    ("hash", 405, 331, "hashhandler", "i"),
    ("gist", 783, 332, "gisthandler", "i"),
    ("gin", 2742, 333, "ginhandler", "i"),
    ("brin", 3580, 335, "brinhandler", "i"),
    ("spgist", 4000, 334, "spghandler", "i"),
];
const INTERNAL_LANGUAGE_OID: i32 = 12;
const C_LANGUAGE_OID: i32 = 13;
const SQL_LANGUAGE_OID: i32 = 14;
const PLPGSQL_LANGUAGE_OID: i32 = 13_563;
const PROCEDURAL_LANGUAGES: [(&str, i32); 4] = [
    ("internal", INTERNAL_LANGUAGE_OID),
    ("c", C_LANGUAGE_OID),
    ("sql", SQL_LANGUAGE_OID),
    ("plpgsql", PLPGSQL_LANGUAGE_OID),
];

pub(crate) fn access_method_oid(name: &str) -> Option<i32> {
    ACCESS_METHODS
        .iter()
        .find_map(|(candidate, oid, _, _, _)| (*candidate == name).then_some(*oid))
}

pub(crate) fn access_method_name(oid: i32) -> Option<&'static str> {
    ACCESS_METHODS
        .iter()
        .find_map(|(name, candidate, _, _, _)| (*candidate == oid).then_some(*name))
}

pub(crate) fn access_method_oid_in(storage: &Storage, txid: u32, name: &str) -> Option<i32> {
    access_method_oid(name).or_else(|| {
        storage
            .access_method_slot(name, txid)
            .and_then(|slot| {
                storage
                    .access_methods_visible_to(txid)
                    .find(|(candidate, _)| *candidate == slot)
            })
            .map(|(_, method)| method.oid().get())
    })
}

pub(crate) fn access_method_name_in(storage: &Storage, txid: u32, oid: i32) -> Option<&str> {
    access_method_name(oid).or_else(|| {
        crate::storage::AccessMethodOid::parse(oid).and_then(|oid| {
            storage
                .access_methods_visible_to(txid)
                .find(|(_, method)| method.oid() == oid)
                .map(|(_, method)| method.definition.name.as_str())
        })
    })
}

pub(crate) fn procedural_language_oid(name: &str) -> Option<i32> {
    PROCEDURAL_LANGUAGES
        .iter()
        .find_map(|(candidate, oid)| (*candidate == name).then_some(*oid))
}

pub(crate) fn procedural_language_name(oid: i32) -> Option<&'static str> {
    PROCEDURAL_LANGUAGES
        .iter()
        .find_map(|(name, candidate)| (*candidate == oid).then_some(*name))
}

#[derive(Clone, Copy)]
struct IntrinsicRoutine {
    oid: i32,
    name: &'static str,
    result_oid: i32,
    argument_types: &'static str,
    argument_count: i32,
    volatility: &'static str,
}

const INTRINSIC_ROUTINES: &[IntrinsicRoutine] = &[
    IntrinsicRoutine {
        oid: 540_000,
        name: "postgres_fdw_handler",
        result_oid: super::types::oid::FDW_HANDLER,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 540_001,
        name: "postgres_fdw_validator",
        result_oid: super::types::oid::VOID,
        argument_types: "1009 26",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 715,
        name: "lo_create",
        result_oid: 26,
        argument_types: "26",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 764,
        name: "lo_import",
        result_oid: 26,
        argument_types: "25",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 765,
        name: "lo_export",
        result_oid: 23,
        argument_types: "26 25",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 767,
        name: "lo_import",
        result_oid: 26,
        argument_types: "25 26",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 952,
        name: "lo_open",
        result_oid: 23,
        argument_types: "26 23",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 953,
        name: "lo_close",
        result_oid: 23,
        argument_types: "23",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 954,
        name: "loread",
        result_oid: 17,
        argument_types: "23 23",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 955,
        name: "lowrite",
        result_oid: 23,
        argument_types: "23 17",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 956,
        name: "lo_lseek",
        result_oid: 23,
        argument_types: "23 23 23",
        argument_count: 3,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 957,
        name: "lo_creat",
        result_oid: 26,
        argument_types: "23",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 958,
        name: "lo_tell",
        result_oid: 23,
        argument_types: "23",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 964,
        name: "lo_unlink",
        result_oid: 23,
        argument_types: "26",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1004,
        name: "lo_truncate",
        result_oid: 23,
        argument_types: "23 23",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3170,
        name: "lo_lseek64",
        result_oid: 20,
        argument_types: "23 20 23",
        argument_count: 3,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3171,
        name: "lo_tell64",
        result_oid: 20,
        argument_types: "23",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3172,
        name: "lo_truncate64",
        result_oid: 23,
        argument_types: "23 20",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3457,
        name: "lo_from_bytea",
        result_oid: 26,
        argument_types: "26 17",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3458,
        name: "lo_get",
        result_oid: 17,
        argument_types: "26",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3459,
        name: "lo_get",
        result_oid: 17,
        argument_types: "26 20 23",
        argument_count: 3,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3460,
        name: "lo_put",
        result_oid: 2278,
        argument_types: "26 20 17",
        argument_count: 3,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1644,
        name: "RI_FKey_check_ins",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1645,
        name: "RI_FKey_check_upd",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1646,
        name: "RI_FKey_cascade_del",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1647,
        name: "RI_FKey_cascade_upd",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1648,
        name: "RI_FKey_restrict_del",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1649,
        name: "RI_FKey_restrict_upd",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1650,
        name: "RI_FKey_setnull_del",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1651,
        name: "RI_FKey_setnull_upd",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1652,
        name: "RI_FKey_setdefault_del",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1653,
        name: "RI_FKey_setdefault_upd",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1654,
        name: "RI_FKey_noaction_del",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1655,
        name: "RI_FKey_noaction_upd",
        result_oid: 2279,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3717,
        name: "prsd_start",
        result_oid: 2281,
        argument_types: "2281 23",
        argument_count: 2,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 3718,
        name: "prsd_nexttoken",
        result_oid: 2281,
        argument_types: "2281 2281 2281",
        argument_count: 3,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 3719,
        name: "prsd_end",
        result_oid: 2278,
        argument_types: "2281",
        argument_count: 1,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 3720,
        name: "prsd_headline",
        result_oid: 2281,
        argument_types: "2281 2281 3615",
        argument_count: 3,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 3721,
        name: "prsd_lextype",
        result_oid: 2281,
        argument_types: "2281",
        argument_count: 1,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 3725,
        name: "dsimple_init",
        result_oid: 2281,
        argument_types: "2281",
        argument_count: 1,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 3726,
        name: "dsimple_lexize",
        result_oid: 2281,
        argument_types: "2281 2281 2281 2281",
        argument_count: 4,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 4568,
        name: "pg_event_trigger_ddl_commands",
        result_oid: super::types::oid::RECORD,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 3566,
        name: "pg_event_trigger_dropped_objects",
        result_oid: super::types::oid::RECORD,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 4566,
        name: "pg_event_trigger_table_rewrite_oid",
        result_oid: 26,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 4567,
        name: "pg_event_trigger_table_rewrite_reason",
        result_oid: 23,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 3415,
        name: "pg_get_statisticsobjdef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 6173,
        name: "pg_get_statisticsobjdef_expressions",
        result_oid: 1009,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 6174,
        name: "pg_get_statisticsobjdef_columns",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 89,
        name: "version",
        result_oid: 25,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 861,
        name: "current_database",
        result_oid: 19,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1081,
        name: "format_type",
        result_oid: 25,
        argument_types: "26 23",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1215,
        name: "obj_description",
        result_oid: 25,
        argument_types: "26 19",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1216,
        name: "col_description",
        result_oid: 25,
        argument_types: "26 23",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1264,
        name: "pg_char_to_encoding",
        result_oid: 23,
        argument_types: "19",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1387,
        name: "pg_get_constraintdef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1402,
        name: "current_schema",
        result_oid: 19,
        argument_types: "",
        argument_count: 0,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1403,
        name: "current_schemas",
        result_oid: 1003,
        argument_types: "16",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1597,
        name: "pg_encoding_to_char",
        result_oid: 19,
        argument_types: "23",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1641,
        name: "pg_get_viewdef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1642,
        name: "pg_get_userbyid",
        result_oid: 19,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1643,
        name: "pg_get_indexdef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1714,
        name: "convert_from",
        result_oid: 25,
        argument_types: "17 19",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1717,
        name: "convert_to",
        result_oid: 17,
        argument_types: "25 19",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1813,
        name: "convert",
        result_oid: 17,
        argument_types: "17 19 19",
        argument_count: 3,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 4374,
        name: "iso8859_1_to_utf8",
        result_oid: 23,
        argument_types: "23 23 2275 2281 23 16",
        argument_count: 6,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 4375,
        name: "utf8_to_iso8859_1",
        result_oid: 23,
        argument_types: "23 23 2275 2281 23 16",
        argument_count: 6,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 1662,
        name: "pg_get_triggerdef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2730,
        name: "pg_get_triggerdef",
        result_oid: 25,
        argument_types: "26 16",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1716,
        name: "pg_get_expr",
        result_oid: 25,
        argument_types: "194 26",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1598,
        name: "random",
        result_oid: 701,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 1599,
        name: "setseed",
        result_oid: 2278,
        argument_types: "701",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 2077,
        name: "current_setting",
        result_oid: 25,
        argument_types: "25",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 3352,
        name: "pg_get_partkeydef",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2078,
        name: "set_config",
        result_oid: 25,
        argument_types: "25 25 16",
        argument_count: 3,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 2079,
        name: "pg_table_is_visible",
        result_oid: 16,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2080,
        name: "pg_type_is_visible",
        result_oid: 16,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2081,
        name: "pg_function_is_visible",
        result_oid: 16,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2168,
        name: "pg_database_size",
        result_oid: 20,
        argument_types: "19",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 2322,
        name: "pg_tablespace_size",
        result_oid: 20,
        argument_types: "26",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 2997,
        name: "pg_table_size",
        result_oid: 20,
        argument_types: "2205",
        argument_count: 1,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3086,
        name: "pg_extension_config_dump",
        result_oid: 2278,
        argument_types: "2205 25",
        argument_count: 2,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3778,
        name: "pg_tablespace_location",
        result_oid: 25,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 3075,
        name: "pg_reload_conf",
        result_oid: 16,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3810,
        name: "pg_is_in_recovery",
        result_oid: 16,
        argument_types: "",
        argument_count: 0,
        volatility: "v",
    },
    IntrinsicRoutine {
        oid: 3815,
        name: "pg_collation_is_visible",
        result_oid: 16,
        argument_types: "26",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 6121,
        name: "pg_relation_is_publishable",
        result_oid: 16,
        argument_types: "2205",
        argument_count: 1,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1158,
        name: "to_timestamp",
        result_oid: 1184,
        argument_types: "701",
        argument_count: 1,
        volatility: "i",
    },
    IntrinsicRoutine {
        oid: 1768,
        name: "to_char",
        result_oid: 25,
        argument_types: "1186 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1770,
        name: "to_char",
        result_oid: 25,
        argument_types: "1184 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1772,
        name: "to_char",
        result_oid: 25,
        argument_types: "1700 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1773,
        name: "to_char",
        result_oid: 25,
        argument_types: "23 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1774,
        name: "to_char",
        result_oid: 25,
        argument_types: "20 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1775,
        name: "to_char",
        result_oid: 25,
        argument_types: "700 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1776,
        name: "to_char",
        result_oid: 25,
        argument_types: "701 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1777,
        name: "to_number",
        result_oid: 1700,
        argument_types: "25 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1778,
        name: "to_timestamp",
        result_oid: 1184,
        argument_types: "25 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 1780,
        name: "to_date",
        result_oid: 1082,
        argument_types: "25 25",
        argument_count: 2,
        volatility: "s",
    },
    IntrinsicRoutine {
        oid: 2049,
        name: "to_char",
        result_oid: 25,
        argument_types: "1114 25",
        argument_count: 2,
        volatility: "s",
    },
];

fn intrinsic_routine_is_strict(routine: IntrinsicRoutine) -> bool {
    !matches!(routine.oid, 1081 | 2078 | 3566 | 4568)
}

fn intrinsic_routine_parallel(routine: IntrinsicRoutine) -> &'static str {
    match routine.oid {
        715 | 764 | 765 | 767 | 952 | 953 | 954 | 955 | 956 | 957 | 958 | 964 | 1004 | 3170
        | 3171 | 3172 | 3457 | 3458 | 3459 | 3460 | 1402 | 1403 | 2078 | 3086 => "u",
        1641 | 3566 | 4568 => "r",
        _ => "s",
    }
}

const DDL_COMMAND_OUTPUT_OIDS: &[i32] = &[26, 26, 23, 25, 25, 25, 25, 16, 32];
const DDL_COMMAND_OUTPUT_NAMES: &[&str] = &[
    "classid",
    "objid",
    "objsubid",
    "command_tag",
    "object_type",
    "schema_name",
    "object_identity",
    "in_extension",
    "command",
];
const DROPPED_OBJECT_OUTPUT_OIDS: &[i32] = &[26, 26, 23, 16, 16, 16, 25, 25, 25, 25, 1009, 1009];
const DROPPED_OBJECT_OUTPUT_NAMES: &[&str] = &[
    "classid",
    "objid",
    "objsubid",
    "original",
    "normal",
    "is_temporary",
    "object_type",
    "schema_name",
    "object_name",
    "object_identity",
    "address_names",
    "address_args",
];

fn intrinsic_record_outputs(
    routine: IntrinsicRoutine,
) -> Option<(&'static [i32], &'static [&'static str])> {
    match routine.oid {
        4568 => Some((DDL_COMMAND_OUTPUT_OIDS, DDL_COMMAND_OUTPUT_NAMES)),
        3566 => Some((DROPPED_OBJECT_OUTPUT_OIDS, DROPPED_OBJECT_OUTPUT_NAMES)),
        _ => None,
    }
}

/// PostgreSQL 18.4 operator OIDs for the evaluator's scalar integer core.
/// The resolver deliberately exposes only operations the engine evaluates;
/// unsupported operator catalog entries remain explicit errors, never fake
/// catalog rows.
#[derive(Clone, Copy)]
struct CatalogOperator {
    oid: i32,
    name: &'static str,
    left: ColType,
    right: ColType,
}

const CATALOG_OPERATORS: &[CatalogOperator] = &[
    CatalogOperator {
        oid: 96,
        name: "=",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 97,
        name: "<",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 518,
        name: "<>",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 521,
        name: ">",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 523,
        name: "<=",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 525,
        name: ">=",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 514,
        name: "*",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 528,
        name: "/",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 530,
        name: "%",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 551,
        name: "+",
        left: ColType::Int4,
        right: ColType::Int4,
    },
    CatalogOperator {
        oid: 555,
        name: "-",
        left: ColType::Int4,
        right: ColType::Int4,
    },
];

const CATALOG_RELATIONS: &[(&str, i32)] = &[
    ("pg_type", PG_TYPE_OID),
    ("pg_proc", PG_PROC_OID),
    ("pg_aggregate", 2600),
    ("pg_class", PG_CLASS_OID),
    ("pg_attribute", 1249),
    ("pg_amop", PG_AMOP_OID),
    ("pg_amproc", PG_AMPROC_OID),
    ("pg_cast", PG_CAST_OID),
    ("pg_constraint", 2606),
    ("pg_statistic_ext", 3381),
    ("pg_statistic_ext_data", 3429),
    ("pg_collation", PG_COLLATION_OID),
    ("pg_conversion", PG_CONVERSION_OID),
    ("pg_depend", 2608),
    ("pg_rewrite", 2618),
    ("pg_largeobject", 2613),
    ("pg_largeobject_metadata", 2995),
    ("pg_namespace", PG_NAMESPACE_OID),
    ("pg_opclass", PG_OPCLASS_OID),
    ("pg_operator", PG_OPERATOR_OID),
    ("pg_opfamily", PG_OPFAMILY_OID),
    ("pg_extension", 3079),
    ("pg_default_acl", 826),
    ("pg_parameter_acl", 6243),
    ("pg_replication_slots", 121),
    ("pg_subscription", 6107),
    ("pg_transform", 3576),
];

fn catalog_relation_oid(name: &str) -> Option<i32> {
    CATALOG_RELATIONS
        .iter()
        .find_map(|(candidate, oid)| (*candidate == name).then_some(*oid))
}

fn catalog_relation_name(oid: i32) -> Option<&'static str> {
    CATALOG_RELATIONS
        .iter()
        .find_map(|(name, candidate)| (*candidate == oid).then_some(*name))
}

pub fn is_catalog_relation(qualifier: Option<&str>, name: &str) -> bool {
    match qualifier {
        Some("pg_catalog") => true,
        Some("information_schema") => matches!(
            name,
            "tables"
                | "columns"
                | "column_options"
                | "foreign_data_wrapper_options"
                | "foreign_data_wrappers"
                | "foreign_server_options"
                | "foreign_servers"
                | "foreign_table_options"
                | "foreign_tables"
                | "schemata"
                | "table_constraints"
                | "key_column_usage"
                | "constraint_column_usage"
                | "referential_constraints"
                | "table_privileges"
                | "role_table_grants"
                | "sequences"
                | "usage_privileges"
                | "domains"
                | "domain_constraints"
                | "check_constraints"
                | "column_domain_usage"
                | "column_udt_usage"
                | "domain_udt_usage"
                | "collations"
                | "collation_character_set_applicability"
                | "applicable_roles"
                | "administrable_role_authorizations"
                | "enabled_roles"
                | "column_privileges"
                | "role_column_grants"
                | "views"
                | "view_table_usage"
                | "view_column_usage"
                | "routines"
                | "parameters"
                | "routine_privileges"
                | "role_routine_grants"
                | "user_mapping_options"
                | "user_mappings"
        ),
        Some(_) => false,
        None => matches!(
            name,
            "pg_class"
                | "pg_attribute"
                | "pg_type"
                | "pg_namespace"
                | "pg_tables"
                | "pg_indexes"
                | "pg_views"
                | "pg_rules"
                | "pg_matviews"
                | "pg_sequences"
                | "pg_sequence"
                | "pg_roles"
                | "pg_authid"
                | "pg_database"
                | "pg_am"
                | "pg_index"
                | "pg_constraint"
                | "pg_attrdef"
                | "pg_collation"
                | "pg_conversion"
                | "pg_policy"
                | "pg_policies"
                | "pg_rewrite"
                | "pg_trigger"
                | "pg_event_trigger"
                | "pg_inherits"
                | "pg_stats"
                | "pg_statistic_ext"
                | "pg_statistic_ext_data"
                | "pg_publication"
                | "pg_publication_rel"
                | "pg_publication_tables"
                | "pg_publication_namespace"
                | "pg_replication_slots"
                | "pg_subscription"
                | "pg_subscription_rel"
                | "pg_foreign_table"
                | "pg_foreign_server"
                | "pg_user_mapping"
                | "pg_user_mappings"
                | "pg_partitioned_table"
                | "pg_description"
                | "pg_shdescription"
                | "pg_seclabels"
                | "pg_shseclabel"
                | "pg_largeobject_metadata"
                | "pg_largeobject"
                | "pg_enum"
                | "pg_range"
                | "pg_settings"
                | "pg_prepared_xacts"
                | "pg_proc"
                | "pg_aggregate"
                | "pg_operator"
                | "pg_opclass"
                | "pg_opfamily"
                | "pg_amop"
                | "pg_amproc"
                | "pg_ts_parser"
                | "pg_ts_template"
                | "pg_ts_dict"
                | "pg_ts_config"
                | "pg_ts_config_map"
                | "pg_language"
                | "pg_auth_members"
                | "pg_db_role_setting"
                | "pg_parameter_acl"
                | "pg_default_acl"
                | "pg_extension"
                | "pg_available_extensions"
                | "pg_available_extension_versions"
                | "pg_depend"
                | "pg_init_privs"
                | "pg_cast"
                | "pg_transform"
                | "pg_tablespace"
                | "pg_foreign_data_wrapper"
        ),
    }
}

/// Builds the requested catalog relation. `qualifier` is the schema (or
/// None). Allocates rows in `arena`.
pub fn synthesize<'a>(
    storage: &Storage,
    qualifier: Option<&str>,
    name: &'a str,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let info = qualifier == Some("information_schema");
    match (info, name) {
        (false, "pg_class") => pg_class(storage, txid, arena),
        (false, "pg_attribute") => pg_attribute(storage, txid, arena),
        (false, "pg_attrdef") => pg_attrdef(storage, txid, arena),
        (false, "pg_collation") => pg_collation(storage, txid, arena),
        (false, "pg_conversion") => pg_conversion(storage, txid, arena),
        (false, "pg_type") => pg_type(storage, txid, arena),
        (false, "pg_namespace") => pg_namespace(storage, txid, arena),
        (false, "pg_tables") => pg_tables(storage, txid, arena),
        (false, "pg_indexes") => pg_indexes(storage, txid, arena),
        (false, "pg_am") => pg_am(storage, txid, arena),
        (false, "pg_constraint") => pg_constraint(storage, txid, arena),
        (false, "pg_index") => pg_index(storage, txid, arena),
        (false, "pg_stats") => pg_stats(storage, txid, arena),
        (false, "pg_policy") => pg_policy(storage, txid, arena),
        (false, "pg_policies") => pg_policies(storage, txid, arena),
        (false, "pg_statistic_ext") => pg_statistic_ext(storage, txid, arena),
        (false, "pg_statistic_ext_data") => pg_statistic_ext_data(storage, txid, arena),
        (false, "pg_publication") => pg_publication(storage, txid, arena),
        (false, "pg_publication_namespace") => pg_publication_namespace(storage, txid, arena),
        (false, "pg_publication_rel") => pg_publication_rel(storage, txid, arena),
        (false, "pg_publication_tables") => pg_publication_tables(storage, txid, arena),
        (false, "pg_replication_slots") => pg_replication_slots(storage, arena),
        (false, "pg_subscription") => pg_subscription(storage, txid, arena),
        (false, "pg_subscription_rel") => pg_subscription_rel(storage, txid, arena),
        (false, "pg_inherits") => pg_inherits(storage, txid, arena),
        (false, "pg_rewrite") => pg_rewrite(storage, txid, arena),
        (false, "pg_trigger") => pg_trigger(storage, txid, arena),
        (false, "pg_event_trigger") => pg_event_trigger(storage, txid, arena),
        (false, "pg_foreign_table") => pg_foreign_table(storage, txid, arena),
        (false, "pg_foreign_server") => pg_foreign_server(storage, txid, arena),
        (false, "pg_foreign_data_wrapper") => pg_foreign_data_wrapper(storage, txid, arena),
        (false, "pg_user_mapping") => pg_user_mapping(storage, txid, arena),
        (false, "pg_user_mappings") => pg_user_mappings(storage, txid, arena),
        (false, "pg_partitioned_table") => pg_partitioned_table(storage, txid, arena),
        (false, "pg_settings") => pg_settings(arena),
        (false, "pg_prepared_xacts") => pg_prepared_xacts(storage, txid, arena),
        (false, "pg_proc") => pg_proc(storage, txid, arena),
        (false, "pg_aggregate") => pg_aggregate(storage, txid, arena),
        (false, "pg_operator") => pg_operator(storage, txid, arena),
        (false, "pg_opclass") => pg_opclass(storage, txid, arena),
        (false, "pg_opfamily") => pg_opfamily(storage, txid, arena),
        (false, "pg_amop") => pg_amop(storage, txid, arena),
        (false, "pg_amproc") => pg_amproc(storage, txid, arena),
        (false, "pg_ts_parser") => pg_ts_parser(storage, txid, arena),
        (false, "pg_ts_template") => pg_ts_template(storage, txid, arena),
        (false, "pg_ts_dict") => pg_ts_dict(storage, txid, arena),
        (false, "pg_ts_config") => pg_ts_config(storage, txid, arena),
        (false, "pg_ts_config_map") => pg_ts_config_map(storage, txid, arena),
        (false, "pg_init_privs") => pg_init_privs(arena),
        (false, "pg_cast") => pg_cast(storage, txid, arena),
        (false, "pg_transform") => finish(
            def_of(
                "pg_transform",
                &[
                    ("tableoid", ColType::Int4),
                    ("oid", ColType::Int4),
                    ("trftype", ColType::Int4),
                    ("trflang", ColType::Int4),
                    ("trffromsql", ColType::Int4),
                    ("trftosql", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_language") => pg_language(storage, txid, arena),
        (false, "pg_auth_members") => pg_auth_members(storage, txid, arena),
        (false, "pg_db_role_setting") => pg_db_role_setting(storage, txid, arena),
        (false, "pg_parameter_acl") => pg_parameter_acl(storage, txid, arena),
        (false, "pg_default_acl") => pg_default_acl(storage, txid, arena),
        (false, "pg_extension") => pg_extension(storage, txid, arena),
        (false, "pg_available_extensions") => pg_available_extensions(storage, txid, arena),
        (false, "pg_available_extension_versions") => {
            pg_available_extension_versions(storage, txid, arena)
        }
        (false, "pg_depend") => pg_depend(storage, txid, arena),
        (false, "pg_tablespace") => pg_tablespace(storage, txid, arena),
        (false, "pg_roles") => pg_roles(storage, txid, arena),
        (false, "pg_authid") => pg_authid(storage, txid, arena),
        (false, "pg_description") => pg_description(storage, txid, arena),
        (false, "pg_shdescription") => pg_shdescription(storage, txid, arena),
        (false, "pg_seclabels") => finish(
            def_of(
                "pg_seclabels",
                &[
                    ("objoid", ColType::Int4),
                    ("classoid", ColType::Int4),
                    ("objsubid", ColType::Int4),
                    ("objtype", ColType::Text),
                    ("objnamespace", ColType::Int4),
                    ("objname", ColType::Text),
                    ("provider", ColType::Text),
                    ("label", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_shseclabel") => finish(
            def_of(
                "pg_shseclabel",
                &[
                    ("objoid", ColType::Int4),
                    ("classoid", ColType::Int4),
                    ("provider", ColType::Text),
                    ("label", ColType::Text),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_largeobject_metadata") => pg_largeobject_metadata(storage, txid, arena),
        (false, "pg_largeobject") => pg_largeobject(storage, txid, arena),
        (false, "pg_enum") => pg_enum(storage, txid, arena),
        (false, "pg_range") => finish(
            def_of(
                "pg_range",
                &[
                    ("rngtypid", ColType::Int4),
                    ("rngsubtype", ColType::Int4),
                    ("rngmultitypid", ColType::Int4),
                ],
            ),
            &[],
            arena,
        ),
        (false, "pg_matviews") => pg_matviews(storage, txid, arena),
        (false, "pg_sequences") => pg_sequences(storage, txid, arena),
        (false, "pg_sequence") => pg_sequence(storage, txid, arena),
        (false, "pg_database") => pg_database(storage, txid, arena),
        (false, "pg_views") => pg_views(storage, txid, arena),
        (false, "pg_rules") => pg_rules(storage, txid, arena),
        (true, "tables") => info_tables(storage, txid, arena),
        (true, "columns") => info_columns(storage, txid, arena),
        (true, "column_options") => info_column_options(storage, txid, arena),
        (true, "foreign_data_wrapper_options") => {
            info_foreign_data_wrapper_options(storage, txid, arena)
        }
        (true, "foreign_data_wrappers") => info_foreign_data_wrappers(storage, txid, arena),
        (true, "foreign_server_options") => info_foreign_server_options(storage, txid, arena),
        (true, "foreign_servers") => info_foreign_servers(storage, txid, arena),
        (true, "foreign_table_options") => info_foreign_table_options(storage, txid, arena),
        (true, "foreign_tables") => info_foreign_tables(storage, txid, arena),
        (true, "schemata") => info_schemata(storage, txid, arena),
        (true, "table_constraints") => info_table_constraints(storage, txid, arena),
        (true, "key_column_usage") => info_key_column_usage(storage, txid, arena),
        (true, "constraint_column_usage") => info_constraint_column_usage(storage, txid, arena),
        (true, "referential_constraints") => info_referential_constraints(storage, txid, arena),
        (true, "table_privileges") => info_table_privileges(storage, txid, arena),
        (true, "role_table_grants") => info_role_table_grants(storage, txid, arena),
        (true, "sequences") => info_sequences(storage, txid, arena),
        (true, "usage_privileges") => info_usage_privileges(storage, txid, arena),
        (true, "domains") => info_domains(storage, txid, arena),
        (true, "domain_constraints") => info_domain_constraints(storage, txid, arena),
        (true, "check_constraints") => info_check_constraints(storage, txid, arena),
        (true, "column_domain_usage") => info_column_domain_usage(storage, txid, arena),
        (true, "column_udt_usage") => info_column_udt_usage(storage, txid, arena),
        (true, "domain_udt_usage") => info_domain_udt_usage(storage, txid, arena),
        (true, "collations") => info_collations(storage, txid, arena),
        (true, "collation_character_set_applicability") => {
            info_collation_character_set_applicability(storage, txid, arena)
        }
        (true, "applicable_roles") => info_applicable_roles(storage, txid, arena, false),
        (true, "administrable_role_authorizations") => {
            info_applicable_roles(storage, txid, arena, true)
        }
        (true, "enabled_roles") => info_enabled_roles(storage, txid, arena),
        (true, "column_privileges") => info_column_privileges(storage, txid, arena, true),
        (true, "role_column_grants") => info_column_privileges(storage, txid, arena, false),
        (true, "views") => info_views(storage, txid, arena),
        (true, "routines") => info_routines(storage, txid, arena),
        (true, "parameters") => info_parameters(storage, txid, arena),
        (true, "routine_privileges") => info_routine_privileges(storage, txid, arena, true),
        (true, "role_routine_grants") => info_routine_privileges(storage, txid, arena, false),
        (true, "view_table_usage") => info_view_table_usage(storage, txid, arena),
        (true, "view_column_usage") => info_view_column_usage(storage, txid, arena),
        (true, "user_mapping_options") => info_user_mapping_options(storage, txid, arena),
        (true, "user_mappings") => info_user_mappings(storage, txid, arena),
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
    user_table_oid(slot)
}

pub(crate) fn user_table_oid(slot: usize) -> i32 {
    FIRST_USER_OID + slot as i32
}

const FIRST_FOREIGN_DATA_WRAPPER_OID: i32 = 510_000;
const FIRST_FOREIGN_SERVER_OID: i32 = 520_000;
const FIRST_USER_MAPPING_OID: i32 = 530_000;

pub(crate) const fn foreign_data_wrapper_oid(slot: usize) -> i32 {
    FIRST_FOREIGN_DATA_WRAPPER_OID + slot as i32
}

pub(crate) const fn foreign_server_oid(slot: usize) -> i32 {
    FIRST_FOREIGN_SERVER_OID + slot as i32
}

const fn user_mapping_oid(slot: usize) -> i32 {
    FIRST_USER_MAPPING_OID + slot as i32
}

fn foreign_options_datum<'a>(
    options: &crate::storage::foreign::ForeignOptions,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if options.entries().is_empty() {
        return Ok(Datum::Null);
    }
    let values = arena
        .alloc_slice_with(options.entries().len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (value, option) in values.iter_mut().zip(options.entries()) {
        let rendered = stack_format!(322, "{}={}", option.name.as_str(), option.value.as_str());
        *value = text(rendered.as_str(), arena)?;
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Text,
        raw: super::array::build(values, arena)?,
    })
}

fn foreign_routine_oid(storage: &Storage, txid: u32, name: &str) -> Result<i32, SqlError> {
    routine_oid_by_name(storage, txid, name, false)?.ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "foreign-data routine \"{}\" is missing from pg_proc",
            name
        )
    })
}

fn pg_foreign_data_wrapper<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_foreign_data_wrapper",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("fdwname", ColType::Name),
            ("fdwowner", ColType::Int4),
            ("fdwhandler", ColType::Int4),
            ("fdwvalidator", ColType::Int4),
            ("fdwacl", ColType::Array(super::types::ArrElem::AclItem)),
            ("fdwoptions", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let count = storage.foreign_wrappers(txid).count();
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (slot, entry)) in storage.foreign_wrappers(txid).enumerate() {
        let wrapper = entry.definition_for(txid);
        rows[index] = row(
            &[
                Datum::Int4(2328),
                Datum::Int4(foreign_data_wrapper_oid(slot)),
                text(wrapper.name.as_str(), arena)?,
                Datum::Int4(Storage::role_oid(entry.ownership.owner_to(txid) as usize)),
                Datum::Int4(match wrapper.handler {
                    crate::storage::foreign::ForeignDataHandler::None => 0,
                    crate::storage::foreign::ForeignDataHandler::Postgres => {
                        foreign_routine_oid(storage, txid, "postgres_fdw_handler")?
                    }
                }),
                Datum::Int4(match wrapper.validator {
                    crate::storage::foreign::ForeignDataValidator::None => 0,
                    crate::storage::foreign::ForeignDataValidator::Postgres => {
                        foreign_routine_oid(storage, txid, "postgres_fdw_validator")?
                    }
                }),
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::ForeignDataWrapper,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                foreign_options_datum(&wrapper.options, arena)?,
            ],
            arena,
        )?;
    }
    finish(definition, rows, arena)
}

fn pg_foreign_server<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_foreign_server",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("srvname", ColType::Name),
            ("srvowner", ColType::Int4),
            ("srvfdw", ColType::Int4),
            ("srvtype", ColType::Text),
            ("srvversion", ColType::Text),
            ("srvacl", ColType::Array(super::types::ArrElem::AclItem)),
            ("srvoptions", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let count = storage.foreign_servers(txid).count();
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (slot, entry)) in storage.foreign_servers(txid).enumerate() {
        let server = entry.definition_for(txid);
        rows[index] = row(
            &[
                Datum::Int4(1417),
                Datum::Int4(foreign_server_oid(slot)),
                text(server.name.as_str(), arena)?,
                Datum::Int4(Storage::role_oid(entry.ownership.owner_to(txid) as usize)),
                Datum::Int4(foreign_data_wrapper_oid(server.wrapper as usize)),
                match server.server_type {
                    Some(value) => text(value.as_str(), arena)?,
                    None => Datum::Null,
                },
                match server.version {
                    Some(value) => text(value.as_str(), arena)?,
                    None => Datum::Null,
                },
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::ForeignServer,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                foreign_options_datum(&server.options, arena)?,
            ],
            arena,
        )?;
    }
    finish(definition, rows, arena)
}

fn pg_foreign_table<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_foreign_table",
        &[
            ("ftrelid", ColType::Int4),
            ("ftserver", ColType::Int4),
            ("ftoptions", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let count = storage.foreign_tables(txid).count();
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (_, entry)) in storage.foreign_tables(txid).enumerate() {
        let table = entry.definition_for(txid);
        rows[index] = row(
            &[
                Datum::Int4(user_table_oid(table.table as usize)),
                Datum::Int4(foreign_server_oid(table.server as usize)),
                foreign_options_datum(&table.options, arena)?,
            ],
            arena,
        )?;
    }
    finish(definition, rows, arena)
}

fn pg_user_mapping<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_user_mapping",
        &[
            ("oid", ColType::Int4),
            ("umuser", ColType::Int4),
            ("umserver", ColType::Int4),
            ("umoptions", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let count = storage.foreign_user_mappings(txid).count();
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (slot, entry)) in storage.foreign_user_mappings(txid).enumerate() {
        let mapping = entry.definition_for(txid);
        let user = match mapping.user {
            crate::storage::foreign::ForeignMappingUser::Public => 0,
            crate::storage::foreign::ForeignMappingUser::Role(slot) => {
                Storage::role_oid(slot as usize)
            }
        };
        rows[index] = row(
            &[
                Datum::Int4(user_mapping_oid(slot)),
                Datum::Int4(user),
                Datum::Int4(foreign_server_oid(mapping.server as usize)),
                if foreign_mapping_options_visible(storage, mapping, txid) {
                    foreign_options_datum(&mapping.options, arena)?
                } else {
                    Datum::Null
                },
            ],
            arena,
        )?;
    }
    finish(definition, rows, arena)
}

fn pg_user_mappings<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_user_mappings",
        &[
            ("umid", ColType::Int4),
            ("srvid", ColType::Int4),
            ("srvname", ColType::Name),
            ("umuser", ColType::Int4),
            ("usename", ColType::Name),
            ("umoptions", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let count = storage.foreign_user_mappings(txid).count();
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (slot, entry)) in storage.foreign_user_mappings(txid).enumerate() {
        let mapping = entry.definition_for(txid);
        let server = storage
            .foreign_server_by_slot(mapping.server as usize, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "user mapping references a missing foreign server"
                )
            })?;
        let (user, user_name) = match mapping.user {
            crate::storage::foreign::ForeignMappingUser::Public => (0, text("PUBLIC", arena)?),
            crate::storage::foreign::ForeignMappingUser::Role(role) => (
                Storage::role_oid(role as usize),
                text(storage.role_name(role as usize, txid).as_str(), arena)?,
            ),
        };
        rows[index] = row(
            &[
                Datum::Int4(user_mapping_oid(slot)),
                Datum::Int4(foreign_server_oid(mapping.server as usize)),
                text(server.name.as_str(), arena)?,
                Datum::Int4(user),
                user_name,
                if foreign_mapping_options_visible(storage, mapping, txid) {
                    foreign_options_datum(&mapping.options, arena)?
                } else {
                    Datum::Null
                },
            ],
            arena,
        )?;
    }
    finish(definition, rows, arena)
}

fn foreign_object_visible(
    storage: &Storage,
    class: crate::storage::AccessClass,
    slot: usize,
    txid: u32,
) -> bool {
    storage.current_role_slot(txid).is_some_and(|role| {
        storage.has_object_privilege(
            crate::storage::AccessObject {
                class,
                slot: slot as u16,
            },
            role,
            crate::storage::PrivilegeSet::USAGE,
            txid,
        )
    })
}

fn foreign_table_visible(storage: &Storage, slot: usize, txid: u32) -> bool {
    let Some(role) = storage.current_role_slot(txid) else {
        return false;
    };
    let object = crate::storage::AccessObject {
        class: crate::storage::AccessClass::Table,
        slot: slot as u16,
    };
    [
        crate::storage::PrivilegeSet::SELECT,
        crate::storage::PrivilegeSet::INSERT,
        crate::storage::PrivilegeSet::UPDATE,
        crate::storage::PrivilegeSet::DELETE,
        crate::storage::PrivilegeSet::TRUNCATE,
        crate::storage::PrivilegeSet::REFERENCES,
        crate::storage::PrivilegeSet::TRIGGER,
        crate::storage::PrivilegeSet::MAINTAIN,
    ]
    .into_iter()
    .any(|privilege| storage.has_object_privilege(object, role, privilege, txid))
}

fn foreign_mapping_options_visible(
    storage: &Storage,
    mapping: crate::storage::foreign::UserMappingDefinition,
    txid: u32,
) -> bool {
    let Some(current) = storage.current_role_slot(txid) else {
        return false;
    };
    if storage.role(current).attributes_to(txid).superuser {
        return true;
    }
    let server = crate::storage::AccessObject {
        class: crate::storage::AccessClass::ForeignServer,
        slot: mapping.server,
    };
    if storage.object_owner(server, txid) == current {
        return true;
    }
    matches!(mapping.user, crate::storage::foreign::ForeignMappingUser::Role(role) if role as usize == current)
        && storage.has_object_privilege(server, current, crate::storage::PrivilegeSet::USAGE, txid)
}

const PG_DATABASE_OWNER_OID: i32 = 6_171;
const PREDEFINED_ROLES: &[(i32, &str)] = &[(PG_DATABASE_OWNER_OID, "pg_database_owner")];

pub(crate) fn predefined_role_name(oid: i32) -> Option<&'static str> {
    PREDEFINED_ROLES
        .iter()
        .find_map(|(candidate, name)| (*candidate == oid).then_some(*name))
}

pub(crate) fn predefined_role_oid(name: &str) -> Option<i32> {
    PREDEFINED_ROLES
        .iter()
        .find_map(|(oid, candidate)| (*candidate == name).then_some(*oid))
}

#[derive(Clone, Copy)]
enum CatalogOwner {
    Role(usize),
    DatabaseOwner,
}

fn catalog_owner(
    storage: &Storage,
    object: crate::storage::AccessObject,
    txid: u32,
) -> CatalogOwner {
    if matches!(object.class, crate::storage::AccessClass::Schema)
        && storage.schema_def(usize::from(object.slot)).name.as_str() == "public"
    {
        CatalogOwner::DatabaseOwner
    } else {
        CatalogOwner::Role(storage.object_owner(object, txid))
    }
}

fn catalog_owner_oid(owner: CatalogOwner) -> i32 {
    match owner {
        CatalogOwner::Role(slot) => Storage::role_oid(slot),
        CatalogOwner::DatabaseOwner => PG_DATABASE_OWNER_OID,
    }
}

fn catalog_owner_name(storage: &Storage, owner: CatalogOwner, txid: u32) -> SqlName {
    match owner {
        CatalogOwner::Role(slot) => storage.role_name(slot, txid),
        CatalogOwner::DatabaseOwner => {
            SqlName::parse("pg_database_owner").expect("built-in role fits")
        }
    }
}

fn owner_oid(storage: &Storage, class: crate::storage::AccessClass, slot: usize, txid: u32) -> i32 {
    catalog_owner_oid(catalog_owner(
        storage,
        crate::storage::AccessObject {
            class,
            slot: slot as u16,
        },
        txid,
    ))
}

fn owner_name<'a>(
    storage: &Storage,
    class: crate::storage::AccessClass,
    slot: usize,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    text(
        catalog_owner_name(
            storage,
            catalog_owner(
                storage,
                crate::storage::AccessObject {
                    class,
                    slot: slot as u16,
                },
                txid,
            ),
            txid,
        )
        .as_str(),
        arena,
    )
}

fn acl<'a>(
    storage: &Storage,
    object: crate::storage::AccessObject,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use core::fmt::Write;
    let owner = storage.object_owner(object, txid);
    let explicit_owner_acl = storage.acl_entries().any(|(slot, entry)| {
        let (grantee, grantor) = storage.acl_identity(slot, txid);
        entry.object == object
            && grantee == owner as u16
            && grantor == owner as u16
            && entry.object.slot != u16::MAX
    });
    let has_entries = storage.acl_entries().any(|(slot, entry)| {
        let (grantee, _) = storage.acl_identity(slot, txid);
        entry.object == object
            && (storage.acl_state(slot, txid).0.0 != 0
                || (explicit_owner_acl && grantee == owner as u16)
                || (matches!(
                    object.class,
                    crate::storage::AccessClass::Domain
                        | crate::storage::AccessClass::Enum
                        | crate::storage::AccessClass::Routine
                        | crate::storage::AccessClass::Language
                ) && grantee == crate::storage::PUBLIC_ROLE))
    });
    if !has_entries {
        return Ok(Datum::Null);
    }
    let mut values = [Datum::Null; crate::storage::MAX_ACL_ENTRIES + 1];
    let owner_name = catalog_owner_name(storage, catalog_owner(storage, object, txid), txid);
    let all = match object.class {
        crate::storage::AccessClass::Table
        | crate::storage::AccessClass::View
        | crate::storage::AccessClass::MaterializedView => crate::storage::PrivilegeSet::TABLE_ALL,
        crate::storage::AccessClass::Sequence => crate::storage::PrivilegeSet::SEQUENCE_ALL,
        crate::storage::AccessClass::Schema => crate::storage::PrivilegeSet::SCHEMA_ALL,
        crate::storage::AccessClass::Domain
        | crate::storage::AccessClass::Enum
        | crate::storage::AccessClass::Composite => crate::storage::PrivilegeSet::TYPE_ALL,
        crate::storage::AccessClass::Index => crate::storage::PrivilegeSet::NONE,
        crate::storage::AccessClass::Routine => crate::storage::PrivilegeSet::FUNCTION_ALL,
        crate::storage::AccessClass::LargeObject => crate::storage::PrivilegeSet::LARGE_OBJECT_ALL,
        crate::storage::AccessClass::ForeignDataWrapper
        | crate::storage::AccessClass::ForeignServer
        | crate::storage::AccessClass::Language => crate::storage::PrivilegeSet::USAGE,
        crate::storage::AccessClass::Tablespace => crate::storage::PrivilegeSet::CREATE,
        crate::storage::AccessClass::Database => crate::storage::PrivilegeSet::DATABASE_ALL,
        crate::storage::AccessClass::Statistics
        | crate::storage::AccessClass::Extension
        | crate::storage::AccessClass::Trigger
        | crate::storage::AccessClass::EventTrigger => crate::storage::PrivilegeSet::NONE,
    };
    let render = |grantee: &str,
                  grantor: &str,
                  privileges: crate::storage::PrivilegeSet,
                  grant_options: crate::storage::PrivilegeSet,
                  output: &mut StackStr<256>| {
        let grantee = super::types::acl_identifier(grantee);
        let grantor = super::types::acl_identifier(grantor);
        let _ = write!(output, "{}=", grantee.as_str());
        let letters = [
            (crate::storage::PrivilegeSet::INSERT, 'a'),
            (crate::storage::PrivilegeSet::SELECT, 'r'),
            (crate::storage::PrivilegeSet::UPDATE, 'w'),
            (crate::storage::PrivilegeSet::DELETE, 'd'),
            (crate::storage::PrivilegeSet::TRUNCATE, 'D'),
            (crate::storage::PrivilegeSet::REFERENCES, 'x'),
            (crate::storage::PrivilegeSet::TRIGGER, 't'),
            (crate::storage::PrivilegeSet::MAINTAIN, 'm'),
            (crate::storage::PrivilegeSet::USAGE, 'U'),
            (crate::storage::PrivilegeSet::CREATE, 'C'),
            (crate::storage::PrivilegeSet::TEMPORARY, 'T'),
            (crate::storage::PrivilegeSet::CONNECT, 'c'),
            (crate::storage::PrivilegeSet::EXECUTE, 'X'),
        ];
        for (privilege, letter) in letters {
            if privileges.contains(privilege) {
                let _ = write!(output, "{letter}");
                if grant_options.contains(privilege) {
                    let _ = write!(output, "*");
                }
            }
        }
        let _ = write!(output, "/{}", grantor.as_str());
    };
    let mut owner_acl = StackStr::<256>::new();
    let (owner_privileges, owner_options) = if explicit_owner_acl {
        storage.acl_from(object, owner as u16, owner as u16, txid)
    } else {
        (all, crate::storage::PrivilegeSet::NONE)
    };
    render(
        owner_name.as_str(),
        owner_name.as_str(),
        owner_privileges,
        owner_options,
        &mut owner_acl,
    );
    values[0] = Datum::Text(
        arena
            .alloc_str(owner_acl.as_str())
            .map_err(|_| arena_full())?,
    );
    let mut count = 1usize;
    for (slot, entry) in storage.acl_entries() {
        let (grantee, grantor) = storage.acl_identity(slot, txid);
        if entry.object != object || (grantee == owner as u16 && grantor == owner as u16) {
            continue;
        }
        if storage
            .acl_entries()
            .filter(|(earlier_slot, _)| *earlier_slot < slot)
            .any(|(earlier_slot, earlier)| {
                earlier.object == object
                    && storage.acl_identity(earlier_slot, txid) != (owner as u16, owner as u16)
                    && storage.acl_identity(earlier_slot, txid) == (grantee, grantor)
            })
        {
            continue;
        }
        let (privileges, grant_options) = storage.acl_from(object, grantee, grantor, txid);
        if privileges.0 == 0 {
            continue;
        }
        let grantee_name = (grantee != crate::storage::PUBLIC_ROLE)
            .then(|| storage.role_name(grantee as usize, txid));
        let grantee = grantee_name.as_ref().map_or("", |name| name.as_str());
        let grantor_name = if matches!(
            catalog_owner(storage, object, txid),
            CatalogOwner::DatabaseOwner
        ) && grantor == owner as u16
        {
            None
        } else {
            Some(storage.role_name(grantor as usize, txid))
        };
        let grantor = grantor_name
            .as_ref()
            .map_or("pg_database_owner", |name| name.as_str());
        let mut rendered = StackStr::<256>::new();
        render(grantee, grantor, privileges, grant_options, &mut rendered);
        values[count] = Datum::Text(
            arena
                .alloc_str(rendered.as_str())
                .map_err(|_| arena_full())?,
        );
        count += 1;
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::AclItem,
        raw: super::array::build(&values[..count], arena)?,
    })
}

fn builtin_acl<'a>(values: &[&str], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let datums = arena
        .alloc_slice_with(values.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (datum, value) in datums.iter_mut().zip(values) {
        *datum = text(value, arena)?;
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::AclItem,
        raw: super::array::build(datums, arena)?,
    })
}

fn pg_init_privs<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_init_privs",
        &[
            ("objoid", ColType::Int4),
            ("classoid", ColType::Int4),
            ("objsubid", ColType::Int4),
            ("privtype", ColType::Bpchar),
            ("initprivs", ColType::Array(super::types::ArrElem::AclItem)),
        ],
    );
    let rows = arena
        .alloc_slice_with(3, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (row_index, function_oid) in [764, 765, 767].into_iter().enumerate() {
        rows[row_index] = row(
            &[
                Datum::Int4(function_oid),
                Datum::Int4(PG_PROC_OID),
                Datum::Int4(0),
                Datum::Bpchar("i"),
                builtin_acl(&["postgres=X/postgres"], arena)?,
            ],
            arena,
        )?;
    }
    finish(definition, rows, arena)
}

fn pg_largeobject_metadata<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_largeobject_metadata",
        &[
            ("oid", ColType::Oid),
            ("lomowner", ColType::Oid),
            ("lomacl", ColType::Array(super::types::ArrElem::AclItem)),
        ],
    );
    let count = storage.large_objects_visible_to(txid).count();
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (slot, object)) in storage.large_objects_visible_to(txid).enumerate() {
        let access = crate::storage::AccessObject {
            class: crate::storage::AccessClass::LargeObject,
            slot: slot as u16,
        };
        rows[index] = row(
            &[
                Datum::Oid(object.oid.get()),
                Datum::Oid(Storage::role_oid(storage.object_owner(access, txid)) as u32),
                acl(storage, access, txid, arena)?,
            ],
            arena,
        )?;
    }
    finish(definition, rows, arena)
}

fn pg_largeobject<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let role = storage.current_role_slot(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for table pg_largeobject"
        )
    })?;
    if !storage.role(role).attributes_to(txid).superuser {
        return Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for table pg_largeobject"
        ));
    }
    let definition = def_of(
        "pg_largeobject",
        &[
            ("loid", ColType::Oid),
            ("pageno", ColType::Int4),
            ("data", ColType::Bytea),
        ],
    );
    let mut count = 0usize;
    super::large_object::for_each_page(storage, txid, &mut |_, _, _| {
        count = count
            .checked_add(1)
            .ok_or_else(|| catalog_capacity_exceeded("pg_largeobject"))?;
        Ok(())
    })?;
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut index = 0usize;
    super::large_object::for_each_page(storage, txid, &mut |oid, page, data| {
        let copied = arena.alloc_slice_copy(data).map_err(|_| arena_full())?;
        rows[index] = row(
            &[
                Datum::Oid(oid.get()),
                Datum::Int4(page as i32),
                Datum::Bytea(copied),
            ],
            arena,
        )?;
        index += 1;
        Ok(())
    })?;
    finish(definition, rows, arena)
}

fn column_acl<'a>(
    storage: &Storage,
    target: crate::storage::ColumnPrivilegeTarget,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use core::fmt::Write;
    let has_entries = storage.column_acl_entries().any(|(slot, entry)| {
        entry.target == target && storage.column_acl_state(slot, txid).0.0 != 0
    });
    if !has_entries {
        return Ok(Datum::Null);
    }
    let mut values = [Datum::Null; crate::storage::MAX_COLUMN_ACL_ENTRIES];
    let mut count = 0usize;
    for (slot, entry) in storage.column_acl_entries() {
        if entry.target != target {
            continue;
        }
        let (grantee, grantor) = storage.column_acl_identity(slot, txid);
        if storage
            .column_acl_entries()
            .filter(|(earlier_slot, _)| *earlier_slot < slot)
            .any(|(earlier_slot, earlier)| {
                earlier.target == target
                    && storage.column_acl_identity(earlier_slot, txid) == (grantee, grantor)
            })
        {
            continue;
        }
        let (privileges, grant_options) = storage.column_acl_from(target, grantee, grantor, txid);
        if privileges.0 == 0 {
            continue;
        }
        let grantee_name = (grantee != crate::storage::PUBLIC_ROLE)
            .then(|| storage.role_name(grantee as usize, txid));
        let grantor_name = storage.role_name(grantor as usize, txid);
        let grantee_name =
            super::types::acl_identifier(grantee_name.as_ref().map_or("", SqlName::as_str));
        let grantor_name = super::types::acl_identifier(grantor_name.as_str());
        let mut rendered = StackStr::<256>::new();
        let _ = write!(rendered, "{}=", grantee_name.as_str());
        for (privilege, letter) in [
            (crate::storage::PrivilegeSet::INSERT, 'a'),
            (crate::storage::PrivilegeSet::SELECT, 'r'),
            (crate::storage::PrivilegeSet::UPDATE, 'w'),
            (crate::storage::PrivilegeSet::REFERENCES, 'x'),
        ] {
            if privileges.contains(privilege) {
                let _ = write!(rendered, "{letter}");
                if grant_options.contains(privilege) {
                    let _ = write!(rendered, "*");
                }
            }
        }
        let _ = write!(rendered, "/{}", grantor_name.as_str());
        values[count] = text(rendered.as_str(), arena)?;
        count += 1;
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::AclItem,
        raw: super::array::build(&values[..count], arena)?,
    })
}

fn pg_parameter_acl<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    use core::fmt::Write;
    let definition = def_of(
        "pg_parameter_acl",
        &[
            ("parname", ColType::Text),
            ("paracl", ColType::Array(super::types::ArrElem::AclItem)),
        ],
    );
    let mut rows: [&[Datum]; crate::storage::MAX_PARAMETER_ACL_ENTRIES] =
        [&[]; crate::storage::MAX_PARAMETER_ACL_ENTRIES];
    let mut row_count = 0usize;
    for (slot, entry) in storage.parameter_acl_entries_visible(txid) {
        let parameter = entry.parameter;
        if storage
            .parameter_acl_entries_visible(txid)
            .take(slot)
            .any(|(_, earlier)| earlier.parameter == parameter)
        {
            continue;
        }
        let mut values = [Datum::Null; crate::storage::MAX_PARAMETER_ACL_ENTRIES];
        let mut count = 0usize;
        for (candidate_slot, candidate) in storage.parameter_acl_entries_visible(txid) {
            if candidate.parameter != parameter {
                continue;
            }
            let (grantee, grantor) = storage.parameter_acl_identity(candidate_slot, txid);
            if storage
                .parameter_acl_entries_visible(txid)
                .take(candidate_slot)
                .any(|(earlier_slot, earlier)| {
                    earlier.parameter == parameter
                        && storage.parameter_acl_identity(earlier_slot, txid) == (grantee, grantor)
                })
            {
                continue;
            }
            let (privileges, grant_options) =
                storage.parameter_acl_from(parameter, grantee, grantor, txid);
            if privileges.bits() == 0 {
                continue;
            }
            let grantee_name = (grantee != crate::storage::PUBLIC_ROLE)
                .then(|| storage.role_name(grantee as usize, txid));
            let grantor_name = storage.role_name(grantor as usize, txid);
            let grantee_name =
                super::types::acl_identifier(grantee_name.as_ref().map_or("", SqlName::as_str));
            let grantor_name = super::types::acl_identifier(grantor_name.as_str());
            let mut rendered = StackStr::<256>::new();
            let _ = write!(rendered, "{}=", grantee_name.as_str());
            for (privilege, letter) in [
                (crate::sql::ast::ParameterPrivileges::SET, 's'),
                (crate::sql::ast::ParameterPrivileges::ALTER_SYSTEM, 'A'),
            ] {
                if privileges.contains(privilege) {
                    let _ = write!(rendered, "{letter}");
                    if grant_options.contains(privilege) {
                        let _ = write!(rendered, "*");
                    }
                }
            }
            let _ = write!(rendered, "/{}", grantor_name.as_str());
            values[count] = text(rendered.as_str(), arena)?;
            count += 1;
        }
        rows[row_count] = row(
            &[
                text(parameter.as_str(), arena)?,
                Datum::Array {
                    element: super::types::ArrElem::AclItem,
                    raw: super::array::build(&values[..count], arena)?,
                },
            ],
            arena,
        )?;
        row_count += 1;
    }
    finish(definition, &rows[..row_count], arena)
}

fn pg_default_acl<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    use crate::storage::{
        DEFAULT_ACL_ALL_SCHEMAS, DefaultPrivilegeClass, MAX_DEFAULT_ACL_ENTRIES, MAX_ROLES,
        PUBLIC_ROLE, PrivilegeSet,
    };
    use core::fmt::Write;

    let def = def_of(
        "pg_default_acl",
        &[
            ("oid", ColType::Int4),
            ("tableoid", ColType::Int4),
            ("defaclrole", ColType::Int4),
            ("defaclnamespace", ColType::Int4),
            ("defaclobjtype", ColType::Bpchar),
            ("defaclacl", ColType::Array(super::types::ArrElem::AclItem)),
        ],
    );
    let mut rows: [&[Datum]; MAX_DEFAULT_ACL_ENTRIES] = [&[]; MAX_DEFAULT_ACL_ENTRIES];
    let mut row_count = 0usize;

    for (entry_slot, entry) in storage.default_acl_entries() {
        let (defined, _, _) =
            storage.default_acl_state(entry.owner, entry.schema, entry.class, entry.grantee, txid);
        if !defined
            || storage
                .default_acl_entries()
                .take(entry_slot)
                .any(|(_, earlier)| {
                    let (earlier_defined, _, _) = storage.default_acl_state(
                        earlier.owner,
                        earlier.schema,
                        earlier.class,
                        earlier.grantee,
                        txid,
                    );
                    earlier_defined
                        && earlier.owner == entry.owner
                        && earlier.schema == entry.schema
                        && earlier.class == entry.class
                })
        {
            continue;
        }
        let mut acl_values = [Datum::Null; MAX_ROLES + 1];
        let mut acl_count = 0usize;
        for role_index in 0..=MAX_ROLES {
            let grantee = if role_index == MAX_ROLES {
                PUBLIC_ROLE
            } else {
                if !storage.role(role_index).visible_to(txid) {
                    continue;
                }
                role_index as u16
            };
            let (explicit, privileges, grant_options) =
                storage.default_acl_state(entry.owner, entry.schema, entry.class, grantee, txid);
            let (privileges, grant_options) = if explicit {
                (privileges, grant_options)
            } else if entry.schema == DEFAULT_ACL_ALL_SCHEMAS {
                Storage::default_acl_baseline(entry.owner, entry.schema, entry.class, grantee)
            } else {
                (PrivilegeSet::NONE, PrivilegeSet::NONE)
            };
            let include = if entry.schema == DEFAULT_ACL_ALL_SCHEMAS {
                grantee == entry.owner
                    || explicit
                    || (grantee == PUBLIC_ROLE && entry.class.default_public_privileges().0 != 0)
            } else {
                explicit
            };
            if !include {
                continue;
            }
            let named_grantee =
                (grantee != PUBLIC_ROLE).then(|| storage.role_name(grantee as usize, txid));
            let grantee_name =
                super::types::acl_identifier(named_grantee.as_ref().map_or("", SqlName::as_str));
            let owner_name = storage.role_name(entry.owner as usize, txid);
            let owner_name = super::types::acl_identifier(owner_name.as_str());
            let mut rendered = StackStr::<256>::new();
            let _ = write!(rendered, "{}=", grantee_name.as_str());
            for (privilege, letter) in [
                (PrivilegeSet::SELECT, 'r'),
                (PrivilegeSet::INSERT, 'a'),
                (PrivilegeSet::UPDATE, 'w'),
                (PrivilegeSet::DELETE, 'd'),
                (PrivilegeSet::TRUNCATE, 'D'),
                (PrivilegeSet::REFERENCES, 'x'),
                (PrivilegeSet::TRIGGER, 't'),
                (PrivilegeSet::MAINTAIN, 'm'),
                (PrivilegeSet::USAGE, 'U'),
                (PrivilegeSet::CREATE, 'C'),
                (PrivilegeSet::EXECUTE, 'X'),
            ] {
                if privileges.contains(privilege) {
                    let _ = write!(rendered, "{letter}");
                    if grant_options.contains(privilege) {
                        let _ = write!(rendered, "*");
                    }
                }
            }
            let _ = write!(rendered, "/{}", owner_name.as_str());
            acl_values[acl_count] = text(rendered.as_str(), arena)?;
            acl_count += 1;
        }
        let namespace = if entry.schema == DEFAULT_ACL_ALL_SCHEMAS {
            0
        } else {
            namespace_oid(
                storage,
                storage.schema_def(entry.schema as usize).name.as_str(),
            )
        };
        let object_type = match entry.class {
            DefaultPrivilegeClass::Table => "r",
            DefaultPrivilegeClass::Sequence => "S",
            DefaultPrivilegeClass::Function => "f",
            DefaultPrivilegeClass::Type => "T",
            DefaultPrivilegeClass::Schema => "n",
        };
        rows[row_count] = row(
            &[
                Datum::Int4(90_000 + entry_slot as i32),
                Datum::Int4(826),
                Datum::Int4(Storage::role_oid(entry.owner as usize)),
                Datum::Int4(namespace),
                text(object_type, arena)?,
                Datum::Array {
                    element: super::types::ArrElem::AclItem,
                    raw: super::array::build(&acl_values[..acl_count], arena)?,
                },
            ],
            arena,
        )?;
        row_count += 1;
    }
    finish(def, &rows[..row_count], arena)
}

/// Schema OIDs: the two built-ins keep PostgreSQL's well-known values; a user
/// schema's OID is derived from its registry slot, above the table range.
const FIRST_SCHEMA_OID: i32 = 80_000;
pub(crate) const fn namespace_oid_for_slot(slot: usize) -> i32 {
    FIRST_SCHEMA_OID + slot as i32
}

fn namespace_oid(storage: &Storage, schema: &str) -> i32 {
    match schema {
        "public" => PUBLIC_NS_OID,
        "pg_catalog" => PG_CATALOG_NS_OID,
        "pg_toast" => PG_TOAST_NS_OID,
        _ => storage
            .find_schema(schema)
            .map(namespace_oid_for_slot)
            .unwrap_or(0),
    }
}

pub(crate) fn schema_name_by_oid(storage: &Storage, txid: u32, oid: i32) -> Option<&str> {
    match oid {
        PUBLIC_NS_OID => Some("public"),
        PG_CATALOG_NS_OID => Some("pg_catalog"),
        PG_TOAST_NS_OID => Some("pg_toast"),
        _ => storage
            .visible_schemas(txid)
            .find(|(_, schema)| namespace_oid(storage, schema.name.as_str()) == oid)
            .map(|(_, schema)| schema.name.as_str()),
    }
}

pub(crate) fn schema_oid_by_name(storage: &Storage, txid: u32, name: &str) -> Option<i32> {
    match name {
        "public" => Some(PUBLIC_NS_OID),
        "pg_catalog" => Some(PG_CATALOG_NS_OID),
        "pg_toast" => Some(PG_TOAST_NS_OID),
        _ => storage
            .find_schema_visible(name, txid)
            .map(namespace_oid_for_slot),
    }
}

/// Index relations get OIDs from a separate range so they never collide with
/// table OIDs; `pos` is the index's position within its table's index list.
const FIRST_INDEX_OID: i32 = 90_000;
const FIRST_EXPLICIT_INDEX_OID: i32 = 190_000;
const MAX_INDEXES_PER_TABLE: i32 = 64;
pub(crate) fn index_oid(slot: usize, pos: usize) -> i32 {
    FIRST_INDEX_OID + slot as i32 * MAX_INDEXES_PER_TABLE + pos as i32
}

pub(crate) fn explicit_index_oid(index: &crate::storage::IndexDef) -> i32 {
    FIRST_EXPLICIT_INDEX_OID
        + i32::try_from(index.created_at).unwrap_or(i32::MAX - FIRST_EXPLICIT_INDEX_OID)
}

/// Sequence relations get OIDs from their own range, above the index range.
const FIRST_SEQUENCE_OID: i32 = 95_000;
pub(crate) fn sequence_oid(slot: usize) -> i32 {
    FIRST_SEQUENCE_OID + slot as i32
}

pub(crate) fn extension_config_relation_oid(
    relation: crate::storage::ExtensionConfigRelation,
) -> i32 {
    match relation {
        crate::storage::ExtensionConfigRelation::Table(slot) => user_table_oid(slot as usize),
        crate::storage::ExtensionConfigRelation::Sequence(slot) => sequence_oid(slot as usize),
    }
}

pub(crate) fn extension_config_relation_by_oid(
    storage: &Storage,
    txid: u32,
    oid: i32,
) -> Option<crate::storage::ExtensionConfigRelation> {
    for slot in 0..storage.table_count() {
        if storage.table_slot_visible_to(slot, txid) && user_table_oid(slot) == oid {
            return u16::try_from(slot)
                .ok()
                .map(crate::storage::ExtensionConfigRelation::Table);
        }
    }
    for slot in 0..storage.sequence_count() {
        if storage.sequence_slot_visible_to(slot, txid) && sequence_oid(slot) == oid {
            return u16::try_from(slot)
                .ok()
                .map(crate::storage::ExtensionConfigRelation::Sequence);
        }
    }
    None
}

pub(crate) fn sequence_state_by_oid(storage: &Storage, oid: i32) -> Option<(i64, bool)> {
    storage
        .sequences_with_slots()
        .find(|(slot, _)| sequence_oid(*slot) == oid)
        .map(|(_, sequence)| (sequence.last_value.get(), sequence.is_called.get()))
}

/// Plain views get OIDs from their own range so `'view'::regclass` resolves and
/// their comments surface, even though a view is not yet a full `pg_class` row.
const FIRST_VIEW_OID: i32 = 100_000;
pub(crate) fn view_oid(slot: usize) -> i32 {
    FIRST_VIEW_OID + slot as i32
}

pub(crate) fn domain_oid(slot: usize) -> i32 {
    crate::sql::types::oid::domain_oid(slot as u16)
}

/// Tables/materialized views and plain views have distinct composite-type OID
/// bands. PostgreSQL gives every row-bearing relation a separate pg_type row.
pub(crate) const FIRST_TABLE_COMPOSITE_TYPE_OID: i32 = 130_000;
pub(crate) const FIRST_VIEW_COMPOSITE_TYPE_OID: i32 = 140_000;
pub(crate) const FIRST_TABLE_COMPOSITE_ARRAY_TYPE_OID: i32 = 1_300_000;
pub(crate) const FIRST_VIEW_COMPOSITE_ARRAY_TYPE_OID: i32 = 1_400_000;
pub(crate) const FIRST_TOAST_RELATION_OID: i32 = 1_500_000;
pub(crate) const FIRST_TOAST_INDEX_OID: i32 = 1_600_000;
pub(crate) const FIRST_FOREIGN_KEY_TRIGGER_OID: i32 = 1_700_000;

pub(crate) const fn toast_relation_oid(slot: usize) -> i32 {
    FIRST_TOAST_RELATION_OID + slot as i32
}

pub(crate) const fn toast_index_oid(slot: usize) -> i32 {
    FIRST_TOAST_INDEX_OID + slot as i32
}

pub(crate) const fn foreign_key_trigger_oid(
    table_slot: usize,
    foreign_key: usize,
    ordinal: usize,
) -> i32 {
    FIRST_FOREIGN_KEY_TRIGGER_OID
        + (table_slot * crate::storage::MAX_FKEYS * 4 + foreign_key * 4 + ordinal) as i32
}

const fn foreign_key_action_routine(action: crate::storage::FkAction, update: bool) -> i32 {
    use crate::storage::FkAction;

    match (action, update) {
        (FkAction::Cascade, false) => 1646,
        (FkAction::Cascade, true) => 1647,
        (FkAction::Restrict, false) => 1648,
        (FkAction::Restrict, true) => 1649,
        (FkAction::SetNull, false) => 1650,
        (FkAction::SetNull, true) => 1651,
        (FkAction::SetDefault, false) => 1652,
        (FkAction::SetDefault, true) => 1653,
        (FkAction::NoAction, false) => 1654,
        (FkAction::NoAction, true) => 1655,
    }
}
/// PostgreSQL gives every named composite a backing `pg_class` relation whose
/// OID links `pg_type.typrelid` and its `pg_attribute` rows.
const FIRST_NAMED_COMPOSITE_RELATION_OID: i32 = 180_000;
const FIRST_EXTENDED_STATISTICS_OID: i32 = 300_000;
const FIRST_EXTENSION_OID: i32 = 500_000;

pub(crate) fn extension_oid(slot: usize) -> i32 {
    FIRST_EXTENSION_OID + slot as i32
}

pub(crate) fn extended_statistics_oid(slot: usize) -> i32 {
    FIRST_EXTENDED_STATISTICS_OID + slot as i32
}

pub(crate) fn extended_statistics_columns_by_oid<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    use core::fmt::Write as _;
    let Some((_, statistics)) = storage
        .extended_statistics_visible(txid)
        .find(|(slot, _)| extended_statistics_oid(*slot) == oid)
    else {
        return Ok(None);
    };
    let mut output = StackStr::<8192>::new();
    for (position, key) in statistics.keys_for(txid).iter().enumerate() {
        if position != 0 {
            let _ = output.write_str(", ");
        }
        match key {
            crate::storage::ExtendedStatisticsKey::Column(column) => {
                write_identifier(&mut output, column.as_str());
            }
            crate::storage::ExtendedStatisticsKey::Expression(expression) => {
                let _ = output.write_str(expression.as_str());
            }
        }
    }
    if output.is_truncated() {
        return Err(catalog_capacity_exceeded("pg_statistic_ext"));
    }
    arena
        .alloc_str(output.as_str())
        .map(Some)
        .map_err(|_| arena_full())
}

pub(crate) fn extended_statistics_definition_by_oid<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    use core::fmt::Write as _;
    let Some((_, statistics)) = storage
        .extended_statistics_visible(txid)
        .find(|(slot, _)| extended_statistics_oid(*slot) == oid)
    else {
        return Ok(None);
    };
    let mutable = statistics.definition_for(txid);
    let table_slot = usize::from(statistics.table);
    let table = storage.table_def(table_slot, txid);
    let mut output = StackStr::<8192>::new();
    let _ = output.write_str("CREATE STATISTICS ");
    write_identifier(&mut output, mutable.schema.as_str());
    let _ = output.write_char('.');
    write_identifier(&mut output, mutable.name.as_str());
    let _ = output.write_str(" ON ");
    for (position, key) in statistics.keys_for(txid).iter().enumerate() {
        if position != 0 {
            let _ = output.write_str(", ");
        }
        match key {
            crate::storage::ExtendedStatisticsKey::Column(column) => {
                write_identifier(&mut output, column.as_str());
            }
            crate::storage::ExtendedStatisticsKey::Expression(expression) => {
                let _ = output.write_str(expression.as_str());
            }
        }
    }
    let _ = output.write_str(" FROM ");
    if !matches!(
        storage.resolve_relation(None, table.name.as_str(), txid),
        Some(crate::storage::ResolvedRelation::Table(slot)) if slot == table_slot
    ) {
        write_identifier(&mut output, table.schema.as_str());
        let _ = output.write_char('.');
    }
    write_identifier(&mut output, table.name.as_str());
    if output.is_truncated() {
        return Err(catalog_capacity_exceeded("pg_statistic_ext"));
    }
    arena
        .alloc_str(output.as_str())
        .map(Some)
        .map_err(|_| arena_full())
}

pub(crate) fn extended_statistics_expressions_by_oid<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<Datum<'a>>, SqlError> {
    let Some((_, statistics)) = storage
        .extended_statistics_visible(txid)
        .find(|(slot, _)| extended_statistics_oid(*slot) == oid)
    else {
        return Ok(None);
    };
    let mut values = [Datum::Null; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
    let mut count = 0usize;
    for key in statistics.keys_for(txid) {
        if let crate::storage::ExtendedStatisticsKey::Expression(expression) = key {
            values[count] = text(expression.as_str(), arena)?;
            count += 1;
        }
    }
    if count == 0 {
        return Ok(None);
    }
    Ok(Some(Datum::Array {
        element: super::types::ArrElem::Text,
        raw: super::array::build(&values[..count], arena)?,
    }))
}

pub(crate) fn named_composite_relation_oid(slot: usize) -> i32 {
    FIRST_NAMED_COMPOSITE_RELATION_OID + slot as i32
}

fn composite_type_oid(storage: &Storage, schema: &str, name: &str, txid: u32) -> Option<i32> {
    match storage.resolve_relation(Some(schema), name, txid)? {
        crate::storage::ResolvedRelation::Table(slot) => {
            Some(FIRST_TABLE_COMPOSITE_TYPE_OID + slot as i32)
        }
        crate::storage::ResolvedRelation::View(slot) => {
            Some(FIRST_VIEW_COMPOSITE_TYPE_OID + slot as i32)
        }
        crate::storage::ResolvedRelation::Catalog => None,
    }
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
    expression_keys: [bool; crate::storage::MAX_INDEX_COLS],
    include_columns: [u16; crate::storage::MAX_INDEX_COLS],
    collations: [crate::sql::ast::Collation; crate::storage::MAX_INDEX_COLS],
    explicit_collations: [bool; crate::storage::MAX_INDEX_COLS],
    operator_classes: [Option<crate::storage::IndexOperatorClass>; crate::storage::MAX_INDEX_COLS],
    resolved_operator_classes:
        [Option<crate::storage::IndexOperatorClass>; crate::storage::MAX_INDEX_COLS],
    descending: [bool; crate::storage::MAX_INDEX_COLS],
    nulls_first: [bool; crate::storage::MAX_INDEX_COLS],
    n_cols: usize,
    n_include_cols: usize,
    nulls_not_distinct: bool,
    predicate: Option<StackStr<{ crate::storage::INDEX_PREDICATE_MAX }>>,
    is_primary: bool,
    is_unique: bool,
    is_constraint: bool,
    is_exclusion: bool,
    timing: crate::storage::ConstraintTiming,
    constraint_parent_oid: i32,
    explicit_definition: Option<crate::storage::IndexMutableDefinition>,
}

fn parent_constraint_oid(
    storage: &Storage,
    txid: u32,
    child_slot: usize,
    columns: &[u16],
    is_primary: bool,
    is_exclusion: bool,
) -> i32 {
    let Some(attachment) = storage.table_def(child_slot, txid).partition.attachment else {
        return 0;
    };
    let parent_slot = usize::from(attachment.parent);
    let parent = storage.table_def(parent_slot, txid);
    let mut position = 0usize;
    for (column, metadata) in parent.columns().iter().enumerate() {
        if metadata.primary {
            if !is_exclusion && is_primary && columns == [column as u16] {
                return index_oid(parent_slot, position) + 500_000;
            }
            position += 1;
        } else if metadata.unique {
            if !is_exclusion && !is_primary && columns == [column as u16] {
                return index_oid(parent_slot, position) + 500_000;
            }
            position += 1;
        }
    }
    for unique in parent.uniques() {
        if !is_exclusion && unique.is_primary == is_primary && unique.columns() == columns {
            return index_oid(parent_slot, position) + 500_000;
        }
        position += 1;
    }
    if is_exclusion {
        let child = storage.table_def(child_slot, txid);
        let Some(child_exclusion) = child
            .exclusions()
            .iter()
            .find(|exclusion| exclusion.columns() == columns)
        else {
            return 0;
        };
        for exclusion in parent.exclusions() {
            if exclusion.columns() == child_exclusion.columns()
                && exclusion.operators[..exclusion.n_cols]
                    == child_exclusion.operators[..child_exclusion.n_cols]
                && exclusion.predicate == child_exclusion.predicate
            {
                return index_oid(parent_slot, position) + 500_000;
            }
            position += 1;
        }
    }
    0
}

/// Enumerates every index relation psql `\d` would show: a single-column PK or
/// UNIQUE (from column flags), a multi-column PK/UNIQUE (from `uniques`), and
/// explicit `CREATE INDEX`es. OIDs are assigned by table slot + position so the
/// same index resolves identically here and in `pg_get_indexdef`.
fn visit_indexes(storage: &Storage, txid: u32, mut visit: impl FnMut(IdxInfo)) {
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let def = storage.table_def(slot, txid);
        let table_name = def.name.as_str();
        let toid = table_oid(storage, slot);
        let mut pos = 0usize;
        let mut mk = |columns: &[u16],
                      expression_keys: [bool; crate::storage::MAX_INDEX_COLS],
                      include_columns: &[u16],
                      descending: [bool; crate::storage::MAX_INDEX_COLS],
                      nulls_first: [bool; crate::storage::MAX_INDEX_COLS],
                      predicate: Option<StackStr<{ crate::storage::INDEX_PREDICATE_MAX }>>,
                      nulls_not_distinct: bool,
                      is_primary: bool,
                      is_unique: bool,
                      is_constraint: bool,
                      is_exclusion: bool,
                      timing: crate::storage::ConstraintTiming,
                      name: StackStr<64>| {
            let mut c = [0u16; crate::storage::MAX_INDEX_COLS];
            c[..columns.len()].copy_from_slice(columns);
            let mut included = [0u16; crate::storage::MAX_INDEX_COLS];
            included[..include_columns.len()].copy_from_slice(include_columns);
            let info = IdxInfo {
                oid: index_oid(slot, pos),
                table_oid: toid,
                table_slot: slot,
                name,
                columns: c,
                expression_keys,
                include_columns: included,
                collations: [crate::sql::ast::Collation::None; crate::storage::MAX_INDEX_COLS],
                explicit_collations: [false; crate::storage::MAX_INDEX_COLS],
                operator_classes: [None; crate::storage::MAX_INDEX_COLS],
                resolved_operator_classes: [None; crate::storage::MAX_INDEX_COLS],
                descending,
                nulls_first,
                n_cols: columns.len(),
                n_include_cols: include_columns.len(),
                nulls_not_distinct,
                predicate,
                is_primary,
                is_unique,
                is_constraint,
                is_exclusion,
                timing,
                constraint_parent_oid: if is_constraint {
                    parent_constraint_oid(storage, txid, slot, columns, is_primary, is_exclusion)
                } else {
                    0
                },
                explicit_definition: None,
            };
            pos += 1;
            info
        };
        // Single-column PK / UNIQUE carried as column flags.
        for (ci, col) in def.columns().iter().enumerate() {
            if col.primary {
                let name = stack_str_64(stack_format!(64, "{}_pkey", table_name).as_str());
                visit(mk(
                    &[ci as u16],
                    [false; crate::storage::MAX_INDEX_COLS],
                    &[],
                    [false; crate::storage::MAX_INDEX_COLS],
                    [false; crate::storage::MAX_INDEX_COLS],
                    None,
                    false,
                    true,
                    true,
                    true,
                    false,
                    crate::storage::ConstraintTiming::NotDeferrable,
                    name,
                ));
            } else if col.unique {
                let name = stack_str_64(
                    stack_format!(64, "{}_{}_key", table_name, col.name.as_str()).as_str(),
                );
                visit(mk(
                    &[ci as u16],
                    [false; crate::storage::MAX_INDEX_COLS],
                    &[],
                    [false; crate::storage::MAX_INDEX_COLS],
                    [false; crate::storage::MAX_INDEX_COLS],
                    None,
                    false,
                    false,
                    true,
                    true,
                    false,
                    crate::storage::ConstraintTiming::NotDeferrable,
                    name,
                ));
            }
        }
        // Multi-column PK / UNIQUE constraints.
        for uk in def.uniques() {
            // ALTER TABLE ... ADD CONSTRAINT ... USING INDEX transfers the
            // index relation into the constraint. Do not synthesize a second
            // catalog index: matching name plus typed key positions is the
            // durable attachment proof.
            if storage
                .indexes_for(def.schema.as_str(), table_name, txid)
                .any(|index| {
                    index.name_for(txid) == uk.name
                        && index.n_cols == uk.n_cols
                        && index.columns[..index.n_cols] == uk.columns[..uk.n_cols]
                        && index.expressions[..index.n_cols]
                            .iter()
                            .all(Option::is_none)
                })
            {
                continue;
            }
            visit(mk(
                uk.columns(),
                [false; crate::storage::MAX_INDEX_COLS],
                &[],
                [false; crate::storage::MAX_INDEX_COLS],
                [false; crate::storage::MAX_INDEX_COLS],
                None,
                false,
                uk.is_primary,
                true,
                true,
                false,
                uk.timing,
                stack_str_64(uk.name.as_str()),
            ));
        }
        for exclusion in def.exclusions() {
            visit(mk(
                exclusion.columns(),
                [false; crate::storage::MAX_INDEX_COLS],
                &[],
                [false; crate::storage::MAX_INDEX_COLS],
                [false; crate::storage::MAX_INDEX_COLS],
                exclusion
                    .predicate
                    .map(|predicate| StackStr::from_str(predicate.as_str())),
                false,
                false,
                false,
                true,
                true,
                exclusion.timing,
                stack_str_64(exclusion.name.as_str()),
            ));
        }
        // Explicit CREATE INDEX on this table.
        for index in storage.indexes_for(def.schema.as_str(), table_name, txid) {
            let attached = def.uniques().iter().find(|key| {
                key.name == index.name_for(txid)
                    && key.n_cols == index.n_cols
                    && key.columns[..key.n_cols] == index.columns[..index.n_cols]
                    && index.expressions[..index.n_cols]
                        .iter()
                        .all(Option::is_none)
            });
            let mut info = mk(
                &index.columns[..index.n_cols],
                index.expressions.map(|expression| expression.is_some()),
                &index.include_columns[..index.n_include_cols],
                index.descending,
                index.nulls_first,
                index.predicate,
                index.nulls_not_distinct,
                attached.is_some_and(|key| key.is_primary),
                index.unique,
                attached.is_some(),
                false,
                attached.map_or(crate::storage::ConstraintTiming::NotDeferrable, |key| {
                    key.timing
                }),
                stack_str_64(index.name_for(txid).as_str()),
            );
            info.oid = explicit_index_oid(&index);
            info.collations = index.collations;
            info.explicit_collations = index.explicit_collations;
            info.operator_classes = index.operator_classes;
            info.resolved_operator_classes = index.resolved_operator_classes;
            info.explicit_definition = Some(index.mutable_for(txid));
            visit(info);
        }
    }
}

fn empty_index() -> IdxInfo {
    IdxInfo {
        oid: 0,
        table_oid: 0,
        table_slot: 0,
        name: StackStr::new(),
        columns: [0; crate::storage::MAX_INDEX_COLS],
        expression_keys: [false; crate::storage::MAX_INDEX_COLS],
        include_columns: [0; crate::storage::MAX_INDEX_COLS],
        collations: [crate::sql::ast::Collation::None; crate::storage::MAX_INDEX_COLS],
        explicit_collations: [false; crate::storage::MAX_INDEX_COLS],
        operator_classes: [None; crate::storage::MAX_INDEX_COLS],
        resolved_operator_classes: [None; crate::storage::MAX_INDEX_COLS],
        descending: [false; crate::storage::MAX_INDEX_COLS],
        nulls_first: [false; crate::storage::MAX_INDEX_COLS],
        n_cols: 0,
        n_include_cols: 0,
        nulls_not_distinct: false,
        predicate: None,
        is_primary: false,
        is_unique: false,
        is_constraint: false,
        is_exclusion: false,
        timing: crate::storage::ConstraintTiming::NotDeferrable,
        constraint_parent_oid: 0,
        explicit_definition: None,
    }
}

fn index_expression_source(
    storage: &Storage,
    info: &IdxInfo,
    position: usize,
    txid: u32,
) -> Option<StackStr<{ crate::storage::INDEX_EXPRESSION_MAX }>> {
    if !info.expression_keys[position] {
        return None;
    }
    let table = storage.table_def(info.table_slot, txid);
    storage
        .indexes_for(table.schema.as_str(), table.name.as_str(), txid)
        .find(|index| index.name_for(txid).as_str() == info.name.as_str())?
        .expressions[position]
}

/// Materializes every visible index in the statement arena. This is sized from
/// the actual catalog, rather than a separate arbitrary limit, so an accepted
/// startup table/index capacity cannot silently disappear from introspection.
fn collect_indexes<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [IdxInfo], SqlError> {
    let mut count = 0usize;
    visit_indexes(storage, txid, |_| count += 1);
    let indexes = arena
        .alloc_slice_with(count, |_| empty_index())
        .map_err(|_| arena_full())?;
    let mut next = 0usize;
    visit_indexes(storage, txid, |index| {
        indexes[next] = index;
        next += 1;
    });
    Ok(indexes)
}

fn index_oid_by_name(
    storage: &Storage,
    txid: u32,
    schema: Option<&str>,
    name: &str,
) -> Option<i32> {
    let mut found = None;
    visit_indexes(storage, txid, |index| {
        if found.is_none()
            && index.name.as_str() == name
            && schema.is_none_or(|schema| {
                storage.table_def(index.table_slot, txid).schema.as_str() == schema
            })
        {
            found = Some(index.oid);
        }
    });
    found
}

fn has_index_oid(storage: &Storage, txid: u32, oid: i32) -> bool {
    let mut found = false;
    visit_indexes(storage, txid, |index| found |= index.oid == oid);
    found
}

fn stack_str_64(s: &str) -> StackStr<64> {
    let mut out = StackStr::<64>::new();
    let _ = core::fmt::Write::write_str(&mut out, s);
    out
}

/// The relation name for an OID, used to render `oid::regclass`. Resolves
/// ordinary tables, synthesized index relations, sequences, plain views, and
/// named composite backing relations.
pub fn relname_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    if let Some(name) = catalog_relation_name(oid) {
        return arena.alloc_str(name).map(Some).map_err(|_| arena_full());
    }
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        if table_oid(storage, slot) == oid {
            let bytes = arena
                .alloc_slice_copy(storage.table_def(slot, txid).name.as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    let indices = collect_indexes(storage, txid, arena)?;
    for info in indices {
        if info.oid == oid {
            let bytes = arena
                .alloc_slice_copy(info.name.as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    for slot in 0..storage.sequence_count() {
        let seq = storage.sequence_for(slot, txid);
        if !storage.sequence_slot_visible_to(slot, txid) {
            continue;
        }
        if sequence_oid(slot) == oid {
            let bytes = arena
                .alloc_slice_copy(seq.name.as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    for slot in 0..storage.view_count() {
        let view = storage.view(slot);
        if !storage.view_slot_visible_to(slot, txid) {
            continue;
        }
        if view_oid(slot) == oid {
            let bytes = arena
                .alloc_slice_copy(view.name_for(txid).as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    for (slot, composite) in storage.composites_with_slots_visible_to(txid) {
        if named_composite_relation_oid(slot) == oid {
            let bytes = arena
                .alloc_slice_copy(composite.name.as_str().as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    Ok(None)
}

/// The OID of the relation named `name`, for `'relname'::regclass`. Resolves
/// ordinary tables, synthesized index relations, sequences, plain views, and
/// named composite backing relations; `None` if no such relation.
pub fn reloid_of_name(storage: &Storage, txid: u32, name: &str) -> Option<i32> {
    let (schema, relation) = name
        .split_once('.')
        .map_or((None, name), |(schema, relation)| (Some(schema), relation));
    if schema.is_none_or(|schema| schema == "pg_catalog")
        && let Some(oid) = catalog_relation_oid(relation)
    {
        return Some(oid);
    }
    for slot in 0..storage.table_count() {
        if storage.table_slot_visible_to(slot, txid)
            && storage.table_def(slot, txid).name.as_str() == relation
            && schema.is_none_or(|schema| storage.table_def(slot, txid).schema.as_str() == schema)
        {
            return Some(table_oid(storage, slot));
        }
    }
    if let Some(oid) = index_oid_by_name(storage, txid, schema, relation) {
        return Some(oid);
    }
    for slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(slot, txid);
        if storage.sequence_slot_visible_to(slot, txid)
            && sequence.name.as_str() == relation
            && schema.is_none_or(|schema| sequence.schema.as_str() == schema)
        {
            return Some(sequence_oid(slot));
        }
    }
    let composite_slot = schema.map_or_else(
        || storage.resolve_composite_slot(relation, txid),
        |schema| storage.composite_slot(schema, relation, txid),
    );
    if let Some(slot) = composite_slot {
        return Some(named_composite_relation_oid(slot));
    }
    (0..storage.view_count())
        .find(|&slot| {
            storage.view_slot_visible_to(slot, txid)
                && storage.view(slot).name_for(txid).as_str() == relation
                && schema
                    .is_none_or(|schema| storage.view(slot).schema_for(txid).as_str() == schema)
        })
        .map(view_oid)
}

/// Catalog identity checks used by PostgreSQL's visibility helpers.  The
/// synthesized catalogs and executable SQL built-ins deliberately have
/// separate namespaces, so an OID is accepted only by the predicate that
/// owns it.
pub fn relation_oid_is_visible(storage: &Storage, txid: u32, oid: i32) -> bool {
    catalog_relation_oid_by_oid(oid)
        || (0..storage.table_count()).any(|slot| {
            storage.table_slot_visible_to(slot, txid) && table_oid(storage, slot) == oid
        })
        || has_index_oid(storage, txid, oid)
        || (0..storage.sequence_count())
            .any(|slot| storage.sequence_slot_visible_to(slot, txid) && sequence_oid(slot) == oid)
        || (0..storage.view_count())
            .any(|slot| storage.view_slot_visible_to(slot, txid) && view_oid(slot) == oid)
        || storage
            .composites_with_slots_visible_to(txid)
            .any(|(slot, _)| named_composite_relation_oid(slot) == oid)
}

fn catalog_relation_oid_by_oid(oid: i32) -> bool {
    catalog_relation_name(oid).is_some()
}

pub fn type_oid_is_visible(storage: &Storage, txid: u32, oid: i32) -> bool {
    if super::types::ColType::from_oid(oid).is_some()
        || matches!(
            oid,
            26 | 2249 | 2202 | 2203 | 2204 | 2205 | 2206 | 3115 | 4096 | 4097
        )
    {
        return true;
    }
    use super::types::oid as type_oid;
    let visible_slot = |first, count, visible: &dyn Fn(usize) -> bool| {
        (first..first + count as i32).contains(&oid) && visible((oid - first) as usize)
    };
    let domain_visible = |slot| storage.domain_slot_visible_to(slot, txid);
    let enum_visible = |slot| storage.enum_slot_visible_to(slot, txid);
    let composite_visible = |slot| storage.composite_slot_visible_to(slot, txid);
    visible_slot(
        type_oid::FIRST_DOMAIN,
        crate::storage::MAX_DOMAINS,
        &domain_visible,
    ) || visible_slot(
        type_oid::FIRST_DOMAIN_ARRAY,
        crate::storage::MAX_DOMAINS,
        &domain_visible,
    ) || visible_slot(
        type_oid::FIRST_ENUM,
        crate::storage::MAX_ENUMS,
        &enum_visible,
    ) || visible_slot(
        type_oid::FIRST_ENUM_ARRAY,
        crate::storage::MAX_ENUMS,
        &enum_visible,
    ) || visible_slot(
        type_oid::FIRST_COMPOSITE,
        crate::storage::MAX_COMPOSITES,
        &composite_visible,
    ) || visible_slot(
        type_oid::FIRST_COMPOSITE_ARRAY,
        crate::storage::MAX_COMPOSITES,
        &composite_visible,
    )
}

/// Resolves the exact OID for a visible user-defined type spelling, including
/// the automatically-created array type.  Call sites that dispatch routines
/// use this instead of reducing a domain to its storage representation.
pub(crate) fn user_type_oid(storage: &Storage, txid: u32, type_name: &str) -> Option<i32> {
    use crate::sql::types::oid;
    let (base, array) = type_name
        .strip_suffix("[]")
        .map_or((type_name, false), |base| (base, true));
    if let Some(slot) = storage.resolve_domain_slot(base, txid) {
        return Some(if array {
            oid::domain_array_oid(slot as u16)
        } else {
            oid::domain_oid(slot as u16)
        });
    }
    if let Some(slot) = storage.resolve_enum_slot(base, txid) {
        return Some(if array {
            oid::enum_array_oid(slot as u16)
        } else {
            oid::enum_oid(slot as u16)
        });
    }
    storage.resolve_composite_slot(base, txid).map(|slot| {
        if array {
            oid::composite_array_oid(slot as u16)
        } else {
            oid::composite_oid(slot as u16)
        }
    })
}

pub fn function_oid_is_visible(oid: i32) -> bool {
    INTRINSIC_ROUTINES.iter().any(|routine| routine.oid == oid)
}

fn identifier_spelling_matches(written: &str, name: &str) -> bool {
    let written = written.trim();
    if let Some(inner) = written
        .strip_prefix('"')
        .and_then(|written| written.strip_suffix('"'))
    {
        let mut expected = name.bytes();
        let mut input = inner.bytes();
        while let Some(byte) = input.next() {
            let byte = if byte == b'"' {
                if input.next() != Some(b'"') {
                    return false;
                }
                b'"'
            } else {
                byte
            };
            if expected.next() != Some(byte) {
                return false;
            }
        }
        expected.next().is_none()
    } else {
        !written.is_empty()
            && !written.bytes().any(|byte| {
                byte.is_ascii_whitespace() || matches!(byte, b'"' | b'.' | b'(' | b')' | b',')
            })
            && written
                .bytes()
                .map(|byte| byte.to_ascii_lowercase())
                .eq(name.bytes())
    }
}

fn split_qualified_routine_name(written: &str) -> Option<(Option<&str>, &str)> {
    let written = written.trim();
    let bytes = written.as_bytes();
    let mut quoted = false;
    let mut separator = None;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' if quoted && bytes.get(index + 1) == Some(&b'"') => index += 2,
            b'"' => {
                quoted = !quoted;
                index += 1;
            }
            b'.' if !quoted => {
                if separator.replace(index).is_some() {
                    return None;
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    if quoted {
        return None;
    }
    Some(match separator {
        Some(separator) => (
            Some(written[..separator].trim()),
            written[separator + 1..].trim(),
        ),
        None => (None, written),
    })
}

fn routine_name_matches(written: &str, schema: &str, name: &str) -> bool {
    let Some((written_schema, written_name)) = split_qualified_routine_name(written) else {
        return false;
    };
    identifier_spelling_matches(written_name, name)
        && written_schema
            .is_none_or(|written_schema| identifier_spelling_matches(written_schema, schema))
}

#[cfg(test)]
mod routine_name_tests {
    use super::{
        ACCESS_METHODS, PROCEDURAL_LANGUAGES, access_method_name, access_method_oid,
        procedural_language_name, procedural_language_oid, routine_name_matches,
    };

    #[test]
    fn static_comment_catalog_identities_round_trip() {
        for (name, oid, _, _, _) in ACCESS_METHODS {
            assert_eq!(access_method_oid(name), Some(oid));
            assert_eq!(access_method_name(oid), Some(name));
        }
        for (name, oid) in PROCEDURAL_LANGUAGES {
            assert_eq!(procedural_language_oid(name), Some(oid));
            assert_eq!(procedural_language_name(oid), Some(name));
        }
    }

    #[test]
    fn routine_names_parse_identifier_spelling() {
        assert!(routine_name_matches(
            "\"current_schema\"",
            "pg_catalog",
            "current_schema"
        ));
        assert!(routine_name_matches(
            "pg_catalog.\"current_schema\"",
            "pg_catalog",
            "current_schema"
        ));
        assert!(routine_name_matches("VERSION", "pg_catalog", "version"));
        assert!(!routine_name_matches("mixed", "public", "Mixed"));
        assert!(routine_name_matches("\"Mixed\"", "public", "Mixed"));
        assert!(routine_name_matches("\"a\"\"b\"", "public", "a\"b"));
    }
}

fn parse_routine_signature<'a>(
    written: &'a str,
    storage: &Storage,
    txid: u32,
    arguments: &mut [crate::storage::RoutineArgumentDef; MAX_ROUTINE_ARGUMENTS],
) -> Option<(&'a str, usize)> {
    let end = written.strip_suffix(')')?.len();
    let bytes = written.as_bytes();
    let mut quoted = false;
    let mut open = None;
    let mut offset = 0usize;
    while offset < end {
        match bytes[offset] {
            b'"' if quoted && bytes.get(offset + 1) == Some(&b'"') => offset += 2,
            b'"' => {
                quoted = !quoted;
                offset += 1;
            }
            b'(' if !quoted => {
                open = Some(offset);
                break;
            }
            _ => offset += 1,
        }
    }
    let open = open?;
    let name = written[..open].trim();
    let written_arguments = &written[open + 1..end];
    let written_arguments = written_arguments.trim();
    if written_arguments.is_empty() {
        return Some((name, 0));
    }
    let mut count = 0;
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut quoted = false;
    let mut offset = 0usize;
    loop {
        let at_end = offset == written_arguments.len();
        let separator = at_end
            || (!quoted && depth == 0 && written_arguments.as_bytes().get(offset) == Some(&b','));
        if separator {
            let written_type = written_arguments[start..offset].trim();
            if written_type.is_empty() || count == arguments.len() {
                return None;
            }
            let written_type = written_type
                .strip_prefix("pg_catalog.")
                .unwrap_or(written_type);
            let resolved = if let Some(ctype) = routine_builtin_type(written_type) {
                crate::storage::RoutineResult::builtin(ctype)
            } else {
                super::exec::resolve_routine_type(storage, txid, written_type).ok()?
            };
            arguments[count] = crate::storage::RoutineArgumentDef {
                name: crate::storage::SqlName::EMPTY,
                ctype: resolved.ctype,
                user_type: resolved.user_type,
            };
            count += 1;
            if at_end {
                break;
            }
            start = offset + 1;
            offset += 1;
            continue;
        }
        match written_arguments.as_bytes()[offset] {
            b'"' if quoted && written_arguments.as_bytes().get(offset + 1) == Some(&b'"') => {
                offset += 2;
            }
            b'"' => {
                quoted = !quoted;
                offset += 1;
            }
            b'(' if !quoted => {
                depth = depth.checked_add(1)?;
                offset += 1;
            }
            b')' if !quoted => {
                depth = depth.checked_sub(1)?;
                offset += 1;
            }
            _ => offset += 1,
        }
    }
    (!quoted && depth == 0).then_some((name, count))
}

fn routine_builtin_type(written: &str) -> Option<ColType> {
    if let Some(base) = written.strip_suffix("[]") {
        return crate::sql::types::ArrElem::from_coltype(routine_builtin_type(base)?)
            .map(ColType::Array);
    }
    match written {
        "timestamp without time zone" => return Some(ColType::Timestamp),
        "timestamp with time zone" => return Some(ColType::Timestamptz),
        "time without time zone" => return Some(ColType::Time),
        _ => {}
    }
    if let Some(ctype) = ColType::from_sql_name(written) {
        return Some(ctype);
    }
    let bytes = written.as_bytes();
    let mut quoted = false;
    let mut open = None;
    let mut depth = 0usize;
    let mut close = None;
    let mut offset = 0usize;
    while offset < bytes.len() {
        match bytes[offset] {
            b'"' if quoted && bytes.get(offset + 1) == Some(&b'"') => offset += 2,
            b'"' => {
                quoted = !quoted;
                offset += 1;
            }
            b'(' if !quoted => {
                if open.is_none() {
                    open = Some(offset);
                }
                depth += 1;
                offset += 1;
            }
            b')' if !quoted && depth != 0 => {
                depth -= 1;
                offset += 1;
                if depth == 0 {
                    close = Some(offset);
                    break;
                }
            }
            _ => offset += 1,
        }
    }
    let (open, close) = (open?, close?);
    let mut base = StackStr::<128>::new();
    use core::fmt::Write as _;
    base.write_str(written[..open].trim_end()).ok()?;
    base.write_str(&written[close..]).ok()?;
    routine_builtin_type(base.as_str())
}

fn intrinsic_argument_matches(
    routine: IntrinsicRoutine,
    arguments: &[crate::storage::RoutineArgumentDef],
) -> bool {
    if routine.argument_count != arguments.len() as i32 {
        return false;
    }
    routine
        .argument_types
        .split_ascii_whitespace()
        .zip(arguments)
        .all(|(oid, argument)| {
            argument.user_type.is_none() && oid.parse::<i32>().ok() == Some(argument.ctype.oid())
        })
}

fn routine_signature<'a>(
    name: &str,
    arguments: impl Iterator<Item = ColType>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    use core::fmt::Write;
    let mut text = StackStr::<256>::new();
    write_identifier(&mut text, name);
    text.write_char('(')
        .map_err(|_| super::eval::arena_full())?;
    for (index, argument) in arguments.enumerate() {
        if index != 0 {
            write!(text, ",").map_err(|_| super::eval::arena_full())?;
        }
        write!(text, "{}", argument.name()).map_err(|_| super::eval::arena_full())?;
    }
    write!(text, ")").map_err(|_| super::eval::arena_full())?;
    arena
        .alloc_str(text.as_str())
        .map_err(|_| super::eval::arena_full())
}

fn routine_declared_signature<'a>(
    storage: &Storage,
    txid: u32,
    routine: crate::storage::RoutineDef,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    use core::fmt::Write;
    let mut text = StackStr::<256>::new();
    let schema = routine.schema_for(txid);
    if !storage.schema_is_on_path(schema) {
        write_identifier(&mut text, schema.as_str());
        text.write_char('.')
            .map_err(|_| super::eval::arena_full())?;
    }
    write_identifier(&mut text, routine.name_for(txid).as_str());
    text.write_char('(')
        .map_err(|_| super::eval::arena_full())?;
    for (index, argument) in routine.arguments().iter().enumerate() {
        if index != 0 {
            write!(text, ",").map_err(|_| super::eval::arena_full())?;
        }
        let qualified = argument
            .user_type
            .is_some_and(|identity| !storage.schema_is_on_path(identity.schema));
        write_routine_type_name(&mut text, argument.ctype, argument.user_type, qualified)
            .map_err(|_| super::eval::arena_full())?;
    }
    write!(text, ")").map_err(|_| super::eval::arena_full())?;
    arena
        .alloc_str(text.as_str())
        .map_err(|_| super::eval::arena_full())
}

fn write_routine_type_name(
    output: &mut impl core::fmt::Write,
    ctype: ColType,
    user_type: Option<crate::storage::UserTypeName>,
    qualified: bool,
) -> Result<(), core::fmt::Error> {
    match user_type {
        Some(identity) => {
            if qualified {
                write!(output, "{}.", identity.schema.as_str())?;
            }
            write!(output, "{}", identity.name.as_str())?;
            if matches!(ctype, ColType::Array(_)) {
                write!(output, "[]")?;
            }
        }
        None => write!(output, "{}", ctype.name())?,
    }
    Ok(())
}

fn write_routine_type<const N: usize>(
    output: &mut StackStr<N>,
    argument: &crate::storage::RoutineArgumentDef,
) -> Result<(), core::fmt::Error> {
    use core::fmt::Write;
    if let Some(identity) = argument.user_type {
        write_identifier(output, identity.schema.as_str());
        output.write_char('.')?;
        write_identifier(output, identity.name.as_str());
        if matches!(argument.ctype, ColType::Array(_)) {
            output.write_str("[]")?;
        }
        Ok(())
    } else {
        output.write_str(argument.ctype.name())
    }
}

fn write_routine_result_type<const N: usize>(
    output: &mut StackStr<N>,
    result: &crate::storage::RoutineResult,
) -> Result<(), super::eval::SqlError> {
    let argument = crate::storage::RoutineArgumentDef {
        name: crate::storage::SqlName::EMPTY,
        ctype: result.ctype,
        user_type: result.user_type,
    };
    write_routine_type(output, &argument).map_err(|_| super::eval::arena_full())
}

/// Resolves the function catalog object types. `regproc` names a routine by
/// unqualified name; `regprocedure` includes its complete argument signature.
pub(crate) fn routine_oid_by_name(
    storage: &Storage,
    txid: u32,
    written: &str,
    signature: bool,
) -> Result<Option<i32>, SqlError> {
    let mut arguments = [crate::storage::RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS];
    let (name, argument_count) = if signature {
        let Some(parsed) = parse_routine_signature(written, storage, txid, &mut arguments) else {
            return Ok(None);
        };
        parsed
    } else {
        (written.trim(), 0)
    };
    let mut found = None;
    let mut consider = |oid| -> Result<(), SqlError> {
        if found.replace(oid).is_some() {
            return Err(sql_err!(
                sqlstate::AMBIGUOUS_FUNCTION,
                "more than one function named \"{}\"",
                written
            ));
        }
        Ok(())
    };
    for routine in INTRINSIC_ROUTINES {
        if !routine_name_matches(name, "pg_catalog", routine.name)
            || (signature && !intrinsic_argument_matches(*routine, &arguments[..argument_count]))
        {
            continue;
        }
        consider(routine.oid)?;
    }
    for slot in 0..storage.routine_count() {
        let routine = storage.routine_for(slot, txid);
        if !storage.routine_slot_visible_to(slot, txid)
            || !routine_name_matches(
                name,
                routine.schema_for(txid).as_str(),
                routine.name_for(txid).as_str(),
            )
            || (signature
                && (routine.argument_count != argument_count
                    || !routine
                        .arguments()
                        .iter()
                        .zip(&arguments[..argument_count])
                        .all(|(defined, argument)| {
                            defined.ctype == argument.ctype
                                && defined.user_type == argument.user_type
                        })))
        {
            continue;
        }
        consider(crate::storage::routine_oid(&routine))?;
    }
    Ok(found)
}

pub(crate) fn routine_name_by_oid<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    signature: bool,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    if let Some(routine) = INTRINSIC_ROUTINES.iter().find(|routine| routine.oid == oid) {
        if !signature {
            return if ident_needs_quotes(routine.name) {
                super::eval::quote_ident_str(routine.name, arena).map(Some)
            } else {
                arena
                    .alloc_str(routine.name)
                    .map(Some)
                    .map_err(|_| super::eval::arena_full())
            };
        }
        let arguments = routine
            .argument_types
            .split_ascii_whitespace()
            .filter_map(|oid| oid.parse::<i32>().ok())
            .filter_map(ColType::from_oid);
        return routine_signature(routine.name, arguments, arena).map(Some);
    }
    let Some(slot) = storage.routine_slot_by_oid(oid, txid) else {
        return Ok(None);
    };
    let routine = storage.routine_for(slot, txid);
    let current_name = routine.name_for(txid);
    let name = current_name.as_str();
    if !signature {
        if !storage.schema_is_on_path(routine.schema_for(txid)) {
            let mut qualified = StackStr::<256>::new();
            write_identifier(&mut qualified, routine.schema_for(txid).as_str());
            use core::fmt::Write;
            qualified
                .write_char('.')
                .map_err(|_| super::eval::arena_full())?;
            write_identifier(&mut qualified, name);
            return arena
                .alloc_str(qualified.as_str())
                .map(Some)
                .map_err(|_| super::eval::arena_full());
        }
        return if ident_needs_quotes(name) {
            super::eval::quote_ident_str(name, arena).map(Some)
        } else {
            arena
                .alloc_str(name)
                .map(Some)
                .map_err(|_| super::eval::arena_full())
        };
    }
    routine_declared_signature(storage, txid, routine, arena).map(Some)
}

fn parse_operator_signature<'a>(
    written: &'a str,
    storage: &Storage,
    txid: u32,
) -> Option<(&'a str, crate::storage::OperatorSignature)> {
    let (name, arguments) = written.strip_suffix(')')?.split_once('(')?;
    let (left, right) = arguments.split_once(',')?;
    let type_of = |name: &str| -> Option<Option<crate::storage::RoutineResult>> {
        let name = name.trim();
        if name.eq_ignore_ascii_case("none") {
            return Some(None);
        }
        let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
        let result = routine_builtin_type(name)
            .map(crate::storage::RoutineResult::builtin)
            .or_else(|| super::exec::resolve_routine_type(storage, txid, name).ok())?;
        Some(Some(result))
    };
    let signature = crate::storage::OperatorSignature {
        left: type_of(left)?,
        right: type_of(right)?,
    };
    (signature.arity() != 0).then_some((name.trim(), signature))
}

pub(crate) fn operator_oid_by_name(
    storage: &Storage,
    txid: u32,
    written: &str,
    signature: bool,
) -> Result<Option<i32>, SqlError> {
    let (name, arguments) = if signature {
        let Some((name, signature)) = parse_operator_signature(written, storage, txid) else {
            return Ok(None);
        };
        (name, Some(signature))
    } else {
        (written.trim(), None)
    };
    let Some((written_schema, written_name)) = split_qualified_routine_name(name) else {
        return Ok(None);
    };
    let ambiguous = || {
        sql_err!(
            sqlstate::AMBIGUOUS_FUNCTION,
            "more than one operator named \"{}\"",
            written
        )
    };
    let builtin = || -> Result<Option<i32>, SqlError> {
        let mut found = None;
        for operator in CATALOG_OPERATORS {
            let builtin_signature = crate::storage::OperatorSignature {
                left: Some(crate::storage::RoutineResult::builtin(operator.left)),
                right: Some(crate::storage::RoutineResult::builtin(operator.right)),
            };
            if !identifier_spelling_matches(written_name, operator.name)
                || arguments.is_some_and(|signature| signature != builtin_signature)
            {
                continue;
            }
            // The modeled evaluator row is only one representative of each
            // overloaded built-in name. A bare regoper must remain ambiguous.
            if !signature || found.replace(operator.oid).is_some() {
                return Err(ambiguous());
            }
        }
        Ok(found)
    };
    let user = |schema: &str| -> Result<Option<i32>, SqlError> {
        let mut found = None;
        for (slot, operator) in storage.operators_visible_to(txid) {
            if operator.schema.as_str() != schema
                || !identifier_spelling_matches(written_name, operator.name.as_str())
                || arguments.is_some_and(|signature| signature != operator.signature)
            {
                continue;
            }
            if found.replace(storage.operator(slot).oid()).is_some() {
                return Err(ambiguous());
            }
        }
        Ok(found)
    };
    if let Some(schema) = written_schema {
        return if identifier_spelling_matches(schema, "pg_catalog") {
            builtin()
        } else {
            let schema = (0..storage.schema_count()).find_map(|slot| {
                let definition = storage.schema_def(slot);
                (definition.visible_to(txid)
                    && identifier_spelling_matches(schema, definition.name.as_str()))
                .then_some(definition.name)
            });
            match schema {
                Some(schema) => user(schema.as_str()),
                None => Ok(None),
            }
        };
    }
    for entry in storage.path().entries() {
        let found = match entry {
            crate::storage::PathEntry::Catalog => builtin()?,
            crate::storage::PathEntry::Schema(slot) => {
                user(storage.schema_def(usize::from(*slot)).name.as_str())?
            }
        };
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

pub(crate) fn operator_oid_for_types(name: &str, left: ColType, right: ColType) -> Option<i32> {
    CATALOG_OPERATORS.iter().find_map(|operator| {
        (operator.name == name && operator.left == left && operator.right == right)
            .then_some(operator.oid)
    })
}

pub(crate) fn operator_name_by_oid<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    signature: bool,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    if let Some(operator) = CATALOG_OPERATORS
        .iter()
        .find(|operator| operator.oid == oid)
    {
        if !signature {
            return arena
                .alloc_str_display(format_args!("pg_catalog.{}", operator.name))
                .map(Some)
                .map_err(|_| super::eval::arena_full());
        }
        let mut text = StackStr::<96>::new();
        use core::fmt::Write;
        write!(
            text,
            "{}({},{})",
            operator.name,
            operator.left.name(),
            operator.right.name()
        )
        .map_err(|_| super::eval::arena_full())?;
        return arena
            .alloc_str(text.as_str())
            .map(Some)
            .map_err(|_| super::eval::arena_full());
    }
    let Some(slot) = storage.operator_slot_by_oid(oid, txid) else {
        return Ok(None);
    };
    let operator = storage.operator_for(slot, txid);
    let mut text = StackStr::<256>::new();
    use core::fmt::Write;
    if !storage.schema_is_on_path(operator.schema) {
        write_identifier(&mut text, operator.schema.as_str());
        text.write_char('.')
            .map_err(|_| super::eval::arena_full())?;
    }
    text.write_str(operator.name.as_str())
        .map_err(|_| super::eval::arena_full())?;
    if signature {
        text.write_char('(')
            .map_err(|_| super::eval::arena_full())?;
        for (index, argument) in [operator.signature.left, operator.signature.right]
            .into_iter()
            .enumerate()
        {
            if index != 0 {
                text.write_char(',')
                    .map_err(|_| super::eval::arena_full())?;
            }
            if let Some(argument) = argument {
                let qualified = argument
                    .user_type
                    .is_some_and(|identity| !storage.schema_is_on_path(identity.schema));
                write_routine_type_name(&mut text, argument.ctype, argument.user_type, qualified)
                    .map_err(|_| super::eval::arena_full())?;
            } else {
                text.write_str("NONE")
                    .map_err(|_| super::eval::arena_full())?;
            }
        }
        text.write_char(')')
            .map_err(|_| super::eval::arena_full())?;
    }
    arena
        .alloc_str(text.as_str())
        .map(Some)
        .map_err(|_| super::eval::arena_full())
}

pub fn function_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Some(slot) = storage.routine_slot_by_oid(oid, txid) else {
        return Ok(None);
    };
    let routine = storage.routine_for(slot, txid);
    if matches!(routine.kind, crate::storage::RoutineKind::Aggregate(_)) {
        return Err(sql_err!(
            sqlstate::WRONG_OBJECT_TYPE,
            "{} is an aggregate function",
            routine.name_for(txid).as_str()
        ));
    }
    use core::fmt::Write;
    let mut definition = crate::util::StackStr::<{ crate::storage::ROUTINE_SQL_MAX * 2 }>::new();
    write!(
        definition,
        "CREATE OR REPLACE {} {}.{}(",
        if matches!(routine.kind, crate::storage::RoutineKind::Procedure) {
            "PROCEDURE"
        } else {
            "FUNCTION"
        },
        routine.schema_for(txid).as_str(),
        routine.name_for(txid).as_str()
    )
    .map_err(|_| super::eval::arena_full())?;
    for (index, parameter) in routine.parameters().iter().enumerate() {
        if index != 0 {
            write!(definition, ", ").map_err(|_| super::eval::arena_full())?;
        }
        match parameter.mode {
            crate::storage::RoutineParameterMode::In { .. }
                if !matches!(routine.kind, crate::storage::RoutineKind::Procedure) => {}
            crate::storage::RoutineParameterMode::In { .. } => {
                write!(definition, "IN ").map_err(|_| super::eval::arena_full())?;
            }
            crate::storage::RoutineParameterMode::Out => {
                write!(definition, "OUT ").map_err(|_| super::eval::arena_full())?;
            }
            crate::storage::RoutineParameterMode::InOut { .. } => {
                write!(definition, "INOUT ").map_err(|_| super::eval::arena_full())?;
            }
            crate::storage::RoutineParameterMode::Variadic { .. } => {
                write!(definition, "VARIADIC ").map_err(|_| super::eval::arena_full())?;
            }
        }
        if !parameter.name.as_str().is_empty() {
            write!(definition, "{} ", parameter.name.as_str())
                .map_err(|_| super::eval::arena_full())?;
        }
        write_routine_type_name(&mut definition, parameter.ctype, parameter.user_type, true)
            .map_err(|_| super::eval::arena_full())?;
        if let Some(default) = parameter.mode.default() {
            write!(definition, " DEFAULT {}", default.as_str())
                .map_err(|_| super::eval::arena_full())?;
        }
    }
    match routine.kind {
        crate::storage::RoutineKind::Function { result } => {
            write!(definition, ") RETURNS ").map_err(|_| super::eval::arena_full())?;
            write_routine_result_type(&mut definition, &result)?;
            Ok(())
        }
        crate::storage::RoutineKind::SetFunction { result } => {
            write!(definition, ") RETURNS SETOF ").map_err(|_| super::eval::arena_full())?;
            write_routine_result_type(&mut definition, &result)?;
            Ok(())
        }
        crate::storage::RoutineKind::TableFunction => {
            write!(definition, ") RETURNS TABLE (").map_err(|_| super::eval::arena_full())?;
            for (index, column) in routine
                .table_columns()
                .expect("table routine columns")
                .iter()
                .enumerate()
            {
                if index != 0 {
                    write!(definition, ", ").map_err(|_| super::eval::arena_full())?;
                }
                write!(definition, "{} ", column.name.as_str())
                    .map_err(|_| super::eval::arena_full())?;
                write_routine_type_name(&mut definition, column.ctype, column.user_type, true)
                    .map_err(|_| super::eval::arena_full())?;
            }
            write!(definition, ")")
        }
        crate::storage::RoutineKind::RecordFunction { set_returning } => {
            write!(
                definition,
                ") RETURNS {}record",
                if set_returning { "SETOF " } else { "" }
            )
        }
        crate::storage::RoutineKind::Trigger => write!(definition, ") RETURNS trigger"),
        crate::storage::RoutineKind::EventTrigger => {
            write!(definition, ") RETURNS event_trigger")
        }
        crate::storage::RoutineKind::Procedure => write!(definition, ")"),
        crate::storage::RoutineKind::Aggregate(_) => unreachable!("rejected above"),
    }
    .map_err(|_| super::eval::arena_full())?;
    write!(
        definition,
        " LANGUAGE {}",
        match routine.language {
            crate::storage::RoutineLanguage::Sql => "sql",
            crate::storage::RoutineLanguage::PlPgSql => "plpgsql",
            crate::storage::RoutineLanguage::Internal => {
                unreachable!("non-aggregate stored routine has an executable language")
            }
        }
    )
    .map_err(|_| super::eval::arena_full())?;
    match routine.attributes.volatility {
        crate::storage::RoutineVolatility::Immutable => write!(definition, " IMMUTABLE"),
        crate::storage::RoutineVolatility::Stable => write!(definition, " STABLE"),
        crate::storage::RoutineVolatility::Volatile => Ok(()),
    }
    .map_err(|_| super::eval::arena_full())?;
    if routine.attributes.strict {
        write!(definition, " STRICT").map_err(|_| super::eval::arena_full())?;
    }
    match routine.attributes.parallel {
        crate::storage::RoutineParallel::Safe => write!(definition, " PARALLEL SAFE"),
        crate::storage::RoutineParallel::Restricted => {
            write!(definition, " PARALLEL RESTRICTED")
        }
        crate::storage::RoutineParallel::Unsafe => Ok(()),
    }
    .map_err(|_| super::eval::arena_full())?;
    if routine.attributes.security_definer {
        write!(definition, " SECURITY DEFINER").map_err(|_| super::eval::arena_full())?;
    }
    if routine.attributes.leakproof {
        write!(definition, " LEAKPROOF").map_err(|_| super::eval::arena_full())?;
    }
    if let Some(bits) = routine.attributes.cost_bits {
        write!(definition, " COST {}", f64::from_bits(bits))
            .map_err(|_| super::eval::arena_full())?;
    }
    if let Some(bits) = routine.attributes.rows_bits {
        write!(definition, " ROWS {}", f64::from_bits(bits))
            .map_err(|_| super::eval::arena_full())?;
    }
    for config in routine.configs() {
        write!(definition, " SET {} TO '", config.name.as_str())
            .map_err(|_| super::eval::arena_full())?;
        for character in config.value.as_str().chars() {
            write!(definition, "{character}").map_err(|_| super::eval::arena_full())?;
            if character == '\'' {
                write!(definition, "'").map_err(|_| super::eval::arena_full())?;
            }
        }
        write!(definition, "'").map_err(|_| super::eval::arena_full())?;
    }
    match routine.body_kind {
        crate::storage::RoutineBodyKind::String => {
            write!(definition, " AS '").map_err(|_| super::eval::arena_full())?;
            for character in routine.body.as_str().chars() {
                write!(definition, "{character}").map_err(|_| super::eval::arena_full())?;
                if character == '\'' {
                    write!(definition, "'").map_err(|_| super::eval::arena_full())?;
                }
            }
            write!(definition, "'").map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineBodyKind::Return => {
            write!(definition, " RETURN {}", routine.body.as_str())
                .map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineBodyKind::Atomic => {
            write!(definition, " BEGIN ATOMIC\n{};\nEND", routine.body.as_str())
                .map_err(|_| super::eval::arena_full())?;
        }
    }
    if definition.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "routine definition exceeds catalog rendering capacity"
        ));
    }
    Ok(Some(
        arena
            .alloc_str(definition.as_str())
            .map_err(|_| super::eval::arena_full())?,
    ))
}

pub fn trigger_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let mut found = None;
    for (_, trigger) in storage.triggers_with_slots_visible_to(txid) {
        if crate::storage::trigger_oid(&trigger) == oid {
            found = Some((trigger, trigger.target));
            break;
        }
        let crate::storage::TriggerTarget::Table(parent) = trigger.target else {
            continue;
        };
        if !matches!(trigger.level, super::ast::TriggerLevel::Row) {
            continue;
        }
        for child in 0..storage.table_count() {
            if storage.table_slot_visible_to(child, txid)
                && storage.partition_descends_from(child, usize::from(parent), txid)
                && partition_trigger_oid(&trigger, child)? == oid
            {
                found = Some((trigger, crate::storage::TriggerTarget::Table(child as u16)));
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let Some((trigger, render_target)) = found else {
        return Ok(None);
    };
    let mut output = StackStr::<8192>::new();
    use core::fmt::Write;
    output
        .write_str("CREATE ")
        .map_err(|_| super::eval::arena_full())?;
    if matches!(trigger.kind, crate::storage::TriggerKind::Constraint { .. }) {
        output
            .write_str("CONSTRAINT ")
            .map_err(|_| super::eval::arena_full())?;
    }
    output
        .write_str("TRIGGER ")
        .map_err(|_| super::eval::arena_full())?;
    write_identifier(&mut output, trigger.name_to(txid).as_str());
    write!(
        output,
        " {}",
        match trigger.timing {
            super::ast::TriggerTiming::Before => "BEFORE",
            super::ast::TriggerTiming::After => "AFTER",
            super::ast::TriggerTiming::InsteadOf => "INSTEAD OF",
        }
    )
    .map_err(|_| super::eval::arena_full())?;
    let mut event_count = 0usize;
    for (event, keyword) in [
        (super::ast::TriggerEvents::INSERT, "INSERT"),
        (super::ast::TriggerEvents::DELETE, "DELETE"),
        (super::ast::TriggerEvents::UPDATE, "UPDATE"),
        (super::ast::TriggerEvents::TRUNCATE, "TRUNCATE"),
    ] {
        if !trigger.events.contains(event) {
            continue;
        }
        write!(
            output,
            "{}{}",
            if event_count == 0 { " " } else { " OR " },
            keyword
        )
        .map_err(|_| super::eval::arena_full())?;
        event_count += 1;
        if event == super::ast::TriggerEvents::UPDATE && trigger.update_columns != 0 {
            output
                .write_str(" OF ")
                .map_err(|_| super::eval::arena_full())?;
            let definition = match render_target {
                crate::storage::TriggerTarget::Table(table) => {
                    *storage.table_def(usize::from(table), txid)
                }
                crate::storage::TriggerTarget::View(view) => {
                    super::exec::view_trigger_definition(storage, usize::from(view), txid, arena)?
                }
            };
            let mut written = 0usize;
            for column in 0..definition.n_columns {
                if trigger.update_columns & (1u64 << column) == 0 {
                    continue;
                }
                if written != 0 {
                    output
                        .write_str(", ")
                        .map_err(|_| super::eval::arena_full())?;
                }
                write_identifier(&mut output, definition.columns()[column].name.as_str());
                written += 1;
            }
        }
    }
    output
        .write_str(" ON ")
        .map_err(|_| super::eval::arena_full())?;
    match render_target {
        crate::storage::TriggerTarget::Table(table) => {
            let definition = storage.table_def(usize::from(table), txid);
            write_identifier(&mut output, definition.schema.as_str());
            output
                .write_char('.')
                .map_err(|_| super::eval::arena_full())?;
            write_identifier(&mut output, definition.name.as_str());
        }
        crate::storage::TriggerTarget::View(view) => {
            let definition = storage.view(usize::from(view));
            write_identifier(&mut output, definition.schema.as_str());
            output
                .write_char('.')
                .map_err(|_| super::eval::arena_full())?;
            write_identifier(&mut output, definition.name.as_str());
        }
    }
    output
        .write_char(' ')
        .map_err(|_| super::eval::arena_full())?;
    if let crate::storage::TriggerKind::Constraint {
        referenced_table,
        timing,
    } = trigger.kind
    {
        if let Some(table) = referenced_table {
            let definition = storage.table_def(usize::from(table), txid);
            output
                .write_str("FROM ")
                .map_err(|_| super::eval::arena_full())?;
            if storage.resolve_relation(None, definition.name.as_str(), txid)
                != Some(crate::storage::ResolvedRelation::Table(usize::from(table)))
            {
                write_identifier(&mut output, definition.schema.as_str());
                output
                    .write_char('.')
                    .map_err(|_| super::eval::arena_full())?;
            }
            write_identifier(&mut output, definition.name.as_str());
            output
                .write_char(' ')
                .map_err(|_| super::eval::arena_full())?;
        }
        write!(
            output,
            "{}DEFERRABLE INITIALLY {} ",
            if timing.is_deferrable() { "" } else { "NOT " },
            if timing.initially_deferred() {
                "DEFERRED"
            } else {
                "IMMEDIATE"
            }
        )
        .map_err(|_| super::eval::arena_full())?;
    }
    if let Some(old) = trigger.transition_tables.old() {
        output
            .write_str("REFERENCING OLD TABLE AS ")
            .map_err(|_| super::eval::arena_full())?;
        write_identifier(&mut output, old.as_str());
        output
            .write_char(' ')
            .map_err(|_| super::eval::arena_full())?;
    }
    if let Some(new) = trigger.transition_tables.new_table() {
        if trigger.transition_tables.old().is_none() {
            output
                .write_str("REFERENCING ")
                .map_err(|_| super::eval::arena_full())?;
        }
        output
            .write_str("NEW TABLE AS ")
            .map_err(|_| super::eval::arena_full())?;
        write_identifier(&mut output, new.as_str());
        output
            .write_char(' ')
            .map_err(|_| super::eval::arena_full())?;
    }
    write!(
        output,
        "FOR EACH {} ",
        if matches!(trigger.level, super::ast::TriggerLevel::Row) {
            "ROW"
        } else {
            "STATEMENT"
        }
    )
    .map_err(|_| super::eval::arena_full())?;
    if let Some(when) = trigger.when {
        write!(output, "WHEN ({}) ", when.as_str()).map_err(|_| super::eval::arena_full())?;
    }
    output
        .write_str("EXECUTE FUNCTION ")
        .map_err(|_| super::eval::arena_full())?;
    let routine = storage.routine_for(usize::from(trigger.function), txid);
    if storage.trigger_slot_for_call(routine.name.as_str(), txid)
        != Some(usize::from(trigger.function))
    {
        write_identifier(&mut output, routine.schema.as_str());
        output
            .write_char('.')
            .map_err(|_| super::eval::arena_full())?;
    }
    write_identifier(&mut output, routine.name.as_str());
    output
        .write_char('(')
        .map_err(|_| super::eval::arena_full())?;
    for (index, argument) in trigger.arguments.values().iter().enumerate() {
        if index != 0 {
            output
                .write_str(", ")
                .map_err(|_| super::eval::arena_full())?;
        }
        output
            .write_str(quote_literal_str(argument.as_str(), arena)?)
            .map_err(|_| super::eval::arena_full())?;
    }
    output
        .write_char(')')
        .map_err(|_| super::eval::arena_full())?;
    alloc_rendered(&output, "trigger definition is too long", arena).map(Some)
}

pub fn function_arguments_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    identity: bool,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Some(slot) = storage.routine_slot_by_oid(oid, txid) else {
        return Ok(None);
    };
    let routine = storage.routine_for(slot, txid);
    let mut output = StackStr::<256>::new();
    use core::fmt::Write;
    for (index, parameter) in routine.parameters().iter().enumerate() {
        if index != 0 {
            write!(output, ", ").map_err(|_| super::eval::arena_full())?;
        }
        let mode = match parameter.mode {
            crate::storage::RoutineParameterMode::In { .. }
                if !matches!(routine.kind, crate::storage::RoutineKind::Procedure) =>
            {
                None
            }
            crate::storage::RoutineParameterMode::In { .. } => Some("IN"),
            crate::storage::RoutineParameterMode::Out => Some("OUT"),
            crate::storage::RoutineParameterMode::InOut { .. } => Some("INOUT"),
            crate::storage::RoutineParameterMode::Variadic { .. } => Some("VARIADIC"),
        };
        if let Some(mode) = mode {
            write!(output, "{} ", mode).map_err(|_| super::eval::arena_full())?;
        }
        if !parameter.name.as_str().is_empty() {
            write!(output, "{} ", parameter.name.as_str())
                .map_err(|_| super::eval::arena_full())?;
        }
        write_routine_type_name(
            &mut output,
            parameter.ctype,
            parameter.user_type,
            parameter
                .user_type
                .is_some_and(|type_identity| !storage.schema_is_on_path(type_identity.schema)),
        )
        .map_err(|_| super::eval::arena_full())?;
        if !identity && let Some(default) = parameter.mode.default() {
            write!(output, " DEFAULT {}", default.as_str())
                .map_err(|_| super::eval::arena_full())?;
        }
    }
    if output.is_truncated() {
        return Err(super::eval::arena_full());
    }
    arena
        .alloc_str(output.as_str())
        .map(Some)
        .map_err(|_| super::eval::arena_full())
}

pub fn function_result_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Some(slot) = storage.routine_slot_by_oid(oid, txid) else {
        return Ok(None);
    };
    let routine = storage.routine_for(slot, txid);
    let mut output = StackStr::<256>::new();
    use core::fmt::Write;
    match &routine.kind {
        crate::storage::RoutineKind::Function { result } => {
            write_routine_type_name(
                &mut output,
                result.ctype,
                result.user_type,
                result
                    .user_type
                    .is_some_and(|identity| !storage.schema_is_on_path(identity.schema)),
            )
            .map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineKind::SetFunction { result } => {
            write!(output, "SETOF ").map_err(|_| super::eval::arena_full())?;
            write_routine_type_name(
                &mut output,
                result.ctype,
                result.user_type,
                result
                    .user_type
                    .is_some_and(|identity| !storage.schema_is_on_path(identity.schema)),
            )
            .map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineKind::TableFunction => {
            write!(output, "TABLE(").map_err(|_| super::eval::arena_full())?;
            for (index, column) in routine
                .table_columns()
                .expect("table routine columns")
                .iter()
                .enumerate()
            {
                if index != 0 {
                    write!(output, ", ").map_err(|_| super::eval::arena_full())?;
                }
                write!(output, "{} ", column.name.as_str())
                    .map_err(|_| super::eval::arena_full())?;
                write_routine_type_name(
                    &mut output,
                    column.ctype,
                    column.user_type,
                    column
                        .user_type
                        .is_some_and(|identity| !storage.schema_is_on_path(identity.schema)),
                )
                .map_err(|_| super::eval::arena_full())?;
            }
            write!(output, ")").map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineKind::RecordFunction { set_returning } => {
            if *set_returning {
                write!(output, "SETOF ").map_err(|_| super::eval::arena_full())?;
            }
            write!(output, "record").map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineKind::Trigger => {
            write!(output, "trigger").map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineKind::EventTrigger => {
            write!(output, "event_trigger").map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineKind::Procedure => {
            write!(output, "void").map_err(|_| super::eval::arena_full())?;
        }
        crate::storage::RoutineKind::Aggregate(aggregate) => {
            write_routine_type_name(
                &mut output,
                aggregate.result_type.ctype,
                aggregate.result_type.user_type,
                aggregate
                    .result_type
                    .user_type
                    .is_some_and(|identity| !storage.schema_is_on_path(identity.schema)),
            )
            .map_err(|_| super::eval::arena_full())?;
        }
    }
    arena
        .alloc_str(output.as_str())
        .map(Some)
        .map_err(|_| super::eval::arena_full())
}

pub fn collation_oid_is_visible(storage: &Storage, txid: u32, oid: i32) -> bool {
    crate::sql::ast::Collation::BUILTIN
        .iter()
        .any(|collation| collation.oid() == oid)
        || storage
            .collations_visible_to(txid)
            .any(|(slot, _)| crate::sql::ast::Collation::Catalog(slot as u8).oid() == oid)
}

pub fn relation_oid_is_publishable(storage: &Storage, txid: u32, oid: i32) -> bool {
    (0..storage.table_count())
        .any(|slot| storage.table_slot_visible_to(slot, txid) && table_oid(storage, slot) == oid)
}

/// Stored SELECT text for `pg_get_viewdef`, by relation OID.
pub fn view_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    for slot in 0..storage.table_count() {
        let table = storage.table(slot);
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        if table_oid(storage, slot) != oid {
            continue;
        }
        let Some(view) =
            storage.find_matview(table.def.schema.as_str(), table.def.name.as_str(), txid)
        else {
            return Ok(None);
        };
        return arena
            .alloc_str_display(format_args!("{};", view.sql.as_str()))
            .map(Some)
            .map_err(|_| arena_full());
    }
    for (slot, _) in storage.views_visible_to(txid) {
        if view_oid(slot) == oid {
            return arena
                .alloc_str_display(format_args!("{};", storage.view_sql(slot)))
                .map(Some)
                .map_err(|_| arena_full());
        }
    }
    Ok(None)
}

pub fn rule_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    use core::fmt::Write as _;
    let Some((_, rule)) = storage
        .rules_visible_to(txid)
        .find(|(_, rule)| rule.oid() == oid)
    else {
        return Ok(None);
    };
    let definition = rule.definition_for(txid);
    let (schema, relation) = match definition.target {
        crate::storage::RuleTarget::Table(slot) => {
            let table = storage.table_def(usize::from(slot), txid);
            (table.schema, table.name)
        }
        crate::storage::RuleTarget::View(slot) => {
            let view = storage.view(usize::from(slot));
            (view.schema_for(txid), view.name)
        }
    };
    let rule_name = super::eval::quote_ident_str(definition.name.as_str(), arena)?;
    let schema = super::eval::quote_ident_str(schema.as_str(), arena)?;
    let relation = super::eval::quote_ident_str(relation.as_str(), arena)?;
    let mut out = StackStr::<{ crate::storage::RULE_SQL_MAX + 512 }>::new();
    let _ = write!(
        out,
        "CREATE RULE {} AS ON {} TO {}.{}",
        rule_name,
        match definition.event {
            crate::storage::RewriteEvent::Select => "SELECT",
            crate::storage::RewriteEvent::Insert => "INSERT",
            crate::storage::RewriteEvent::Update => "UPDATE",
            crate::storage::RewriteEvent::Delete => "DELETE",
        },
        schema,
        relation,
    );
    if let Some(condition) = definition.condition_sql() {
        let _ = write!(out, " WHERE ({condition})");
    }
    let _ = write!(
        out,
        " DO {}",
        if matches!(definition.mode, crate::storage::RewriteMode::Instead) {
            "INSTEAD "
        } else {
            "ALSO "
        }
    );
    if definition.action_count == 0 {
        let _ = out.write_str("NOTHING;");
    } else if definition.action_count == 1 {
        let _ = write!(
            out,
            "{};",
            definition.action_sql().next().expect("one action")
        );
    } else {
        let _ = out.write_char('(');
        for action in definition.action_sql() {
            let _ = write!(out, "{action};");
        }
        let _ = out.write_str(");");
    }
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "formatted rewrite rule exceeds static output capacity"
        ));
    }
    arena
        .alloc_str(out.as_str())
        .map(Some)
        .map_err(|_| arena_full())
}

/// The bytes occupied by a relation's visible encoded row images. Plain views
/// and indexes have no physical row store in pos3ql, so their exact size is
/// zero; tables and materialized views share the table row store.
pub fn relation_size(storage: &Storage, txid: u32, oid: i32) -> Result<Option<i64>, SqlError> {
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        if table_oid(storage, slot) != oid {
            continue;
        }
        return table_size(storage, txid, slot).map(Some);
    }
    if storage
        .views_visible_to(txid)
        .any(|(slot, _)| view_oid(slot) == oid)
    {
        return Ok(Some(0));
    }
    if has_index_oid(storage, txid, oid) {
        return Ok(Some(0));
    }
    Ok(None)
}

pub fn database_size(storage: &Storage, txid: u32) -> Result<i64, SqlError> {
    let mut total = 0i64;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        total = total
            .checked_add(table_size(storage, txid, slot)?)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::NUMERIC_OUT_OF_RANGE,
                    "database size exceeds bigint"
                )
            })?;
    }
    Ok(total)
}

fn table_size(storage: &Storage, txid: u32, slot: usize) -> Result<i64, SqlError> {
    let mut bytes = 0i64;
    storage.for_each_row_state(slot, &mut |rowid, state| {
        if let Some(home) = storage.visible_row_home(slot, rowid, state, txid)? {
            let len = match home {
                crate::storage::RowHome::Heap(location) => location.len,
                crate::storage::RowHome::Spilled { len, .. } => len,
            };
            bytes = bytes.checked_add(i64::from(len)).ok_or_else(|| {
                sql_err!(
                    sqlstate::NUMERIC_OUT_OF_RANGE,
                    "relation size exceeds bigint"
                )
            })?;
        }
        Ok(core::ops::ControlFlow::Continue(()))
    })?;
    Ok(bytes)
}

/// `pg_description`: one row per committed object comment, resolving each to
/// its `(objoid, classoid, objsubid)`. A comment whose object no longer
/// resolves to an OID is omitted, matching that its row would be dangling.
fn pg_description<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_description",
        &[
            ("objoid", ColType::Oid),
            ("classoid", ColType::Oid),
            ("objsubid", ColType::Int4),
            ("description", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; crate::storage::MAX_COMMENTS] = [&[]; crate::storage::MAX_COMMENTS];
    let mut n = 0;
    for (class, schema, name, subid, description) in storage.comments_visible(txid) {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_description"));
        }
        let (objoid, classoid) = match class {
            crate::storage::CommentClass::Relation => {
                match relation_oid_of(storage, txid, schema, name) {
                    Some(oid) => (oid, PG_CLASS_OID),
                    None => continue,
                }
            }
            crate::storage::CommentClass::Schema => {
                (namespace_oid(storage, name), PG_NAMESPACE_OID)
            }
            crate::storage::CommentClass::Type => match type_oid_of(storage, schema, name, txid) {
                Some(oid) => (oid, PG_TYPE_OID),
                None => continue,
            },
            crate::storage::CommentClass::Tablespace
            | crate::storage::CommentClass::Database
            | crate::storage::CommentClass::Role => {
                continue;
            }
            crate::storage::CommentClass::ForeignServer => {
                let Ok(slot) = usize::try_from(subid) else {
                    continue;
                };
                let Some(_) = storage.foreign_server_by_slot(slot, txid) else {
                    continue;
                };
                (foreign_server_oid(slot), 1417)
            }
            crate::storage::CommentClass::ForeignDataWrapper => {
                let Ok(slot) = usize::try_from(subid) else {
                    continue;
                };
                let Some(_) = storage.foreign_wrapper_by_slot(slot, txid) else {
                    continue;
                };
                (foreign_data_wrapper_oid(slot), 2328)
            }
            crate::storage::CommentClass::AccessMethod => {
                let Ok(oid) = i32::try_from(subid) else {
                    continue;
                };
                if access_method_name_in(storage, txid, oid) != Some(name) {
                    continue;
                }
                (oid, PG_AM_OID)
            }
            crate::storage::CommentClass::ProceduralLanguage => {
                let Ok(oid) = i32::try_from(subid) else {
                    continue;
                };
                if procedural_language_name(oid) != Some(name) {
                    continue;
                }
                (oid, PG_LANGUAGE_OID)
            }
            crate::storage::CommentClass::Cast => {
                let Some((_, cast)) = storage
                    .casts_visible_to(txid)
                    .find(|(_, cast)| cast.oid() as u32 == subid)
                else {
                    continue;
                };
                (cast.oid(), PG_CAST_OID)
            }
            crate::storage::CommentClass::Operator => {
                let Ok(oid) = i32::try_from(subid) else {
                    continue;
                };
                let Some(slot) = storage.operator_slot_by_oid(oid, txid) else {
                    continue;
                };
                (storage.operator(slot).oid(), PG_OPERATOR_OID)
            }
            crate::storage::CommentClass::OperatorFamily => {
                let Ok(oid) = i32::try_from(subid) else {
                    continue;
                };
                let Some(slot) = storage.operator_family_slot_by_oid(oid, txid) else {
                    continue;
                };
                (storage.operator_family(slot).oid(), PG_OPFAMILY_OID)
            }
            crate::storage::CommentClass::OperatorClass => {
                let Ok(oid) = i32::try_from(subid) else {
                    continue;
                };
                let Some(oid) = crate::storage::OperatorClassOid::parse(oid) else {
                    continue;
                };
                let Some(slot) = storage.operator_class_slot_by_oid(oid, txid) else {
                    continue;
                };
                (storage.operator_class(slot).oid(), PG_OPCLASS_OID)
            }
            crate::storage::CommentClass::Constraint => {
                let Ok(table) = usize::try_from(subid) else {
                    continue;
                };
                if !storage.table_slot_visible_to(table, txid) {
                    continue;
                }
                let Some(oid) = table_constraint_oid(storage, txid, table, name) else {
                    continue;
                };
                (oid, PG_CONSTRAINT_OID)
            }
            crate::storage::CommentClass::Extension => {
                let Some(slot) = storage.extension_slot(name, txid) else {
                    continue;
                };
                (extension_oid(slot), 3079)
            }
            crate::storage::CommentClass::Trigger => {
                let trigger = storage
                    .triggers_with_slots_visible_to(txid)
                    .map(|(_, trigger)| trigger)
                    .find(|trigger| {
                        trigger.name_to(txid).as_str() == name
                            && trigger.target.comment_subid() == subid
                    });
                let Some(trigger) = trigger else { continue };
                (crate::storage::trigger_oid(&trigger), 2620)
            }
            crate::storage::CommentClass::Collation => {
                let Some(slot) = storage.collation_slot(schema, name, txid) else {
                    continue;
                };
                (storage.collation(slot).oid(slot), PG_COLLATION_OID)
            }
            crate::storage::CommentClass::Conversion => {
                let Some(slot) = storage.conversion_slot(schema, name, txid) else {
                    continue;
                };
                (storage.conversion(slot).oid(slot), PG_CONVERSION_OID)
            }
            crate::storage::CommentClass::EventTrigger => {
                let Some(slot) = storage.event_trigger_slot(name, txid) else {
                    continue;
                };
                (storage.event_trigger(slot).oid(), 3466)
            }
            crate::storage::CommentClass::LargeObject => {
                let Ok(raw_oid) = name.parse::<u32>() else {
                    continue;
                };
                let Some(oid) = crate::sql::ast::LargeObjectId::parse(raw_oid) else {
                    continue;
                };
                if storage.large_object_slot(oid, txid).is_none() {
                    continue;
                }
                (raw_oid as i32, PG_LARGEOBJECT_OID)
            }
            crate::storage::CommentClass::Routine => {
                let Ok(oid) = i32::try_from(subid) else {
                    continue;
                };
                let Some(slot) = storage.routine_slot_by_oid(oid, txid) else {
                    continue;
                };
                (
                    crate::storage::routine_oid(&storage.routine_for(slot, txid)),
                    PG_PROC_OID,
                )
            }
            crate::storage::CommentClass::Policy => {
                let Ok(oid) = i32::try_from(subid) else {
                    continue;
                };
                let Some((_, policy)) = storage
                    .policies_with_slots_visible_to(txid)
                    .find(|(_, policy)| crate::storage::policy_oid(policy) == oid)
                else {
                    continue;
                };
                (crate::storage::policy_oid(policy), PG_POLICY_OID)
            }
            crate::storage::CommentClass::Statistics => {
                let Some((slot, _)) = storage
                    .extended_statistics_visible(txid)
                    .find(|(slot, _)| *slot as u32 == subid)
                else {
                    continue;
                };
                (extended_statistics_oid(slot), PG_STATISTIC_EXT_OID)
            }
            crate::storage::CommentClass::Publication => {
                let Some((slot, _)) = storage
                    .publications_with_slots_visible_to(txid)
                    .find(|(slot, _)| *slot as u32 == subid)
                else {
                    continue;
                };
                (publication_oid(slot), PG_PUBLICATION_OID)
            }
            crate::storage::CommentClass::Subscription => {
                let Some((_, subscription)) = storage
                    .subscriptions_with_slots_visible_to(txid)
                    .find(|(_, subscription)| subscription.created_at as u32 == subid)
                else {
                    continue;
                };
                (subscription_oid(subscription), PG_SUBSCRIPTION_OID)
            }
            crate::storage::CommentClass::Rule => {
                let rule = storage
                    .rules_visible_to(txid)
                    .map(|(_, rule)| rule)
                    .find(|rule| {
                        let definition = rule.definition_for(txid);
                        definition.name.as_str() == name
                            && definition.target.comment_subid() == subid
                    });
                let Some(rule) = rule else { continue };
                (rule.oid(), PG_REWRITE_OID)
            }
            crate::storage::CommentClass::TextSearchParser
            | crate::storage::CommentClass::TextSearchTemplate
            | crate::storage::CommentClass::TextSearchDictionary
            | crate::storage::CommentClass::TextSearchConfiguration => {
                let (kind, classoid) = match class {
                    crate::storage::CommentClass::TextSearchParser => (
                        crate::sql::ast::TextSearchObjectKind::Parser,
                        PG_TS_PARSER_OID,
                    ),
                    crate::storage::CommentClass::TextSearchTemplate => (
                        crate::sql::ast::TextSearchObjectKind::Template,
                        PG_TS_TEMPLATE_OID,
                    ),
                    crate::storage::CommentClass::TextSearchDictionary => (
                        crate::sql::ast::TextSearchObjectKind::Dictionary,
                        PG_TS_DICT_OID,
                    ),
                    crate::storage::CommentClass::TextSearchConfiguration => (
                        crate::sql::ast::TextSearchObjectKind::Configuration,
                        PG_TS_CONFIG_OID,
                    ),
                    _ => unreachable!("text-search comment class guard"),
                };
                let Some(slot) = storage.text_search_slot(kind, schema, name, txid) else {
                    continue;
                };
                (
                    storage.text_search_object(slot).definition_for(txid).oid(),
                    classoid,
                )
            }
        };
        let catalog_subid = if matches!(
            class,
            crate::storage::CommentClass::Trigger
                | crate::storage::CommentClass::Rule
                | crate::storage::CommentClass::Routine
                | crate::storage::CommentClass::Policy
                | crate::storage::CommentClass::Statistics
                | crate::storage::CommentClass::Publication
                | crate::storage::CommentClass::Subscription
                | crate::storage::CommentClass::ForeignServer
                | crate::storage::CommentClass::ForeignDataWrapper
                | crate::storage::CommentClass::AccessMethod
                | crate::storage::CommentClass::ProceduralLanguage
                | crate::storage::CommentClass::Cast
                | crate::storage::CommentClass::Operator
                | crate::storage::CommentClass::OperatorFamily
                | crate::storage::CommentClass::OperatorClass
                | crate::storage::CommentClass::Constraint
        ) {
            0
        } else {
            subid as i32
        };
        out[n] = row(
            &[
                Datum::Oid(objoid as u32),
                Datum::Oid(classoid as u32),
                Datum::Int4(catalog_subid),
                text(description, arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_shdescription<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_shdescription",
        &[
            ("objoid", ColType::Int4),
            ("classoid", ColType::Int4),
            ("description", ColType::Text),
        ],
    );
    let mut rows: [&[Datum]; crate::storage::MAX_COMMENTS] = [&[]; crate::storage::MAX_COMMENTS];
    let mut count = 0;
    for (class, _, name, subid, description) in storage.comments_visible(txid) {
        if !matches!(
            class,
            crate::storage::CommentClass::Tablespace
                | crate::storage::CommentClass::Database
                | crate::storage::CommentClass::Role
        ) || subid != 0
        {
            continue;
        }
        if class == crate::storage::CommentClass::Database {
            let Some(slot) = storage.database_slot(name, txid) else {
                continue;
            };
            rows[count] = row(
                &[
                    Datum::Int4(storage.database(slot).oid.get()),
                    Datum::Int4(1262),
                    text(description, arena)?,
                ],
                arena,
            )?;
            count += 1;
            continue;
        }
        if class == crate::storage::CommentClass::Role {
            let Ok(oid) = i32::try_from(subid) else {
                continue;
            };
            if storage.role_slot_by_oid(oid, txid).is_none() {
                continue;
            }
            rows[count] = row(
                &[
                    Datum::Int4(oid),
                    Datum::Int4(1260),
                    text(description, arena)?,
                ],
                arena,
            )?;
            count += 1;
            continue;
        }
        let Some((_, tablespace)) = storage
            .tablespaces_visible_to(txid)
            .find(|(_, tablespace)| tablespace.name_for(txid).as_str() == name)
        else {
            continue;
        };
        rows[count] = row(
            &[
                Datum::Int4(tablespace_oid(*tablespace)),
                Datum::Int4(PG_TABLESPACE_OID),
                text(description, arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(def, &rows[..count], arena)
}

/// The `pg_class` OID of a relation named `name` in `schema`: an ordinary
/// table or materialized-view backing table, a sequence, a plain view, or an
/// index.
fn relation_oid_of(storage: &Storage, txid: u32, schema: &str, name: &str) -> Option<i32> {
    if let Some(slot) = storage.find_visible(schema, name, txid) {
        return Some(table_oid(storage, slot));
    }
    if let Some(slot) = storage.sequence_slot(schema, name, txid) {
        return Some(sequence_oid(slot));
    }
    for slot in 0..storage.view_count() {
        let view = storage.view(slot);
        if storage.view_slot_visible_to(slot, txid)
            && view.schema_for(txid).as_str() == schema
            && view.name_for(txid).as_str() == name
        {
            return Some(view_oid(slot));
        }
    }
    index_oid_by_name(storage, txid, Some(schema), name)
}

/// Resolves a modeled built-in SQL type to its real catalog name and OID.
/// Qualified names accept only `pg_type.typname` spellings; PostgreSQL does
/// not resolve aliases such as `pg_catalog.integer`.
pub fn builtin_type_identity(name: &str, allow_aliases: bool) -> Option<(&'static str, i32)> {
    use crate::sql::types::{ArrElem, oid};

    let catalog_only = match name {
        "oid" => Some(("oid", 26)),
        "record" => Some(("record", oid::RECORD)),
        "regproc" => Some(("regproc", oid::REGPROC)),
        "regprocedure" => Some(("regprocedure", oid::REGPROCEDURE)),
        "regoper" => Some(("regoper", oid::REGOPER)),
        "regoperator" => Some(("regoperator", oid::REGOPERATOR)),
        "regclass" => Some(("regclass", oid::REGCLASS)),
        "regtype" => Some(("regtype", oid::REGTYPE)),
        "regnamespace" => Some(("regnamespace", oid::REGNAMESPACE)),
        "regrole" => Some(("regrole", oid::REGROLE)),
        "regconfig" => Some(("regconfig", oid::REGCONFIG)),
        "regdictionary" => Some(("regdictionary", oid::REGDICTIONARY)),
        "fdw_handler" => Some(("fdw_handler", oid::FDW_HANDLER)),
        _ => None,
    };
    if catalog_only.is_some() {
        return catalog_only;
    }
    // SERIAL spellings are DDL shorthand, not entries in pg_type.
    if matches!(
        name,
        "serial" | "serial2" | "serial4" | "serial8" | "smallserial" | "bigserial"
    ) {
        return None;
    }
    if let Some(element) = ArrElem::BUILTIN
        .iter()
        .find(|element| element.catalog_name() == name)
    {
        return Some((element.catalog_name(), element.array_oid()));
    }
    let column_type = ColType::from_sql_name(name)?;
    let canonical = column_type.catalog_name();
    if allow_aliases || name == canonical {
        return Some((canonical, column_type.oid()));
    }
    if let (ColType::Array(element), Some(base)) = (column_type, name.strip_suffix("[]"))
        && ColType::from_sql_name(base).is_some_and(|base_type| base_type.catalog_name() == base)
    {
        return Some((element.catalog_name(), element.array_oid()));
    }
    None
}

fn type_oid_of(storage: &Storage, schema: &str, name: &str, txid: u32) -> Option<i32> {
    if let Some(slot) = storage.domain_slot(schema, name, txid) {
        return Some(domain_oid(slot));
    }
    if let Some(slot) = storage.enum_slot(schema, name, txid) {
        return Some(crate::sql::types::oid::enum_oid(slot as u16));
    }
    if let Some(slot) = storage.composite_slot(schema, name, txid) {
        return Some(crate::sql::types::oid::composite_oid(slot as u16));
    }
    if let Some(oid) = composite_type_oid(storage, schema, name, txid) {
        return Some(oid);
    }
    if schema == "pg_catalog" {
        return builtin_type_identity(name, false).map(|(_, oid)| oid);
    }
    None
}

/// Search-path-aware SQL spelling of a user-defined type OID. `format_type`
/// uses this for domains/enums and their automatically-created array types;
/// built-in types deliberately return `None` and stay on the static path.
pub fn user_type_name_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    use crate::sql::types::oid as type_oid;
    let (schema, name, visible, array, enumeration) = if (type_oid::FIRST_DOMAIN
        ..type_oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_DOMAIN) as usize;
        let definition = storage.domain(slot);
        (
            definition.schema,
            definition.name,
            storage.domain_slot_visible_to(slot, txid),
            false,
            false,
        )
    } else if (type_oid::FIRST_DOMAIN_ARRAY
        ..type_oid::FIRST_DOMAIN_ARRAY + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_DOMAIN_ARRAY) as usize;
        let definition = storage.domain(slot);
        (
            definition.schema,
            definition.name,
            storage.domain_slot_visible_to(slot, txid),
            true,
            false,
        )
    } else if (type_oid::FIRST_ENUM..type_oid::FIRST_ENUM + crate::storage::MAX_ENUMS as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_ENUM) as usize;
        let definition = storage.enum_for(slot, txid);
        (
            definition.schema,
            definition.name,
            storage.enum_slot_visible_to(slot, txid),
            false,
            true,
        )
    } else if (type_oid::FIRST_ENUM_ARRAY
        ..type_oid::FIRST_ENUM_ARRAY + crate::storage::MAX_ENUMS as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_ENUM_ARRAY) as usize;
        let definition = storage.enum_for(slot, txid);
        (
            definition.schema,
            definition.name,
            storage.enum_slot_visible_to(slot, txid),
            true,
            true,
        )
    } else if (type_oid::FIRST_COMPOSITE
        ..type_oid::FIRST_COMPOSITE + crate::storage::MAX_COMPOSITES as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_COMPOSITE) as usize;
        let definition = storage.composite_for(slot, txid);
        (
            definition.schema,
            definition.name,
            storage.composite_slot_visible_to(slot, txid),
            false,
            false,
        )
    } else if (type_oid::FIRST_COMPOSITE_ARRAY
        ..type_oid::FIRST_COMPOSITE_ARRAY + crate::storage::MAX_COMPOSITES as i32)
        .contains(&oid)
    {
        let slot = (oid - type_oid::FIRST_COMPOSITE_ARRAY) as usize;
        let definition = storage.composite_for(slot, txid);
        (
            definition.schema,
            definition.name,
            storage.composite_slot_visible_to(slot, txid),
            true,
            false,
        )
    } else {
        return Ok(None);
    };
    if !visible {
        return Ok(None);
    }
    let unqualified_visible = if enumeration {
        storage.resolve_enum_slot(name.as_str(), txid)
            == Some(if array {
                (oid - type_oid::FIRST_ENUM_ARRAY) as usize
            } else {
                (oid - type_oid::FIRST_ENUM) as usize
            })
    } else if (type_oid::FIRST_COMPOSITE
        ..type_oid::FIRST_COMPOSITE_ARRAY + crate::storage::MAX_COMPOSITES as i32)
        .contains(&oid)
    {
        storage.resolve_composite_slot(name.as_str(), txid)
            == Some(if array {
                (oid - type_oid::FIRST_COMPOSITE_ARRAY) as usize
            } else {
                (oid - type_oid::FIRST_COMPOSITE) as usize
            })
    } else {
        storage.resolve_domain_slot(name.as_str(), txid)
            == Some(if array {
                (oid - type_oid::FIRST_DOMAIN_ARRAY) as usize
            } else {
                (oid - type_oid::FIRST_DOMAIN) as usize
            })
    };
    let mut rendered = StackStr::<140>::new();
    use core::fmt::Write as _;
    if !unqualified_visible {
        let _ = write!(rendered, "{}.", schema.as_str());
    }
    let _ = rendered.write_str(name.as_str());
    if array {
        let _ = rendered.write_str("[]");
    }
    Ok(Some(alloc_rendered(
        &rendered,
        "formatted type name is too long",
        arena,
    )?))
}

/// The comment text `txid` sees on the object with this OID (and column
/// `subid`), for `obj_description`/`col_description`. `catalog_name` selects
/// the object's catalog, defaulting to `pg_class` for deprecated one-argument
/// `obj_description`.
pub fn comment_text_for<'a>(
    storage: &Storage,
    txid: u32,
    catalog_name: &str,
    oid: u32,
    subid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let signed_oid = i32::try_from(oid).ok();
    for (class, schema, name, csub, text) in storage.comments_visible(txid) {
        let hit = match catalog_name {
            "pg_namespace" => {
                class == crate::storage::CommentClass::Schema
                    && subid == 0
                    && Some(namespace_oid(storage, name)) == signed_oid
            }
            "pg_type" => {
                class == crate::storage::CommentClass::Type
                    && subid == 0
                    && type_oid_of(storage, schema, name, txid) == signed_oid
            }
            "pg_tablespace" => {
                class == crate::storage::CommentClass::Tablespace
                    && subid == 0
                    && storage.tablespaces_visible_to(txid).any(|(_, tablespace)| {
                        tablespace.name_for(txid).as_str() == name
                            && Some(tablespace_oid(*tablespace)) == signed_oid
                    })
            }
            "pg_database" => {
                class == crate::storage::CommentClass::Database
                    && subid == 0
                    && storage.database_slot(name, txid).is_some_and(|slot| {
                        u32::try_from(storage.database(slot).oid.get()).ok() == Some(oid)
                    })
            }
            "pg_authid" => {
                class == crate::storage::CommentClass::Role
                    && subid == 0
                    && signed_oid.is_some_and(|role_oid| {
                        i32::try_from(csub) == Ok(role_oid)
                            && storage.role_slot_by_oid(role_oid, txid).is_some()
                    })
            }
            "pg_foreign_server" => {
                class == crate::storage::CommentClass::ForeignServer
                    && subid == 0
                    && signed_oid.is_some_and(|server_oid| {
                        usize::try_from(csub).ok().is_some_and(|slot| {
                            foreign_server_oid(slot) == server_oid
                                && storage.foreign_server_by_slot(slot, txid).is_some()
                        })
                    })
            }
            "pg_foreign_data_wrapper" => {
                class == crate::storage::CommentClass::ForeignDataWrapper
                    && subid == 0
                    && signed_oid.is_some_and(|wrapper_oid| {
                        usize::try_from(csub).ok().is_some_and(|slot| {
                            foreign_data_wrapper_oid(slot) == wrapper_oid
                                && storage.foreign_wrapper_by_slot(slot, txid).is_some()
                        })
                    })
            }
            "pg_am" => {
                class == crate::storage::CommentClass::AccessMethod
                    && subid == 0
                    && signed_oid.is_some_and(|access_method_oid| {
                        csub == access_method_oid as u32
                            && access_method_name_in(storage, txid, access_method_oid) == Some(name)
                    })
            }
            "pg_language" => {
                class == crate::storage::CommentClass::ProceduralLanguage
                    && subid == 0
                    && signed_oid.is_some_and(|language_oid| {
                        csub == language_oid as u32
                            && procedural_language_name(language_oid) == Some(name)
                    })
            }
            "pg_cast" => {
                class == crate::storage::CommentClass::Cast
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        storage
                            .casts_visible_to(txid)
                            .any(|(_, cast)| cast.oid() == oid && csub == oid as u32)
                    })
            }
            "pg_operator" => {
                class == crate::storage::CommentClass::Operator
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        storage.operator_slot_by_oid(oid, txid).is_some() && csub == oid as u32
                    })
            }
            "pg_opfamily" => {
                class == crate::storage::CommentClass::OperatorFamily
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        storage.operator_family_slot_by_oid(oid, txid).is_some()
                            && csub == oid as u32
                    })
            }
            "pg_opclass" => {
                class == crate::storage::CommentClass::OperatorClass
                    && subid == 0
                    && signed_oid
                        .and_then(crate::storage::OperatorClassOid::parse)
                        .is_some_and(|oid| {
                            storage.operator_class_slot_by_oid(oid, txid).is_some()
                                && csub == oid.get() as u32
                        })
            }
            "pg_constraint" => {
                class == crate::storage::CommentClass::Constraint
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        usize::try_from(csub).ok().is_some_and(|table| {
                            storage.table_slot_visible_to(table, txid)
                                && table_constraint_oid(storage, txid, table, name) == Some(oid)
                        })
                    })
            }
            "pg_trigger" => {
                class == crate::storage::CommentClass::Trigger
                    && subid == 0
                    && storage
                        .triggers_with_slots_visible_to(txid)
                        .map(|(_, trigger)| trigger)
                        .any(|trigger| {
                            trigger.name_to(txid).as_str() == name
                                && trigger.target.comment_subid() == csub
                                && Some(crate::storage::trigger_oid(&trigger)) == signed_oid
                        })
            }
            "pg_collation" => {
                class == crate::storage::CommentClass::Collation
                    && subid == 0
                    && storage
                        .collation_slot(schema, name, txid)
                        .is_some_and(|slot| Some(storage.collation(slot).oid(slot)) == signed_oid)
            }
            "pg_conversion" => {
                class == crate::storage::CommentClass::Conversion
                    && subid == 0
                    && storage
                        .conversion_slot(schema, name, txid)
                        .is_some_and(|slot| Some(storage.conversion(slot).oid(slot)) == signed_oid)
            }
            "pg_event_trigger" => {
                class == crate::storage::CommentClass::EventTrigger
                    && subid == 0
                    && storage
                        .event_trigger_slot(name, txid)
                        .is_some_and(|slot| Some(storage.event_trigger(slot).oid()) == signed_oid)
            }
            "pg_largeobject" => {
                class == crate::storage::CommentClass::LargeObject
                    && subid == 0
                    && name.parse::<u32>() == Ok(oid)
            }
            "pg_ts_parser" => {
                class == crate::storage::CommentClass::TextSearchParser
                    && subid == 0
                    && storage
                        .text_search_slot(
                            crate::sql::ast::TextSearchObjectKind::Parser,
                            schema,
                            name,
                            txid,
                        )
                        .is_some_and(|slot| {
                            Some(storage.text_search_object(slot).definition_for(txid).oid())
                                == signed_oid
                        })
            }
            "pg_ts_template" => {
                class == crate::storage::CommentClass::TextSearchTemplate
                    && subid == 0
                    && storage
                        .text_search_slot(
                            crate::sql::ast::TextSearchObjectKind::Template,
                            schema,
                            name,
                            txid,
                        )
                        .is_some_and(|slot| {
                            Some(storage.text_search_object(slot).definition_for(txid).oid())
                                == signed_oid
                        })
            }
            "pg_ts_dict" => {
                class == crate::storage::CommentClass::TextSearchDictionary
                    && subid == 0
                    && storage
                        .text_search_slot(
                            crate::sql::ast::TextSearchObjectKind::Dictionary,
                            schema,
                            name,
                            txid,
                        )
                        .is_some_and(|slot| {
                            Some(storage.text_search_object(slot).definition_for(txid).oid())
                                == signed_oid
                        })
            }
            "pg_ts_config" => {
                class == crate::storage::CommentClass::TextSearchConfiguration
                    && subid == 0
                    && storage
                        .text_search_slot(
                            crate::sql::ast::TextSearchObjectKind::Configuration,
                            schema,
                            name,
                            txid,
                        )
                        .is_some_and(|slot| {
                            Some(storage.text_search_object(slot).definition_for(txid).oid())
                                == signed_oid
                        })
            }
            "pg_rewrite" => {
                class == crate::storage::CommentClass::Rule
                    && subid == 0
                    && storage.rules_visible_to(txid).any(|(_, rule)| {
                        let definition = rule.definition_for(txid);
                        definition.name.as_str() == name
                            && definition.target.comment_subid() == csub
                            && Some(rule.oid()) == signed_oid
                    })
            }
            "pg_proc" => {
                class == crate::storage::CommentClass::Routine
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        i32::try_from(csub) == Ok(oid)
                            && storage.routine_slot_by_oid(oid, txid).is_some()
                    })
            }
            "pg_policy" => {
                class == crate::storage::CommentClass::Policy
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        i32::try_from(csub) == Ok(oid)
                            && storage
                                .policies_with_slots_visible_to(txid)
                                .any(|(_, policy)| crate::storage::policy_oid(policy) == oid)
                    })
            }
            "pg_statistic_ext" => {
                class == crate::storage::CommentClass::Statistics
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        storage.extended_statistics_visible(txid).any(|(slot, _)| {
                            extended_statistics_oid(slot) == oid && csub == slot as u32
                        })
                    })
            }
            "pg_publication" => {
                class == crate::storage::CommentClass::Publication
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        storage
                            .publications_with_slots_visible_to(txid)
                            .any(|(slot, _)| publication_oid(slot) == oid && csub == slot as u32)
                    })
            }
            "pg_subscription" => {
                class == crate::storage::CommentClass::Subscription
                    && subid == 0
                    && signed_oid.is_some_and(|oid| {
                        storage.subscriptions_with_slots_visible_to(txid).any(
                            |(_, subscription)| {
                                subscription_oid(subscription) == oid
                                    && csub == subscription.created_at as u32
                            },
                        )
                    })
            }
            _ => {
                class == crate::storage::CommentClass::Relation
                    && csub as i32 == subid
                    && relation_oid_of(storage, txid, schema, name) == signed_oid
            }
        };
        if hit {
            let bytes = arena
                .alloc_slice_copy(text.as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
    }
    Ok(None)
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

pub(crate) const FIRST_FK_OID: i32 = 200_000;
pub(crate) const FIRST_CHECK_OID: i32 = 300_000;
pub(crate) const FIRST_DOMAIN_CHECK_OID: i32 = 400_000;
pub(crate) const FIRST_NOT_NULL_OID: i32 = 450_000;
pub(crate) const FIRST_DETACHED_PARTITION_CHECK_OID: i32 = 475_000;

/// The current catalog OID of a named table constraint. Constraint comments
/// retain the table-slot/name identity because index and check positions are
/// presentation-derived and can change after an unrelated DROP CONSTRAINT.
pub(crate) fn table_constraint_oid(
    storage: &Storage,
    txid: u32,
    table_slot: usize,
    name: &str,
) -> Option<i32> {
    if !storage.table_slot_visible_to(table_slot, txid) {
        return None;
    }
    let mut index_constraint = None;
    visit_indexes(storage, txid, |index| {
        if index.is_constraint && index.table_slot == table_slot && index.name.as_str() == name {
            index_constraint = Some(index.oid + 500_000);
        }
    });
    if index_constraint.is_some() {
        return index_constraint;
    }
    let table = storage.table_def(table_slot, txid);
    if table
        .partition
        .detached_bound
        .is_some_and(|constraint| constraint.name.as_str() == name)
    {
        return Some(FIRST_DETACHED_PARTITION_CHECK_OID + table_slot as i32);
    }
    if let Some((index, _)) = table
        .checks()
        .iter()
        .enumerate()
        .find(|(_, constraint)| constraint.name.as_str() == name)
    {
        return Some(
            FIRST_CHECK_OID + table_slot as i32 * crate::storage::MAX_CHECKS as i32 + index as i32,
        );
    }
    if let Some((index, _)) = table
        .fkeys()
        .iter()
        .enumerate()
        .find(|(_, constraint)| constraint.name.as_str() == name)
    {
        return Some(FIRST_FK_OID + table_slot as i32 * MAX_INDEXES_PER_TABLE + index as i32);
    }
    table
        .columns()
        .iter()
        .enumerate()
        .find(|(_, column)| {
            column.not_null.is_required()
                && not_null_constraint_name(table, column).as_str() == name
        })
        .map(|(index, _)| {
            FIRST_NOT_NULL_OID + table_slot as i32 * MAX_COLUMNS as i32 + index as i32
        })
}

/// Enumerates every foreign-key constraint, resolving each child/parent table to
/// its OID. A child whose parent no longer exists is skipped (it cannot be
/// rendered), matching that a dropped parent leaves no referential row.
fn visit_fkeys(storage: &Storage, txid: u32, mut visit: impl FnMut(FkInfo)) {
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let def = storage.table_def(slot, txid);
        let conrelid = table_oid(storage, slot);
        for (i, fk) in def.fkeys().iter().enumerate() {
            let Some(pslot) =
                storage.find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
            else {
                continue;
            };
            visit(FkInfo {
                oid: FIRST_FK_OID + slot as i32 * MAX_INDEXES_PER_TABLE + i as i32,
                conrelid,
                confrelid: table_oid(storage, pslot),
                child_slot: slot,
                fk_index: i,
                name: stack_str_64(fk.name.as_str()),
            });
        }
    }
}

fn empty_foreign_key() -> FkInfo {
    FkInfo {
        oid: 0,
        conrelid: 0,
        confrelid: 0,
        child_slot: 0,
        fk_index: 0,
        name: StackStr::new(),
    }
}

fn collect_fkeys<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [FkInfo], SqlError> {
    let mut count = 0usize;
    visit_fkeys(storage, txid, |_| count += 1);
    let foreign_keys = arena
        .alloc_slice_with(count, |_| empty_foreign_key())
        .map_err(|_| arena_full())?;
    let mut next = 0usize;
    visit_fkeys(storage, txid, |foreign_key| {
        foreign_keys[next] = foreign_key;
        next += 1;
    });
    Ok(foreign_keys)
}

fn inherited_foreign_key_parent_oid(
    storage: &Storage,
    txid: u32,
    child_slot: usize,
    child: &crate::storage::ForeignKey,
) -> i32 {
    let Some(attachment) = storage.table_def(child_slot, txid).partition.attachment else {
        return 0;
    };
    let parent_slot = usize::from(attachment.parent);
    storage
        .table_def(parent_slot, txid)
        .fkeys()
        .iter()
        .position(|parent| {
            parent.name == child.name
                && parent.columns() == child.columns()
                && parent.parent_schema == child.parent_schema
                && parent.parent == child.parent
                && parent.parent_cols() == child.parent_cols()
                && parent.on_delete == child.on_delete
                && parent.on_update == child.on_update
        })
        .map_or(0, |index| {
            FIRST_FK_OID + parent_slot as i32 * MAX_INDEXES_PER_TABLE + index as i32
        })
}

fn check_inheritance_count(
    storage: &Storage,
    txid: u32,
    child_slot: usize,
    child: &crate::storage::CheckConstraint,
) -> usize {
    let definition = storage.table_def(child_slot, txid);
    let partition_count = usize::from(definition.partition.attachment.is_some_and(|attachment| {
        storage
            .table_def(usize::from(attachment.parent), txid)
            .checks()
            .iter()
            .any(|parent| parent.name == child.name && parent.expression == child.expression)
    }));
    partition_count
        + definition
            .inheritance
            .parents_ref()
            .iter()
            .filter(|parent| {
                storage
                    .table_def(usize::from(**parent), txid)
                    .checks()
                    .iter()
                    .any(|parent| {
                        parent.name == child.name && parent.expression == child.expression
                    })
            })
            .count()
}

fn append_constraint_attributes(
    rendered: &mut impl core::fmt::Write,
    timing: crate::storage::ConstraintTiming,
    validation: crate::storage::ConstraintValidation,
) {
    match timing {
        crate::storage::ConstraintTiming::NotDeferrable => {}
        crate::storage::ConstraintTiming::DeferrableImmediate => {
            let _ = rendered.write_str(" DEFERRABLE");
        }
        crate::storage::ConstraintTiming::DeferrableDeferred => {
            let _ = rendered.write_str(" DEFERRABLE INITIALLY DEFERRED");
        }
    }
    match validation {
        crate::storage::ConstraintValidation::EnforcedValidated => {}
        crate::storage::ConstraintValidation::EnforcedNotValid => {
            let _ = rendered.write_str(" NOT VALID");
        }
        crate::storage::ConstraintValidation::NotEnforced => {
            let _ = rendered.write_str(" NOT ENFORCED");
        }
    }
}

/// The schema-qualified foreign-key definition used by catalog clients and
/// dump/restore.
pub fn constraint_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let fks = collect_fkeys(storage, txid, arena)?;
    for info in fks {
        if info.oid != oid {
            continue;
        }
        let child = storage.table_def(info.child_slot, txid);
        let fk = &child.fkeys()[info.fk_index];
        let Some(pslot) = storage.find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
        else {
            return Ok(None);
        };
        let parent = storage.table_def(pslot, txid);
        let mut s = StackStr::<1280>::new();
        use core::fmt::Write as _;
        let _ = s.write_str("FOREIGN KEY (");
        for (k, &c) in fk.columns[..fk.n_cols].iter().enumerate() {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            write_identifier(&mut s, child.columns()[c as usize].name.as_str());
        }
        let _ = s.write_str(") REFERENCES ");
        write_identifier(&mut s, parent.schema.as_str());
        let _ = s.write_char('.');
        write_identifier(&mut s, parent.name.as_str());
        let _ = s.write_char('(');
        for (k, &c) in fk.parent_cols[..fk.n_parent_cols].iter().enumerate() {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            write_identifier(&mut s, parent.columns()[c as usize].name.as_str());
        }
        let _ = s.write_str(")");
        let _ = s.write_str(fk_action_suffix(fk.on_delete, "DELETE"));
        let _ = s.write_str(fk_action_suffix(fk.on_update, "UPDATE"));
        append_constraint_attributes(&mut s, fk.timing, fk.validation);
        return Ok(Some(alloc_rendered(
            &s,
            "foreign key definition is too long",
            arena,
        )?));
    }
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        for (check_index, check) in storage.table_def(slot, txid).checks().iter().enumerate() {
            let check_oid = FIRST_CHECK_OID
                + slot as i32 * crate::storage::MAX_CHECKS as i32
                + check_index as i32;
            if check_oid != oid {
                continue;
            }
            let mut rendered = StackStr::<1024>::new();
            use core::fmt::Write as _;
            let _ = write!(rendered, "CHECK (({}))", check.expression.as_str());
            append_constraint_attributes(
                &mut rendered,
                crate::storage::ConstraintTiming::NotDeferrable,
                check.validation,
            );
            return Ok(Some(alloc_rendered(
                &rendered,
                "table constraint definition is too long",
                arena,
            )?));
        }
        let table = storage.table_def(slot, txid);
        if let Some(constraint) = table.partition.detached_bound
            && oid == FIRST_DETACHED_PARTITION_CHECK_OID + slot as i32
        {
            return Ok(Some(detached_partition_constraint_def_text(
                table, constraint, arena,
            )?));
        }
    }
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        for (check_index, check) in domain.checks().iter().enumerate() {
            let check_oid = FIRST_DOMAIN_CHECK_OID
                + slot as i32 * crate::storage::MAX_DOMAIN_CHECKS as i32
                + check_index as i32;
            if check_oid != oid {
                continue;
            }
            let mut rendered = StackStr::<1024>::new();
            use core::fmt::Write as _;
            let _ = write!(rendered, "CHECK (({}))", check.expression.as_str());
            append_constraint_attributes(
                &mut rendered,
                crate::storage::ConstraintTiming::NotDeferrable,
                check.validation,
            );
            return Ok(Some(alloc_rendered(
                &rendered,
                "domain constraint definition is too long",
                arena,
            )?));
        }
    }
    let indexes = collect_indexes(storage, txid, arena)?;
    for info in indexes {
        if oid != info.oid + 500_000 || !info.is_constraint {
            continue;
        }
        let table = storage.table_def(info.table_slot, txid);
        let mut rendered = StackStr::<640>::new();
        use core::fmt::Write as _;
        if info.is_exclusion {
            let exclusion = table
                .exclusions()
                .iter()
                .find(|exclusion| exclusion.name.as_str() == info.name.as_str())
                .expect("exclusion index has its constraint");
            let _ = rendered.write_str("EXCLUDE USING gist (");
            for position in 0..exclusion.n_cols {
                if position != 0 {
                    let _ = rendered.write_str(", ");
                }
                write_identifier(
                    &mut rendered,
                    table.columns()[exclusion.columns[position] as usize]
                        .name
                        .as_str(),
                );
                let _ = write!(rendered, " WITH {}", exclusion.operators[position].sql());
            }
            let _ = rendered.write_str(")");
            if let Some(predicate) = &exclusion.predicate {
                let _ = write!(rendered, " WHERE ({})", predicate.as_str());
            }
            append_constraint_attributes(
                &mut rendered,
                exclusion.timing,
                crate::storage::ConstraintValidation::EnforcedValidated,
            );
            return Ok(Some(alloc_rendered(
                &rendered,
                "exclusion constraint definition is too long",
                arena,
            )?));
        }
        let _ = rendered.write_str(if info.is_primary {
            "PRIMARY KEY ("
        } else {
            "UNIQUE ("
        });
        for (index, &column) in info.columns[..info.n_cols].iter().enumerate() {
            if index > 0 {
                let _ = rendered.write_str(", ");
            }
            let _ = rendered.write_str(table.columns()[column as usize].name.as_str());
        }
        let _ = rendered.write_str(")");
        append_constraint_attributes(
            &mut rendered,
            info.timing,
            crate::storage::ConstraintValidation::EnforcedValidated,
        );
        return Ok(Some(alloc_rendered(
            &rendered,
            "unique constraint definition is too long",
            arena,
        )?));
    }
    Ok(None)
}

/// PostgreSQL's executable partition-key clause for pg_dump and catalog
/// clients. The OID boundary resolves once to a typed table slot; the durable
/// scheme already owns column offsets and therefore cannot render a stale name.
pub fn partition_key_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let Some(slot) = oid
        .checked_sub(FIRST_USER_OID)
        .and_then(|slot| usize::try_from(slot).ok())
        .filter(|slot| *slot < storage.table_count())
    else {
        return Ok(None);
    };
    if !storage.table_slot_visible_to(slot, txid) {
        return Ok(None);
    }
    let definition = storage.table_def(slot, txid);
    let Some(scheme) = definition.partition.scheme else {
        return Ok(None);
    };
    let mut rendered = StackStr::<512>::new();
    use core::fmt::Write as _;
    let strategy = match scheme.strategy {
        PartitionStrategy::Range => "RANGE",
        PartitionStrategy::List => "LIST",
        PartitionStrategy::Hash => "HASH",
    };
    let _ = write!(rendered, "{} (", strategy);
    for (position, column) in scheme.keys[..usize::from(scheme.n_keys)].iter().enumerate() {
        if position != 0 {
            let _ = rendered.write_str(", ");
        }
        write_identifier(
            &mut rendered,
            definition.columns()[usize::from(*column)].name.as_str(),
        );
    }
    let _ = rendered.write_char(')');
    Ok(Some(alloc_rendered(
        &rendered,
        "partition key definition is too long",
        arena,
    )?))
}

fn partition_bound_def_text(bound: PartitionBound, arena: &Arena) -> Result<&str, SqlError> {
    use core::fmt::Write as _;

    let mut rendered = StackStr::<4096>::new();
    match bound {
        PartitionBound::Default => {
            let _ = rendered.write_str("DEFAULT");
        }
        PartitionBound::Range {
            lower,
            upper,
            n_keys,
        } => {
            let _ = rendered.write_str("FOR VALUES FROM (");
            for (index, value) in lower.iter().copied().take(usize::from(n_keys)).enumerate() {
                if index != 0 {
                    let _ = rendered.write_str(", ");
                }
                write_partition_bound_value(&mut rendered, value, arena)?;
            }
            let _ = rendered.write_str(") TO (");
            for (index, value) in upper.iter().copied().take(usize::from(n_keys)).enumerate() {
                if index != 0 {
                    let _ = rendered.write_str(", ");
                }
                write_partition_bound_value(&mut rendered, value, arena)?;
            }
            let _ = rendered.write_char(')');
        }
        PartitionBound::List { values, n_values } => {
            let _ = rendered.write_str("FOR VALUES IN (");
            for (index, value) in values
                .iter()
                .copied()
                .take(usize::from(n_values))
                .enumerate()
            {
                if index != 0 {
                    let _ = rendered.write_str(", ");
                }
                write_partition_value(&mut rendered, value, arena)?;
            }
            let _ = rendered.write_char(')');
        }
        PartitionBound::Hash { modulus, remainder } => {
            let _ = write!(
                rendered,
                "FOR VALUES WITH (modulus {modulus}, remainder {remainder})"
            );
        }
    }
    alloc_rendered(&rendered, "partition bound definition is too long", arena)
}

fn detached_partition_constraint_def_text<'a>(
    definition: &TableDef,
    constraint: crate::storage::DetachedPartitionBound,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    use core::fmt::Write as _;

    let mut rendered = StackStr::<4096>::new();
    let _ = rendered.write_str("CHECK (");
    match (constraint.scheme.strategy, constraint.bound) {
        (PartitionStrategy::List, PartitionBound::List { values, n_values }) => {
            if constraint.scheme.n_keys != 1 {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "generated list partition constraint has multiple keys"
                ));
            }
            let values = &values[..usize::from(n_values)];
            if values.is_empty() {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "generated list partition constraint has no values"
                ));
            }
            let has_null = values.contains(&OwnedDatum::Null);
            let non_null_count = values
                .iter()
                .filter(|value| **value != OwnedDatum::Null)
                .count();
            write_partition_constraint_key(&mut rendered, definition, constraint.scheme, 0);
            if has_null {
                let _ = rendered.write_str(" IS NULL");
                if non_null_count != 0 {
                    let _ = rendered.write_str(" OR ");
                    write_partition_constraint_key(&mut rendered, definition, constraint.scheme, 0);
                    let _ = rendered.write_str(" IN (");
                }
            } else {
                let _ = rendered.write_str(" IS NOT NULL AND ");
                write_partition_constraint_key(&mut rendered, definition, constraint.scheme, 0);
                let _ = rendered.write_str(" IN (");
            }
            if non_null_count != 0 {
                for (index, value) in values
                    .iter()
                    .copied()
                    .filter(|value| *value != OwnedDatum::Null)
                    .enumerate()
                {
                    if index != 0 {
                        let _ = rendered.write_str(", ");
                    }
                    write_partition_value(&mut rendered, value, arena)?;
                }
                let _ = rendered.write_char(')');
            }
        }
        (
            PartitionStrategy::Range,
            PartitionBound::Range {
                lower,
                upper,
                n_keys,
            },
        ) => {
            if n_keys != constraint.scheme.n_keys {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "generated range partition constraint has inconsistent key count"
                ));
            }
            for index in 0..usize::from(n_keys) {
                if index != 0 {
                    let _ = rendered.write_str(" AND ");
                }
                write_partition_constraint_key(&mut rendered, definition, constraint.scheme, index);
                let _ = rendered.write_str(" IS NOT NULL");
            }
            let _ = rendered.write_str(" AND ");
            write_range_partition_comparison(
                &mut rendered,
                definition,
                constraint.scheme,
                &lower[..usize::from(n_keys)],
                true,
                arena,
            )?;
            let _ = rendered.write_str(" AND ");
            write_range_partition_comparison(
                &mut rendered,
                definition,
                constraint.scheme,
                &upper[..usize::from(n_keys)],
                false,
                arena,
            )?;
        }
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "unsupported generated partition constraint state"
            ));
        }
    }
    let _ = rendered.write_char(')');
    alloc_rendered(
        &rendered,
        "partition constraint definition is too long",
        arena,
    )
}

fn write_partition_constraint_key(
    out: &mut StackStr<4096>,
    definition: &TableDef,
    scheme: crate::storage::PartitionScheme,
    index: usize,
) {
    write_identifier(
        out,
        definition.columns()[usize::from(scheme.keys[index])]
            .name
            .as_str(),
    );
}

/// Emits the lexicographic half of a range partition check.  The explicit
/// recursion preserves SQL NULL semantics once the caller has emitted the
/// required non-NULL checks, including bounds containing MINVALUE/MAXVALUE.
fn write_range_partition_comparison(
    out: &mut StackStr<4096>,
    definition: &TableDef,
    scheme: crate::storage::PartitionScheme,
    bound: &[PartitionBoundValue],
    lower: bool,
    arena: &Arena,
) -> Result<(), SqlError> {
    use core::fmt::Write as _;

    let Some((head, tail)) = bound.split_first() else {
        let _ = out.write_str(if lower { "TRUE" } else { "FALSE" });
        return Ok(());
    };
    match head {
        PartitionBoundValue::MinValue => {
            let _ = out.write_str(if lower { "TRUE" } else { "FALSE" });
        }
        PartitionBoundValue::MaxValue => {
            let _ = out.write_str(if lower { "FALSE" } else { "TRUE" });
        }
        PartitionBoundValue::Value(value) => {
            let index = usize::from(scheme.n_keys) - bound.len();
            let _ = out.write_char('(');
            write_partition_constraint_key(out, definition, scheme, index);
            let _ = out.write_str(if lower { " > " } else { " < " });
            write_partition_value(out, *value, arena)?;
            let _ = out.write_str(" OR (");
            write_partition_constraint_key(out, definition, scheme, index);
            let _ = out.write_str(" = ");
            write_partition_value(out, *value, arena)?;
            let _ = out.write_str(" AND ");
            write_range_partition_comparison(out, definition, scheme, tail, lower, arena)?;
            let _ = out.write_str("))");
        }
    }
    Ok(())
}

fn write_partition_bound_value(
    out: &mut impl core::fmt::Write,
    value: PartitionBoundValue,
    arena: &Arena,
) -> Result<(), SqlError> {
    match value {
        PartitionBoundValue::MinValue => {
            let _ = out.write_str("MINVALUE");
        }
        PartitionBoundValue::MaxValue => {
            let _ = out.write_str("MAXVALUE");
        }
        PartitionBoundValue::Value(value) => write_partition_value(out, value, arena)?,
    }
    Ok(())
}

fn write_partition_value(
    out: &mut impl core::fmt::Write,
    value: OwnedDatum,
    arena: &Arena,
) -> Result<(), SqlError> {
    match value {
        OwnedDatum::Null => {
            let _ = out.write_str("NULL");
        }
        OwnedDatum::Bool(value) => {
            let _ = out.write_str(if value { "true" } else { "false" });
        }
        OwnedDatum::Int4(_)
        | OwnedDatum::Oid(_)
        | OwnedDatum::Int8(_)
        | OwnedDatum::Numeric { .. } => {
            let _ = out.write_str(datum_to_text(value.as_datum(), arena)?);
        }
        OwnedDatum::Float8(number) if number.is_finite() => {
            let _ = out.write_str(datum_to_text(value.as_datum(), arena)?);
        }
        _ => {
            let text = datum_to_text(value.as_datum(), arena)?;
            let _ = out.write_str(quote_literal_str(text, arena)?);
        }
    }
    Ok(())
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

/// The complete `CREATE INDEX` statement returned by `pg_get_indexdef`.
fn write_index_key_metadata(
    out: &mut impl core::fmt::Write,
    storage: &Storage,
    txid: u32,
    info: &IdxInfo,
    position: usize,
) {
    if info.explicit_collations[position] {
        let _ = out.write_str(" COLLATE ");
        write_identifier(out, info.collations[position].name());
    }
    if let Some(crate::storage::IndexOperatorClass::Catalog(oid)) = info.operator_classes[position]
    {
        let _ = out.write_char(' ');
        let slot = storage
            .operator_class_slot_by_oid(oid, txid)
            .expect("index operator class dependency is visible");
        let definition = storage.operator_class_for(slot, txid);
        if !storage.schema_is_on_path(definition.schema) {
            write_identifier(out, definition.schema.as_str());
            let _ = out.write_char('.');
        }
        write_identifier(out, definition.name.as_str());
    }
}

fn write_index_target(out: &mut impl core::fmt::Write, table: &TableDef, info: &IdxInfo) {
    let _ = out.write_str(" ON ");
    if info
        .explicit_definition
        .is_some_and(|definition| definition.kind.is_partitioned())
    {
        let _ = out.write_str("ONLY ");
    }
    write_identifier(out, table.schema.as_str());
    let _ = out.write_char('.');
    write_identifier(out, table.name.as_str());
}

fn write_index_storage_options(
    out: &mut impl core::fmt::Write,
    definition: Option<crate::storage::IndexMutableDefinition>,
) {
    let Some(definition) = definition else {
        return;
    };
    if definition.options.fillfactor.is_none() && definition.options.deduplicate_items.is_none() {
        return;
    }
    let _ = out.write_str(" WITH (");
    let mut separator = "";
    if let Some(fillfactor) = definition.options.fillfactor {
        let _ = write!(out, "fillfactor='{fillfactor}'");
        separator = ", ";
    }
    if let Some(deduplicate) = definition.options.deduplicate_items {
        let _ = write!(
            out,
            "{separator}deduplicate_items={}",
            if deduplicate { "on" } else { "off" }
        );
    }
    let _ = out.write_str(")");
}

pub fn index_def_text<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    col: usize,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let indices = collect_indexes(storage, txid, arena)?;
    for info in indices {
        if info.oid != oid {
            continue;
        }
        let def = storage.table_def(info.table_slot, txid);
        let col_name = |ci: usize| def.columns()[info.columns[ci] as usize].name.as_str();
        // `col > 0`: just the name of that 1-based indexed column.
        if col > 0 {
            let name = if col <= info.n_cols {
                col_name(col - 1)
            } else {
                return Ok(None);
            };
            let bytes = arena
                .alloc_slice_copy(name.as_bytes())
                .map_err(|_| arena_full())?;
            return Ok(Some(unsafe { core::str::from_utf8_unchecked(bytes) }));
        }
        let mut s = StackStr::<896>::new();
        use core::fmt::Write as _;
        let _ = write!(
            s,
            "CREATE {}INDEX ",
            if info.is_unique { "UNIQUE " } else { "" }
        );
        write_identifier(&mut s, info.name.as_str());
        write_index_target(&mut s, def, info);
        let _ = s.write_str(if info.is_exclusion {
            " USING gist ("
        } else {
            " USING btree ("
        });
        for k in 0..info.n_cols {
            if k > 0 {
                let _ = s.write_str(", ");
            }
            if let Some(expression) = index_expression_source(storage, info, k, txid) {
                let _ = s.write_str(expression.as_str());
            } else {
                write_identifier(&mut s, col_name(k));
            }
            write_index_key_metadata(&mut s, storage, txid, info, k);
            if info.descending[k] {
                let _ = s.write_str(" DESC");
            }
            if info.nulls_first[k] != info.descending[k] {
                let _ = s.write_str(if info.nulls_first[k] {
                    " NULLS FIRST"
                } else {
                    " NULLS LAST"
                });
            }
        }
        let _ = s.write_str(")");
        if info.n_include_cols != 0 {
            let _ = s.write_str(" INCLUDE (");
            for k in 0..info.n_include_cols {
                if k > 0 {
                    let _ = s.write_str(", ");
                }
                write_identifier(
                    &mut s,
                    def.columns()[info.include_columns[k] as usize]
                        .name
                        .as_str(),
                );
            }
            let _ = s.write_str(")");
        }
        if info.nulls_not_distinct {
            let _ = s.write_str(" NULLS NOT DISTINCT");
        }
        write_index_storage_options(&mut s, info.explicit_definition);
        if let Some(predicate) = info.predicate {
            let _ = write!(s, " WHERE {}", predicate.as_str());
        }
        return Ok(Some(alloc_rendered(
            &s,
            "index definition is too long",
            arena,
        )?));
    }
    Ok(None)
}

#[derive(Clone, Copy)]
struct SynthDef<'a> {
    name: &'a str,
    columns: &'a [(&'a str, ColType)],
}

fn def_of<'a>(name: &'a str, columns: &'a [(&'a str, ColType)]) -> SynthDef<'a> {
    SynthDef { name, columns }
}

fn materialize_def(specification: SynthDef<'_>) -> TableDef {
    let mut definition = TableDef {
        name: SqlName::parse(specification.name).expect("catalog name fits"),
        columns: [ColumnMeta {
            name: SqlName::parse("").unwrap(),
            ctype: ColType::Bool,
            type_mod: -1,
            collation: crate::sql::ast::Collation::None,
            not_null: crate::storage::NotNullOrigin::Nullable,
            unique: false,
            primary: false,
            auto_increment: false,
            default: crate::storage::ColumnDefault::NONE,
            is_identity: false,
            identity_always: false,
            auto_increment_step: 1,
            user_type: None,
            statistics_target: -1,
        }; MAX_COLUMNS],
        n_columns: specification.columns.len(),
        ..TableDef::empty()
    };
    for (index, (name, column_type)) in specification.columns.iter().enumerate() {
        definition.columns[index].name = SqlName::parse(name).expect("catalog column fits");
        definition.columns[index].ctype = *column_type;
        definition.columns[index].collation = if column_type.is_collatable() {
            crate::sql::ast::Collation::Default
        } else {
            crate::sql::ast::Collation::None
        };
    }
    definition
}

/// Allocates `rows` (each a slice already in the arena) as a row slice.
fn finish<'a>(
    specification: SynthDef<'_>,
    rows: &[&'a [Datum<'a>]],
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = arena
        .alloc(materialize_def(specification))
        .map_err(|_| arena_full())?;
    let rows = arena.alloc_slice_copy(rows).map_err(|_| arena_full())?;
    Ok(SynthTable { def, rows: &*rows })
}

fn row<'a>(vals: &[Datum<'a>], arena: &'a Arena) -> Result<&'a [Datum<'a>], SqlError> {
    arena
        .alloc_slice_copy(vals)
        .map(|r| &*r)
        .map_err(|_| arena_full())
}

fn catalog_capacity_exceeded(relation: &str) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "{} exceeds static catalog capacity",
        relation
    )
}

fn text<'a>(s: &str, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    arena
        .alloc_str(s)
        .map(Datum::Text)
        .map_err(|_| arena_full())
}

pub(crate) fn describe_view<'a>(
    storage: &'a Storage,
    txid: u32,
    view: &'a crate::storage::ViewDef,
    arena: &'a Arena,
    out: &mut [super::types::ColDesc<'a>],
) -> Result<usize, SqlError> {
    let user = crate::sql::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(storage.view_creation_path_for(view), user.as_str(), txid);
    let slot = storage
        .views_visible_to(txid)
        .find_map(|(slot, candidate)| {
            (candidate.schema == view.schema && candidate.name == view.name).then_some(slot)
        })
        .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_TABLE, "view does not exist"))?;
    let count = super::query::describe_stored_query(
        storage.view_sql_for(view),
        storage,
        txid,
        path,
        storage.view_dependencies(slot),
        arena,
        out,
    )?;
    overlay_view_column_names(view, txid, out, count)
}

/// A view's body supplies types; its stored output relation supplies names.
/// Keeping that overlay here makes SQL descriptions, catalogs, and wire
/// metadata consume exactly the same identity.
pub(crate) fn overlay_view_column_names<'a>(
    view: &'a crate::storage::ViewDef,
    txid: u32,
    out: &mut [super::types::ColDesc<'a>],
    count: usize,
) -> Result<usize, SqlError> {
    let columns = view.columns_for(txid);
    if columns.len() != count {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "view \"{}\" has an invalid output-column definition",
            view.name.as_str()
        ));
    }
    for (column, name) in out[..count].iter_mut().zip(columns.names()) {
        column.name = name.as_str();
    }
    Ok(count)
}

fn describe_stored_view<'a>(
    storage: &'a Storage,
    txid: u32,
    slot: usize,
    arena: &'a Arena,
    out: &mut [super::types::ColDesc<'a>],
) -> Result<usize, SqlError> {
    let user = crate::sql::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(storage.view_creation_path(slot), user.as_str(), txid);
    let count = super::query::describe_stored_query(
        storage.view_sql(slot),
        storage,
        txid,
        path,
        storage.view_dependencies(slot),
        arena,
        out,
    )?;
    overlay_view_column_names(storage.view(slot), txid, out, count)
}

fn pg_stats<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_stats",
        &[
            ("schemaname", ColType::Text),
            ("tablename", ColType::Text),
            ("attname", ColType::Text),
            ("inherited", ColType::Bool),
            ("null_frac", ColType::Float4),
            ("avg_width", ColType::Int4),
            ("n_distinct", ColType::Float4),
            ("most_common_vals", ColType::Text),
            (
                "most_common_freqs",
                ColType::Array(super::types::ArrElem::Float4),
            ),
            ("histogram_bounds", ColType::Text),
            ("correlation", ColType::Float4),
            ("most_common_elems", ColType::Text),
            (
                "most_common_elem_freqs",
                ColType::Array(super::types::ArrElem::Float4),
            ),
            (
                "elem_count_histogram",
                ColType::Array(super::types::ArrElem::Float4),
            ),
            ("range_length_histogram", ColType::Text),
            ("range_empty_frac", ColType::Float4),
            ("range_bounds_histogram", ColType::Text),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut count = 0usize;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let statistics = storage.table_statistics(slot, txid);
        if !statistics.valid {
            continue;
        }
        let table_definition = storage.table_def(slot, txid);
        for (column, metadata) in table_definition.columns().iter().enumerate() {
            let column_statistics = statistics.columns[column];
            if !column_statistics.valid {
                continue;
            }
            if count == rows.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_stats exceeds {} rows",
                    rows.len()
                ));
            }
            let distinct = if column_statistics.distinct_fraction_ppm != 0 {
                -(column_statistics.distinct_fraction_ppm as f32 / 1_000_000.0)
            } else {
                column_statistics.distinct_values as f32
            };
            rows[count] = row(
                &[
                    text(table_definition.schema.as_str(), arena)?,
                    text(table_definition.name.as_str(), arena)?,
                    text(metadata.name.as_str(), arena)?,
                    Datum::Bool(false),
                    Datum::Float4(column_statistics.null_fraction_ppm as f32 / 1_000_000.0),
                    Datum::Int4(column_statistics.average_width.min(i32::MAX as u32) as i32),
                    Datum::Float4(distinct),
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(def, &rows[..count], arena)
}

pub(crate) fn publication_oid(slot: usize) -> i32 {
    FIRST_USER_OID + 80_000 + slot as i32
}

pub(crate) fn subscription_oid(subscription: &crate::storage::SubscriptionDef) -> i32 {
    FIRST_USER_OID + 95_000 + subscription.created_at as i32
}

fn policy_command_name(command: PolicyCommandKind) -> &'static str {
    match command {
        PolicyCommandKind::All => "ALL",
        PolicyCommandKind::Select => "SELECT",
        PolicyCommandKind::Insert => "INSERT",
        PolicyCommandKind::Update => "UPDATE",
        PolicyCommandKind::Delete => "DELETE",
    }
}

fn policy_role_oids<'a>(
    roles: crate::storage::PolicyRoles,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; crate::storage::MAX_POLICY_ROLES];
    for (index, role) in roles.entries().iter().copied().enumerate() {
        values[index] = Datum::Int4(if role == crate::storage::PUBLIC_ROLE {
            0
        } else {
            Storage::role_oid(usize::from(role))
        });
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Oid,
        raw: super::array::build(&values[..roles.entries().len()], arena)?,
    })
}

fn policy_role_names<'a>(
    storage: &Storage,
    roles: crate::storage::PolicyRoles,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; crate::storage::MAX_POLICY_ROLES];
    for (index, role) in roles.entries().iter().copied().enumerate() {
        values[index] = if role == crate::storage::PUBLIC_ROLE {
            text("public", arena)?
        } else {
            let name = storage.role_name(usize::from(role), txid);
            text(name.as_str(), arena)?
        };
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Name,
        raw: super::array::build(&values[..roles.entries().len()], arena)?,
    })
}

fn extended_statistics_kinds<'a>(
    statistics: &crate::storage::ExtendedStatisticsDef,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; 4];
    let mut count = 0usize;
    for (enabled, code) in [
        (statistics.kinds.ndistinct(), "d"),
        (statistics.kinds.dependencies(), "f"),
        (statistics.kinds.mcv(), "m"),
        (
            statistics
                .keys_for(txid)
                .iter()
                .any(|key| matches!(key, crate::storage::ExtendedStatisticsKey::Expression(_))),
            "e",
        ),
    ] {
        if enabled {
            values[count] = Datum::Char(code.as_bytes()[0]);
            count += 1;
        }
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Char,
        raw: super::array::build(&values[..count], arena)?,
    })
}

fn extended_statistics_expressions<'a>(
    statistics: &crate::storage::ExtendedStatisticsDef,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use core::fmt::Write as _;
    let mut output = StackStr::<8192>::new();
    for key in statistics.keys_for(txid) {
        let crate::storage::ExtendedStatisticsKey::Expression(expression) = key else {
            continue;
        };
        if !output.as_str().is_empty() {
            let _ = output.write_str(", ");
        }
        let _ = write!(output, "({})", expression.as_str());
    }
    if output.as_str().is_empty() {
        Ok(Datum::Null)
    } else if output.is_truncated() {
        Err(catalog_capacity_exceeded("pg_statistic_ext"))
    } else {
        text(output.as_str(), arena)
    }
}

fn pg_statistic_ext<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_statistic_ext",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("stxrelid", ColType::Int4),
            ("stxname", ColType::Name),
            ("stxnamespace", ColType::Int4),
            ("stxowner", ColType::Int4),
            ("stxkeys", ColType::Int2Vector),
            ("stxstattarget", ColType::Int2),
            ("stxkind", ColType::Array(super::types::ArrElem::Char)),
            ("stxexprs", ColType::PgNodeTree),
        ],
    );
    let rows = arena
        .alloc_slice_with(storage.extended_statistics_count(), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    for (slot, statistics) in storage.extended_statistics_visible(txid) {
        let mutable = statistics.definition_for(txid);
        let mut columns = [0u16; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
        let mut n_columns = 0usize;
        for key in statistics.keys_for(txid) {
            if let crate::storage::ExtendedStatisticsKey::Column(column) = key {
                let table = storage.table_def(usize::from(statistics.table), txid);
                columns[n_columns] = table
                    .column_index(column.as_str())
                    .expect("statistics column remains a table dependency")
                    as u16;
                n_columns += 1;
            }
        }
        rows[count] = row(
            &[
                Datum::Int4(3381),
                Datum::Int4(extended_statistics_oid(slot)),
                Datum::Int4(table_oid(storage, usize::from(statistics.table))),
                text(mutable.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, mutable.schema.as_str())),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Statistics,
                    slot,
                    txid,
                )),
                int2vector(&columns[..n_columns], arena)?,
                mutable
                    .target
                    .map_or(Datum::Null, |target| Datum::Int2(target as i16)),
                extended_statistics_kinds(statistics, txid, arena)?,
                extended_statistics_expressions(statistics, txid, arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn extended_statistics_key_numbers(
    storage: &Storage,
    statistics: &crate::storage::ExtendedStatisticsDef,
    txid: u32,
) -> [i16; crate::storage::MAX_EXTENDED_STATISTICS_KEYS] {
    let table = storage.table_def(usize::from(statistics.table), txid);
    let mut numbers = [0i16; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
    let mut expression = 0i16;
    for (position, key) in statistics.keys_for(txid).iter().enumerate() {
        numbers[position] = match key {
            crate::storage::ExtendedStatisticsKey::Column(column) => table
                .column_index(column.as_str())
                .map(|column| column as i16 + 1)
                .expect("statistics column remains a table dependency"),
            crate::storage::ExtendedStatisticsKey::Expression(_) => {
                expression -= 1;
                expression
            }
        };
    }
    numbers
}

fn extended_statistics_ndistinct(
    key_numbers: &[i16],
    data: crate::storage::ExtendedStatisticsData,
) -> StackStr<256> {
    use core::fmt::Write as _;
    let mut output = StackStr::new();
    let _ = output.write_str("{\"");
    for (key, number) in key_numbers.iter().enumerate() {
        if key != 0 {
            let _ = output.write_str(", ");
        }
        let _ = write!(output, "{}", number);
    }
    let _ = write!(output, "\": {}", data.distinct_values);
    let _ = output.write_char('}');
    output
}

fn extended_statistics_dependencies(
    key_numbers: &[i16],
    data: crate::storage::ExtendedStatisticsData,
) -> StackStr<4096> {
    use core::fmt::Write as _;
    let mut output = StackStr::new();
    let mut first = true;
    let _ = output.write_char('{');
    for determinant in 0..key_numbers.len() {
        for dependent in 0..key_numbers.len() {
            let strength = data.dependencies_ppm
                [determinant * crate::storage::MAX_EXTENDED_STATISTICS_KEYS + dependent];
            if strength == 0 {
                continue;
            }
            if !first {
                let _ = output.write_str(", ");
            }
            first = false;
            let _ = write!(
                output,
                "\"{} => {}\": {}.{:06}",
                key_numbers[determinant],
                key_numbers[dependent],
                strength / 1_000_000,
                strength % 1_000_000
            );
        }
    }
    let _ = output.write_char('}');
    output
}

fn extended_statistics_mcv<'a>(
    data: crate::storage::ExtendedStatisticsData,
    key_count: usize,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    const DIMENSION_INFO_BYTES: usize = 20;
    const ALIGNMENT: usize = 8;

    fn append(out: &mut [u8], at: &mut usize, bytes: &[u8]) {
        out[*at..*at + bytes.len()].copy_from_slice(bytes);
        *at += bytes.len();
    }

    fn value<'a>(raw: &'a [u8], dimension: usize) -> Datum<'a> {
        super::array::get(raw, super::types::ArrElem::Text, dimension)
            .expect("ANALYZE stores one MCV member per statistics key")
    }

    fn same_value(left: Datum<'_>, right: Datum<'_>) -> bool {
        match (left, right) {
            (Datum::Null, Datum::Null) => true,
            (Datum::Text(left), Datum::Text(right)) => left == right,
            _ => false,
        }
    }

    let item_count = usize::from(data.n_mcv);
    if item_count == 0 {
        return Ok(Datum::Null);
    }
    let mut values = [&[][..]; crate::storage::MAX_EXTENDED_STATISTICS_MCV];
    for (position, item) in data.mcv[..item_count].iter().enumerate() {
        values[position] =
            super::array::parse_literal(item.values.as_str(), super::types::ArrElem::Text, arena)?;
        if super::array::len(values[position]) != key_count {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "corrupt extended-statistics MCV width"
            ));
        }
    }

    let mut value_counts = [0usize; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
    let mut value_bytes = [0usize; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
    let mut aligned_bytes = [0usize; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
    for dimension in 0..key_count {
        for raw in &values[..item_count] {
            let Datum::Text(text) = value(raw, dimension) else {
                continue;
            };
            value_counts[dimension] += 1;
            value_bytes[dimension] += 4 + text.len();
            aligned_bytes[dimension] += (4 + text.len()).next_multiple_of(ALIGNMENT);
        }
    }
    let item_bytes = item_count
        .checked_mul(key_count + 16 + 2 * key_count)
        .ok_or_else(arena_full)?;
    let total = 12usize
        .checked_add(2)
        .and_then(|bytes| bytes.checked_add(4 * key_count))
        .and_then(|bytes| bytes.checked_add(DIMENSION_INFO_BYTES * key_count))
        .and_then(|bytes| bytes.checked_add(value_bytes[..key_count].iter().sum()))
        .and_then(|bytes| bytes.checked_add(item_bytes))
        .ok_or_else(arena_full)?;
    let binary = arena
        .alloc_slice_with(total, |_| 0u8)
        .map_err(|_| arena_full())?;
    let mut at = 0usize;
    append(binary, &mut at, &0xE1A6_51C2u32.to_ne_bytes());
    append(binary, &mut at, &1u32.to_ne_bytes());
    append(binary, &mut at, &(item_count as u32).to_ne_bytes());
    append(binary, &mut at, &(key_count as i16).to_ne_bytes());
    for _ in 0..key_count {
        append(binary, &mut at, &25u32.to_ne_bytes());
    }
    for dimension in 0..key_count {
        append(
            binary,
            &mut at,
            &(value_counts[dimension] as i32).to_ne_bytes(),
        );
        append(
            binary,
            &mut at,
            &(value_bytes[dimension] as i32).to_ne_bytes(),
        );
        append(
            binary,
            &mut at,
            &(aligned_bytes[dimension] as i32).to_ne_bytes(),
        );
        append(binary, &mut at, &(-1i32).to_ne_bytes());
        append(binary, &mut at, &[0, 0, 0, 0]);
    }
    for dimension in 0..key_count {
        for raw in &values[..item_count] {
            let Datum::Text(text) = value(raw, dimension) else {
                continue;
            };
            append(binary, &mut at, &(text.len() as u32).to_ne_bytes());
            append(binary, &mut at, text.as_bytes());
        }
    }
    for (item, raw) in values[..item_count].iter().enumerate() {
        for dimension in 0..key_count {
            binary[at] = u8::from(value(raw, dimension).is_null());
            at += 1;
        }
        let frequency = data.mcv[item].count as f64 / data.rows.max(1) as f64;
        append(binary, &mut at, &frequency.to_ne_bytes());
        let mut base_frequency = 1.0f64;
        for dimension in 0..key_count {
            let sought = value(raw, dimension);
            let marginal = values[..item_count]
                .iter()
                .enumerate()
                .filter(|(_, candidate)| same_value(value(candidate, dimension), sought))
                .map(|(candidate, _)| data.mcv[candidate].count)
                .sum::<u64>();
            base_frequency *= marginal as f64 / data.rows.max(1) as f64;
        }
        append(binary, &mut at, &base_frequency.to_ne_bytes());
        for dimension in 0..key_count {
            let index = values[..item]
                .iter()
                .filter(|candidate| !value(candidate, dimension).is_null())
                .count() as u16;
            append(binary, &mut at, &index.to_ne_bytes());
        }
    }
    debug_assert_eq!(at, total);
    let hex = super::encoding::hex_encode(binary, arena)?;
    let rendered = arena
        .alloc_slice_with(hex.len() + 2, |_| 0u8)
        .map_err(|_| arena_full())?;
    rendered[..2].copy_from_slice(b"\\x");
    rendered[2..].copy_from_slice(hex.as_bytes());
    text(
        core::str::from_utf8(rendered).expect("hex MCV representation is UTF-8"),
        arena,
    )
}

fn extended_statistics_expression_row(
    column: crate::storage::ColumnStatistics,
    rows: u64,
) -> StackStr<512> {
    use core::fmt::Write as _;
    let null_fraction = f64::from(column.null_fraction_ppm) / 1_000_000.0;
    let distinct = if rows != 0 && column.distinct_values > rows / 10 {
        -(column.distinct_values as f64 / rows as f64)
    } else {
        column.distinct_values as f64
    };
    let mut output = StackStr::new();
    let _ = write!(
        output,
        "(0,0,f,{null_fraction},{},{distinct},0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,,,,,,,,,,)",
        column.average_width
    );
    output
}

fn pg_statistic_ext_data<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_statistic_ext_data",
        &[
            ("tableoid", ColType::Int4),
            ("stxoid", ColType::Int4),
            ("stxdinherit", ColType::Bool),
            ("stxdndistinct", ColType::PgNdistinct),
            ("stxddependencies", ColType::PgDependencies),
            ("stxdmcv", ColType::PgMcvList),
            ("stxdexpr", ColType::PgStatisticArray),
        ],
    );
    let rows = arena
        .alloc_slice_with(storage.extended_statistics_count(), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    for (slot, statistics) in storage.extended_statistics_visible(txid) {
        let data = storage.extended_statistics_data(slot, txid);
        if !data.valid {
            continue;
        }
        let key_numbers = extended_statistics_key_numbers(storage, statistics, txid);
        let key_numbers = &key_numbers[..usize::from(statistics.n_keys)];
        let ndistinct = extended_statistics_ndistinct(key_numbers, data);
        let dependencies = extended_statistics_dependencies(key_numbers, data);
        let mut expressions = [Datum::Null; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
        let mut n_expressions = 0usize;
        for (key, key_definition) in statistics.keys_for(txid).iter().enumerate() {
            if matches!(
                key_definition,
                crate::storage::ExtendedStatisticsKey::Expression(_)
            ) {
                let column = data.expression_statistics[key];
                expressions[n_expressions] = text(
                    extended_statistics_expression_row(column, data.rows).as_str(),
                    arena,
                )?;
                n_expressions += 1;
            }
        }
        rows[count] = row(
            &[
                Datum::Int4(3429),
                Datum::Int4(extended_statistics_oid(slot)),
                Datum::Bool(data.inherited),
                if statistics.kinds.ndistinct() {
                    text(ndistinct.as_str(), arena)?
                } else {
                    Datum::Null
                },
                if statistics.kinds.dependencies() {
                    text(dependencies.as_str(), arena)?
                } else {
                    Datum::Null
                },
                if statistics.kinds.mcv() {
                    extended_statistics_mcv(data, key_numbers.len(), arena)?
                } else {
                    Datum::Null
                },
                if n_expressions == 0 {
                    Datum::Null
                } else {
                    Datum::Array {
                        element: super::types::ArrElem::Text,
                        raw: super::array::build(&expressions[..n_expressions], arena)?,
                    }
                },
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_policy<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_policy",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("polname", ColType::Name),
            ("polrelid", ColType::Int4),
            ("polcmd", ColType::Bpchar),
            ("polpermissive", ColType::Bool),
            ("polroles", ColType::Array(super::types::ArrElem::Oid)),
            ("polqual", ColType::PgNodeTree),
            ("polwithcheck", ColType::PgNodeTree),
        ],
    );
    let rows = arena
        .alloc_slice_with(storage.policy_count(), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (_, policy) in storage.policies_with_slots_visible_to(txid) {
        let policy_definition = policy.definition_for(txid);
        rows[count] = row(
            &[
                Datum::Int4(3256),
                Datum::Int4(crate::storage::policy_oid(policy)),
                text(policy.name.as_str(), arena)?,
                Datum::Int4(table_oid(storage, usize::from(policy.table))),
                text(
                    core::str::from_utf8(&[policy.command.code()]).unwrap_or("*"),
                    arena,
                )?,
                Datum::Bool(policy.permissive),
                policy_role_oids(policy_definition.roles, arena)?,
                policy_definition
                    .using
                    .map(|source| text(source.as_str(), arena))
                    .transpose()?
                    .unwrap_or(Datum::Null),
                policy_definition
                    .with_check
                    .map(|source| text(source.as_str(), arena))
                    .transpose()?
                    .unwrap_or(Datum::Null),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_policies<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_policies",
        &[
            ("schemaname", ColType::Name),
            ("tablename", ColType::Name),
            ("policyname", ColType::Name),
            ("permissive", ColType::Text),
            ("roles", ColType::Array(super::types::ArrElem::Name)),
            ("cmd", ColType::Text),
            ("qual", ColType::Text),
            ("with_check", ColType::Text),
        ],
    );
    let rows = arena
        .alloc_slice_with(storage.policy_count(), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (_, policy) in storage.policies_with_slots_visible_to(txid) {
        let table = storage.table_def(usize::from(policy.table), txid);
        let policy_definition = policy.definition_for(txid);
        rows[count] = row(
            &[
                text(table.schema.as_str(), arena)?,
                text(table.name.as_str(), arena)?,
                text(policy.name.as_str(), arena)?,
                text(
                    if policy.permissive {
                        "PERMISSIVE"
                    } else {
                        "RESTRICTIVE"
                    },
                    arena,
                )?,
                policy_role_names(storage, policy_definition.roles, txid, arena)?,
                text(policy_command_name(policy.command), arena)?,
                policy_definition
                    .using
                    .map(|source| text(source.as_str(), arena))
                    .transpose()?
                    .unwrap_or(Datum::Null),
                policy_definition
                    .with_check
                    .map(|source| text(source.as_str(), arena))
                    .transpose()?
                    .unwrap_or(Datum::Null),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_publication<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_publication",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("pubname", ColType::Text),
            ("pubowner", ColType::Int4),
            ("puballtables", ColType::Bool),
            ("pubinsert", ColType::Bool),
            ("pubupdate", ColType::Bool),
            ("pubdelete", ColType::Bool),
            ("pubtruncate", ColType::Bool),
            ("pubviaroot", ColType::Bool),
            ("pubgencols", ColType::Bpchar),
        ],
    );
    let mut rows: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0;
    for (slot, publication) in storage.publications_with_slots_visible_to(txid) {
        let definition = publication.definition_for(txid);
        rows[count] = row(
            &[
                Datum::Int4(6104),
                Datum::Int4(publication_oid(slot)),
                text(publication.name_for(txid).as_str(), arena)?,
                Datum::Int4(Storage::role_oid(
                    publication.ownership.owner_to(txid) as usize
                )),
                Datum::Bool(definition.all_tables),
                Datum::Bool(definition.publish_insert),
                Datum::Bool(definition.publish_update),
                Datum::Bool(definition.publish_delete),
                Datum::Bool(definition.publish_truncate),
                Datum::Bool(definition.publish_via_partition_root),
                text(definition.publish_generated_columns.pg_code(), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_publication_namespace<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_publication_namespace",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("pnpubid", ColType::Int4),
            ("pnnspid", ColType::Int4),
        ],
    );
    let mut rows: [&[Datum]; crate::storage::MAX_SCHEMAS * 256] =
        [&[]; crate::storage::MAX_SCHEMAS * 256];
    let mut count = 0;
    for (publication_slot, publication) in storage.publications_with_slots_visible_to(txid) {
        let publication_definition = publication.definition_for(txid);
        for schema_slot in &publication_definition.schemas[..publication_definition.schema_count] {
            if count == rows.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_publication_namespace exceeds {} rows",
                    rows.len()
                ));
            }
            let schema = storage.schema_def(*schema_slot as usize);
            rows[count] = row(
                &[
                    Datum::Int4(6105),
                    Datum::Int4(FIRST_USER_OID + 85_000 + count as i32),
                    Datum::Int4(publication_oid(publication_slot)),
                    Datum::Int4(namespace_oid(storage, schema.name.as_str())),
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn pg_publication_rel<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_publication_rel",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("prpubid", ColType::Int4),
            ("prrelid", ColType::Int4),
            ("prqual", ColType::Text),
            ("prattrs", ColType::Int2Vector),
        ],
    );
    let mut rows: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0;
    for (publication_slot, publication) in storage.publications_with_slots_visible_to(txid) {
        let definition = publication.definition_for(txid);
        if definition.all_tables {
            continue;
        }
        for (index, (member, column_mask)) in definition.tables[..definition.table_count]
            .iter()
            .zip(&definition.table_column_masks[..definition.table_count])
            .enumerate()
        {
            let filter = definition.table_filters.get(index);
            let prqual = if filter.is_empty() {
                Datum::Null
            } else {
                let rendered = stack_format!(66, "({filter})");
                text(rendered.as_str(), arena)?
            };
            if count == rows.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_publication_rel exceeds {} rows",
                    rows.len()
                ));
            }
            let prattrs = if *column_mask == 0 {
                Datum::Null
            } else {
                let mut attributes = [0u16; crate::storage::MAX_COLUMNS];
                let mut attribute_count = 0usize;
                for column in 0..crate::storage::MAX_COLUMNS {
                    if column_mask & (1u64 << column) != 0 {
                        attributes[attribute_count] = column as u16;
                        attribute_count += 1;
                    }
                }
                int2vector(&attributes[..attribute_count], arena)?
            };
            rows[count] = row(
                &[
                    Datum::Int4(6106),
                    Datum::Int4(FIRST_USER_OID + 90_000 + count as i32),
                    Datum::Int4(publication_oid(publication_slot)),
                    Datum::Int4(table_oid(storage, *member as usize)),
                    prqual,
                    prattrs,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn pg_publication_tables<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_publication_tables",
        &[
            ("pubname", ColType::Name),
            ("schemaname", ColType::Name),
            ("tablename", ColType::Name),
            ("attnames", ColType::Array(super::types::ArrElem::Name)),
            ("rowfilter", ColType::Text),
        ],
    );
    let mut rows: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0;
    for (_, publication) in storage.publications_with_slots_visible_to(txid) {
        let published = publication.definition_for(txid);
        let mut emitted = [usize::MAX; 256];
        let mut emitted_count = 0;
        for (table_slot, table) in storage.live_tables() {
            if !table.visible_to(txid) {
                continue;
            }
            let explicit = super::publication_partition_member(storage, publication, table_slot);
            let schema =
                super::publication_partition_schema_member(storage, publication, table_slot);
            if !published.all_tables && !schema && explicit.is_none() {
                continue;
            }
            let output = if published.publish_via_partition_root {
                super::partition_root(storage, table_slot)
            } else {
                table_slot
            };
            if emitted[..emitted_count].contains(&output) {
                continue;
            }
            if emitted_count == emitted.len() || count == rows.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_publication_tables exceeds {} rows",
                    rows.len()
                ));
            }
            emitted[emitted_count] = output;
            emitted_count += 1;
            let output_definition = storage.table_def(output, txid);
            let effective_explicit =
                super::publication_partition_member(storage, publication, output);
            let implicit_mask = || {
                output_definition
                    .columns()
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| {
                        !column.default.is_generated()
                            || published.publish_generated_columns
                                == crate::storage::PublishGeneratedColumns::Stored
                    })
                    .fold(0u64, |mask, (column, _)| mask | (1u64 << column))
            };
            let column_mask = if published.all_tables || schema {
                implicit_mask()
            } else if let Some(index) = effective_explicit.or(explicit) {
                if usize::from(published.tables[index]) != output
                    && !published.publish_via_partition_root
                {
                    implicit_mask()
                } else {
                    let mask = published.table_column_masks[index];
                    if mask == 0 { implicit_mask() } else { mask }
                }
            } else {
                implicit_mask()
            };
            let mut attribute_values = [Datum::Null; crate::storage::MAX_COLUMNS];
            let mut attribute_count = 0;
            for (column, metadata) in output_definition.columns().iter().enumerate() {
                if column_mask & (1u64 << column) != 0 {
                    attribute_values[attribute_count] = text(metadata.name.as_str(), arena)?;
                    attribute_count += 1;
                }
            }
            let attributes = Datum::Array {
                element: super::types::ArrElem::Name,
                raw: super::array::build(&attribute_values[..attribute_count], arena)?,
            };
            let row_filter = effective_explicit
                .or(explicit)
                .map(|index| published.table_filters.get(index))
                .filter(|filter| !filter.is_empty());
            rows[count] = row(
                &[
                    text(publication.name_for(txid).as_str(), arena)?,
                    text(output_definition.schema.as_str(), arena)?,
                    text(output_definition.name.as_str(), arena)?,
                    attributes,
                    match row_filter {
                        Some(filter) => text(filter, arena)?,
                        None => Datum::Null,
                    },
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn pg_replication_slots<'a>(
    storage: &Storage,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_replication_slots",
        &[
            ("slot_name", ColType::Name),
            ("plugin", ColType::Name),
            ("slot_type", ColType::Text),
            ("datoid", ColType::Int4),
            ("database", ColType::Name),
            ("temporary", ColType::Bool),
            ("active", ColType::Bool),
            ("active_pid", ColType::Int4),
            ("xmin", ColType::Text),
            ("catalog_xmin", ColType::Text),
            ("restart_lsn", ColType::Text),
            ("confirmed_flush_lsn", ColType::Text),
            ("wal_status", ColType::Text),
            ("safe_wal_size", ColType::Int8),
            ("two_phase", ColType::Bool),
            ("two_phase_at", ColType::Text),
            ("inactive_since", ColType::Timestamptz),
            ("conflicting", ColType::Bool),
            ("invalidation_reason", ColType::Text),
            ("failover", ColType::Bool),
            ("synced", ColType::Bool),
        ],
    );
    let mut rows: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0;
    let database_definition = storage.database_definition(
        storage
            .database_slot_by_oid(storage.current_database_oid(), 0)
            .expect("current database is visible"),
        0,
    );
    for (_, slot) in storage.replication_slots_with_slots() {
        if count == rows.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "pg_replication_slots exceeds {} rows",
                rows.len()
            ));
        }
        let restart_lsn = stack_format!(32, "0/{:X}", slot.restart_lsn);
        let confirmed_lsn = stack_format!(32, "0/{:X}", slot.confirmed_flush_lsn);
        rows[count] = row(
            &[
                text(slot.name.as_str(), arena)?,
                text("pgoutput", arena)?,
                text("logical", arena)?,
                Datum::Int4(storage.current_database_oid().get()),
                text(database_definition.name.as_str(), arena)?,
                Datum::Bool(false),
                Datum::Bool(slot.active),
                Datum::Null,
                Datum::Null,
                Datum::Null,
                text(restart_lsn.as_str(), arena)?,
                text(confirmed_lsn.as_str(), arena)?,
                text("reserved", arena)?,
                Datum::Null,
                Datum::Bool(slot.behavior.two_phase),
                if slot.behavior.two_phase {
                    text(restart_lsn.as_str(), arena)?
                } else {
                    Datum::Null
                },
                Datum::Null,
                Datum::Bool(false),
                Datum::Null,
                Datum::Bool(slot.behavior.failover),
                Datum::Bool(false),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_subscription<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_subscription",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("subdbid", ColType::Int4),
            ("subskiplsn", ColType::Text),
            ("subname", ColType::Name),
            ("subowner", ColType::Int4),
            ("subenabled", ColType::Bool),
            ("subbinary", ColType::Bool),
            ("substream", ColType::Bpchar),
            ("subtwophasestate", ColType::Bpchar),
            ("subdisableonerr", ColType::Bool),
            ("subpasswordrequired", ColType::Bool),
            ("subrunasowner", ColType::Bool),
            ("subfailover", ColType::Bool),
            ("subconninfo", ColType::Text),
            ("subslotname", ColType::Name),
            ("subsynccommit", ColType::Text),
            (
                "subpublications",
                ColType::Array(super::types::ArrElem::Text),
            ),
            ("suborigin", ColType::Text),
        ],
    );
    let mut rows: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0;
    for (_slot, subscription) in storage.subscriptions_with_slots_visible_to(txid) {
        if count == rows.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "pg_subscription exceeds {} rows",
                rows.len()
            ));
        }
        let definition = subscription.definition_to(txid);
        let mut publications = [Datum::Null; crate::storage::MAX_SUBSCRIPTION_PUBLICATIONS];
        for (index, publication) in definition.publications().iter().enumerate() {
            publications[index] = text(publication.as_str(), arena)?;
        }
        let publications = Datum::Array {
            element: super::types::ArrElem::Text,
            raw: super::array::build(&publications[..definition.publication_count()], arena)?,
        };
        let skip_lsn = match definition.behavior.skip_lsn {
            Some(lsn) => {
                let value = stack_format!(32, "0/{lsn:X}");
                text(value.as_str(), arena)?
            }
            None => Datum::Null,
        };
        rows[count] = row(
            &[
                Datum::Int4(6107),
                Datum::Int4(subscription_oid(subscription)),
                Datum::Int4(storage.current_database_oid().get()),
                skip_lsn,
                text(subscription.name.as_str(), arena)?,
                Datum::Int4(Storage::role_oid(
                    subscription.ownership.owner_to(txid) as usize
                )),
                Datum::Bool(subscription.enabled_to(txid)),
                Datum::Bool(definition.behavior.binary),
                text(definition.behavior.streaming.pg_code(), arena)?,
                text(
                    if definition.behavior.two_phase {
                        if matches!(
                            subscription.bootstrap_to(txid),
                            crate::storage::SubscriptionBootstrap::Ready
                        ) {
                            "e"
                        } else {
                            "p"
                        }
                    } else {
                        "d"
                    },
                    arena,
                )?,
                Datum::Bool(definition.behavior.disable_on_error),
                Datum::Bool(definition.behavior.password_required),
                Datum::Bool(definition.behavior.run_as_owner),
                Datum::Bool(definition.behavior.failover),
                text(definition.connection.as_str(), arena)?,
                match definition.slot.name() {
                    Some(slot) => text(slot.as_str(), arena)?,
                    None => Datum::Null,
                },
                text(definition.behavior.synchronous_commit.as_str(), arena)?,
                publications,
                text(definition.behavior.origin.as_str(), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_subscription_rel<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_subscription_rel",
        &[
            ("srsubid", ColType::Int4),
            ("srrelid", ColType::Int4),
            ("srsubstate", ColType::Bpchar),
            ("srsublsn", ColType::Text),
        ],
    );
    let count = storage
        .subscriptions_with_slots_visible_to(txid)
        .map(|(_, subscription)| {
            storage
                .subscription_relations_visible_to(subscription, txid)
                .count()
        })
        .sum();
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut index = 0;
    for (_, subscription) in storage.subscriptions_with_slots_visible_to(txid) {
        for relation in storage.subscription_relations_visible_to(subscription, txid) {
            let lsn = relation.synchronization_lsn();
            let rendered_lsn = stack_format!(32, "0/{lsn:X}");
            rows[index] = row(
                &[
                    Datum::Int4(subscription_oid(subscription)),
                    Datum::Int4(table_oid(storage, relation.table_slot())),
                    text(relation.state().pg_code(), arena)?,
                    if lsn == 0 {
                        Datum::Null
                    } else {
                        text(rendered_lsn.as_str(), arena)?
                    },
                ],
                arena,
            )?;
            index += 1;
        }
    }
    finish(definition, rows, arena)
}

fn pg_inherits<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_inherits",
        &[
            ("inhrelid", ColType::Int4),
            ("inhparent", ColType::Int4),
            ("inhseqno", ColType::Int4),
            ("inhdetachpending", ColType::Bool),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let inherited_indexes = indexes
        .iter()
        .filter(|index| {
            index.constraint_parent_oid != 0
                || index
                    .explicit_definition
                    .is_some_and(|definition| definition.parent.is_some())
        })
        .count();
    let rows = arena
        .alloc_slice_with(
            storage.table_count() * (1 + crate::storage::MAX_TABLE_INHERITANCE_PARENTS)
                + inherited_indexes,
            |_| &[] as &[Datum],
        )
        .map_err(|_| arena_full())?;
    let mut n = 0;
    for child in 0..storage.table_count() {
        if !storage.table_slot_visible_to(child, txid) {
            continue;
        }
        let definition = storage.table_def(child, txid);
        for (position, parent) in definition.inheritance.parents_ref().iter().enumerate() {
            rows[n] = row(
                &[
                    Datum::Int4(table_oid(storage, child)),
                    Datum::Int4(table_oid(storage, usize::from(*parent))),
                    Datum::Int4((position + 1) as i32),
                    Datum::Bool(false),
                ],
                arena,
            )?;
            n += 1;
        }
        if let Some(crate::storage::PartitionAttachment { parent, state, .. }) =
            definition.partition.attachment
        {
            rows[n] = row(
                &[
                    Datum::Int4(table_oid(storage, child)),
                    Datum::Int4(table_oid(storage, usize::from(parent))),
                    Datum::Int4(1),
                    Datum::Bool(matches!(
                        state,
                        crate::storage::PartitionAttachmentState::DetachPending
                    )),
                ],
                arena,
            )?;
            n += 1;
        }
    }
    for index in indexes {
        let parent_oid = if index.constraint_parent_oid != 0 {
            index.constraint_parent_oid - 500_000
        } else if let Some(parent) = index
            .explicit_definition
            .and_then(|definition| definition.parent)
            .and_then(|slot| storage.index_visible_to(usize::from(slot), txid))
        {
            explicit_index_oid(&parent)
        } else {
            continue;
        };
        rows[n] = row(
            &[
                Datum::Int4(index.oid),
                Datum::Int4(parent_oid),
                Datum::Int4(1),
                Datum::Bool(false),
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &rows[..n], arena)
}

fn pg_partitioned_table<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_partitioned_table",
        &[
            ("partrelid", ColType::Int4),
            ("partstrat", ColType::Bpchar),
            ("partattrs", ColType::Text),
            ("partclass", ColType::Array(super::types::ArrElem::Oid)),
            ("partexprs", ColType::Text),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let Some(crate::storage::PartitionScheme {
            strategy,
            keys,
            n_keys,
        }) = storage.table_def(slot, txid).partition.scheme
        else {
            continue;
        };
        if n == rows.len() {
            return Err(catalog_capacity_exceeded("pg_partitioned_table"));
        }
        let strategy = match strategy {
            PartitionStrategy::Range => "r",
            PartitionStrategy::List => "l",
            PartitionStrategy::Hash => "h",
        };
        let mut attributes = StackStr::<128>::new();
        use core::fmt::Write;
        for (i, key) in keys[..usize::from(n_keys)].iter().enumerate() {
            if i != 0 {
                let _ = write!(attributes, " ");
            }
            let _ = write!(attributes, "{}", key + 1);
        }
        rows[n] = row(
            &[
                Datum::Int4(table_oid(storage, slot)),
                text(strategy, arena)?,
                text(attributes.as_str(), arena)?,
                Datum::Null,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &rows[..n], arena)
}

fn pg_class<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
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
            ("tableoid", ColType::Int4),
            ("reltype", ColType::Int4),
            ("relacl", ColType::Array(super::types::ArrElem::AclItem)),
            ("relallvisible", ColType::Int4),
            ("relallfrozen", ColType::Int4),
            ("relfrozenxid", ColType::Int4),
            ("relminmxid", ColType::Int4),
            ("reloptions", ColType::Array(super::types::ArrElem::Text)),
            ("relispopulated", ColType::Bool),
            ("relpartbound", ColType::PgNodeTree),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let foreign_keys = collect_fkeys(storage, txid, arena)?;
    let mut out: [&[Datum]; 512] = [&[]; 512];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table_def = storage.table_def(slot, txid);
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        let toid = table_oid(storage, slot);
        let has_index = indexes.iter().any(|i| i.table_oid == toid);
        let has_triggers = storage.triggers_for_table(slot, txid).next().is_some()
            || !table_def.fkeys().is_empty()
            || foreign_keys
                .iter()
                .any(|foreign_key| foreign_key.confrelid == toid);
        let has_rules = storage.table_has_rules(slot, txid);
        let n_checks = table_def.n_checks as i32;
        let statistics = storage.table_statistics(slot, txid);
        let reltuples = if statistics.valid {
            statistics.rows as f64
        } else {
            -1.0
        };
        let relpages = if statistics.valid {
            statistics
                .rows
                .saturating_mul(u64::from(statistics.average_row_width))
                .div_ceil(8_192)
                .min(i32::MAX as u64) as i32
        } else {
            0
        };
        // A table that has a matching matview catalog entry is a materialized
        // view (relkind 'm'), not an ordinary table ('r').
        let relkind = if table_def.kind == crate::storage::TableKind::Foreign {
            "f"
        } else if table_def.partition.is_partitioned() {
            "p"
        } else if storage
            .find_matview(table_def.schema.as_str(), table_def.name.as_str(), txid)
            .is_some()
        {
            "m"
        } else {
            "r"
        };
        let relation_object = storage
            .matview_slot(table_def.schema.as_str(), table_def.name.as_str(), txid)
            .map_or(
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Table,
                    slot: slot as u16,
                },
                |matview| crate::storage::AccessObject {
                    class: crate::storage::AccessClass::MaterializedView,
                    slot: matview as u16,
                },
            );
        let relation_owner = Storage::role_oid(storage.object_owner(relation_object, txid));
        let reloptions = match table_def.storage_options.fillfactor {
            Some(fillfactor) => Datum::Array {
                element: super::types::ArrElem::Text,
                raw: super::array::build(
                    &[text(
                        stack_format!(32, "fillfactor={fillfactor}").as_str(),
                        arena,
                    )?],
                    arena,
                )?,
            },
            None => Datum::Null,
        };
        out[n] = row(
            &[
                Datum::Int4(toid),
                text(table_def.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, table_def.schema.as_str())),
                text(relkind, arena)?, // relkind: ordinary table 'r' / matview 'm'
                Datum::Int4(table_def.n_columns as i32),
                Datum::Float8(reltuples),
                Datum::Int4(relpages),
                Datum::Int4(match table_def.access_method {
                    crate::storage::TableAccessMethod::Heap => 2,
                    crate::storage::TableAccessMethod::Catalog(oid) => oid.get(),
                }),
                Datum::Int4(relation_owner),
                Datum::Int4(n_checks), // relchecks
                Datum::Bool(has_index),
                Datum::Bool(has_rules),
                Datum::Bool(has_triggers), // FK enforcement is trigger-backed in PostgreSQL
                Datum::Bool(table_def.row_level_security.enabled),
                Datum::Bool(table_def.row_level_security.forced),
                Datum::Bool(table_def.partition.is_attached()),
                Datum::Int4(catalog_tablespace_oid(storage, table_def.tablespace, txid)),
                Datum::Int4(
                    table_def
                        .type_membership
                        .composite_slot()
                        .map_or(0, |slot| crate::sql::types::oid::composite_oid(slot as u16)),
                ),
                Datum::Int4(if table_def.has_toast {
                    toast_relation_oid(slot)
                } else {
                    0
                }),
                text("p", arena)?, // relpersistence: permanent
                text(
                    match table_def.replica_identity {
                        crate::storage::ReplicaIdentityMode::Default => "d",
                        crate::storage::ReplicaIdentityMode::Full => "f",
                        crate::storage::ReplicaIdentityMode::Nothing => "n",
                        crate::storage::ReplicaIdentityMode::Index => "i",
                    },
                    arena,
                )?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(FIRST_TABLE_COMPOSITE_TYPE_OID + slot as i32),
                acl(storage, relation_object, txid, arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                reloptions,
                Datum::Bool(true),
                table_def
                    .partition
                    .attachment
                    .map(|attachment| partition_bound_def_text(attachment.bound, arena))
                    .transpose()?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null),
            ],
            arena,
        )?;
        n += 1;
    }
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) || !storage.table_def(slot, txid).has_toast {
            continue;
        }
        if n + 2 > out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        let table_oid = table_oid(storage, slot);
        let toast_name = crate::stack_format!(64, "pg_toast_{}", table_oid);
        let toast_index_name = crate::stack_format!(72, "{}_index", toast_name.as_str());
        let owner = Storage::role_oid(storage.object_owner(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Table,
                slot: slot as u16,
            },
            txid,
        ));
        out[n] = row(
            &[
                Datum::Int4(toast_relation_oid(slot)),
                text(toast_name.as_str(), arena)?,
                Datum::Int4(PG_TOAST_NS_OID),
                text("t", arena)?,
                Datum::Int4(3),
                Datum::Float8(-1.0),
                Datum::Int4(0),
                Datum::Int4(2),
                Datum::Int4(owner),
                Datum::Int4(0),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                text("p", arena)?,
                text("n", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(0),
                Datum::Null,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
        out[n] = row(
            &[
                Datum::Int4(toast_index_oid(slot)),
                text(toast_index_name.as_str(), arena)?,
                Datum::Int4(PG_TOAST_NS_OID),
                text("i", arena)?,
                Datum::Int4(2),
                Datum::Float8(-1.0),
                Datum::Int4(0),
                Datum::Int4(403),
                Datum::Int4(owner),
                Datum::Int4(0),
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
                text("n", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(0),
                Datum::Null,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // A named composite owns a backing catalog relation. It is not a
    // user-addressable table, but `pg_type.typrelid` and `pg_attribute` must
    // resolve through this `relkind = 'c'` row exactly as they do in PostgreSQL.
    for (slot, composite) in storage.composites_with_slots_visible_to(txid) {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::Composite,
            slot: slot as u16,
        };
        out[n] = row(
            &[
                Datum::Int4(named_composite_relation_oid(slot)),
                text(composite.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, composite.schema.as_str())),
                text("c", arena)?,
                Datum::Int4(composite.active_field_count() as i32),
                Datum::Float8(-1.0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(Storage::role_oid(storage.object_owner(object, txid))),
                Datum::Int4(0),
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
                text("n", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(crate::sql::types::oid::composite_oid(slot as u16)),
                acl(storage, object, txid, arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // Each index is itself a relation (relkind 'i'), so psql's `\d` join
    // (pg_index i JOIN pg_class c2 ON i.indexrelid = c2.oid) finds its name.
    for info in indexes {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        out[n] = row(
            &[
                Datum::Int4(info.oid),
                text(info.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(
                    storage,
                    storage.table_def(info.table_slot, txid).schema.as_str(),
                )),
                text(
                    if info
                        .explicit_definition
                        .is_some_and(|definition| definition.kind.is_partitioned())
                    {
                        "I"
                    } else {
                        "i"
                    },
                    arena,
                )?,
                Datum::Int4((info.n_cols + info.n_include_cols) as i32),
                Datum::Float8(0.0),
                Datum::Int4(0), // relpages
                Datum::Int4(if info.is_exclusion { 783 } else { 403 }),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Table,
                    info.table_slot,
                    txid,
                )),
                Datum::Int4(0), // relchecks
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(
                    info.explicit_definition
                        .is_some_and(|definition| definition.parent.is_some()),
                ),
                Datum::Int4(info.explicit_definition.map_or(0, |definition| {
                    catalog_tablespace_oid(storage, definition.tablespace, txid)
                })),
                Datum::Int4(0),
                Datum::Int4(0),
                text("p", arena)?,
                text("d", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(0),
                Datum::Null,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                index_reloptions(info.explicit_definition, arena)?,
                Datum::Bool(true),
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // Sequences are relations of kind 'S', each with its own OID range so
    // psql's `\d`/`\dm` and pg_get_serial_sequence-style joins resolve.
    for slot in 0..storage.sequence_count() {
        let seq = storage.sequence_for(slot, txid);
        if !storage.sequence_slot_visible_to(slot, txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        out[n] = row(
            &[
                Datum::Int4(sequence_oid(slot)),
                text(seq.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, seq.schema.as_str())),
                text("S", arena)?, // relkind: sequence
                Datum::Int4(1),    // a sequence has one row of state
                Datum::Float8(1.0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Sequence,
                    slot,
                    txid,
                )),
                Datum::Int4(0),
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
                text("n", arena)?, // relreplident: nothing (sequences)
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(0),
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Sequence,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // Plain views are pg_class relations too (relkind 'v'). Their column count
    // is derived under the creator's captured search path, the same binding
    // rule used when the view executes.
    for (slot, view) in storage.views_visible_to(txid) {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let n_columns = describe_view(storage, txid, view, arena, &mut columns)?;
        let mut option_values = [Datum::Null; 3];
        let mut option_count = 0;
        if matches!(
            view.security_for(txid),
            crate::storage::ViewSecurity::Invoker
        ) {
            option_values[option_count] = text("security_invoker=true", arena)?;
            option_count += 1;
        }
        if let Some(enabled) = view.security_barrier_for(txid).reloption() {
            option_values[option_count] = text(
                if enabled {
                    "security_barrier=true"
                } else {
                    "security_barrier=false"
                },
                arena,
            )?;
            option_count += 1;
        }
        if let Some(option) = view.check_option_for(txid) {
            option_values[option_count] = text(
                match option {
                    crate::storage::ViewCheckOption::Local => "check_option=local",
                    crate::storage::ViewCheckOption::Cascaded => "check_option=cascaded",
                },
                arena,
            )?;
            option_count += 1;
        }
        let reloptions = if option_count != 0 {
            Datum::Array {
                element: super::types::ArrElem::Text,
                raw: super::array::build(&option_values[..option_count], arena)?,
            }
        } else {
            Datum::Null
        };
        out[n] = row(
            &[
                Datum::Int4(view_oid(slot)),
                text(view.name_for(txid).as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, view.schema_for(txid).as_str())),
                text("v", arena)?,
                Datum::Int4(n_columns as i32),
                Datum::Float8(0.0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::View,
                    slot,
                    txid,
                )),
                Datum::Int4(0),
                Datum::Bool(false),
                Datum::Bool(true), // a view is represented by a rewrite rule
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                text("p", arena)?,
                text("n", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(FIRST_VIEW_COMPOSITE_TYPE_OID + slot as i32),
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::View,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                reloptions,
                Datum::Bool(true),
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    for (oid, name, relation_type, relation_acl) in [
        (
            2613,
            "pg_largeobject",
            10025,
            &["postgres=arwdDxtm/postgres"] as &[&str],
        ),
        (
            PG_LARGEOBJECT_METADATA_OID,
            "pg_largeobject_metadata",
            10023,
            &["postgres=arwdDxtm/postgres", "=r/postgres"] as &[&str],
        ),
    ] {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        out[n] = row(
            &[
                Datum::Int4(oid),
                text(name, arena)?,
                Datum::Int4(PG_CATALOG_NS_OID),
                text("r", arena)?,
                Datum::Int4(3),
                Datum::Float8(0.0),
                Datum::Int4(0),
                Datum::Int4(2),
                Datum::Int4(10),
                Datum::Int4(0),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                text("p", arena)?,
                text("n", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(relation_type),
                builtin_acl(relation_acl, arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(1),
                Datum::Null,
                Datum::Bool(true),
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    for (oid, name, attributes) in [
        (2683, "pg_largeobject_loid_pn_index", 2),
        (2996, "pg_largeobject_metadata_oid_index", 1),
    ] {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_class"));
        }
        out[n] = row(
            &[
                Datum::Int4(oid),
                text(name, arena)?,
                Datum::Int4(PG_CATALOG_NS_OID),
                text("i", arena)?,
                Datum::Int4(attributes),
                Datum::Float8(1.0),
                Datum::Int4(2),
                Datum::Int4(403),
                Datum::Int4(10),
                Datum::Int4(0),
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
                text("n", arena)?,
                Datum::Int4(PG_CLASS_OID),
                Datum::Int4(0),
                Datum::Null,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Null,
                Datum::Bool(true),
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

pub(crate) fn tablespace_oid(tablespace: crate::storage::TablespaceDef) -> i32 {
    210_000 + i32::try_from(tablespace.created_at).unwrap_or(i32::MAX - 210_000)
}

fn catalog_tablespace_oid(storage: &Storage, id: u16, txid: u32) -> i32 {
    match id {
        // PostgreSQL stores zero for a relation using its database's default
        // tablespace. Reporting pg_default's OID makes pg_dump emit an
        // explicit TABLESPACE clause, which is illegal for partitioned
        // relations.
        0 => 0,
        1 => 1664,
        _ => storage.tablespace_by_id(id, txid).map_or(0, tablespace_oid),
    }
}

fn tablespace_options<'a>(
    options: crate::storage::TablespaceOptions,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; 4];
    let mut count = 0;
    let mut cost = |name: &str,
                    value: Option<crate::sql::ast::TablespaceCost>|
     -> Result<(), SqlError> {
        if let Some(value) = value {
            let rendered = stack_format!(64, "{name}={}", super::types::PgFloat8(value.value()));
            values[count] = text(rendered.as_str(), arena)?;
            count += 1;
        }
        Ok(())
    };
    cost("random_page_cost", options.random_page_cost)?;
    cost("seq_page_cost", options.seq_page_cost)?;
    for (name, value) in [
        ("effective_io_concurrency", options.effective_io_concurrency),
        (
            "maintenance_io_concurrency",
            options.maintenance_io_concurrency,
        ),
    ] {
        if let Some(value) = value {
            values[count] = text(stack_format!(64, "{name}={value}").as_str(), arena)?;
            count += 1;
        }
    }
    if count == 0 {
        Ok(Datum::Null)
    } else {
        Ok(Datum::Array {
            element: super::types::ArrElem::Text,
            raw: super::array::build(&values[..count], arena)?,
        })
    }
}

fn pg_tablespace<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_tablespace",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("spcname", ColType::Name),
            ("spcowner", ColType::Int4),
            ("spcacl", ColType::Array(super::types::ArrElem::AclItem)),
            ("spcoptions", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let mut rows: [&[Datum]; crate::storage::MAX_TABLESPACES] =
        [&[]; crate::storage::MAX_TABLESPACES];
    let mut count = 0;
    for (slot, tablespace) in storage.tablespaces_visible_to(txid) {
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::Tablespace,
            slot: slot as u16,
        };
        rows[count] = row(
            &[
                Datum::Int4(1213),
                Datum::Int4(match tablespace.name_for(txid).as_str() {
                    "pg_default" => 1663,
                    "pg_global" => 1664,
                    _ => tablespace_oid(*tablespace),
                }),
                text(tablespace.name_for(txid).as_str(), arena)?,
                Datum::Int4(Storage::role_oid(storage.object_owner(object, txid))),
                acl(storage, object, txid, arena)?,
                tablespace_options(tablespace.options_for(txid), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(def, &rows[..count], arena)
}

pub(crate) fn tablespace_location_by_oid<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    if oid == 1663 || oid == 1664 {
        return Ok(Some(""));
    }
    for (_, tablespace) in storage.tablespaces_visible_to(txid) {
        if tablespace_oid(*tablespace) == oid {
            return arena
                .alloc_str(tablespace.location.as_str())
                .map(Some)
                .map_err(|_| arena_full());
        }
    }
    Ok(None)
}

pub(crate) fn tablespace_name_by_oid<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let builtin = match oid {
        1663 => Some("pg_default"),
        1664 => Some("pg_global"),
        _ => None,
    };
    if let Some(name) = builtin {
        return Ok(Some(name));
    }
    for (_, tablespace) in storage.tablespaces_visible_to(txid) {
        if tablespace_oid(*tablespace) == oid {
            return arena
                .alloc_str(tablespace.name_for(txid).as_str())
                .map(Some)
                .map_err(|_| arena_full());
        }
    }
    Ok(None)
}

fn index_reloptions<'a>(
    definition: Option<crate::storage::IndexMutableDefinition>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let Some(definition) = definition else {
        return Ok(Datum::Null);
    };
    let mut values = [Datum::Null; 2];
    let mut count = 0;
    if let Some(fillfactor) = definition.options.fillfactor {
        values[count] = text(stack_format!(32, "fillfactor={fillfactor}").as_str(), arena)?;
        count += 1;
    }
    if let Some(deduplicate) = definition.options.deduplicate_items {
        values[count] = text(
            if deduplicate {
                "deduplicate_items=on"
            } else {
                "deduplicate_items=off"
            },
            arena,
        )?;
        count += 1;
    }
    if count == 0 {
        Ok(Datum::Null)
    } else {
        Ok(Datum::Array {
            element: super::types::ArrElem::Text,
            raw: super::array::build(&values[..count], arena)?,
        })
    }
}

fn pg_constraint<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_constraint",
        &[
            ("oid", ColType::Int4),
            ("conname", ColType::Text),
            ("conrelid", ColType::Int4),
            ("contypid", ColType::Int4),
            ("contype", ColType::Bpchar),
            ("conparentid", ColType::Int4),
            ("conindid", ColType::Int4),
            ("confrelid", ColType::Int4),
            ("condeferrable", ColType::Bool),
            ("condeferred", ColType::Bool),
            ("conenforced", ColType::Bool),
            ("convalidated", ColType::Bool),
            ("conperiod", ColType::Bool),
            ("confupdtype", ColType::Bpchar),
            ("confdeltype", ColType::Bpchar),
            ("conkey", ColType::Array(super::types::ArrElem::Int4)),
            ("confkey", ColType::Array(super::types::ArrElem::Int4)),
            ("tableoid", ColType::Int4),
            ("connamespace", ColType::Int4),
            ("confmatchtype", ColType::Bpchar),
            ("conislocal", ColType::Bool),
            ("coninhcount", ColType::Int4),
            ("connoinherit", ColType::Bool),
            ("conpfeqop", ColType::Array(super::types::ArrElem::Int4)),
            ("conppeqop", ColType::Array(super::types::ArrElem::Int4)),
            ("conffeqop", ColType::Array(super::types::ArrElem::Int4)),
            (
                "confdelsetcols",
                ColType::Array(super::types::ArrElem::Int4),
            ),
            ("conexclop", ColType::Array(super::types::ArrElem::Int4)),
            ("conbin", ColType::Text),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let mut out: [&[Datum]; 512] = [&[]; 512];
    let mut n = 0;
    // A PRIMARY KEY or UNIQUE constraint has a backing index; its `conindid`
    // links to that index so psql's `\d` labels a UNIQUE index as a constraint.
    for info in indexes {
        if !info.is_constraint {
            continue;
        }
        let contype = if info.is_exclusion {
            "x"
        } else if info.is_primary {
            "p"
        } else if info.is_unique {
            "u"
        } else {
            continue;
        };
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_constraint"));
        }
        out[n] = row(
            &[
                Datum::Int4(info.oid + 500_000), // constraint oid, distinct from the index's
                text(info.name.as_str(), arena)?,
                Datum::Int4(info.table_oid),
                Datum::Int4(0),
                text(contype, arena)?,
                Datum::Int4(info.constraint_parent_oid),
                Datum::Int4(info.oid), // conindid -> the backing index
                Datum::Int4(0),        // confrelid
                Datum::Bool(info.timing.is_deferrable()),
                Datum::Bool(info.timing.initially_deferred()),
                Datum::Bool(true),
                Datum::Bool(true),  // convalidated
                Datum::Bool(false), // conperiod
                text(" ", arena)?,  // confupdtype (n/a for non-FK)
                text(" ", arena)?,  // confdeltype
                attnum_array(&info.columns[..info.n_cols], arena)?,
                empty_int_array(arena)?,
                Datum::Int4(2606),
                Datum::Int4(namespace_oid(
                    storage,
                    storage.table_def(info.table_slot, txid).schema.as_str(),
                )),
                text(" ", arena)?,
                Datum::Bool(info.constraint_parent_oid == 0),
                Datum::Int4(i32::from(info.constraint_parent_oid != 0)),
                Datum::Bool(false),
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                if info.is_exclusion {
                    let table = storage.table_def(info.table_slot, txid);
                    let exclusion = table
                        .exclusions()
                        .iter()
                        .find(|exclusion| exclusion.name.as_str() == info.name.as_str())
                        .expect("exclusion index has its constraint");
                    exclusion_operator_array(&exclusion.operators[..exclusion.n_cols], arena)?
                } else {
                    empty_int_array(arena)?
                },
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // Foreign-key constraints (contype 'f'): conrelid on the child, confrelid on
    // the referenced parent, so psql's "Foreign-key constraints" (child) and
    // "Referenced by" (parent) sections both resolve.
    let fks = collect_fkeys(storage, txid, arena)?;
    for info in fks {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_constraint"));
        }
        let fk = &storage.table_def(info.child_slot, txid).fkeys()[info.fk_index];
        let constraint_parent_oid =
            inherited_foreign_key_parent_oid(storage, txid, info.child_slot, fk);
        // conindid points at the parent's unique/PK index backing the referenced
        // columns, which JDBC joins to for foreign-key metadata.
        let conindid = indexes
            .iter()
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
                Datum::Int4(0),
                text("f", arena)?,
                Datum::Int4(constraint_parent_oid),
                Datum::Int4(conindid),
                Datum::Int4(info.confrelid),
                Datum::Bool(fk.timing.is_deferrable()),
                Datum::Bool(fk.timing.initially_deferred()),
                Datum::Bool(fk.validation.enforced()),
                Datum::Bool(fk.validation.validated()),
                Datum::Bool(false),
                text(fk_action_char(fk.on_update), arena)?,
                text(fk_action_char(fk.on_delete), arena)?,
                attnum_array(&fk.columns[..fk.n_cols], arena)?,
                attnum_array(&fk.parent_cols[..fk.n_parent_cols], arena)?,
                Datum::Int4(2606),
                Datum::Int4(namespace_oid(
                    storage,
                    storage.table_def(info.child_slot, txid).schema.as_str(),
                )),
                text("s", arena)?,
                Datum::Bool(constraint_parent_oid == 0),
                Datum::Int4(i32::from(constraint_parent_oid != 0)),
                Datum::Bool(false),
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // User constraint triggers have a pg_constraint identity linked from
    // pg_trigger.tgconstraint. They do not own an index.
    for (_, trigger) in storage.triggers_with_slots_visible_to(txid) {
        let crate::storage::TriggerKind::Constraint { timing, .. } = trigger.kind else {
            continue;
        };
        let crate::storage::TriggerTarget::Table(table) = trigger.target else {
            continue;
        };
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_constraint"));
        }
        let table = usize::from(table);
        let root_row = n;
        out[n] = row(
            &[
                Datum::Int4(crate::storage::trigger_oid(&trigger) + 500_000),
                text(trigger.name_to(txid).as_str(), arena)?,
                Datum::Int4(table_oid(storage, table)),
                Datum::Int4(0),
                text("t", arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Bool(timing.is_deferrable()),
                Datum::Bool(timing.initially_deferred()),
                Datum::Bool(true),
                Datum::Bool(true),
                Datum::Bool(false),
                text(" ", arena)?,
                text(" ", arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                Datum::Int4(2606),
                Datum::Int4(namespace_oid(
                    storage,
                    storage.table_def(table, txid).schema.as_str(),
                )),
                text(" ", arena)?,
                Datum::Bool(true),
                Datum::Int4(0),
                Datum::Bool(false),
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
        if !matches!(trigger.level, crate::sql::ast::TriggerLevel::Row) {
            continue;
        }
        for child in 0..storage.table_count() {
            if !storage.table_slot_visible_to(child, txid)
                || !storage.partition_descends_from(child, table, txid)
            {
                continue;
            }
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_constraint"));
            }
            let mut clone = [Datum::Null; 29];
            clone.copy_from_slice(out[root_row]);
            clone[0] = Datum::Int4(partition_trigger_oid(&trigger, child)? + 500_000);
            clone[2] = Datum::Int4(table_oid(storage, child));
            clone[18] = Datum::Int4(namespace_oid(
                storage,
                storage.table_def(child, txid).schema.as_str(),
            ));
            out[n] = row(&clone, arena)?;
            n += 1;
        }
    }
    // CHECK constraints are catalog objects too. Their source predicate is
    // preserved by the table definition and reconstructed by
    // pg_get_constraintdef, which is what psql's "Check constraints" section
    // reads.
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        for (check_index, check) in storage.table_def(slot, txid).checks().iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_constraint"));
            }
            let inheritance_count = check_inheritance_count(storage, txid, slot, check);
            let inherited = inheritance_count != 0;
            out[n] = row(
                &[
                    Datum::Int4(
                        FIRST_CHECK_OID
                            + slot as i32 * crate::storage::MAX_CHECKS as i32
                            + check_index as i32,
                    ),
                    text(check.name.as_str(), arena)?,
                    Datum::Int4(table_oid(storage, slot)),
                    Datum::Int4(0),
                    text("c", arena)?,
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    Datum::Bool(false),
                    Datum::Bool(check.validation.enforced()),
                    Datum::Bool(check.validation.validated()),
                    Datum::Bool(false),
                    text(" ", arena)?,
                    text(" ", arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Int4(2606),
                    Datum::Int4(namespace_oid(
                        storage,
                        storage.table_def(slot, txid).schema.as_str(),
                    )),
                    text(" ", arena)?,
                    Datum::Bool(!inherited),
                    Datum::Int4(inheritance_count as i32),
                    Datum::Bool(false),
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let Some(constraint) = table.partition.detached_bound else {
            continue;
        };
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_constraint"));
        }
        out[n] = row(
            &[
                Datum::Int4(FIRST_DETACHED_PARTITION_CHECK_OID + slot as i32),
                text(constraint.name.as_str(), arena)?,
                Datum::Int4(table_oid(storage, slot)),
                Datum::Int4(0),
                text("c", arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(true),
                Datum::Bool(false),
                text(" ", arena)?,
                text(" ", arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                Datum::Int4(2606),
                Datum::Int4(namespace_oid(storage, table.schema.as_str())),
                text(" ", arena)?,
                Datum::Bool(true),
                Datum::Int4(0),
                Datum::Bool(false),
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                empty_int_array(arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // PostgreSQL 18 represents NOT NULL constraints in pg_constraint as well
    // as pg_attribute. pg_dump uses these rows to preserve the constraint
    // before adding an identity property in its dependency-ordered output.
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for (column_index, column) in table.columns().iter().enumerate() {
            if !column.not_null.is_required() {
                continue;
            }
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "pg_constraint result exceeds static capacity"
                ));
            }
            let constraint_name = not_null_constraint_name(table, column);
            out[n] = row(
                &[
                    Datum::Int4(
                        FIRST_NOT_NULL_OID + slot as i32 * MAX_COLUMNS as i32 + column_index as i32,
                    ),
                    text(constraint_name.as_str(), arena)?,
                    Datum::Int4(table_oid(storage, slot)),
                    Datum::Int4(0),
                    text("n", arena)?,
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    Datum::Bool(false),
                    Datum::Bool(true),
                    Datum::Bool(true),
                    Datum::Bool(false),
                    text(" ", arena)?,
                    text(" ", arena)?,
                    attnum_array(&[column_index as u16], arena)?,
                    empty_int_array(arena)?,
                    Datum::Int4(2606),
                    Datum::Int4(namespace_oid(storage, table.schema.as_str())),
                    text(" ", arena)?,
                    Datum::Bool(column.not_null.is_local()),
                    Datum::Int4(i32::from(column.not_null.is_inherited())),
                    Datum::Bool(false),
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    // Domain CHECK constraints are attached to `contypid`, not a table.
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        for (check_index, check) in domain.checks().iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_constraint"));
            }
            out[n] = row(
                &[
                    Datum::Int4(
                        FIRST_DOMAIN_CHECK_OID
                            + slot as i32 * crate::storage::MAX_DOMAIN_CHECKS as i32
                            + check_index as i32,
                    ),
                    text(check.name.as_str(), arena)?,
                    Datum::Int4(0),
                    Datum::Int4(domain_oid(slot)),
                    text("c", arena)?,
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    Datum::Bool(false),
                    Datum::Bool(check.validation.enforced()),
                    Datum::Bool(check.validation.validated()),
                    Datum::Bool(false),
                    text(" ", arena)?,
                    text(" ", arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Int4(2606),
                    Datum::Int4(namespace_oid(storage, domain.schema.as_str())),
                    text(" ", arena)?,
                    Datum::Bool(true),
                    Datum::Int4(0),
                    Datum::Bool(false),
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    empty_int_array(arena)?,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn pg_rewrite<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_rewrite",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("rulename", ColType::Text),
            ("ev_class", ColType::Int4),
            ("ev_type", ColType::Bpchar),
            ("ev_enabled", ColType::Bpchar),
            ("is_instead", ColType::Bool),
        ],
    );
    let count = storage.rules_visible_to(txid).count();
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (_, rule)) in storage.rules_visible_to(txid).enumerate() {
        let definition = rule.definition_for(txid);
        let relation_oid = match definition.target {
            crate::storage::RuleTarget::Table(slot) => table_oid(storage, usize::from(slot)),
            crate::storage::RuleTarget::View(slot) => view_oid(usize::from(slot)),
        };
        let event = [definition.event.catalog_code()];
        let event = core::str::from_utf8(&event).expect("catalog event code is ASCII");
        out[index] = row(
            &[
                Datum::Int4(2618),
                Datum::Int4(rule.oid()),
                text(definition.name.as_str(), arena)?,
                Datum::Int4(relation_oid),
                text(event, arena)?,
                text("O", arena)?,
                Datum::Bool(matches!(
                    definition.mode,
                    crate::storage::RewriteMode::Instead
                )),
            ],
            arena,
        )?;
    }
    finish(def, out, arena)
}

fn available_extension_requires<'a>(
    package: &crate::storage::ExtensionPackage,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; crate::storage::MAX_EXTENSION_REQUIRES];
    for (index, required) in package.requires().iter().enumerate() {
        values[index] = text(required.as_str(), arena)?;
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Name,
        raw: super::array::build(&values[..package.requires().len()], arena)?,
    })
}

fn pg_available_extensions<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_available_extensions",
        &[
            ("name", ColType::Name),
            ("default_version", ColType::Text),
            ("installed_version", ColType::Text),
            ("comment", ColType::Text),
        ],
    );
    let count = storage.extension_packages().count();
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (_, package)) in storage.extension_packages().enumerate() {
        let installed = storage
            .extension_slot(package.name.as_str(), txid)
            .map(|slot| storage.extension(slot).definition_to(txid).2);
        out[index] = row(
            &[
                text(package.name.as_str(), arena)?,
                match package.default_version {
                    Some(version) => text(version.as_str(), arena)?,
                    None => Datum::Null,
                },
                match installed {
                    Some(version) => text(version.as_str(), arena)?,
                    None => Datum::Null,
                },
                if package.comment.as_str().is_empty() {
                    Datum::Null
                } else {
                    text(package.comment.as_str(), arena)?
                },
            ],
            arena,
        )?;
    }
    finish(def, out, arena)
}

fn pg_available_extension_versions<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_available_extension_versions",
        &[
            ("name", ColType::Name),
            ("version", ColType::Text),
            ("installed", ColType::Bool),
            ("superuser", ColType::Bool),
            ("trusted", ColType::Bool),
            ("relocatable", ColType::Bool),
            ("schema", ColType::Name),
            ("requires", ColType::Array(super::types::ArrElem::Name)),
            ("comment", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut emitted: [Option<(usize, crate::storage::ExtensionVersion)>; 256] = [None; 256];
    let mut count = 0usize;
    for (package_slot, package) in storage.extension_packages() {
        for (_, script) in storage.extension_scripts_for(package_slot) {
            if emitted[..count].contains(&Some((package_slot, script.to))) {
                continue;
            }
            if count == out.len() {
                return Err(catalog_capacity_exceeded("pg_available_extension_versions"));
            }
            emitted[count] = Some((package_slot, script.to));
            let effective = script.effective;
            let installed = storage
                .extension_slot(package.name.as_str(), txid)
                .is_some_and(|slot| storage.extension(slot).definition_to(txid).2 == script.to);
            out[count] = row(
                &[
                    text(package.name.as_str(), arena)?,
                    text(script.to.as_str(), arena)?,
                    Datum::Bool(installed),
                    Datum::Bool(effective.superuser),
                    Datum::Bool(effective.trusted),
                    Datum::Bool(effective.relocatable),
                    match effective.schema {
                        Some(schema) => text(schema.as_str(), arena)?,
                        None => Datum::Null,
                    },
                    available_extension_requires(&effective, arena)?,
                    if effective.comment.as_str().is_empty() {
                        Datum::Null
                    } else {
                        text(effective.comment.as_str(), arena)?
                    },
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(def, &out[..count], arena)
}

fn pg_extension<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_extension",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("extname", ColType::Name),
            ("extowner", ColType::Int4),
            ("extnamespace", ColType::Int4),
            ("extrelocatable", ColType::Bool),
            ("extversion", ColType::Text),
            ("extconfig", ColType::Array(super::types::ArrElem::Oid)),
            ("extcondition", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let count = storage.extensions_visible_to(txid).count();
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (slot, extension)) in storage.extensions_visible_to(txid).enumerate() {
        let (namespace, relocatable, version) = extension.definition_to(txid);
        let mut config_oids = [Datum::Null; crate::storage::MAX_EXTENSION_CONFIG_RELATIONS];
        let mut config_conditions = [Datum::Null; crate::storage::MAX_EXTENSION_CONFIG_RELATIONS];
        let mut configs = [None; crate::storage::MAX_EXTENSION_CONFIG_RELATIONS];
        let mut config_count = 0usize;
        for (_, config) in storage.extension_configs_visible_to(txid) {
            if config.extension as usize != slot {
                continue;
            }
            configs[config_count] = Some(*config);
            config_count += 1;
        }
        for index in 1..config_count {
            let config = configs[index].expect("filled extension configuration slot");
            let mut insertion = index;
            while insertion > 0
                && configs[insertion - 1]
                    .expect("filled extension configuration slot")
                    .ordinal
                    > config.ordinal
            {
                configs[insertion] = configs[insertion - 1];
                insertion -= 1;
            }
            configs[insertion] = Some(config);
        }
        for (index, config) in configs[..config_count].iter().enumerate() {
            let config = config.expect("sorted extension configuration slot");
            let oid = extension_config_relation_oid(config.relation);
            config_oids[index] = Datum::Oid(oid as u32);
            config_conditions[index] = text(config.condition_to(txid).as_str(), arena)?;
        }
        let extconfig = if config_count == 0 {
            Datum::Null
        } else {
            Datum::Array {
                element: super::types::ArrElem::Oid,
                raw: super::array::build(&config_oids[..config_count], arena)?,
            }
        };
        let extcondition = if config_count == 0 {
            Datum::Null
        } else {
            Datum::Array {
                element: super::types::ArrElem::Text,
                raw: super::array::build(&config_conditions[..config_count], arena)?,
            }
        };
        out[index] = row(
            &[
                Datum::Int4(3079),
                Datum::Int4(extension_oid(slot)),
                text(extension.name.as_str(), arena)?,
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Extension,
                    slot,
                    txid,
                )),
                Datum::Int4(namespace_oid(
                    storage,
                    storage.schema_def(namespace as usize).name.as_str(),
                )),
                Datum::Bool(relocatable),
                text(version.as_str(), arena)?,
                extconfig,
                extcondition,
            ],
            arena,
        )?;
    }
    finish(def, out, arena)
}

fn extension_dependency_catalog_identity(
    storage: &Storage,
    txid: u32,
    object: crate::storage::AccessObject,
) -> Option<(i32, i32)> {
    use crate::storage::AccessClass;
    let slot = object.slot as usize;
    Some(match object.class {
        AccessClass::Table => (PG_CLASS_OID, table_oid(storage, slot)),
        AccessClass::View => (PG_CLASS_OID, view_oid(slot)),
        AccessClass::MaterializedView => {
            let table = storage.matview_table(slot);
            if !storage.table_slot_visible_to(table, txid) {
                return None;
            }
            (PG_CLASS_OID, table_oid(storage, table))
        }
        AccessClass::Sequence => (PG_CLASS_OID, sequence_oid(slot)),
        AccessClass::Domain => (PG_TYPE_OID, domain_oid(slot)),
        AccessClass::Enum => (PG_TYPE_OID, crate::sql::types::oid::enum_oid(object.slot)),
        AccessClass::Composite => (
            PG_TYPE_OID,
            crate::sql::types::oid::composite_oid(object.slot),
        ),
        AccessClass::Routine => (
            PG_PROC_OID,
            crate::storage::routine_oid(&storage.routine_for(slot, txid)),
        ),
        AccessClass::Index => {
            let index = storage.index_visible_to(slot, txid)?;
            (PG_CLASS_OID, explicit_index_oid(&index))
        }
        AccessClass::Schema => (
            PG_NAMESPACE_OID,
            namespace_oid(storage, storage.schema_def(slot).name.as_str()),
        ),
        AccessClass::Statistics => (3381, extended_statistics_oid(slot)),
        AccessClass::Tablespace => return None,
        AccessClass::Extension => (3079, extension_oid(slot)),
        AccessClass::Trigger => (2620, crate::storage::trigger_oid(storage.trigger(slot))),
        AccessClass::EventTrigger => (3466, storage.event_trigger(slot).oid()),
        AccessClass::Database => (1262, 5),
        AccessClass::LargeObject => (
            PG_LARGEOBJECT_OID,
            storage
                .large_objects_visible_to(txid)
                .find_map(|(candidate, object)| (candidate == slot).then_some(object.oid.get()))?
                as i32,
        ),
        AccessClass::ForeignDataWrapper => (2328, foreign_data_wrapper_oid(slot)),
        AccessClass::ForeignServer => (1417, foreign_server_oid(slot)),
        AccessClass::Language => (PG_LANGUAGE_OID, object.slot.into()),
    })
}

fn pg_depend<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_depend",
        &[
            ("classid", ColType::Oid),
            ("objid", ColType::Oid),
            ("objsubid", ColType::Int4),
            ("refclassid", ColType::Oid),
            ("refobjid", ColType::Oid),
            ("refobjsubid", ColType::Int4),
            ("deptype", ColType::Bpchar),
        ],
    );
    let mut out: [&[Datum]; 4096] = [&[]; 4096];
    let mut count = 0usize;
    let mut push = |class: i32,
                    object: i32,
                    referenced_class: i32,
                    referenced_object: i32,
                    referenced_subobject: i32,
                    dependency_type: &str|
     -> Result<(), SqlError> {
        if count == out.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "catalog dependency result exceeds static capacity"
            ));
        }
        out[count] = row(
            &[
                Datum::Int4(class),
                Datum::Int4(object),
                Datum::Int4(0),
                Datum::Int4(referenced_class),
                Datum::Int4(referenced_object),
                Datum::Int4(referenced_subobject),
                text(dependency_type, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };

    // Sequence ownership is how pg_dump distinguishes serial/identity
    // generators from independent sequences and orders them with the owning
    // column.
    for sequence_slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(sequence_slot, txid);
        if !storage.sequence_slot_visible_to(sequence_slot, txid) {
            continue;
        }
        let Some(owner) = sequence.owner else {
            continue;
        };
        let Some(table_slot) =
            storage.find_visible(owner.table_schema.as_str(), owner.table.as_str(), txid)
        else {
            continue;
        };
        let table = storage.table_def(table_slot, txid);
        let Some(column) = table.column_index(owner.column.as_str()) else {
            continue;
        };
        push(
            PG_CLASS_OID,
            sequence_oid(sequence_slot),
            PG_CLASS_OID,
            table_oid(storage, table_slot),
            column as i32 + 1,
            if table.columns()[column].is_identity {
                "i"
            } else {
                "a"
            },
        )?;
    }

    // A domain is ordered after its parent domain or direct user-defined base
    // type. pg_dump reads this graph to emit a restorable type definition.
    for domain_slot in 0..storage.domain_count() {
        let domain = storage.domain_for(domain_slot, txid);
        if !storage.domain_slot_visible_to(domain_slot, txid) {
            continue;
        }
        let typed_base = || match domain.base {
            ColType::Enum(slot) => Some(crate::sql::types::oid::enum_oid(slot)),
            ColType::Composite(slot) => Some(crate::sql::types::oid::composite_oid(slot)),
            _ => None,
        };
        let referenced_type = if let Some(parent) = domain.base_domain {
            storage
                .domain_identity_slot(parent.schema.as_str(), parent.name.as_str(), txid)
                .map(domain_oid)
        } else {
            domain
                .base_user_type
                .and_then(|base| {
                    storage
                        .enum_slot(base.schema.as_str(), base.name.as_str(), txid)
                        .map(|slot| crate::sql::types::oid::enum_oid(slot as u16))
                        .or_else(|| {
                            composite_type_oid(
                                storage,
                                base.schema.as_str(),
                                base.name.as_str(),
                                txid,
                            )
                        })
                })
                .or_else(typed_base)
        };
        if let Some(referenced_type) = referenced_type {
            push(
                PG_TYPE_OID,
                domain_oid(domain_slot),
                PG_TYPE_OID,
                referenced_type,
                0,
                "n",
            )?;
        }
    }

    for (_, cast) in storage.casts_visible_to(txid) {
        let source = catalog_routine_result_oid(storage, txid, cast.source)?;
        let target = catalog_routine_result_oid(storage, txid, cast.target)?;
        for type_oid in [source, target] {
            if ColType::from_oid(type_oid).is_none() {
                push(PG_CAST_OID, cast.oid(), PG_TYPE_OID, type_oid, 0, "n")?;
            }
        }
        if let crate::storage::CastMethod::Function(function) = cast.method {
            push(PG_CAST_OID, cast.oid(), PG_PROC_OID, function, 0, "n")?;
        }
    }

    for (slot, operator) in storage.operators_visible_to(txid) {
        let oid = storage.operator(slot).oid();
        push(
            PG_OPERATOR_OID,
            oid,
            PG_NAMESPACE_OID,
            namespace_oid(storage, operator.schema.as_str()),
            0,
            "n",
        )?;
        if let Some(function) = operator.implementation.routine() {
            push(PG_OPERATOR_OID, oid, PG_PROC_OID, function, 0, "n")?;
        }
        for argument in [operator.signature.left, operator.signature.right]
            .into_iter()
            .flatten()
        {
            let type_oid = catalog_routine_result_oid(storage, txid, argument)?;
            if ColType::from_oid(type_oid).is_none() {
                push(PG_OPERATOR_OID, oid, PG_TYPE_OID, type_oid, 0, "n")?;
            }
        }
    }

    for (slot, collation) in storage.collations_visible_to(txid) {
        push(
            PG_COLLATION_OID,
            storage.collation(slot).oid(slot),
            PG_NAMESPACE_OID,
            namespace_oid(storage, collation.schema.as_str()),
            0,
            "n",
        )?;
    }
    for (slot, conversion) in storage.conversions_visible_to(txid) {
        let oid = 21_000 + slot as i32;
        push(
            PG_CONVERSION_OID,
            oid,
            PG_NAMESPACE_OID,
            namespace_oid(storage, conversion.schema.as_str()),
            0,
            "n",
        )?;
        push(
            PG_CONVERSION_OID,
            oid,
            PG_PROC_OID,
            conversion.procedure,
            0,
            "n",
        )?;
    }

    for (slot, family) in storage.operator_families_visible_to(txid) {
        push(
            PG_OPFAMILY_OID,
            storage.operator_family(slot).oid(),
            PG_NAMESPACE_OID,
            namespace_oid(storage, family.schema.as_str()),
            0,
            "n",
        )?;
    }

    for (class_slot, class) in storage.operator_classes_visible_to(txid) {
        let class_oid = storage.operator_class(class_slot).oid();
        push(
            PG_OPCLASS_OID,
            class_oid,
            PG_NAMESPACE_OID,
            namespace_oid(storage, class.schema.as_str()),
            0,
            "n",
        )?;
        push(
            PG_OPCLASS_OID,
            class_oid,
            PG_OPFAMILY_OID,
            class.family,
            0,
            "a",
        )?;
        for type_definition in [class.input, class.storage] {
            let type_oid = catalog_routine_result_oid(storage, txid, type_definition)?;
            if ColType::from_oid(type_oid).is_none() {
                push(PG_OPCLASS_OID, class_oid, PG_TYPE_OID, type_oid, 0, "n")?;
            }
        }
        let family_slot = storage
            .operator_family_slot_by_oid(class.family, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "operator class family identity is unavailable"
                )
            })?;
        let family = storage.operator_family_for(family_slot, txid);
        for member in class.operators.into_iter().filter(|member| member.used) {
            let Some((member_index, _)) = family
                .operators
                .iter()
                .enumerate()
                .find(|(_, candidate)| **candidate == member)
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "operator class member identity is unavailable"
                ));
            };
            let member_oid = operator_family_member_oid(class.family, member_index);
            push(PG_AMOP_OID, member_oid, PG_OPCLASS_OID, class_oid, 0, "i")?;
            push(
                PG_AMOP_OID,
                member_oid,
                PG_OPERATOR_OID,
                member.operator,
                0,
                "n",
            )?;
        }
        for member in class.functions.into_iter().filter(|member| member.used) {
            let Some((member_index, _)) = family
                .functions
                .iter()
                .enumerate()
                .find(|(_, candidate)| **candidate == member)
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "operator class support-function identity is unavailable"
                ));
            };
            let member_oid = operator_family_member_oid(class.family, member_index);
            push(PG_AMPROC_OID, member_oid, PG_OPCLASS_OID, class_oid, 0, "i")?;
            push(
                PG_AMPROC_OID,
                member_oid,
                PG_PROC_OID,
                member.function,
                0,
                "n",
            )?;
        }
    }
    for (family_slot, family) in storage.operator_families_visible_to(txid) {
        let family_oid = storage.operator_family(family_slot).oid();
        for (member_index, member) in family
            .operators
            .iter()
            .enumerate()
            .filter(|(_, member)| member.used)
        {
            let class_owned = storage
                .operator_classes_visible_to(txid)
                .any(|(_, class)| class.family == family_oid && class.operators.contains(member));
            if class_owned {
                continue;
            }
            let member_oid = operator_family_member_oid(family_oid, member_index);
            push(PG_AMOP_OID, member_oid, PG_OPFAMILY_OID, family_oid, 0, "a")?;
            push(
                PG_AMOP_OID,
                member_oid,
                PG_OPERATOR_OID,
                member.operator,
                0,
                "a",
            )?;
        }
        for (member_index, member) in family
            .functions
            .iter()
            .enumerate()
            .filter(|(_, member)| member.used)
        {
            let class_owned = storage
                .operator_classes_visible_to(txid)
                .any(|(_, class)| class.family == family_oid && class.functions.contains(member));
            if class_owned {
                continue;
            }
            let member_oid = operator_family_member_oid(family_oid, member_index);
            push(
                PG_AMPROC_OID,
                member_oid,
                PG_OPFAMILY_OID,
                family_oid,
                0,
                "a",
            )?;
            push(
                PG_AMPROC_OID,
                member_oid,
                PG_PROC_OID,
                member.function,
                0,
                "a",
            )?;
        }
    }

    for index_slot in 0..storage.index_count() {
        let Some(index) = storage.index_visible_to(index_slot, txid) else {
            continue;
        };
        for position in 0..index.n_cols {
            let Some(crate::storage::IndexOperatorClass::Catalog(class_oid)) =
                index.resolved_operator_classes[position]
            else {
                continue;
            };
            if index.resolved_operator_classes[..position].contains(&Some(
                crate::storage::IndexOperatorClass::Catalog(class_oid),
            )) {
                continue;
            }
            push(
                PG_CLASS_OID,
                explicit_index_oid(&index),
                PG_OPCLASS_OID,
                class_oid.get(),
                0,
                "n",
            )?;
        }
    }

    let referenced_oid = |dependency: &crate::storage::StoredQueryDependency| match dependency.class
    {
        crate::storage::DependencyClass::Table => {
            Some((PG_CLASS_OID, table_oid(storage, dependency.slot as usize)))
        }
        crate::storage::DependencyClass::View => {
            Some((PG_CLASS_OID, view_oid(dependency.slot as usize)))
        }
        crate::storage::DependencyClass::Sequence => {
            Some((PG_CLASS_OID, sequence_oid(dependency.slot as usize)))
        }
        crate::storage::DependencyClass::Domain => {
            Some((PG_TYPE_OID, domain_oid(dependency.slot as usize)))
        }
        crate::storage::DependencyClass::Enum => Some((
            PG_TYPE_OID,
            crate::sql::types::oid::enum_oid(dependency.slot),
        )),
        crate::storage::DependencyClass::Composite => Some((
            PG_TYPE_OID,
            crate::sql::types::oid::composite_oid(dependency.slot),
        )),
        crate::storage::DependencyClass::Routine => Some((
            PG_PROC_OID,
            match dependency.identity {
                crate::storage::StoredDependencyIdentity::RoutineOid(oid) => oid,
                crate::storage::StoredDependencyIdentity::Name
                | crate::storage::StoredDependencyIdentity::OperatorOid(_) => return None,
            },
        )),
        crate::storage::DependencyClass::Operator => Some((
            PG_OPERATOR_OID,
            match dependency.identity {
                crate::storage::StoredDependencyIdentity::OperatorOid(oid) => oid,
                crate::storage::StoredDependencyIdentity::Name
                | crate::storage::StoredDependencyIdentity::RoutineOid(_) => return None,
            },
        )),
        crate::storage::DependencyClass::Collation => Some((
            PG_COLLATION_OID,
            crate::sql::ast::Collation::Catalog(dependency.slot as u8).oid(),
        )),
        crate::storage::DependencyClass::TextSearchConfiguration => Some((
            PG_TS_CONFIG_OID,
            storage
                .text_search_object(dependency.slot as usize)
                .definition_for(txid)
                .oid(),
        )),
    };
    for (view_slot, _) in storage.views_visible_to(txid) {
        let rewrite_oid = storage.rule(storage.view_return_rule(view_slot)).oid();
        push(2618, rewrite_oid, PG_CLASS_OID, view_oid(view_slot), 0, "i")?;
        for dependency in storage.view_dependencies(view_slot).entries() {
            let Some((referenced_class, referenced_object)) = referenced_oid(dependency) else {
                continue;
            };
            if dependency.referenced_columns == 0 {
                push(
                    2618,
                    rewrite_oid,
                    referenced_class,
                    referenced_object,
                    0,
                    "n",
                )?;
            } else {
                for column in 0..u64::BITS as usize {
                    if dependency.referenced_columns & (1u64 << column) != 0 {
                        push(
                            2618,
                            rewrite_oid,
                            referenced_class,
                            referenced_object,
                            column as i32 + 1,
                            "n",
                        )?;
                    }
                }
            }
        }
    }
    for (_, rule) in storage.rules_visible_to(txid) {
        let definition = rule.definition_for(txid);
        if matches!(definition.event, crate::storage::RewriteEvent::Select) {
            continue;
        }
        let relation_oid = match definition.target {
            crate::storage::RuleTarget::Table(slot) => table_oid(storage, usize::from(slot)),
            crate::storage::RuleTarget::View(slot) => view_oid(usize::from(slot)),
        };
        push(2618, rule.oid(), PG_CLASS_OID, relation_oid, 0, "i")?;
        for dependency in definition.dependencies.entries() {
            let Some((referenced_class, referenced_object)) = referenced_oid(dependency) else {
                continue;
            };
            if dependency.referenced_columns == 0 {
                push(
                    2618,
                    rule.oid(),
                    referenced_class,
                    referenced_object,
                    0,
                    "n",
                )?;
            } else {
                for column in 0..u64::BITS as usize {
                    if dependency.referenced_columns & (1u64 << column) != 0 {
                        push(
                            2618,
                            rule.oid(),
                            referenced_class,
                            referenced_object,
                            column as i32 + 1,
                            "n",
                        )?;
                    }
                }
            }
        }
    }
    for (materialized_slot, _) in storage.matviews_visible_to(txid) {
        let table_slot = storage.matview_table(materialized_slot);
        for dependency in storage.matview_dependencies(materialized_slot).entries() {
            let Some((referenced_class, referenced_object)) = referenced_oid(dependency) else {
                continue;
            };
            push(
                PG_CLASS_OID,
                table_oid(storage, table_slot),
                referenced_class,
                referenced_object,
                0,
                "n",
            )?;
        }
    }
    for (_, trigger) in storage.triggers_with_slots_visible_to(txid) {
        let trigger_oid = crate::storage::trigger_oid(&trigger);
        let relation_oid = match trigger.target {
            crate::storage::TriggerTarget::Table(table) => table_oid(storage, usize::from(table)),
            crate::storage::TriggerTarget::View(view) => view_oid(usize::from(view)),
        };
        push(
            2620,
            trigger_oid,
            PG_PROC_OID,
            crate::storage::routine_oid(&storage.routine_for(usize::from(trigger.function), txid)),
            0,
            "n",
        )?;
        push(2620, trigger_oid, PG_CLASS_OID, relation_oid, 0, "a")?;
        if let crate::storage::TriggerKind::Constraint {
            referenced_table, ..
        } = trigger.kind
        {
            if let Some(referenced) = referenced_table {
                push(
                    2620,
                    trigger_oid,
                    PG_CLASS_OID,
                    table_oid(storage, usize::from(referenced)),
                    0,
                    "a",
                )?;
            }
            push(2606, trigger_oid + 500_000, 2620, trigger_oid, 0, "i")?;
        }
        for column in 0..MAX_COLUMNS {
            if trigger.update_columns & (1u64 << column) != 0 {
                push(
                    2620,
                    trigger_oid,
                    PG_CLASS_OID,
                    relation_oid,
                    column as i32 + 1,
                    "n",
                )?;
            }
        }

        let crate::storage::TriggerTarget::Table(parent_table) = trigger.target else {
            continue;
        };
        if !matches!(trigger.level, crate::sql::ast::TriggerLevel::Row) {
            continue;
        }
        let parent_table = usize::from(parent_table);
        for child in 0..storage.table_count() {
            if !storage.table_slot_visible_to(child, txid)
                || !storage.partition_descends_from(child, parent_table, txid)
            {
                continue;
            }
            let clone_oid = partition_trigger_oid(&trigger, child)?;
            let child_relation_oid = table_oid(storage, child);
            push(
                2620,
                clone_oid,
                PG_PROC_OID,
                crate::storage::routine_oid(
                    &storage.routine_for(usize::from(trigger.function), txid),
                ),
                0,
                "n",
            )?;
            push(2620, clone_oid, PG_CLASS_OID, child_relation_oid, 0, "a")?;
            if let crate::storage::TriggerKind::Constraint {
                referenced_table, ..
            } = trigger.kind
            {
                if let Some(referenced) = referenced_table {
                    push(
                        2620,
                        clone_oid,
                        PG_CLASS_OID,
                        table_oid(storage, usize::from(referenced)),
                        0,
                        "a",
                    )?;
                }
                push(2606, clone_oid + 500_000, 2620, clone_oid, 0, "i")?;
            }
            for column in 0..MAX_COLUMNS {
                if trigger.update_columns & (1u64 << column) != 0 {
                    push(
                        2620,
                        clone_oid,
                        PG_CLASS_OID,
                        child_relation_oid,
                        column as i32 + 1,
                        "n",
                    )?;
                }
            }
            let direct_parent = usize::from(
                storage
                    .table_def(child, txid)
                    .partition
                    .attachment
                    .expect("partition descendant has a parent")
                    .parent,
            );
            let parent_oid = if direct_parent == parent_table {
                trigger_oid
            } else {
                partition_trigger_oid(&trigger, direct_parent)?
            };
            push(2620, clone_oid, 2620, parent_oid, 0, "P")?;
            push(2620, clone_oid, PG_CLASS_OID, child_relation_oid, 0, "S")?;
        }
    }
    for (_, dependency) in storage.extension_dependencies_visible_to(txid) {
        let Some((class, object)) =
            extension_dependency_catalog_identity(storage, txid, dependency.object)
        else {
            continue;
        };
        push(
            class,
            object,
            3079,
            extension_oid(dependency.extension as usize),
            0,
            match dependency.kind {
                crate::storage::ExtensionDependencyKind::Member => "e",
                crate::storage::ExtensionDependencyKind::Automatic => "x",
                crate::storage::ExtensionDependencyKind::Required => "n",
            },
        )?;
    }
    finish(def, &out[..count], arena)
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

fn exclusion_operator_array<'a>(
    operators: &[crate::storage::ExclusionOperator],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (index, operator) in operators.iter().copied().enumerate() {
        values[index] = Datum::Int4(match operator {
            crate::storage::ExclusionOperator::Equal => 3882,
            crate::storage::ExclusionOperator::Overlaps => 3888,
            crate::storage::ExclusionOperator::Adjacent => 3897,
        });
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&values[..operators.len()], arena)?,
    })
}

fn pg_index<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_index",
        &[
            ("indexrelid", ColType::Int4),
            ("indrelid", ColType::Int4),
            ("indisprimary", ColType::Bool),
            ("indisunique", ColType::Bool),
            ("indisclustered", ColType::Bool),
            ("indischeckxmin", ColType::Bool),
            ("indisvalid", ColType::Bool),
            ("indimmediate", ColType::Bool),
            ("indisreplident", ColType::Bool),
            ("indnullsnotdistinct", ColType::Bool),
            ("indnatts", ColType::Int4),
            ("indnkeyatts", ColType::Int4),
            ("indkey", ColType::Int2Vector),
            ("indoption", ColType::Array(super::types::ArrElem::Int4)),
            ("indpred", ColType::Text),
            ("indisready", ColType::Bool),
            ("indislive", ColType::Bool),
            ("indexprs", ColType::Text),
            ("indcollation", ColType::Array(super::types::ArrElem::Int4)),
            ("indclass", ColType::OidVector),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let toast_indexes = (0..storage.table_count())
        .filter(|slot| {
            storage.table_slot_visible_to(*slot, txid) && storage.table_def(*slot, txid).has_toast
        })
        .count();
    let out = arena
        .alloc_slice_with(indexes.len() + toast_indexes + 2, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut n = 0;
    for info in indexes {
        // indkey is the 1-based attribute numbers as an int2vector-like array;
        // indoption is one flag per column (0 = default ascending).
        let mut attributes = [0u16; crate::storage::MAX_INDEX_COLS * 2];
        attributes[..info.n_cols].copy_from_slice(&info.columns[..info.n_cols]);
        for (position, attribute) in attributes.iter_mut().enumerate().take(info.n_cols) {
            if info.expression_keys[position] {
                // PostgreSQL reserves attribute number zero in `indkey` for
                // an expression key; the source lives in `indexprs`.
                *attribute = u16::MAX;
            }
        }
        attributes[info.n_cols..info.n_cols + info.n_include_cols]
            .copy_from_slice(&info.include_columns[..info.n_include_cols]);
        out[n] = row(
            &[
                Datum::Int4(info.oid),
                Datum::Int4(info.table_oid),
                Datum::Bool(info.is_primary),
                Datum::Bool(info.is_unique),
                Datum::Bool(
                    info.explicit_definition
                        .is_some_and(|definition| definition.clustered),
                ),
                Datum::Bool(false), // indischeckxmin
                Datum::Bool(
                    info.explicit_definition
                        .is_none_or(|definition| definition.kind.valid()),
                ),
                Datum::Bool(!info.timing.is_deferrable()),
                Datum::Bool(
                    info.explicit_definition
                        .is_some_and(|definition| definition.replica_identity),
                ),
                Datum::Bool(info.nulls_not_distinct),
                Datum::Int4((info.n_cols + info.n_include_cols) as i32),
                Datum::Int4(info.n_cols as i32),
                int2vector(&attributes[..info.n_cols + info.n_include_cols], arena)?,
                index_options(info, arena)?,
                match info.predicate {
                    Some(predicate) => text(predicate.as_str(), arena)?,
                    None => Datum::Null,
                },
                Datum::Bool(true),
                Datum::Bool(true),
                match (0..info.n_cols)
                    .find_map(|position| index_expression_source(storage, info, position, txid))
                {
                    Some(expression) => text(expression.as_str(), arena)?,
                    None => Datum::Null,
                },
                index_collations(storage, info, txid, arena)?,
                index_operator_classes(storage, info, txid, arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) || !storage.table_def(slot, txid).has_toast {
            continue;
        }
        let keys = [1_u16, 2];
        let options = [Datum::Int4(0), Datum::Int4(0)];
        let collations = [Datum::Oid(0), Datum::Oid(0)];
        let operator_classes = [1981_i32, 1978];
        out[n] = row(
            &[
                Datum::Int4(toast_index_oid(slot)),
                Datum::Int4(toast_relation_oid(slot)),
                Datum::Bool(true),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(2),
                Datum::Int4(2),
                int2vector(&keys, arena)?,
                Datum::Array {
                    element: super::types::ArrElem::Int4,
                    raw: super::array::build(&options, arena)?,
                },
                Datum::Null,
                Datum::Bool(true),
                Datum::Bool(true),
                Datum::Null,
                Datum::Array {
                    element: super::types::ArrElem::Oid,
                    raw: super::array::build(&collations, arena)?,
                },
                oidvector(&operator_classes, arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    for (index_oid, relation_oid, attributes, operator_classes) in [
        (2683, 2613, &[0_u16, 1][..], &[1981_i32, 1978][..]),
        (
            2996,
            PG_LARGEOBJECT_METADATA_OID,
            &[0_u16][..],
            &[1981_i32][..],
        ),
    ] {
        let options = [Datum::Int4(0), Datum::Int4(0)];
        let collations = [Datum::Int4(0), Datum::Int4(0)];
        out[n] = row(
            &[
                Datum::Int4(index_oid),
                Datum::Int4(relation_oid),
                Datum::Bool(true),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(attributes.len() as i32),
                Datum::Int4(attributes.len() as i32),
                int2vector(attributes, arena)?,
                Datum::Array {
                    element: super::types::ArrElem::Int4,
                    raw: super::array::build(&options[..attributes.len()], arena)?,
                },
                Datum::Null,
                Datum::Bool(true),
                Datum::Bool(true),
                Datum::Null,
                Datum::Array {
                    element: super::types::ArrElem::Int4,
                    raw: super::array::build(&collations[..attributes.len()], arena)?,
                },
                oidvector(operator_classes, arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

/// An index-option array (one 0-flag per column) for `pg_index.indoption`.
fn index_options<'a>(info: &IdxInfo, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let mut vals = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (position, value) in vals.iter_mut().enumerate().take(info.n_cols) {
        *value = Datum::Int4(
            i32::from(info.descending[position]) | (i32::from(info.nulls_first[position]) << 1),
        );
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&vals[..info.n_cols], arena)?,
    })
}

fn index_collations<'a>(
    storage: &Storage,
    info: &IdxInfo,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut values = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    let table = storage.table_def(info.table_slot, txid);
    for (position, value) in values.iter_mut().enumerate().take(info.n_cols) {
        let collation = if info.explicit_definition.is_some() {
            info.collations[position]
        } else if info.expression_keys[position] {
            let source = index_expression_source(storage, info, position, txid)
                .expect("expression index has source");
            let expression = crate::sql::parser::parse_expr(source.as_str(), arena)?;
            let catalog = super::query::storage_catalog(storage, arena, txid);
            resolved_expression_collation(expression, &TableColumnTypes(table), Some(&catalog))?
        } else {
            table.columns()[info.columns[position] as usize].collation
        };
        *value = Datum::Int4(collation.oid());
    }
    Ok(Datum::Array {
        element: super::types::ArrElem::Int4,
        raw: super::array::build(&values[..info.n_cols], arena)?,
    })
}

fn index_operator_classes<'a>(
    storage: &Storage,
    info: &IdxInfo,
    txid: u32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let table = storage.table_def(info.table_slot, txid);
    let mut values = [0i32; crate::storage::MAX_INDEX_COLS];
    for (position, value) in values.iter_mut().enumerate().take(info.n_cols) {
        *value = if let Some(operator_class) = info.resolved_operator_classes[position] {
            operator_class.oid()
        } else if info.expression_keys[position] {
            let source = index_expression_source(storage, info, position, txid)
                .expect("expression index has source");
            let expression = crate::sql::parser::parse_expr(source.as_str(), arena)?;
            let (oid, _) = super::exec::infer_type_catalog(expression, Some(table), storage, txid)?;
            super::exec::catalog_column_type(storage, txid, oid)
                .map(|(ctype, _)| ctype)
                .and_then(crate::sql::types::BtreeOperatorClass::for_type)
                .map_or(0, crate::sql::types::BtreeOperatorClass::oid)
        } else {
            crate::sql::types::BtreeOperatorClass::for_type(
                table.columns()[info.columns[position] as usize].ctype,
            )
            .map_or(0, crate::sql::types::BtreeOperatorClass::oid)
        };
    }
    oidvector(&values[..info.n_cols], arena)
}

/// Type and collation lookup for a stored table definition.  Catalog
/// projection must derive expression-index metadata from the same source as
/// executor analysis without pretending that a row is available.
struct TableColumnTypes<'a>(&'a TableDef);

impl<'a> ColumnLookup<'a> for TableColumnTypes<'_> {
    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }

    fn col_type(&self, _qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.0
            .columns()
            .iter()
            .find(|column| column.name.as_str().eq_ignore_ascii_case(name))
            .map(|column| column.ctype)
    }

    fn collation(&self, _qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        self.0
            .columns()
            .iter()
            .find(|column| column.name.as_str().eq_ignore_ascii_case(name))
            .map(|column| column.collation)
            .unwrap_or(crate::sql::ast::Collation::None)
    }
}

fn int2vector<'a>(columns: &[u16], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let raw = arena
        .alloc_slice_with(columns.len() * 2, |_| 0u8)
        .map_err(|_| arena_full())?;
    for (index, column) in columns.iter().enumerate() {
        let attribute_number = if *column == u16::MAX {
            0
        } else {
            *column as i16 + 1
        };
        raw[index * 2..index * 2 + 2].copy_from_slice(&attribute_number.to_le_bytes());
    }
    Ok(Datum::Int2Vector(raw))
}

fn oidvector<'a>(oids: &[i32], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let raw = arena
        .alloc_slice_with(oids.len() * 4, |_| 0u8)
        .map_err(|_| arena_full())?;
    for (index, oid) in oids.iter().enumerate() {
        raw[index * 4..index * 4 + 4].copy_from_slice(&oid.to_le_bytes());
    }
    Ok(Datum::OidVector(raw))
}

fn catalog_column_type_oid(
    storage: &Storage,
    column: &ColumnMeta,
    txid: u32,
) -> Result<i32, SqlError> {
    Ok(storage.declared_column_type(column, txid)?.catalog_oid())
}

fn catalog_column_type_mod(column: &ColumnMeta) -> i32 {
    if column.user_type.is_some() {
        -1
    } else {
        column.type_mod
    }
}

fn type_storage(ctype: ColType) -> &'static str {
    if matches!(ctype, ColType::TsQuery) || ctype.typlen() >= 0 {
        "p"
    } else {
        "x"
    }
}

fn pg_attribute<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
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
            ("attstorage", ColType::Bpchar),
            ("attcompression", ColType::Bpchar),
            ("attstattarget", ColType::Int4),
            ("attisdropped", ColType::Bool),
            ("attnum_ord", ColType::Int4),
            ("attalign", ColType::Bpchar),
            ("attislocal", ColType::Bool),
            ("attoptions", ColType::Array(super::types::ArrElem::Text)),
            ("attfdwoptions", ColType::Array(super::types::ArrElem::Text)),
            ("atthasmissing", ColType::Bool),
            ("attmissingval", ColType::Text),
            ("attacl", ColType::Array(super::types::ArrElem::AclItem)),
        ],
    );
    let mut out: [&[Datum]; 1024] = [&[]; 1024];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table_definition = storage.table_def(slot, txid);
        let relation = match storage.matview_slot(
            table_definition.schema.as_str(),
            table_definition.name.as_str(),
            txid,
        ) {
            Some(matview) => crate::storage::AccessObject {
                class: crate::storage::AccessClass::MaterializedView,
                slot: matview as u16,
            },
            None => crate::storage::AccessObject {
                class: crate::storage::AccessClass::Table,
                slot: slot as u16,
            },
        };
        for (i, c) in storage.table_def(slot, txid).columns().iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            let foreign_column_options =
                if let Some((_, binding)) = storage.foreign_table(slot as u16, txid) {
                    let mut options = crate::storage::foreign::ForeignOptions::EMPTY;
                    for option in binding.column_options.options_for(i as u16) {
                        options.restore_option(option.name.as_str(), option.value.as_str())?;
                    }
                    foreign_options_datum(&options, arena)?
                } else {
                    Datum::Null
                };
            out[n] = row(
                &[
                    Datum::Int4(table_oid(storage, slot)),
                    text(c.name.as_str(), arena)?,
                    Datum::Int4(catalog_column_type_oid(storage, c, txid)?),
                    Datum::Int4(i as i32 + 1),
                    Datum::Bool(c.not_null.is_required()),
                    Datum::Int4(i32::from(c.ctype.typlen())),
                    Datum::Int4(catalog_column_type_mod(c)),
                    Datum::Bool(!matches!(c.default, crate::storage::ColumnDefault::None)),
                    Datum::Int4(c.collation.oid()),
                    text(
                        if c.is_identity && c.identity_always {
                            "a"
                        } else if c.is_identity {
                            "d"
                        } else {
                            ""
                        },
                        arena,
                    )?, // attidentity: always 'a' / by default 'd'
                    text(if c.default.is_generated() { "s" } else { "" }, arena)?, // attgenerated
                    // Fixed-width values are plain; variable-width values use
                    // PostgreSQL's ordinary extended storage policy.
                    text(type_storage(c.ctype), arena)?,
                    text("", arena)?, // attcompression: type default
                    // PostgreSQL exposes its durable `-1` default sentinel as NULL.
                    if c.statistics_target < 0 {
                        Datum::Null
                    } else {
                        Datum::Int4(i32::from(c.statistics_target))
                    },
                    Datum::Bool(false), // attisdropped
                    Datum::Int4(i as i32 + 1),
                    text("i", arena)?,
                    Datum::Bool(true),
                    Datum::Null,
                    foreign_column_options,
                    Datum::Bool(false),
                    Datum::Null,
                    column_acl(
                        storage,
                        crate::storage::ColumnPrivilegeTarget::new(relation, i as u16)?,
                        txid,
                        arena,
                    )?,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) || !storage.table_def(slot, txid).has_toast {
            continue;
        }
        let attributes = [
            ("chunk_id", 26, 4),
            ("chunk_seq", 23, 4),
            ("chunk_data", 17, -1),
        ];
        for (relation_oid, count) in [
            (toast_relation_oid(slot), attributes.len()),
            (toast_index_oid(slot), 2),
        ] {
            for (attribute, (name, type_oid, type_length)) in attributes[..count].iter().enumerate()
            {
                if n == out.len() {
                    return Err(catalog_capacity_exceeded("pg_attribute"));
                }
                out[n] = row(
                    &[
                        Datum::Int4(relation_oid),
                        text(name, arena)?,
                        Datum::Int4(*type_oid),
                        Datum::Int4(attribute as i32 + 1),
                        Datum::Bool(false),
                        Datum::Int4(*type_length),
                        Datum::Int4(-1),
                        Datum::Bool(false),
                        Datum::Int4(0),
                        text("", arena)?,
                        text("", arena)?,
                        text("p", arena)?,
                        text("", arena)?,
                        Datum::Int4(-1),
                        Datum::Bool(false),
                        Datum::Int4(attribute as i32 + 1),
                        text("i", arena)?,
                        Datum::Bool(true),
                        Datum::Null,
                        Datum::Null,
                        Datum::Bool(false),
                        Datum::Null,
                        Datum::Null,
                    ],
                    arena,
                )?;
                n += 1;
            }
        }
    }
    // Standalone composite fields belong to the generated `pg_class` relation
    // named by their `pg_type.typrelid`, just as PostgreSQL does.
    for (slot, composite) in storage.composites_with_slots_visible_to(txid) {
        for field in composite.fields() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            let column = crate::storage::ColumnMeta {
                name: field.name,
                ctype: field.ctype,
                type_mod: field.type_mod,
                collation: field.collation,
                not_null: crate::storage::NotNullOrigin::Nullable,
                unique: false,
                primary: false,
                auto_increment: false,
                default: crate::storage::ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type: field.user_type,
                statistics_target: -1,
            };
            out[n] = row(
                &[
                    Datum::Int4(named_composite_relation_oid(slot)),
                    text(field.name.as_str(), arena)?,
                    if field.dropped {
                        Datum::Int4(0)
                    } else {
                        Datum::Int4(catalog_column_type_oid(storage, &column, txid)?)
                    },
                    Datum::Int4(i32::from(field.attribute_number)),
                    Datum::Bool(field.not_null),
                    if field.dropped {
                        Datum::Int4(0)
                    } else {
                        Datum::Int4(i32::from(field.ctype.typlen()))
                    },
                    if field.dropped {
                        Datum::Int4(-1)
                    } else {
                        Datum::Int4(catalog_column_type_mod(&column))
                    },
                    Datum::Bool(false),
                    if field.dropped {
                        Datum::Int4(0)
                    } else {
                        Datum::Int4(field.collation.oid())
                    },
                    text("", arena)?,
                    text("", arena)?,
                    text(
                        if field.dropped {
                            "p"
                        } else {
                            type_storage(field.ctype)
                        },
                        arena,
                    )?,
                    text("", arena)?,
                    Datum::Int4(-1),
                    Datum::Bool(field.dropped),
                    Datum::Int4(i32::from(field.attribute_number)),
                    text("i", arena)?,
                    Datum::Bool(true),
                    Datum::Null,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    let indexes = collect_indexes(storage, txid, arena)?;
    for info in indexes {
        let table = storage.table_def(info.table_slot, txid);
        for attribute in 0..info.n_cols + info.n_include_cols {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            let expression = (attribute < info.n_cols && info.expression_keys[attribute])
                .then(|| index_expression_source(storage, info, attribute, txid))
                .flatten();
            let (name, ctype, type_oid, type_mod, collation) = if let Some(source) = expression {
                let source = arena.alloc_str(source.as_str()).map_err(|_| arena_full())?;
                let expression = crate::sql::parser::parse_expr(source, arena)?;
                let (type_oid, type_mod) =
                    super::exec::infer_type_catalog(expression, Some(table), storage, txid)?;
                let (ctype, user_type) = super::exec::catalog_column_type(storage, txid, type_oid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "index expression type OID {} does not exist",
                            type_oid
                        )
                    })?;
                (
                    super::exec::derived_name(expression),
                    ctype,
                    type_oid,
                    if user_type.is_some() {
                        -1
                    } else {
                        i32::from(type_mod)
                    },
                    info.collations[attribute],
                )
            } else {
                let column_index = if attribute < info.n_cols {
                    info.columns[attribute]
                } else {
                    info.include_columns[attribute - info.n_cols]
                };
                let column = &table.columns()[column_index as usize];
                (
                    column.name.as_str(),
                    column.ctype,
                    catalog_column_type_oid(storage, column, txid)?,
                    catalog_column_type_mod(column),
                    column.collation,
                )
            };
            out[n] =
                row(
                    &[
                        Datum::Int4(info.oid),
                        text(name, arena)?,
                        Datum::Int4(type_oid),
                        Datum::Int4(attribute as i32 + 1),
                        Datum::Bool(false),
                        Datum::Int4(i32::from(ctype.typlen())),
                        Datum::Int4(type_mod),
                        Datum::Bool(false),
                        Datum::Int4(collation.oid()),
                        text("", arena)?,
                        text("", arena)?,
                        text(type_storage(ctype), arena)?,
                        text("", arena)?,
                        if expression.is_some() {
                            Datum::Int4(info.explicit_definition.map_or(-1, |definition| {
                                i32::from(definition.statistics[attribute])
                            }))
                        } else {
                            Datum::Null
                        },
                        Datum::Bool(false),
                        Datum::Int4(attribute as i32 + 1),
                        text("i", arena)?,
                        Datum::Bool(true),
                        Datum::Null,
                        Datum::Null,
                        Datum::Bool(false),
                        Datum::Null,
                        Datum::Null,
                    ],
                    arena,
                )?;
            n += 1;
        }
    }
    for (slot, view) in storage.views_visible_to(txid) {
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let count = describe_view(storage, txid, view, arena, &mut columns)?;
        let defaults = view.columns_for(txid);
        for (attribute, column) in columns[..count].iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            let (ctype, user_type) = view_column_catalog_type(storage, txid, column.type_oid)?;
            out[n] = row(
                &[
                    Datum::Int4(view_oid(slot)),
                    text(column.name, arena)?,
                    Datum::Int4(column.type_oid),
                    Datum::Int4(attribute as i32 + 1),
                    Datum::Bool(false),
                    Datum::Int4(i32::from(column.typlen)),
                    Datum::Int4(if user_type.is_some() {
                        -1
                    } else {
                        column.type_mod
                    }),
                    Datum::Bool(!matches!(
                        defaults.default_at(attribute),
                        Some(crate::storage::ColumnDefault::None) | None
                    )),
                    Datum::Int4(0),
                    text("", arena)?,
                    text("", arena)?,
                    text(type_storage(ctype), arena)?,
                    text("", arena)?,
                    Datum::Int4(-1),
                    Datum::Bool(false),
                    Datum::Int4(attribute as i32 + 1),
                    text("i", arena)?,
                    Datum::Bool(true),
                    Datum::Null,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    for (relation, columns) in [
        (
            2613,
            &[
                ("loid", 26, true, 4, "p", "i"),
                ("pageno", 23, true, 4, "p", "i"),
                ("data", 17, true, -1, "x", "i"),
            ][..],
        ),
        (
            PG_LARGEOBJECT_METADATA_OID,
            &[
                ("oid", 26, true, 4, "p", "i"),
                ("lomowner", 26, true, 4, "p", "i"),
                ("lomacl", 1034, false, -1, "x", "d"),
            ][..],
        ),
    ] {
        for (attribute, (name, type_oid, not_null, type_len, storage_kind, alignment)) in
            columns.iter().enumerate()
        {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            let number = attribute as i32 + 1;
            out[n] = row(
                &[
                    Datum::Int4(relation),
                    text(name, arena)?,
                    Datum::Int4(*type_oid),
                    Datum::Int4(number),
                    Datum::Bool(*not_null),
                    Datum::Int4(*type_len),
                    Datum::Int4(-1),
                    Datum::Bool(false),
                    Datum::Int4(0),
                    text("", arena)?,
                    text("", arena)?,
                    text(storage_kind, arena)?,
                    text("", arena)?,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Int4(number),
                    text(alignment, arena)?,
                    Datum::Bool(true),
                    Datum::Null,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    for (relation, columns) in [
        (2683, &[("loid", 26, 4), ("pageno", 23, 4)][..]),
        (2996, &[("oid", 26, 4)][..]),
    ] {
        for (attribute, (name, type_oid, type_len)) in columns.iter().enumerate() {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attribute"));
            }
            let number = attribute as i32 + 1;
            out[n] = row(
                &[
                    Datum::Int4(relation),
                    text(name, arena)?,
                    Datum::Int4(*type_oid),
                    Datum::Int4(number),
                    Datum::Bool(false),
                    Datum::Int4(*type_len),
                    Datum::Int4(-1),
                    Datum::Bool(false),
                    Datum::Int4(0),
                    text("", arena)?,
                    text("", arena)?,
                    text("p", arena)?,
                    text("", arena)?,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Int4(number),
                    text("i", arena)?,
                    Datum::Bool(true),
                    Datum::Null,
                    Datum::Null,
                    Datum::Bool(false),
                    Datum::Null,
                    Datum::Null,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn pg_attrdef<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_attrdef",
        &[
            ("oid", ColType::Int4),
            ("adrelid", ColType::Int4),
            ("adnum", ColType::Int4),
            ("adbin", ColType::Text),
            ("tableoid", ColType::Int4),
        ],
    );
    // A row per column carrying a DEFAULT — the raw source text in `adbin`.
    // This is the engine's source text, not PostgreSQL's serialized node tree;
    // pg_get_expr exposes it through the same catalog contract.
    let mut out: [&[Datum]; 512] = [&[]; 512];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let relid = table_oid(storage, slot);
        for (ci, c) in table.columns().iter().enumerate() {
            let Some(text_expr) = c.default.expression() else {
                continue;
            };
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attrdef"));
            }
            out[n] = row(
                &[
                    Datum::Int4(relid * 100 + ci as i32 + 1), // synthetic adbin oid
                    Datum::Int4(relid),
                    Datum::Int4(ci as i32 + 1),
                    text(text_expr.as_str(), arena)?,
                    Datum::Int4(2604),
                ],
                arena,
            )?;
            n += 1;
        }
    }
    for (slot, view) in storage.views_visible_to(txid) {
        let columns = view.columns_for(txid);
        for (column, default) in (0..columns.len()).filter_map(|index| {
            columns
                .default_at_ref(index)
                .and_then(|default| default.expression().map(|expression| (index, expression)))
        }) {
            if n == out.len() {
                return Err(catalog_capacity_exceeded("pg_attrdef"));
            }
            let relid = view_oid(slot);
            out[n] = row(
                &[
                    Datum::Int4(relid * 100 + column as i32 + 1),
                    Datum::Int4(relid),
                    Datum::Int4(column as i32 + 1),
                    text(default.as_str(), arena)?,
                    Datum::Int4(2604),
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn catalog_routine_result_oid(
    storage: &Storage,
    txid: u32,
    result: crate::storage::RoutineResult,
) -> Result<i32, SqlError> {
    storage
        .routine_type_oid(result.ctype, result.user_type, txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog type identity is unavailable"
            )
        })
}

fn catalog_regproc<'a>(
    storage: &Storage,
    txid: u32,
    oid: i32,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let name = if oid == 0 {
        arena.alloc_str("-").map_err(|_| arena_full())?
    } else {
        routine_name_by_oid(storage, txid, oid, false, arena)?.ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog routine identity is unavailable"
            )
        })?
    };
    Ok(Datum::RegObject {
        type_oid: super::types::oid::REGPROC,
        referenced_oid: oid,
        name,
    })
}

fn pg_cast<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_cast",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("castsource", ColType::Oid),
            ("casttarget", ColType::Oid),
            ("castfunc", ColType::Oid),
            ("castcontext", ColType::Bpchar),
            ("castmethod", ColType::Bpchar),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut count = 0usize;
    for (_, cast) in storage.casts_visible_to(txid) {
        if count == rows.len() {
            return Err(catalog_capacity_exceeded("pg_cast"));
        }
        let function = match cast.method {
            crate::storage::CastMethod::Function(oid) => oid,
            crate::storage::CastMethod::Binary | crate::storage::CastMethod::InOut => 0,
        };
        rows[count] = row(
            &[
                Datum::Int4(2605),
                Datum::Int4(cast.oid()),
                Datum::Int4(catalog_routine_result_oid(storage, txid, cast.source)?),
                Datum::Int4(catalog_routine_result_oid(storage, txid, cast.target)?),
                Datum::Int4(function),
                Datum::Bpchar(match cast.context {
                    crate::storage::CastContext::Explicit => "e",
                    crate::storage::CastContext::Assignment => "a",
                    crate::storage::CastContext::Implicit => "i",
                }),
                Datum::Bpchar(match cast.method {
                    crate::storage::CastMethod::Function(_) => "f",
                    crate::storage::CastMethod::Binary => "b",
                    crate::storage::CastMethod::InOut => "i",
                }),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_operator<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_operator",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("oprname", ColType::Name),
            ("oprnamespace", ColType::Oid),
            ("oprowner", ColType::Oid),
            ("oprkind", ColType::Bpchar),
            ("oprcanmerge", ColType::Bool),
            ("oprcanhash", ColType::Bool),
            ("oprleft", ColType::Oid),
            ("oprright", ColType::Oid),
            ("oprresult", ColType::Oid),
            ("oprcom", ColType::Oid),
            ("oprnegate", ColType::Oid),
            ("oprcode", ColType::Regproc),
            ("oprrest", ColType::Regproc),
            ("oprjoin", ColType::Regproc),
        ],
    );
    const OPCODES: [i32; 11] = [65, 66, 144, 147, 149, 150, 141, 154, 156, 177, 181];
    const OPCODE_NAMES: [&str; 11] = [
        "int4eq", "int4lt", "int4ne", "int4gt", "int4le", "int4ge", "int4mul", "int4div",
        "int4mod", "int4pl", "int4mi",
    ];
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    for (index, operator) in CATALOG_OPERATORS.iter().enumerate() {
        rows[index] = row(
            &[
                Datum::Int4(2617),
                Datum::Int4(operator.oid),
                text(operator.name, arena)?,
                Datum::Int4(PG_CATALOG_NS_OID),
                Datum::Int4(10),
                Datum::Bpchar("b"),
                Datum::Bool(index < 6),
                Datum::Bool(index == 0),
                Datum::Int4(operator.left.oid()),
                Datum::Int4(operator.right.oid()),
                Datum::Int4(if index < 6 {
                    ColType::Bool.oid()
                } else {
                    ColType::Int4.oid()
                }),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::RegObject {
                    type_oid: super::types::oid::REGPROC,
                    referenced_oid: OPCODES[index],
                    name: OPCODE_NAMES[index],
                },
                catalog_regproc(storage, txid, 0, arena)?,
                catalog_regproc(storage, txid, 0, arena)?,
            ],
            arena,
        )?;
    }
    let mut count = CATALOG_OPERATORS.len();
    for (slot, operator) in storage.operators_visible_to(txid) {
        if count == rows.len() {
            return Err(catalog_capacity_exceeded("pg_operator"));
        }
        let left = operator
            .signature
            .left
            .map(|result| catalog_routine_result_oid(storage, txid, result))
            .transpose()?
            .unwrap_or(0);
        let right = operator
            .signature
            .right
            .map(|result| catalog_routine_result_oid(storage, txid, result))
            .transpose()?
            .unwrap_or(0);
        let linked_oid = |linked: Option<i32>| linked.unwrap_or(0);
        rows[count] = row(
            &[
                Datum::Int4(2617),
                Datum::Int4(storage.operator(slot).oid()),
                text(operator.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, operator.schema.as_str())),
                Datum::Int4(operator.owner),
                Datum::Bpchar(match (operator.signature.left, operator.signature.right) {
                    (None, Some(_)) => "l",
                    (Some(_), None) => "r",
                    (Some(_), Some(_)) => "b",
                    (None, None) => unreachable!("operator has an operand"),
                }),
                Datum::Bool(operator.merges),
                Datum::Bool(operator.hashes),
                Datum::Int4(left),
                Datum::Int4(right),
                Datum::Int4(match operator.implementation.result() {
                    Some(result) => catalog_routine_result_oid(storage, txid, result)?,
                    None => 0,
                }),
                Datum::Int4(linked_oid(operator.commutator)),
                Datum::Int4(linked_oid(operator.negator)),
                catalog_regproc(
                    storage,
                    txid,
                    operator.implementation.routine().unwrap_or(0),
                    arena,
                )?,
                catalog_regproc(storage, txid, 0, arena)?,
                catalog_regproc(storage, txid, 0, arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_opfamily<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_opfamily",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("opfmethod", ColType::Oid),
            ("opfname", ColType::Name),
            ("opfnamespace", ColType::Oid),
            ("opfowner", ColType::Oid),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut count = 0usize;
    for (slot, family) in storage.operator_families_visible_to(txid) {
        if count == rows.len() {
            return Err(catalog_capacity_exceeded("pg_opfamily"));
        }
        rows[count] = row(
            &[
                Datum::Int4(2753),
                Datum::Int4(storage.operator_family(slot).oid()),
                Datum::Int4(403),
                text(family.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, family.schema.as_str())),
                Datum::Int4(family.owner),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_opclass<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_opclass",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("opcmethod", ColType::Oid),
            ("opcname", ColType::Name),
            ("opcnamespace", ColType::Oid),
            ("opcowner", ColType::Oid),
            ("opcfamily", ColType::Oid),
            ("opcintype", ColType::Oid),
            ("opcdefault", ColType::Bool),
            ("opckeytype", ColType::Oid),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut count = 0usize;
    for (slot, class) in storage.operator_classes_visible_to(txid) {
        if count == rows.len() {
            return Err(catalog_capacity_exceeded("pg_opclass"));
        }
        let key_type = (class.storage != class.input)
            .then(|| catalog_routine_result_oid(storage, txid, class.storage))
            .transpose()?
            .unwrap_or(0);
        rows[count] = row(
            &[
                Datum::Int4(2616),
                Datum::Int4(storage.operator_class(slot).oid()),
                Datum::Int4(403),
                text(class.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, class.schema.as_str())),
                Datum::Int4(class.owner),
                Datum::Int4(class.family),
                Datum::Int4(catalog_routine_result_oid(storage, txid, class.input)?),
                Datum::Bool(class.default),
                Datum::Int4(key_type),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn operator_family_member_oid(family_oid: i32, position: usize) -> i32 {
    const MEMBER_OID_BASE: i64 = 680_000;
    let family = i64::from(family_oid - crate::storage::OPERATOR_FAMILY_OID_BASE);
    let position = i64::try_from(position).expect("operator-family position is bounded");
    i32::try_from(
        MEMBER_OID_BASE + family * crate::storage::MAX_OPERATOR_FAMILY_MEMBERS as i64 + position,
    )
    .expect("operator-family member OID range exhausted")
}

fn pg_amop<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_amop",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("amopfamily", ColType::Oid),
            ("amoplefttype", ColType::Oid),
            ("amoprighttype", ColType::Oid),
            ("amopstrategy", ColType::Int2),
            ("amoppurpose", ColType::Bpchar),
            ("amopopr", ColType::Oid),
            ("amopmethod", ColType::Oid),
            ("amopsortfamily", ColType::Oid),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut count = 0usize;
    for (family_slot, family) in storage.operator_families_visible_to(txid) {
        for (member_index, member) in family
            .operators
            .iter()
            .enumerate()
            .filter(|(_, member)| member.used)
        {
            if count == rows.len() {
                return Err(catalog_capacity_exceeded("pg_amop"));
            }
            rows[count] = row(
                &[
                    Datum::Int4(2602),
                    Datum::Int4(operator_family_member_oid(
                        storage.operator_family(family_slot).oid(),
                        member_index,
                    )),
                    Datum::Int4(storage.operator_family(family_slot).oid()),
                    Datum::Int4(catalog_routine_result_oid(storage, txid, member.left)?),
                    Datum::Int4(catalog_routine_result_oid(storage, txid, member.right)?),
                    Datum::Int2(i16::from(member.strategy.number())),
                    Datum::Bpchar("s"),
                    Datum::Int4(member.operator),
                    Datum::Int4(403),
                    Datum::Int4(0),
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn pg_amproc<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_amproc",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("amprocfamily", ColType::Oid),
            ("amproclefttype", ColType::Oid),
            ("amprocrighttype", ColType::Oid),
            ("amprocnum", ColType::Int2),
            ("amproc", ColType::Regproc),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut count = 0usize;
    for (family_slot, family) in storage.operator_families_visible_to(txid) {
        for (member_index, member) in family
            .functions
            .iter()
            .enumerate()
            .filter(|(_, member)| member.used)
        {
            if count == rows.len() {
                return Err(catalog_capacity_exceeded("pg_amproc"));
            }
            rows[count] = row(
                &[
                    Datum::Int4(2603),
                    Datum::Int4(operator_family_member_oid(
                        storage.operator_family(family_slot).oid(),
                        member_index,
                    )),
                    Datum::Int4(storage.operator_family(family_slot).oid()),
                    Datum::Int4(catalog_routine_result_oid(storage, txid, member.left)?),
                    Datum::Int4(catalog_routine_result_oid(storage, txid, member.right)?),
                    Datum::Int2(1),
                    catalog_regproc(storage, txid, member.function, arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

const FIRST_PARTITION_TRIGGER_OID: i32 = 1_000_000;

fn partition_trigger_oid(
    trigger: &crate::storage::TriggerDef,
    table: usize,
) -> Result<i32, SqlError> {
    let ordinal = trigger
        .created_at
        .checked_mul(u64::from(u16::MAX) + 1)
        .and_then(|value| value.checked_add(table as u64))
        .and_then(|value| i32::try_from(value).ok())
        .and_then(|value| FIRST_PARTITION_TRIGGER_OID.checked_add(value))
        .ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "partition trigger OID range exhausted"
            )
        })?;
    Ok(ordinal)
}

fn pg_trigger<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_trigger",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("tgrelid", ColType::Oid),
            ("tgparentid", ColType::Oid),
            ("tgname", ColType::Name),
            ("tgfoid", ColType::Oid),
            ("tgtype", ColType::Int2),
            ("tgenabled", ColType::Bpchar),
            ("tgisinternal", ColType::Bool),
            ("tgconstrrelid", ColType::Oid),
            ("tgconstrindid", ColType::Oid),
            ("tgconstraint", ColType::Oid),
            ("tgdeferrable", ColType::Bool),
            ("tginitdeferred", ColType::Bool),
            ("tgnargs", ColType::Int2),
            ("tgattr", ColType::Int2Vector),
            ("tgargs", ColType::Bytea),
            ("tgqual", ColType::Text),
            ("tgoldtable", ColType::Name),
            ("tgnewtable", ColType::Name),
        ],
    );
    let mut rows: [&[Datum]; 512] = [&[]; 512];
    let mut count = 0usize;
    let indexes = collect_indexes(storage, txid, arena)?;
    for child_slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(child_slot, txid) {
            continue;
        }
        let child = storage.table_def(child_slot, txid);
        for (foreign_key_index, foreign_key) in child.fkeys().iter().enumerate() {
            let Some(parent_slot) = storage.find_visible(
                foreign_key.parent_schema.as_str(),
                foreign_key.parent.as_str(),
                txid,
            ) else {
                continue;
            };
            let parent_oid = table_oid(storage, parent_slot);
            let child_oid = table_oid(storage, child_slot);
            let parent_index = indexes
                .iter()
                .find(|index| {
                    index.table_slot == parent_slot
                        && index.is_unique
                        && index.columns[..index.n_cols] == *foreign_key.parent_cols()
                })
                .map_or(0, |index| index.oid);
            let constraint_oid =
                FIRST_FK_OID + child_slot as i32 * MAX_INDEXES_PER_TABLE + foreign_key_index as i32;
            for ordinal in 0..4 {
                if count == rows.len() {
                    return Err(catalog_capacity_exceeded("pg_trigger"));
                }
                let oid = foreign_key_trigger_oid(child_slot, foreign_key_index, ordinal);
                let side = if ordinal < 2 { "a" } else { "c" };
                let name = crate::stack_format!(64, "RI_ConstraintTrigger_{}_{}", side, oid);
                let (relation, function, trigger_type, constrained_relation) = match ordinal {
                    0 => (
                        parent_oid,
                        foreign_key_action_routine(foreign_key.on_delete, false),
                        9,
                        child_oid,
                    ),
                    1 => (
                        parent_oid,
                        foreign_key_action_routine(foreign_key.on_update, true),
                        17,
                        child_oid,
                    ),
                    2 => (child_oid, 1644, 5, parent_oid),
                    _ => (child_oid, 1645, 17, parent_oid),
                };
                rows[count] = row(
                    &[
                        Datum::Int4(PG_TRIGGER_OID),
                        Datum::Int4(oid),
                        Datum::Int4(relation),
                        Datum::Int4(0),
                        text(name.as_str(), arena)?,
                        Datum::Int4(function),
                        Datum::Int2(trigger_type),
                        Datum::Bpchar("O"),
                        Datum::Bool(true),
                        Datum::Int4(constrained_relation),
                        Datum::Int4(parent_index),
                        Datum::Int4(constraint_oid),
                        Datum::Bool(foreign_key.timing.is_deferrable()),
                        Datum::Bool(foreign_key.timing.initially_deferred()),
                        Datum::Int2(0),
                        int2vector(&[], arena)?,
                        Datum::Bytea(&[]),
                        Datum::Null,
                        Datum::Null,
                        Datum::Null,
                    ],
                    arena,
                )?;
                count += 1;
            }
        }
    }
    for (trigger_slot, trigger) in storage.triggers_with_slots_visible_to(txid) {
        if count == rows.len() {
            return Err(catalog_capacity_exceeded("pg_trigger"));
        }
        let relation_oid = match trigger.target {
            crate::storage::TriggerTarget::Table(slot) => {
                let slot = usize::from(slot);
                if !storage.table_slot_visible_to(slot, txid) {
                    continue;
                }
                table_oid(storage, slot)
            }
            crate::storage::TriggerTarget::View(slot) => {
                let slot = usize::from(slot);
                if !storage.view_slot_visible_to(slot, txid) {
                    continue;
                }
                view_oid(slot)
            }
        };
        let function = storage.routine(usize::from(trigger.function));
        let (constraint_oid, constraint_timing, referenced_oid) = match trigger.kind {
            crate::storage::TriggerKind::Ordinary => {
                (0, crate::storage::ConstraintTiming::NotDeferrable, 0)
            }
            crate::storage::TriggerKind::Constraint {
                referenced_table,
                timing,
            } => (
                crate::storage::trigger_oid(&trigger) + 500_000,
                timing,
                referenced_table.map_or(0, |slot| table_oid(storage, usize::from(slot))),
            ),
        };
        let mut trigger_type = match trigger.level {
            crate::sql::ast::TriggerLevel::Row => 1i16,
            crate::sql::ast::TriggerLevel::Statement => 0,
        };
        trigger_type |= match trigger.timing {
            crate::sql::ast::TriggerTiming::Before => 2,
            crate::sql::ast::TriggerTiming::After => 0,
            crate::sql::ast::TriggerTiming::InsteadOf => 64,
        };
        if trigger
            .events
            .contains(crate::sql::ast::TriggerEvents::INSERT)
        {
            trigger_type |= 4;
        }
        if trigger
            .events
            .contains(crate::sql::ast::TriggerEvents::DELETE)
        {
            trigger_type |= 8;
        }
        if trigger
            .events
            .contains(crate::sql::ast::TriggerEvents::UPDATE)
        {
            trigger_type |= 16;
        }
        if trigger
            .events
            .contains(crate::sql::ast::TriggerEvents::TRUNCATE)
        {
            trigger_type |= 32;
        }
        let mut update_columns = [u16::MAX; MAX_COLUMNS];
        let mut update_count = 0usize;
        for column in 0..MAX_COLUMNS {
            if trigger.update_columns & (1u64 << column) != 0 {
                update_columns[update_count] = column as u16;
                update_count += 1;
            }
        }
        let argument_bytes = trigger
            .arguments
            .values()
            .iter()
            .map(|argument| argument.as_str().len() + 1)
            .sum::<usize>();
        let encoded_arguments = arena
            .alloc_slice_with(argument_bytes, |_| 0u8)
            .map_err(|_| arena_full())?;
        let mut argument_at = 0usize;
        for argument in trigger.arguments.values() {
            let bytes = argument.as_str().as_bytes();
            encoded_arguments[argument_at..argument_at + bytes.len()].copy_from_slice(bytes);
            argument_at += bytes.len() + 1;
        }
        let (old_table, new_table) = match &trigger.transition_tables {
            crate::storage::TriggerTransitionTables::None => (None, None),
            crate::storage::TriggerTransitionTables::Old(old) => (Some(old.as_str()), None),
            crate::storage::TriggerTransitionTables::New(new) => (None, Some(new.as_str())),
            crate::storage::TriggerTransitionTables::OldNew { old, new } => {
                (Some(old.as_str()), Some(new.as_str()))
            }
        };
        let root_row = count;
        rows[count] = row(
            &[
                Datum::Int4(2620),
                Datum::Int4(crate::storage::trigger_oid(&trigger)),
                Datum::Int4(relation_oid),
                Datum::Int4(0),
                text(trigger.name_to(txid).as_str(), arena)?,
                Datum::Int4(crate::storage::routine_oid(function)),
                Datum::Int2(trigger_type),
                Datum::Bpchar(match trigger.enabled_to(txid) {
                    crate::storage::TriggerEnabled::Origin => "O",
                    crate::storage::TriggerEnabled::Replica => "R",
                    crate::storage::TriggerEnabled::Always => "A",
                    crate::storage::TriggerEnabled::Disabled => "D",
                }),
                Datum::Bool(false),
                Datum::Int4(referenced_oid),
                Datum::Int4(0),
                Datum::Int4(constraint_oid),
                Datum::Bool(constraint_timing.is_deferrable()),
                Datum::Bool(constraint_timing.initially_deferred()),
                Datum::Int2(trigger.arguments.values().len() as i16),
                int2vector(&update_columns[..update_count], arena)?,
                Datum::Bytea(encoded_arguments),
                trigger
                    .when
                    .map_or(Ok(Datum::Null), |when| text(when.as_str(), arena))?,
                old_table.map_or(Ok(Datum::Null), |name| text(name, arena))?,
                new_table.map_or(Ok(Datum::Null), |name| text(name, arena))?,
            ],
            arena,
        )?;
        count += 1;
        let crate::storage::TriggerTarget::Table(parent_table) = trigger.target else {
            continue;
        };
        if !matches!(trigger.level, crate::sql::ast::TriggerLevel::Row) {
            continue;
        }
        let parent_table = usize::from(parent_table);
        for child in 0..storage.table_count() {
            if !storage.table_slot_visible_to(child, txid)
                || !storage.partition_descends_from(child, parent_table, txid)
            {
                continue;
            }
            if count == rows.len() {
                return Err(catalog_capacity_exceeded("pg_trigger"));
            }
            let direct_parent = usize::from(
                storage
                    .table_def(child, txid)
                    .partition
                    .attachment
                    .expect("partition descendant has a direct parent")
                    .parent,
            );
            let parent_oid = if direct_parent == parent_table {
                crate::storage::trigger_oid(&trigger)
            } else {
                partition_trigger_oid(&trigger, direct_parent)?
            };
            let clone_oid = partition_trigger_oid(&trigger, child)?;
            let mut clone = [Datum::Null; 20];
            clone.copy_from_slice(rows[root_row]);
            clone[1] = Datum::Int4(clone_oid);
            clone[2] = Datum::Int4(table_oid(storage, child));
            clone[3] = Datum::Int4(parent_oid);
            clone[7] = Datum::Bpchar(
                match storage.partition_trigger_enabled_to(trigger_slot, child, txid) {
                    crate::storage::TriggerEnabled::Origin => "O",
                    crate::storage::TriggerEnabled::Replica => "R",
                    crate::storage::TriggerEnabled::Always => "A",
                    crate::storage::TriggerEnabled::Disabled => "D",
                },
            );
            if matches!(trigger.kind, crate::storage::TriggerKind::Constraint { .. }) {
                clone[11] = Datum::Int4(clone_oid + 500_000);
            }
            rows[count] = row(&clone, arena)?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn pg_event_trigger<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_event_trigger",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("evtname", ColType::Name),
            ("evtevent", ColType::Name),
            ("evtowner", ColType::Oid),
            ("evtfoid", ColType::Oid),
            ("evtenabled", ColType::Bpchar),
            ("evttags", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let mut rows: [&[Datum]; crate::storage::MAX_EVENT_TRIGGERS] =
        [&[]; crate::storage::MAX_EVENT_TRIGGERS];
    let mut count = 0usize;
    for (slot, event_trigger) in storage.event_triggers_visible_to(txid) {
        let routine = storage.routine(usize::from(event_trigger.function));
        let mut tag_values = [Datum::Null; crate::storage::MAX_EVENT_TRIGGER_TAGS];
        for (index, tag) in event_trigger.tags.values().iter().enumerate() {
            tag_values[index] = text(tag.as_str(), arena)?;
        }
        let tags = if event_trigger.tags.values().is_empty() {
            Datum::Null
        } else {
            Datum::Array {
                element: super::types::ArrElem::Text,
                raw: super::array::build(&tag_values[..event_trigger.tags.values().len()], arena)?,
            }
        };
        rows[count] = row(
            &[
                Datum::Int4(3466),
                Datum::Int4(storage.event_trigger(slot).oid()),
                text(event_trigger.name.as_str(), arena)?,
                text(event_trigger.event.name(), arena)?,
                Datum::Int4(Storage::role_oid(usize::from(
                    event_trigger.ownership.owner_to(txid),
                ))),
                Datum::Int4(crate::storage::routine_oid(routine)),
                Datum::Bpchar(match event_trigger.enabled {
                    crate::storage::TriggerEnabled::Origin => "O",
                    crate::storage::TriggerEnabled::Replica => "R",
                    crate::storage::TriggerEnabled::Always => "A",
                    crate::storage::TriggerEnabled::Disabled => "D",
                }),
                tags,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_am<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_am",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("amname", ColType::Name),
            ("amhandler", ColType::Regproc),
            ("amtype", ColType::Char),
        ],
    );
    let mut rows: [&[Datum]; ACCESS_METHODS.len() + crate::storage::MAX_ACCESS_METHODS] =
        [&[]; ACCESS_METHODS.len() + crate::storage::MAX_ACCESS_METHODS];
    let mut count = 0usize;
    for (name, oid, handler, handler_name, method_type) in ACCESS_METHODS {
        rows[count] = row(
            &[
                Datum::Int4(PG_AM_OID),
                Datum::Int4(oid),
                text(name, arena)?,
                Datum::RegObject {
                    type_oid: super::types::oid::REGPROC,
                    referenced_oid: handler,
                    name: arena.alloc_str(handler_name).map_err(|_| arena_full())?,
                },
                Datum::Char(method_type.as_bytes()[0]),
            ],
            arena,
        )?;
        count += 1;
    }
    for (_, method) in storage.access_methods_visible_to(txid) {
        if count == rows.len() {
            return Err(catalog_capacity_exceeded("pg_am"));
        }
        rows[count] = row(
            &[
                Datum::Int4(PG_AM_OID),
                Datum::Int4(method.oid().get()),
                text(method.definition.name.as_str(), arena)?,
                access_method_handler_regproc(method.definition.handler, arena)?,
                Datum::Char(b't'),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn access_method_handler_regproc<'a>(
    handler: crate::storage::TableAccessMethodHandler,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let oid = handler.postgres_handler_oid();
    let name = ACCESS_METHODS
        .iter()
        .find_map(|(_, _, candidate_oid, candidate_name, _)| {
            (*candidate_oid == oid).then_some(*candidate_name)
        })
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog access method handler identity is unavailable"
            )
        })?;
    Ok(Datum::RegObject {
        type_oid: super::types::oid::REGPROC,
        referenced_oid: oid,
        name: arena.alloc_str(name).map_err(|_| arena_full())?,
    })
}

fn pg_language<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_language",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("lanname", ColType::Name),
            ("lanowner", ColType::Int4),
            ("lanpltrusted", ColType::Bool),
            ("lanispl", ColType::Bool),
            ("lanplcallfoid", ColType::Int4),
            ("lanvalidator", ColType::Int4),
            ("laninline", ColType::Int4),
            ("lanacl", ColType::Array(super::types::ArrElem::AclItem)),
        ],
    );
    let internal = row(
        &[
            Datum::Int4(PG_LANGUAGE_OID),
            Datum::Int4(INTERNAL_LANGUAGE_OID),
            text("internal", arena)?,
            Datum::Int4(10),
            Datum::Bool(false),
            Datum::Bool(false),
            Datum::Int4(0),
            Datum::Int4(2246),
            Datum::Int4(0),
            acl(
                storage,
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Language,
                    slot: INTERNAL_LANGUAGE_OID as u16,
                },
                txid,
                arena,
            )?,
        ],
        arena,
    )?;
    let c = row(
        &[
            Datum::Int4(PG_LANGUAGE_OID),
            Datum::Int4(C_LANGUAGE_OID),
            text("c", arena)?,
            Datum::Int4(10),
            Datum::Bool(false),
            Datum::Bool(false),
            Datum::Int4(0),
            Datum::Int4(2247),
            Datum::Int4(0),
            acl(
                storage,
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Language,
                    slot: C_LANGUAGE_OID as u16,
                },
                txid,
                arena,
            )?,
        ],
        arena,
    )?;
    let sql = row(
        &[
            Datum::Int4(PG_LANGUAGE_OID),
            Datum::Int4(SQL_LANGUAGE_OID),
            text("sql", arena)?,
            Datum::Int4(10),
            Datum::Bool(true),
            Datum::Bool(false),
            Datum::Int4(0),
            Datum::Int4(2248),
            Datum::Int4(0),
            acl(
                storage,
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Language,
                    slot: SQL_LANGUAGE_OID as u16,
                },
                txid,
                arena,
            )?,
        ],
        arena,
    )?;
    let plpgsql = row(
        &[
            Datum::Int4(PG_LANGUAGE_OID),
            Datum::Int4(PLPGSQL_LANGUAGE_OID),
            text("plpgsql", arena)?,
            Datum::Int4(10),
            Datum::Bool(true),
            Datum::Bool(true),
            Datum::Int4(13644),
            Datum::Int4(13646),
            Datum::Int4(13645),
            acl(
                storage,
                crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Language,
                    slot: PLPGSQL_LANGUAGE_OID as u16,
                },
                txid,
                arena,
            )?,
        ],
        arena,
    )?;
    finish(definition, &[internal, c, sql, plpgsql], arena)
}

fn pg_proc<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_proc",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("proname", ColType::Name),
            ("pronamespace", ColType::Oid),
            ("pronargs", ColType::Int4),
            ("prorettype", ColType::Oid),
            ("proretset", ColType::Bool),
            ("prokind", ColType::Bpchar),
            ("proargtypes", ColType::OidVector),
            ("provolatile", ColType::Bpchar),
            ("proparallel", ColType::Bpchar),
            ("proowner", ColType::Oid),
            ("prosecdef", ColType::Bool),
            ("proacl", ColType::Array(super::types::ArrElem::AclItem)),
            ("prolang", ColType::Oid),
            ("prosrc", ColType::Text),
            ("probin", ColType::Text),
            ("proisstrict", ColType::Bool),
            ("proleakproof", ColType::Bool),
            ("proconfig", ColType::Array(super::types::ArrElem::Text)),
            ("procost", ColType::Float8),
            ("prorows", ColType::Float8),
            ("protrftypes", ColType::Array(super::types::ArrElem::Oid)),
            ("prosupport", ColType::Regproc),
            ("pronargdefaults", ColType::Int4),
            ("provariadic", ColType::Oid),
            ("proallargtypes", ColType::Array(super::types::ArrElem::Oid)),
            ("proargmodes", ColType::Array(super::types::ArrElem::Char)),
            ("proargnames", ColType::Array(super::types::ArrElem::Text)),
            ("proargdefaults", ColType::PgNodeTree),
            ("prosqlbody", ColType::PgNodeTree),
        ],
    );
    const MAX_ROWS: usize = 512;
    let mut rows: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    for (index, routine) in INTRINSIC_ROUTINES.iter().enumerate() {
        let mut argument_oids = [0_i32; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut argument_count = 0usize;
        for written in routine.argument_types.split_ascii_whitespace() {
            if argument_count == argument_oids.len() {
                return Err(catalog_capacity_exceeded("pg_proc.proargtypes"));
            }
            argument_oids[argument_count] = written.parse().map_err(|_| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "intrinsic routine has an invalid argument OID"
                )
            })?;
            argument_count += 1;
        }
        if argument_count != routine.argument_count as usize {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "intrinsic routine argument count does not match its OID vector"
            ));
        }
        let record_outputs = intrinsic_record_outputs(*routine);
        let mut all_types = [Datum::Null; MAX_ROUTINE_ARGUMENTS];
        let mut modes = [Datum::Null; MAX_ROUTINE_ARGUMENTS];
        let mut names = [Datum::Null; MAX_ROUTINE_ARGUMENTS];
        if let Some((output_oids, output_names)) = record_outputs {
            for output in 0..output_oids.len() {
                all_types[output] = Datum::Oid(output_oids[output] as u32);
                modes[output] = Datum::Char(b'o');
                names[output] = Datum::Text(output_names[output]);
            }
        }
        rows[index] = row(
            &[
                Datum::Int4(1255),
                Datum::Int4(routine.oid),
                text(routine.name, arena)?,
                Datum::Int4(PG_CATALOG_NS_OID),
                Datum::Int4(routine.argument_count),
                Datum::Int4(routine.result_oid),
                Datum::Bool(record_outputs.is_some()),
                Datum::Bpchar("f"),
                oidvector(&argument_oids[..argument_count], arena)?,
                Datum::Bpchar(routine.volatility),
                Datum::Bpchar(intrinsic_routine_parallel(*routine)),
                Datum::Int4(10),
                Datum::Bool(false),
                if matches!(routine.oid, 764 | 765 | 767) {
                    builtin_acl(&["postgres=X/postgres"], arena)?
                } else {
                    Datum::Null
                },
                Datum::Int4(12),
                text(
                    if routine.oid == 89 {
                        "pgsql_version"
                    } else {
                        routine.name
                    },
                    arena,
                )?,
                Datum::Null,
                Datum::Bool(intrinsic_routine_is_strict(*routine)),
                Datum::Bool(false),
                Datum::Null,
                Datum::Float8(1.0),
                Datum::Float8(0.0),
                Datum::Null,
                Datum::RegObject {
                    type_oid: super::types::oid::REGPROC,
                    referenced_oid: 0,
                    name: "-",
                },
                Datum::Int4(0),
                Datum::Int4(0),
                match record_outputs {
                    Some((output_oids, _)) => Datum::Array {
                        element: super::types::ArrElem::Oid,
                        raw: super::array::build(&all_types[..output_oids.len()], arena)?,
                    },
                    None => Datum::Null,
                },
                match record_outputs {
                    Some((output_oids, _)) => Datum::Array {
                        element: super::types::ArrElem::Char,
                        raw: super::array::build(&modes[..output_oids.len()], arena)?,
                    },
                    None => Datum::Null,
                },
                match record_outputs {
                    Some((_, output_names)) => Datum::Array {
                        element: super::types::ArrElem::Text,
                        raw: super::array::build(&names[..output_names.len()], arena)?,
                    },
                    None => Datum::Null,
                },
                Datum::Null,
                Datum::Null,
            ],
            arena,
        )?;
    }
    let mut count = INTRINSIC_ROUTINES.len();
    for slot in 0..storage.routine_count() {
        let routine = storage.routine_for(slot, txid);
        if !storage.routine_slot_visible_to(slot, txid) {
            continue;
        }
        if count == rows.len() {
            return Err(catalog_capacity_exceeded("pg_proc"));
        }
        let mut argument_oids = [0_i32; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (index, argument) in routine.arguments().iter().enumerate() {
            let argument_oid = storage
                .routine_type_oid(argument.ctype, argument.user_type, txid)
                .unwrap_or_else(|| {
                    panic!(
                        "routine {} has an unresolved declared argument type",
                        routine.name.as_str()
                    )
                });
            argument_oids[index] = argument_oid;
        }
        let mut all_argument_types = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut argument_modes = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut argument_names = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut has_modes = false;
        let mut has_names = false;
        let mut default_count = 0i32;
        let mut default_expressions = crate::util::StackStr::<256>::new();
        let mut variadic_oid = 0;
        for (index, parameter) in routine.parameters().iter().enumerate() {
            let parameter_oid = storage
                .routine_type_oid(parameter.ctype, parameter.user_type, txid)
                .unwrap_or_else(|| {
                    panic!(
                        "routine {} has an unresolved declared parameter type",
                        routine.name.as_str()
                    )
                });
            all_argument_types[index] = Datum::Oid(parameter_oid as u32);
            let mode = match parameter.mode {
                crate::storage::RoutineParameterMode::In { .. } => "i",
                crate::storage::RoutineParameterMode::Out => {
                    has_modes = true;
                    "o"
                }
                crate::storage::RoutineParameterMode::InOut { .. } => {
                    has_modes = true;
                    "b"
                }
                crate::storage::RoutineParameterMode::Variadic { .. } => {
                    has_modes = true;
                    variadic_oid = match parameter.ctype {
                        ColType::Array(element) => element.element_oid(),
                        _ => 0,
                    };
                    "v"
                }
            };
            argument_modes[index] = Datum::Char(mode.as_bytes()[0]);
            if !parameter.name.as_str().is_empty() {
                has_names = true;
            }
            argument_names[index] = text(parameter.name.as_str(), arena)?;
            if let Some(default) = parameter.mode.default() {
                if default_count != 0 {
                    let _ = core::fmt::Write::write_str(&mut default_expressions, ", ");
                }
                let _ = core::fmt::Write::write_str(&mut default_expressions, default.as_str());
                default_count += 1;
            }
        }
        if default_expressions.is_truncated() {
            return Err(catalog_capacity_exceeded("pg_proc.proargdefaults"));
        }
        let mut routine_configs = [Datum::Null; crate::storage::MAX_ROUTINE_CONFIGS];
        for (index, config) in routine.configs().iter().enumerate() {
            let rendered = arena
                .alloc_str_display(format_args!(
                    "{}={}",
                    config.name.as_str(),
                    config.value.as_str()
                ))
                .map_err(|_| catalog_capacity_exceeded("pg_proc.proconfig"))?;
            routine_configs[index] = Datum::Text(rendered);
        }
        rows[count] = row(
            &[
                Datum::Int4(1255),
                Datum::Int4(crate::storage::routine_oid(&routine)),
                text(routine.name_for(txid).as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, routine.schema_for(txid).as_str())),
                Datum::Int4(routine.argument_count as i32),
                Datum::Int4(match routine.kind {
                    crate::storage::RoutineKind::Function { .. }
                    | crate::storage::RoutineKind::SetFunction { .. }
                    | crate::storage::RoutineKind::RecordFunction { .. } => storage
                        .routine_function_result_oid(&routine, txid)
                        .unwrap_or_else(|| {
                            panic!(
                                "routine {} has an unresolved declared result type",
                                routine.name.as_str()
                            )
                        }),
                    crate::storage::RoutineKind::TableFunction => crate::sql::types::oid::RECORD,
                    crate::storage::RoutineKind::Trigger => crate::sql::types::oid::TRIGGER,
                    crate::storage::RoutineKind::EventTrigger => {
                        crate::sql::types::oid::EVENT_TRIGGER
                    }
                    crate::storage::RoutineKind::Procedure => crate::sql::types::oid::VOID,
                    crate::storage::RoutineKind::Aggregate(aggregate) => storage
                        .routine_type_oid(
                            aggregate.result_type.ctype,
                            aggregate.result_type.user_type,
                            txid,
                        )
                        .expect("aggregate result type is catalog-resolved"),
                }),
                Datum::Bool(routine.kind.is_set_returning()),
                Datum::Bpchar(routine.kind.catalog_kind()),
                oidvector(&argument_oids[..routine.argument_count], arena)?,
                Datum::Bpchar(match routine.attributes.volatility {
                    crate::storage::RoutineVolatility::Immutable => "i",
                    crate::storage::RoutineVolatility::Stable => "s",
                    crate::storage::RoutineVolatility::Volatile => "v",
                }),
                Datum::Bpchar(match routine.kind {
                    crate::storage::RoutineKind::Aggregate(aggregate) => match aggregate.parallel {
                        crate::storage::RoutineParallel::Safe => "s",
                        crate::storage::RoutineParallel::Restricted => "r",
                        crate::storage::RoutineParallel::Unsafe => "u",
                    },
                    _ => match routine.attributes.parallel {
                        crate::storage::RoutineParallel::Safe => "s",
                        crate::storage::RoutineParallel::Restricted => "r",
                        crate::storage::RoutineParallel::Unsafe => "u",
                    },
                }),
                Datum::Int4(Storage::role_oid(routine.ownership.owner_to(txid) as usize)),
                Datum::Bool(routine.attributes.security_definer),
                acl(storage, Storage::routine_access_object(slot), txid, arena)?,
                Datum::Int4(match routine.language {
                    crate::storage::RoutineLanguage::Sql => 14,
                    crate::storage::RoutineLanguage::PlPgSql => 13563,
                    crate::storage::RoutineLanguage::Internal => 12,
                }),
                text(
                    if matches!(routine.kind, crate::storage::RoutineKind::Aggregate(_)) {
                        "aggregate_dummy"
                    } else if routine.body_kind != crate::storage::RoutineBodyKind::String {
                        ""
                    } else {
                        routine.body.as_str()
                    },
                    arena,
                )?,
                Datum::Null,
                Datum::Bool(routine.attributes.strict),
                Datum::Bool(routine.attributes.leakproof),
                if routine.config_count == 0 {
                    Datum::Null
                } else {
                    Datum::Array {
                        element: super::types::ArrElem::Text,
                        raw: super::array::build(&routine_configs[..routine.config_count], arena)?,
                    }
                },
                Datum::Float8(
                    routine
                        .attributes
                        .cost_bits
                        .map(f64::from_bits)
                        .unwrap_or(100.0),
                ),
                Datum::Float8(
                    routine
                        .attributes
                        .rows_bits
                        .map(f64::from_bits)
                        .unwrap_or_else(|| {
                            if routine.kind.is_set_returning() {
                                1000.0
                            } else {
                                0.0
                            }
                        }),
                ),
                Datum::Null,
                Datum::RegObject {
                    type_oid: super::types::oid::REGPROC,
                    referenced_oid: 0,
                    name: "-",
                },
                Datum::Int4(default_count),
                Datum::Oid(variadic_oid as u32),
                if has_modes {
                    Datum::Array {
                        element: super::types::ArrElem::Oid,
                        raw: super::array::build(
                            &all_argument_types[..routine.parameter_count],
                            arena,
                        )?,
                    }
                } else {
                    Datum::Null
                },
                if has_modes {
                    Datum::Array {
                        element: super::types::ArrElem::Char,
                        raw: super::array::build(
                            &argument_modes[..routine.parameter_count],
                            arena,
                        )?,
                    }
                } else {
                    Datum::Null
                },
                if has_names {
                    Datum::Array {
                        element: super::types::ArrElem::Text,
                        raw: super::array::build(
                            &argument_names[..routine.parameter_count],
                            arena,
                        )?,
                    }
                } else {
                    Datum::Null
                },
                if default_count == 0 {
                    Datum::Null
                } else {
                    text(default_expressions.as_str(), arena)?
                },
                if routine.body_kind == crate::storage::RoutineBodyKind::String
                    || matches!(routine.kind, crate::storage::RoutineKind::Aggregate(_))
                {
                    Datum::Null
                } else {
                    text(routine.body.as_str(), arena)?
                },
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_aggregate<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let regproc = |oid: i32| -> Result<Datum<'a>, SqlError> {
        let name = if oid == 0 {
            arena.alloc_str("-").map_err(|_| arena_full())?
        } else {
            let slot = storage.routine_slot_by_oid(oid, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "aggregate support function {} does not exist",
                    oid
                )
            })?;
            let routine = storage.routine_for(slot, txid);
            let mut qualified = crate::util::StackStr::<130>::new();
            write_identifier(&mut qualified, routine.schema_for(txid).as_str());
            let _ = core::fmt::Write::write_char(&mut qualified, '.');
            write_identifier(&mut qualified, routine.name_for(txid).as_str());
            if qualified.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "qualified routine name is too long"
                ));
            }
            arena
                .alloc_str(qualified.as_str())
                .map_err(|_| arena_full())?
        };
        Ok(Datum::RegObject {
            type_oid: super::types::oid::REGPROC,
            referenced_oid: oid,
            name,
        })
    };
    let definition = def_of(
        "pg_aggregate",
        &[
            ("aggfnoid", ColType::Regproc),
            ("aggkind", ColType::Bpchar),
            ("aggnumdirectargs", ColType::Int2),
            ("aggtransfn", ColType::Regproc),
            ("aggfinalfn", ColType::Regproc),
            ("aggcombinefn", ColType::Regproc),
            ("aggserialfn", ColType::Regproc),
            ("aggdeserialfn", ColType::Regproc),
            ("aggmtransfn", ColType::Regproc),
            ("aggminvtransfn", ColType::Regproc),
            ("aggmfinalfn", ColType::Regproc),
            ("aggfinalextra", ColType::Bool),
            ("aggmfinalextra", ColType::Bool),
            ("aggfinalmodify", ColType::Bpchar),
            ("aggmfinalmodify", ColType::Bpchar),
            ("aggsortop", ColType::Oid),
            ("aggtranstype", ColType::Oid),
            ("aggtransspace", ColType::Int4),
            ("aggmtranstype", ColType::Oid),
            ("aggmtransspace", ColType::Int4),
            ("agginitval", ColType::Text),
            ("aggminitval", ColType::Text),
        ],
    );
    let count = (0..storage.routine_count())
        .filter(|slot| {
            storage.routine_slot_visible_to(*slot, txid)
                && matches!(
                    storage.routine_for(*slot, txid).kind,
                    crate::storage::RoutineKind::Aggregate(_)
                )
        })
        .count();
    let rows = arena
        .alloc_slice_with(count.max(1), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let modify = |value: crate::storage::AggregateFinalModify| match value {
        crate::storage::AggregateFinalModify::ReadOnly => "r",
        crate::storage::AggregateFinalModify::Shareable => "s",
        crate::storage::AggregateFinalModify::ReadWrite => "w",
    };
    let mut index = 0usize;
    for slot in 0..storage.routine_count() {
        let routine = storage.routine_for(slot, txid);
        if !storage.routine_slot_visible_to(slot, txid) {
            continue;
        }
        let crate::storage::RoutineKind::Aggregate(aggregate) = routine.kind else {
            continue;
        };
        let final_function = aggregate.final_function;
        let partial = aggregate.partial;
        let serde = partial.and_then(|partial| partial.serde);
        let moving = aggregate.moving;
        let moving_final = moving.and_then(|moving| moving.final_function);
        let state_oid = storage
            .routine_type_oid(
                aggregate.state_type.ctype,
                aggregate.state_type.user_type,
                txid,
            )
            .expect("aggregate state type is catalog-resolved");
        let moving_state_oid = moving.map_or(0, |moving| {
            storage
                .routine_type_oid(moving.state_type.ctype, moving.state_type.user_type, txid)
                .expect("moving aggregate state type is catalog-resolved")
        });
        rows[index] = row(
            &[
                regproc(crate::storage::routine_oid(&routine))?,
                Datum::Bpchar(match aggregate.kind {
                    crate::storage::AggregateKind::Normal => "n",
                    crate::storage::AggregateKind::OrderedSet => "o",
                    crate::storage::AggregateKind::HypotheticalSet => "h",
                }),
                Datum::Int2(i16::from(aggregate.direct_argument_count)),
                regproc(aggregate.transition_oid)?,
                regproc(final_function.map_or(0, |function| function.function_oid))?,
                regproc(partial.map_or(0, |partial| partial.combine_oid))?,
                regproc(serde.map_or(0, |serde| serde.serialize_oid))?,
                regproc(serde.map_or(0, |serde| serde.deserialize_oid))?,
                regproc(moving.map_or(0, |moving| moving.transition_oid))?,
                regproc(moving.map_or(0, |moving| moving.inverse_oid))?,
                regproc(moving_final.map_or(0, |function| function.function_oid))?,
                Datum::Bool(final_function.is_some_and(|function| function.extra)),
                Datum::Bool(moving_final.is_some_and(|function| function.extra)),
                Datum::Bpchar(final_function.map_or("r", |function| modify(function.modify))),
                Datum::Bpchar(moving_final.map_or("r", |function| modify(function.modify))),
                Datum::Int4(aggregate.sort_operator_oid.unwrap_or(0)),
                Datum::Int4(state_oid),
                Datum::Int4(
                    aggregate
                        .state_space
                        .map_or(0, |space| i32::try_from(space).unwrap_or(i32::MAX)),
                ),
                Datum::Int4(moving_state_oid),
                Datum::Int4(
                    moving
                        .and_then(|moving| moving.state_space)
                        .map_or(0, |space| i32::try_from(space).unwrap_or(i32::MAX)),
                ),
                aggregate
                    .initial_condition
                    .map_or(Ok(Datum::Null), |value| text(value.as_str(), arena))?,
                moving
                    .and_then(|moving| moving.initial_condition)
                    .map_or(Ok(Datum::Null), |value| text(value.as_str(), arena))?,
            ],
            arena,
        )?;
        index += 1;
    }
    finish(definition, &rows[..index], arena)
}

fn text_search_regproc<'a>(oid: i32, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let name = match oid {
        0 => "-",
        3717 => "prsd_start",
        3718 => "prsd_nexttoken",
        3719 => "prsd_end",
        3720 => "prsd_headline",
        3721 => "prsd_lextype",
        3725 => "dsimple_init",
        3726 => "dsimple_lexize",
        13_232 => "dsnowball_init",
        13_233 => "dsnowball_lexize",
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "unknown text search support routine OID {}",
                oid
            ));
        }
    };
    Ok(Datum::RegObject {
        type_oid: super::types::oid::REGPROC,
        referenced_oid: oid,
        name: arena.alloc_str(name).map_err(|_| arena_full())?,
    })
}

fn pg_ts_parser<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_ts_parser",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("prsname", ColType::Name),
            ("prsnamespace", ColType::Oid),
            ("prsstart", ColType::Regproc),
            ("prstoken", ColType::Regproc),
            ("prsend", ColType::Regproc),
            ("prsheadline", ColType::Regproc),
            ("prslextype", ColType::Regproc),
        ],
    );
    let rows = arena
        .alloc_slice_with(crate::storage::MAX_TEXT_SEARCH_OBJECTS, |_| {
            &[] as &[Datum]
        })
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (_, object) in storage.text_search_objects_visible_to(txid) {
        let crate::storage::TextSearchDefinition::Parser {
            schema,
            name,
            oid,
            start,
            gettoken,
            end,
            headline,
            lextypes,
        } = object
        else {
            continue;
        };
        rows[count] = row(
            &[
                Datum::Oid(PG_TS_PARSER_OID as u32),
                Datum::Oid(oid as u32),
                text(name.as_str(), arena)?,
                Datum::Oid(namespace_oid(storage, schema.as_str()) as u32),
                text_search_regproc(start, arena)?,
                text_search_regproc(gettoken, arena)?,
                text_search_regproc(end, arena)?,
                text_search_regproc(headline, arena)?,
                text_search_regproc(lextypes, arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_ts_template<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_ts_template",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("tmplname", ColType::Name),
            ("tmplnamespace", ColType::Oid),
            ("tmplinit", ColType::Regproc),
            ("tmpllexize", ColType::Regproc),
        ],
    );
    let rows = arena
        .alloc_slice_with(crate::storage::MAX_TEXT_SEARCH_OBJECTS, |_| {
            &[] as &[Datum]
        })
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (_, object) in storage.text_search_objects_visible_to(txid) {
        let crate::storage::TextSearchDefinition::Template {
            schema,
            name,
            oid,
            init,
            lexize,
            ..
        } = object
        else {
            continue;
        };
        rows[count] = row(
            &[
                Datum::Oid(PG_TS_TEMPLATE_OID as u32),
                Datum::Oid(oid as u32),
                text(name.as_str(), arena)?,
                Datum::Oid(namespace_oid(storage, schema.as_str()) as u32),
                text_search_regproc(init, arena)?,
                text_search_regproc(lexize, arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_ts_dict<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_ts_dict",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("dictname", ColType::Name),
            ("dictnamespace", ColType::Oid),
            ("dictowner", ColType::Oid),
            ("dicttemplate", ColType::Oid),
            ("dictinitoption", ColType::Text),
        ],
    );
    let rows = arena
        .alloc_slice_with(crate::storage::MAX_TEXT_SEARCH_OBJECTS, |_| {
            &[] as &[Datum]
        })
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (_, object) in storage.text_search_objects_visible_to(txid) {
        let crate::storage::TextSearchDefinition::Dictionary {
            schema,
            name,
            oid,
            owner,
            template,
            options,
            ..
        } = object
        else {
            continue;
        };
        rows[count] = row(
            &[
                Datum::Oid(PG_TS_DICT_OID as u32),
                Datum::Oid(oid as u32),
                text(name.as_str(), arena)?,
                Datum::Oid(namespace_oid(storage, schema.as_str()) as u32),
                Datum::Oid(owner as u32),
                Datum::Oid(template as u32),
                if options.is_empty() {
                    Datum::Null
                } else {
                    text(options.as_str(), arena)?
                },
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_ts_config<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_ts_config",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("cfgname", ColType::Name),
            ("cfgnamespace", ColType::Oid),
            ("cfgowner", ColType::Oid),
            ("cfgparser", ColType::Oid),
        ],
    );
    let rows = arena
        .alloc_slice_with(crate::storage::MAX_TEXT_SEARCH_OBJECTS, |_| {
            &[] as &[Datum]
        })
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (_, object) in storage.text_search_objects_visible_to(txid) {
        let crate::storage::TextSearchDefinition::Configuration {
            schema,
            name,
            oid,
            owner,
            parser,
            ..
        } = object
        else {
            continue;
        };
        rows[count] = row(
            &[
                Datum::Oid(PG_TS_CONFIG_OID as u32),
                Datum::Oid(oid as u32),
                text(name.as_str(), arena)?,
                Datum::Oid(namespace_oid(storage, schema.as_str()) as u32),
                Datum::Oid(owner as u32),
                Datum::Oid(parser as u32),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn pg_ts_config_map<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_ts_config_map",
        &[
            ("mapcfg", ColType::Oid),
            ("maptokentype", ColType::Int4),
            ("mapseqno", ColType::Int4),
            ("mapdict", ColType::Oid),
        ],
    );
    let capacity = crate::storage::MAX_TEXT_SEARCH_OBJECTS
        * crate::storage::TEXT_SEARCH_TOKEN_TYPES
        * crate::storage::TEXT_SEARCH_DICTIONARIES_PER_TOKEN;
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (_, object) in storage.text_search_objects_visible_to(txid) {
        let crate::storage::TextSearchDefinition::Configuration { oid, mappings, .. } = object
        else {
            continue;
        };
        for token in 0..crate::storage::TEXT_SEARCH_TOKEN_TYPES {
            for (sequence, dictionary) in mappings.dictionaries[token]
                .iter()
                .take(mappings.counts[token] as usize)
                .enumerate()
            {
                rows[count] = row(
                    &[
                        Datum::Oid(oid as u32),
                        Datum::Int4((token + 1) as i32),
                        Datum::Int4((sequence + 1) as i32),
                        Datum::Oid(*dictionary as u32),
                    ],
                    arena,
                )?;
                count += 1;
            }
        }
    }
    finish(definition, &rows[..count], arena)
}

fn pg_collation<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_collation",
        &[
            ("tableoid", ColType::Oid),
            ("oid", ColType::Oid),
            ("collname", ColType::Name),
            ("collnamespace", ColType::Oid),
            ("collowner", ColType::Oid),
            ("collprovider", ColType::Bpchar),
            ("collisdeterministic", ColType::Bool),
            ("collencoding", ColType::Int4),
            ("collcollate", ColType::Text),
            ("collctype", ColType::Text),
            ("colllocale", ColType::Text),
            ("collicurules", ColType::Text),
            ("collversion", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; 4 + crate::storage::MAX_COLLATIONS] =
        [&[]; 4 + crate::storage::MAX_COLLATIONS];
    for (index, collation) in crate::sql::ast::Collation::BUILTIN.iter().enumerate() {
        let locale = collation.libc_locale();
        output[index] = row(
            &[
                Datum::Int4(PG_COLLATION_OID),
                Datum::Int4(collation.oid()),
                text(collation.name(), arena)?,
                Datum::Int4(PG_CATALOG_NS_OID),
                Datum::Int4(10),
                Datum::Bpchar(collation.provider()),
                Datum::Bool(true),
                Datum::Int4(collation.encoding()),
                if locale.is_empty() {
                    Datum::Null
                } else {
                    text(locale, arena)?
                },
                if locale.is_empty() {
                    Datum::Null
                } else {
                    text(locale, arena)?
                },
                match collation {
                    crate::sql::ast::Collation::UcsBasic => text("C", arena)?,
                    _ => Datum::Null,
                },
                Datum::Null,
                match collation {
                    crate::sql::ast::Collation::UcsBasic => text("1", arena)?,
                    _ => Datum::Null,
                },
            ],
            arena,
        )?;
    }
    let mut count = 4;
    for (slot, collation) in storage.collations_visible_to(txid) {
        let stored = storage.collation(slot);
        let optional = |value: &str| {
            if value.is_empty() {
                Ok(Datum::Null)
            } else {
                text(value, arena)
            }
        };
        output[count] = row(
            &[
                Datum::Int4(PG_COLLATION_OID),
                Datum::Int4(stored.oid(slot)),
                text(collation.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, collation.schema.as_str())),
                Datum::Int4(collation.owner),
                Datum::Bpchar(collation.provider.code()),
                Datum::Bool(collation.deterministic),
                Datum::Int4(collation.encoding.map_or(-1, |encoding| encoding.code())),
                optional(collation.collate.as_str())?,
                optional(collation.ctype.as_str())?,
                optional(collation.locale.as_str())?,
                optional(collation.rules.as_str())?,
                optional(collation.version.as_str())?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn pg_conversion<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_conversion",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("conname", ColType::Name),
            ("connamespace", ColType::Int4),
            ("conowner", ColType::Int4),
            ("conforencoding", ColType::Int4),
            ("contoencoding", ColType::Int4),
            ("conproc", ColType::Regproc),
            ("condefault", ColType::Bool),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_CONVERSIONS] =
        [&[]; crate::storage::MAX_CONVERSIONS];
    let mut count = 0;
    for (slot, conversion) in storage.conversions_visible_to(txid) {
        output[count] = row(
            &[
                Datum::Int4(PG_CONVERSION_OID),
                Datum::Int4(storage.conversion(slot).oid(slot)),
                text(conversion.name.as_str(), arena)?,
                Datum::Int4(namespace_oid(storage, conversion.schema.as_str())),
                Datum::Int4(conversion.owner),
                Datum::Int4(conversion.source.code()),
                Datum::Int4(conversion.destination.code()),
                catalog_regproc(storage, txid, conversion.procedure, arena)?,
                Datum::Bool(conversion.default),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn pg_enum<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_enum",
        &[
            ("oid", ColType::Int4),
            ("enumtypid", ColType::Int4),
            ("enumsortorder", ColType::Float8),
            ("enumlabel", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::storage::MAX_ENUMS * crate::storage::MAX_ENUM_LABELS;
    let mut out: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut n = 0;
    for slot in 0..storage.enum_count() {
        let e = storage.enum_for(slot, txid);
        if !storage.enum_slot_visible_to(slot, txid) {
            continue;
        }
        let typid = crate::sql::types::oid::enum_oid(slot as u16);
        for (i, m) in e.members().iter().enumerate() {
            out[n] = row(
                &[
                    // A stable, unique synthetic OID per member.
                    Datum::Int4(typid * 1000 + i as i32),
                    Datum::Int4(typid),
                    Datum::Float8(m.sort),
                    text(
                        arena
                            .alloc_str(m.label.as_str())
                            .map_err(|_| arena_full())?,
                        arena,
                    )?,
                ],
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn pg_type<'a>(storage: &Storage, txid: u32, arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_type",
        &[
            ("oid", ColType::Int4),
            ("typname", ColType::Name),
            ("typlen", ColType::Int4),
            ("typcollation", ColType::Int4),
            ("typnamespace", ColType::Int4),
            ("typtype", ColType::Bpchar), // 'b' = base type
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
            ("typacl", ColType::Array(super::types::ArrElem::AclItem)),
            ("tableoid", ColType::Int4),
            ("typowner", ColType::Int4),
            ("typisdefined", ColType::Bool),
            ("typstorage", ColType::Bpchar),
            ("typdefaultbin", ColType::Text),
        ],
    );
    let types = [
        ColType::Void,
        ColType::Internal,
        ColType::PgDdlCommand,
        ColType::Bool,
        ColType::Int2,
        ColType::Int2Vector,
        ColType::OidVector,
        ColType::PgNodeTree,
        ColType::PgNdistinct,
        ColType::PgDependencies,
        ColType::PgMcvList,
        ColType::Int4,
        ColType::Oid,
        ColType::Regtype,
        ColType::Regproc,
        ColType::Regprocedure,
        ColType::Regoper,
        ColType::Regoperator,
        ColType::Regclass,
        ColType::Regnamespace,
        ColType::Regrole,
        ColType::Regconfig,
        ColType::Regdictionary,
        ColType::Int8,
        ColType::Float4,
        ColType::Float8,
        ColType::Text,
        ColType::Name,
        ColType::Varchar,
        ColType::Bpchar,
        ColType::Date,
        ColType::Timestamp,
        ColType::Timestamptz,
        ColType::Time,
        ColType::Timetz,
        ColType::Interval,
        ColType::Json,
        ColType::Jsonb,
        ColType::TsVector,
        ColType::TsQuery,
        ColType::Uuid,
        ColType::Bytea,
        ColType::Numeric,
        ColType::Bit { varying: false },
        ColType::Bit { varying: true },
        ColType::Range(super::types::RangeKind::Int4),
        ColType::Range(super::types::RangeKind::Int8),
        ColType::Range(super::types::RangeKind::Num),
        ColType::Range(super::types::RangeKind::Date),
        ColType::Range(super::types::RangeKind::Ts),
        ColType::Range(super::types::RangeKind::Tstz),
        ColType::Multirange(super::types::RangeKind::Int4),
        ColType::Multirange(super::types::RangeKind::Int8),
        ColType::Multirange(super::types::RangeKind::Num),
        ColType::Multirange(super::types::RangeKind::Date),
        ColType::Multirange(super::types::RangeKind::Ts),
        ColType::Multirange(super::types::RangeKind::Tstz),
        ColType::Inet,
        ColType::Cidr,
        ColType::Macaddr,
        ColType::Macaddr8,
        ColType::Geometry(super::types::GeometryKind::Point),
        ColType::Geometry(super::types::GeometryKind::Lseg),
        ColType::Geometry(super::types::GeometryKind::Path),
        ColType::Geometry(super::types::GeometryKind::Box),
        ColType::Geometry(super::types::GeometryKind::Polygon),
        ColType::Geometry(super::types::GeometryKind::Line),
        ColType::Geometry(super::types::GeometryKind::Circle),
        ColType::Record,
    ];
    let category = |t: &ColType| match t {
        ColType::Void | ColType::Internal | ColType::PgDdlCommand | ColType::Record => "P",
        ColType::Bool => "B",
        ColType::Int2
        | ColType::Int4
        | ColType::Int8
        | ColType::Float4
        | ColType::Float8
        | ColType::Numeric => "N",
        ColType::Date | ColType::Time | ColType::Timestamp | ColType::Timestamptz => "D",
        ColType::Interval => "T",
        ColType::Uuid | ColType::Bytea | ColType::TsVector | ColType::TsQuery => "U",
        ColType::PgNodeTree
        | ColType::PgNdistinct
        | ColType::PgDependencies
        | ColType::PgMcvList => "Z",
        // Network address types are PostgreSQL typcategory 'I'.
        ColType::Inet | ColType::Cidr | ColType::Macaddr | ColType::Macaddr8 => "I",
        ColType::Geometry(_) => "G",
        _ => "S",
    };
    let mut out: [&[Datum];
        512 + crate::storage::MAX_DOMAINS * 2
            + crate::storage::MAX_ENUMS * 2
            + crate::storage::MAX_COMPOSITES * 2] = [&[]; 512
        + crate::storage::MAX_DOMAINS * 2
        + crate::storage::MAX_ENUMS * 2
        + crate::storage::MAX_COMPOSITES * 2];
    for (i, t) in types.iter().enumerate() {
        out[i] = row(
            &[
                Datum::Int4(t.oid()),
                text(t.internal_name(), arena)?,
                Datum::Int4(i32::from(t.typlen())),
                Datum::Int4(if t.is_collatable() { 100 } else { 0 }),
                Datum::Int4(PG_CATALOG_NS_OID),
                text(if t.is_pseudo() { "p" } else { "b" }, arena)?,
                text(category(t), arena)?,
                Datum::Int4(0), // typbasetype
                Datum::Int4(0), // typelem
                Datum::Int4(
                    super::types::ArrElem::from_coltype(*t)
                        .map_or(0, super::types::ArrElem::array_oid),
                ), // typarray
                Datum::Int4(0), // typrelid
                Datum::Int4(-1), // typtypmod
                Datum::Bool(false),
                Datum::Null, // typdefault
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(10),
                Datum::Bool(true),
                text(type_storage(*t), arena)?,
                Datum::Null,
            ],
            arena,
        )?;
    }
    let mut n = types.len();
    for (oid, name, length, category, element, array, relation, kind) in [
        (18, "char", 1, "Z", 0, 1002, 0, "b"),
        (super::types::oid::TRIGGER, "trigger", 4, "P", 0, 0, 0, "p"),
        (
            super::types::oid::EVENT_TRIGGER,
            "event_trigger",
            4,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::FDW_HANDLER,
            "fdw_handler",
            4,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ACLITEM,
            "aclitem",
            12,
            "U",
            0,
            super::types::oid::ACLITEM_ARRAY,
            0,
            "b",
        ),
        (
            super::types::oid::ANYELEMENT,
            "anyelement",
            4,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ANYARRAY,
            "anyarray",
            -1,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ANYNONARRAY,
            "anynonarray",
            4,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (super::types::oid::ANYENUM, "anyenum", 4, "P", 0, 0, 0, "p"),
        (
            super::types::oid::ANYRANGE,
            "anyrange",
            -1,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ANYMULTIRANGE,
            "anymultirange",
            -1,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ANYCOMPATIBLE,
            "anycompatible",
            4,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ANYCOMPATIBLEARRAY,
            "anycompatiblearray",
            -1,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ANYCOMPATIBLENONARRAY,
            "anycompatiblenonarray",
            4,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ANYCOMPATIBLERANGE,
            "anycompatiblerange",
            -1,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::ANYCOMPATIBLEMULTIRANGE,
            "anycompatiblemultirange",
            -1,
            "P",
            0,
            0,
            0,
            "p",
        ),
        (
            super::types::oid::PG_STATISTIC_ROW,
            "pg_statistic",
            -1,
            "C",
            0,
            super::types::oid::PG_STATISTIC_ARRAY,
            2619,
            "c",
        ),
        (
            super::types::oid::PG_STATISTIC_ARRAY,
            "_pg_statistic",
            -1,
            "A",
            super::types::oid::PG_STATISTIC_ROW,
            0,
            0,
            "b",
        ),
    ] {
        out[n] = row(
            &[
                Datum::Int4(oid),
                text(name, arena)?,
                Datum::Int4(length),
                Datum::Int4(0),
                Datum::Int4(PG_CATALOG_NS_OID),
                text(kind, arena)?,
                text(category, arena)?,
                Datum::Int4(0),
                Datum::Int4(element),
                Datum::Int4(array),
                Datum::Int4(relation),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(10),
                Datum::Bool(true),
                text(if length < 0 { "x" } else { "p" }, arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // Arrays are catalog types in their own right. Keeping this inventory on
    // `ArrElem` means an accepted array OID is simultaneously visible to
    // `pg_type`, has a matching `typelem`, and is discoverable by catalog
    // clients such as pg_dump.
    for element in super::types::ArrElem::BUILTIN {
        out[n] = row(
            &[
                Datum::Int4(element.array_oid()),
                text(element.catalog_name(), arena)?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(PG_CATALOG_NS_OID),
                text("b", arena)?,
                text("A", arena)?,
                Datum::Int4(0),
                Datum::Int4(element.element_oid()),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(10),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // User-defined domains: typtype 'd', with their base type and constraints.
    for slot in 0..storage.domain_count() {
        let d = storage.domain_for(slot, txid);
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        let base_oid = match d.base_domain {
            Some(parent) => storage
                .domain_identity_slot(parent.schema.as_str(), parent.name.as_str(), txid)
                .map(domain_oid)
                .expect("visible domain retains its parent identity"),
            None => d.base.oid(),
        };
        let array_oid = crate::sql::types::oid::domain_array_oid(slot as u16);
        out[n] = row(
            &[
                Datum::Int4(domain_oid(slot)),
                text(
                    arena.alloc_str(d.name.as_str()).map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(i32::from(d.base.typlen())),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, d.schema.as_str())),
                text("d", arena)?,
                text(category(&d.base), arena)?,
                Datum::Int4(base_oid),
                Datum::Int4(0),
                Datum::Int4(array_oid),
                Datum::Int4(0),
                Datum::Int4(d.base_type_mod),
                Datum::Bool(d.not_null),
                match &d.default_expr {
                    Some(e) => text(
                        arena.alloc_str(e.as_str()).map_err(|_| arena_full())?,
                        arena,
                    )?,
                    None => Datum::Null,
                },
                text("", arena)?,
                text("", arena)?,
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Domain,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Domain,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text(type_storage(d.base), arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
        let array_name = crate::stack_format!(128, "_{}", d.name.as_str());
        out[n] = row(
            &[
                Datum::Int4(array_oid),
                text(
                    arena
                        .alloc_str(array_name.as_str())
                        .map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, d.schema.as_str())),
                text("b", arena)?,
                text("A", arena)?,
                Datum::Int4(0),
                Datum::Int4(domain_oid(slot)),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Domain,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // User-defined enum types: typtype 'e', typcategory 'E', no base type.
    for slot in 0..storage.enum_count() {
        let e = storage.enum_for(slot, txid);
        if !storage.enum_slot_visible_to(slot, txid) {
            continue;
        }
        let enum_oid = crate::sql::types::oid::enum_oid(slot as u16);
        let array_oid = crate::sql::types::oid::enum_array_oid(slot as u16);
        out[n] = row(
            &[
                Datum::Int4(enum_oid),
                text(
                    arena.alloc_str(e.name.as_str()).map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(4), // typlen: enums are a 4-byte oid on the wire
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, e.schema.as_str())),
                text("e", arena)?,
                text("E", arena)?,
                Datum::Int4(0), // typbasetype
                Datum::Int4(0),
                Datum::Int4(array_oid),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Enum,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Enum,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("p", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
        let array_name = crate::stack_format!(128, "_{}", e.name.as_str());
        out[n] = row(
            &[
                Datum::Int4(array_oid),
                text(
                    arena
                        .alloc_str(array_name.as_str())
                        .map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, e.schema.as_str())),
                text("b", arena)?,
                text("A", arena)?,
                Datum::Int4(0),
                Datum::Int4(enum_oid),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Enum,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // Named composite types have their own `pg_type` identity (typtype 'c')
    // and automatically-created array type. Attribute rows are synthesized
    // separately from the same bounded `CompositeDef` field list.
    for (slot, composite) in storage.composites_with_slots_visible_to(txid) {
        let composite_oid = crate::sql::types::oid::composite_oid(slot as u16);
        let array_oid = crate::sql::types::oid::composite_array_oid(slot as u16);
        out[n] = row(
            &[
                Datum::Int4(composite_oid),
                text(
                    arena
                        .alloc_str(composite.name.as_str())
                        .map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, composite.schema.as_str())),
                text("c", arena)?,
                text("C", arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(array_oid),
                Datum::Int4(named_composite_relation_oid(slot)),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Composite,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Composite,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
        let array_name = crate::stack_format!(128, "_{}", composite.name.as_str());
        out[n] = row(
            &[
                Datum::Int4(array_oid),
                text(
                    arena
                        .alloc_str(array_name.as_str())
                        .map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, composite.schema.as_str())),
                text("b", arena)?,
                text("A", arena)?,
                Datum::Int4(0),
                Datum::Int4(composite_oid),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Composite,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    // Every table, materialized view and plain view owns a composite row type.
    for slot in 0..storage.table_count() {
        let table = storage.table(slot);
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_type"));
        }
        out[n] = row(
            &[
                Datum::Int4(FIRST_TABLE_COMPOSITE_TYPE_OID + slot as i32),
                text(table.def.name.as_str(), arena)?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, table.def.schema.as_str())),
                text("c", arena)?,
                text("C", arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(FIRST_TABLE_COMPOSITE_ARRAY_TYPE_OID + slot as i32),
                Datum::Int4(table_oid(storage, slot)),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(
                    storage
                        .matview_slot(table.def.schema.as_str(), table.def.name.as_str(), txid)
                        .map_or(
                            owner_oid(storage, crate::storage::AccessClass::Table, slot, txid),
                            |matview| {
                                owner_oid(
                                    storage,
                                    crate::storage::AccessClass::MaterializedView,
                                    matview,
                                    txid,
                                )
                            },
                        ),
                ),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    for slot in 0..storage.table_count() {
        let table = storage.table(slot);
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_type"));
        }
        let array_name = stack_format!(128, "_{}", table.def.name.as_str());
        out[n] = row(
            &[
                Datum::Int4(FIRST_TABLE_COMPOSITE_ARRAY_TYPE_OID + slot as i32),
                text(array_name.as_str(), arena)?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, table.def.schema.as_str())),
                text("b", arena)?,
                text("A", arena)?,
                Datum::Int4(0),
                Datum::Int4(FIRST_TABLE_COMPOSITE_TYPE_OID + slot as i32),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Table,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    for (slot, view) in storage.views_visible_to(txid) {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_type"));
        }
        out[n] = row(
            &[
                Datum::Int4(FIRST_VIEW_COMPOSITE_TYPE_OID + slot as i32),
                text(view.name_for(txid).as_str(), arena)?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, view.schema_for(txid).as_str())),
                text("c", arena)?,
                text("C", arena)?,
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(FIRST_VIEW_COMPOSITE_ARRAY_TYPE_OID + slot as i32),
                Datum::Int4(view_oid(slot)),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::View,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    for (slot, view) in storage.views_visible_to(txid) {
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_type"));
        }
        let array_name = stack_format!(128, "_{}", view.name_for(txid).as_str());
        out[n] = row(
            &[
                Datum::Int4(FIRST_VIEW_COMPOSITE_ARRAY_TYPE_OID + slot as i32),
                text(array_name.as_str(), arena)?,
                Datum::Int4(-1),
                Datum::Int4(0),
                Datum::Int4(namespace_oid(storage, view.schema_for(txid).as_str())),
                text("b", arena)?,
                text("A", arena)?,
                Datum::Int4(0),
                Datum::Int4(FIRST_VIEW_COMPOSITE_TYPE_OID + slot as i32),
                Datum::Int4(0),
                Datum::Int4(0),
                Datum::Int4(-1),
                Datum::Bool(false),
                Datum::Null,
                text("", arena)?,
                text("", arena)?,
                Datum::Null,
                Datum::Int4(PG_TYPE_OID),
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::View,
                    slot,
                    txid,
                )),
                Datum::Bool(true),
                text("x", arena)?,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_namespace<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_namespace",
        &[
            ("tableoid", ColType::Int4),
            ("oid", ColType::Int4),
            ("nspname", ColType::Text),
            ("nspowner", ColType::Int4),
            ("nspacl", ColType::Array(super::types::ArrElem::AclItem)),
        ],
    );
    let mut out: [&[Datum]; 3 + crate::storage::MAX_SCHEMAS] =
        [&[]; 3 + crate::storage::MAX_SCHEMAS];
    out[0] = row(
        &[
            Datum::Int4(PG_NAMESPACE_OID),
            Datum::Int4(PG_CATALOG_NS_OID),
            text("pg_catalog", arena)?,
            Datum::Int4(10),
            Datum::Null,
        ],
        arena,
    )?;
    out[1] = row(
        &[
            Datum::Int4(PG_NAMESPACE_OID),
            Datum::Int4(PG_TOAST_NS_OID),
            text("pg_toast", arena)?,
            Datum::Int4(10),
            Datum::Null,
        ],
        arena,
    )?;
    let mut n = 2;
    for (slot, schema) in storage.visible_schemas(txid) {
        out[n] = row(
            &[
                Datum::Int4(PG_NAMESPACE_OID),
                Datum::Int4(namespace_oid(storage, schema.name.as_str())),
                text(
                    arena
                        .alloc_str(schema.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                Datum::Int4(owner_oid(
                    storage,
                    crate::storage::AccessClass::Schema,
                    slot,
                    txid,
                )),
                acl(
                    storage,
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::Schema,
                        slot: slot as u16,
                    },
                    txid,
                    arena,
                )?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

/// `pg_indexes`: one row per index relation, with the full `CREATE INDEX`
/// text PostgreSQL's view reconstructs. The same enumeration as `pg_class`'s
/// index rows, so psql and this view can never disagree about what exists.
fn pg_indexes<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_indexes",
        &[
            ("schemaname", ColType::Text),
            ("tablename", ColType::Text),
            ("indexname", ColType::Text),
            ("tablespace", ColType::Text),
            ("indexdef", ColType::Text),
        ],
    );
    let indices = collect_indexes(storage, txid, arena)?;
    let out = arena
        .alloc_slice_with(indices.len() + 2, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut n = 0;
    for info in indices {
        let table_def = storage.table_def(info.table_slot, txid);
        let mut indexdef = StackStr::<896>::new();
        {
            use core::fmt::Write as _;
            let _ = write!(
                indexdef,
                "CREATE {}INDEX ",
                if info.is_unique { "UNIQUE " } else { "" },
            );
            write_identifier(&mut indexdef, info.name.as_str());
            write_index_target(&mut indexdef, table_def, info);
            let _ = indexdef.write_str(if info.is_exclusion {
                " USING gist ("
            } else {
                " USING btree ("
            });
            for k in 0..info.n_cols {
                if k > 0 {
                    let _ = indexdef.write_str(", ");
                }
                if let Some(expression) = index_expression_source(storage, info, k, txid) {
                    let _ = indexdef.write_str(expression.as_str());
                } else {
                    write_identifier(
                        &mut indexdef,
                        table_def.columns()[info.columns[k] as usize].name.as_str(),
                    );
                }
                write_index_key_metadata(&mut indexdef, storage, txid, info, k);
                if info.descending[k] {
                    let _ = indexdef.write_str(" DESC");
                }
                if info.nulls_first[k] != info.descending[k] {
                    let _ = indexdef.write_str(if info.nulls_first[k] {
                        " NULLS FIRST"
                    } else {
                        " NULLS LAST"
                    });
                }
            }
            let _ = indexdef.write_str(")");
            if info.n_include_cols != 0 {
                let _ = indexdef.write_str(" INCLUDE (");
                for k in 0..info.n_include_cols {
                    if k > 0 {
                        let _ = indexdef.write_str(", ");
                    }
                    write_identifier(
                        &mut indexdef,
                        table_def.columns()[info.include_columns[k] as usize]
                            .name
                            .as_str(),
                    );
                }
                let _ = indexdef.write_str(")");
            }
            if info.nulls_not_distinct {
                let _ = indexdef.write_str(" NULLS NOT DISTINCT");
            }
            write_index_storage_options(&mut indexdef, info.explicit_definition);
            if let Some(predicate) = info.predicate {
                let _ = write!(indexdef, " WHERE {}", predicate.as_str());
            }
        }
        out[n] = row(
            &[
                text(
                    arena
                        .alloc_str(table_def.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(table_def.name.as_str(), arena)?,
                text(
                    arena
                        .alloc_str(info.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                match info.explicit_definition.and_then(|definition| {
                    (definition.tablespace != 0)
                        .then(|| storage.tablespace_name(definition.tablespace, txid))
                        .flatten()
                }) {
                    Some(name) => text(name.as_str(), arena)?,
                    None => Datum::Null,
                },
                text(
                    alloc_rendered(&indexdef, "index definition is too long", arena)?,
                    arena,
                )?,
            ],
            arena,
        )?;
        n += 1;
    }
    for (table_name, index_name, definition) in [
        (
            "pg_largeobject",
            "pg_largeobject_loid_pn_index",
            "CREATE UNIQUE INDEX pg_largeobject_loid_pn_index ON pg_catalog.pg_largeobject USING btree (loid, pageno)",
        ),
        (
            "pg_largeobject_metadata",
            "pg_largeobject_metadata_oid_index",
            "CREATE UNIQUE INDEX pg_largeobject_metadata_oid_index ON pg_catalog.pg_largeobject_metadata USING btree (oid)",
        ),
    ] {
        out[n] = row(
            &[
                text("pg_catalog", arena)?,
                text(table_name, arena)?,
                text(index_name, arena)?,
                Datum::Null,
                text(definition, arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_tables<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_tables",
        &[
            ("schemaname", ColType::Text),
            ("tablename", ColType::Text),
            ("tableowner", ColType::Text),
            ("tablespace", ColType::Text),
            ("hasindexes", ColType::Bool),
            ("hasrules", ColType::Bool),
            ("hastriggers", ColType::Bool),
            ("rowsecurity", ColType::Bool),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        if table.kind == crate::storage::TableKind::Foreign
            || storage.matview_slot_for_table(slot, txid).is_some()
        {
            continue;
        }
        if n == out.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "information_schema.tables exceeds static capacity"
            ));
        }
        out[n] = row(
            &[
                text(
                    arena
                        .alloc_str(table.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(table.name.as_str(), arena)?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::Table,
                    slot,
                    txid,
                    arena,
                )?,
                match table.tablespace {
                    0 => Datum::Null,
                    tablespace => storage
                        .tablespace_name(tablespace, txid)
                        .map_or(Ok(Datum::Null), |name| text(name.as_str(), arena))?,
                },
                Datum::Bool(indexes.iter().any(|index| index.table_slot == slot)),
                Datum::Bool(storage.table_has_rules(slot, txid)),
                Datum::Bool(
                    storage.triggers_for_table(slot, txid).next().is_some()
                        || !table.fkeys().is_empty(),
                ),
                Datum::Bool(table.row_level_security.enabled),
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_roles<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_roles",
        &[
            ("oid", ColType::Int4),
            ("rolname", ColType::Name),
            ("rolsuper", ColType::Bool),
            ("rolinherit", ColType::Bool),
            ("rolcreaterole", ColType::Bool),
            ("rolcreatedb", ColType::Bool),
            ("rolcanlogin", ColType::Bool),
            ("rolconnlimit", ColType::Int4),
            ("rolpassword", ColType::Text),
            ("rolvaliduntil", ColType::Timestamptz),
            ("rolreplication", ColType::Bool),
            ("rolbypassrls", ColType::Bool),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_ROLES + PREDEFINED_ROLES.len()] =
        [&[]; crate::storage::MAX_ROLES + PREDEFINED_ROLES.len()];
    let mut count = 0usize;
    for &(oid, name) in PREDEFINED_ROLES {
        output[count] = row(
            &[
                Datum::Int4(oid),
                text(name, arena)?,
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(-1),
                text("********", arena)?,
                Datum::Null,
                Datum::Bool(false),
                Datum::Bool(false),
            ],
            arena,
        )?;
        count += 1;
    }
    for slot in 0..storage.role_count() {
        let role = storage.role(slot);
        if !role.visible_to(txid) {
            continue;
        }
        let attributes = role.attributes_to(txid);
        let valid_until = match attributes.valid_until.as_ref() {
            None => Datum::Null,
            Some(value) => {
                Datum::Timestamptz(crate::sql::datetime::parse_timestamp(value.as_str(), true)?)
            }
        };
        output[count] = row(
            &[
                Datum::Int4(Storage::role_oid(slot)),
                text(
                    arena
                        .alloc_str(role.name_to(txid).as_str())
                        .map_err(|_| arena_full())?,
                    arena,
                )?,
                Datum::Bool(attributes.superuser),
                Datum::Bool(attributes.inherit),
                Datum::Bool(attributes.create_role),
                Datum::Bool(attributes.create_database),
                Datum::Bool(attributes.can_login),
                Datum::Int4(attributes.connection_limit),
                text("********", arena)?,
                valid_until,
                Datum::Bool(attributes.replication),
                Datum::Bool(attributes.bypass_row_level_security),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(def, &output[..count], arena)
}

fn append_base64<const N: usize>(bytes: &[u8], output: &mut StackStr<N>) {
    use core::fmt::Write;
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut at = 0usize;
    while at < bytes.len() {
        let remaining = bytes.len() - at;
        let first = bytes[at];
        let second = if remaining > 1 { bytes[at + 1] } else { 0 };
        let third = if remaining > 2 { bytes[at + 2] } else { 0 };
        let _ = output.write_char(ALPHABET[(first >> 2) as usize] as char);
        let _ =
            output.write_char(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        let _ = output.write_char(if remaining > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        let _ = output.write_char(if remaining > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
        at += 3;
    }
}

fn pg_authid<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let current = storage.current_role_slot(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role is not present in the role catalog"
        )
    })?;
    if !storage.role(current).attributes_to(txid).superuser {
        return Err(sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "permission denied for table pg_authid"
        ));
    }
    let def = def_of(
        "pg_authid",
        &[
            ("oid", ColType::Int4),
            ("rolname", ColType::Name),
            ("rolsuper", ColType::Bool),
            ("rolinherit", ColType::Bool),
            ("rolcreaterole", ColType::Bool),
            ("rolcreatedb", ColType::Bool),
            ("rolcanlogin", ColType::Bool),
            ("rolreplication", ColType::Bool),
            ("rolbypassrls", ColType::Bool),
            ("rolconnlimit", ColType::Int4),
            ("rolpassword", ColType::Text),
            ("rolvaliduntil", ColType::Timestamptz),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_ROLES + PREDEFINED_ROLES.len()] =
        [&[]; crate::storage::MAX_ROLES + PREDEFINED_ROLES.len()];
    let mut count = 0usize;
    for &(oid, name) in PREDEFINED_ROLES {
        output[count] = row(
            &[
                Datum::Int4(oid),
                text(name, arena)?,
                Datum::Bool(false),
                Datum::Bool(true),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Bool(false),
                Datum::Int4(-1),
                Datum::Null,
                Datum::Null,
            ],
            arena,
        )?;
        count += 1;
    }
    for slot in 0..storage.role_count() {
        let role = storage.role(slot);
        if !role.visible_to(txid) {
            continue;
        }
        let attributes = role.attributes_to(txid);
        let password = if let Some(password) = attributes.password {
            use core::fmt::Write;
            let mut verifier = StackStr::<256>::new();
            match password {
                crate::storage::RoleCredential::Scram(password) => {
                    let _ = verifier.write_str("SCRAM-SHA-256$");
                    let _ = write!(verifier, "{}:", password.iterations);
                    append_base64(password.salt.as_bytes(), &mut verifier);
                    let _ = verifier.write_char('$');
                    append_base64(&password.stored_key, &mut verifier);
                    let _ = verifier.write_char(':');
                    append_base64(&password.server_key, &mut verifier);
                }
                crate::storage::RoleCredential::Md5(password) => {
                    let _ = verifier.write_str("md5");
                    let _ = verifier.write_str(
                        core::str::from_utf8(&password.hash).expect("MD5 verifier is ASCII"),
                    );
                }
            }
            if verifier.is_truncated() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "role verifier exceeds catalog rendering limit"
                ));
            }
            Datum::Text(
                arena
                    .alloc_str(verifier.as_str())
                    .map_err(|_| arena_full())?,
            )
        } else {
            Datum::Null
        };
        let valid_until = match attributes.valid_until.as_ref() {
            None => Datum::Null,
            Some(value) => {
                Datum::Timestamptz(crate::sql::datetime::parse_timestamp(value.as_str(), true)?)
            }
        };
        output[count] = row(
            &[
                Datum::Int4(Storage::role_oid(slot)),
                text(role.name_to(txid).as_str(), arena)?,
                Datum::Bool(attributes.superuser),
                Datum::Bool(attributes.inherit),
                Datum::Bool(attributes.create_role),
                Datum::Bool(attributes.create_database),
                Datum::Bool(attributes.can_login),
                Datum::Bool(attributes.replication),
                Datum::Bool(attributes.bypass_row_level_security),
                Datum::Int4(attributes.connection_limit),
                password,
                valid_until,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(def, &output[..count], arena)
}

fn pg_auth_members<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_auth_members",
        &[
            ("oid", ColType::Int4),
            ("roleid", ColType::Int4),
            ("member", ColType::Int4),
            ("grantor", ColType::Int4),
            ("admin_option", ColType::Bool),
            ("inherit_option", ColType::Bool),
            ("set_option", ColType::Bool),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_ROLE_MEMBERSHIPS] =
        [&[]; crate::storage::MAX_ROLE_MEMBERSHIPS];
    let mut count = 0usize;
    for slot in 0..storage.role_membership_count() {
        let membership = storage.role_membership(slot);
        if !membership.visible_to(txid) {
            continue;
        }
        let options = membership.options_to(txid);
        output[count] = row(
            &[
                Datum::Int4(Storage::role_membership_oid(slot)),
                Datum::Int4(Storage::role_oid(membership.role as usize)),
                Datum::Int4(Storage::role_oid(membership.member as usize)),
                Datum::Int4(Storage::role_oid(membership.grantor_to(txid) as usize)),
                Datum::Bool(options.admin),
                Datum::Bool(options.inherit),
                Datum::Bool(options.set),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(def, &output[..count], arena)
}

fn pg_db_role_setting<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    use crate::storage::{MAX_ROLE_SETTINGS, RoleSettingScope};
    use core::fmt::Write;
    let definition = def_of(
        "pg_db_role_setting",
        &[
            ("setdatabase", ColType::Int4),
            ("setrole", ColType::Int4),
            ("setconfig", ColType::Array(super::types::ArrElem::Text)),
        ],
    );
    let output = arena
        .alloc_slice_with(MAX_ROLE_SETTINGS, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut processed = [false; MAX_ROLE_SETTINGS];
    let mut output_count = 0usize;
    for (slot, setting) in storage.role_settings() {
        if processed[slot] || !setting.visible_to(txid) {
            continue;
        }
        let scope = setting.scope;
        let mut values = [Datum::Null; MAX_ROLE_SETTINGS];
        let mut value_count = 0usize;
        for (candidate_slot, candidate) in storage.role_settings() {
            if candidate.visible_to(txid) && candidate.scope == scope {
                processed[candidate_slot] = true;
                let mut rendered =
                    StackStr::<{ crate::storage::ROLE_SETTING_VALUE_MAX + 66 }>::new();
                let _ = write!(
                    rendered,
                    "{}={}",
                    candidate.name.as_str(),
                    candidate.value_to(txid).as_str()
                );
                if rendered.is_truncated() {
                    return Err(catalog_capacity_exceeded("pg_db_role_setting"));
                }
                values[value_count] = text(rendered.as_str(), arena)?;
                value_count += 1;
            }
        }
        let (database, role) = match scope {
            RoleSettingScope::RoleAllDatabases(role) => (0, Storage::role_oid(role as usize)),
            RoleSettingScope::RoleInDatabase { role, database } => {
                (database.get(), Storage::role_oid(role as usize))
            }
            RoleSettingScope::AllRolesInDatabase(database) => (database.get(), 0),
        };
        output[output_count] = row(
            &[
                Datum::Int4(database),
                Datum::Int4(role),
                Datum::Array {
                    element: super::types::ArrElem::Text,
                    raw: super::array::build(&values[..value_count], arena)?,
                },
            ],
            arena,
        )?;
        output_count += 1;
    }
    finish(definition, &output[..output_count], arena)
}

fn pg_database<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_database",
        &[
            ("oid", ColType::Int4),
            ("datname", ColType::Name),
            ("datdba", ColType::Int4),
            ("encoding", ColType::Int4),
            ("datlocprovider", ColType::Bpchar),
            ("datistemplate", ColType::Bool),
            ("datallowconn", ColType::Bool),
            ("dathasloginevt", ColType::Bool),
            ("datconnlimit", ColType::Int4),
            ("datfrozenxid", ColType::Int4),
            ("datminmxid", ColType::Int4),
            ("dattablespace", ColType::Int4),
            ("datcollate", ColType::Text),
            ("datctype", ColType::Text),
            ("datlocale", ColType::Text),
            ("daticurules", ColType::Text),
            ("datcollversion", ColType::Text),
            ("datacl", ColType::Array(super::types::ArrElem::AclItem)),
        ],
    );
    let output = arena
        .alloc_slice_with(crate::storage::MAX_DATABASES, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (slot, database) in storage.databases_visible_to(txid) {
        let database_definition = database.definition_for(txid);
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::Database,
            slot: slot as u16,
        };
        let tablespace_oid = match database_definition.tablespace {
            0 => 1663,
            1 => 1664,
            id => storage
                .tablespaces_visible_to(txid)
                .find(|(slot, _)| *slot + 2 == usize::from(id))
                .map_or(0, |(_, tablespace)| tablespace_oid(*tablespace)),
        };
        output[count] = row(
            &[
                Datum::Int4(database.oid.get()),
                text(database_definition.name.as_str(), arena)?,
                Datum::Int4(Storage::role_oid(storage.object_owner(object, txid))),
                Datum::Int4(database_definition.encoding.code()),
                text(
                    core::str::from_utf8(&[database_definition.locale_provider.code()])
                        .expect("locale provider codes are ASCII"),
                    arena,
                )?,
                Datum::Bool(database_definition.is_template),
                Datum::Bool(database_definition.allow_connections),
                Datum::Bool(false),
                Datum::Int4(database_definition.connection_limit),
                Datum::Int4(0),
                Datum::Int4(1),
                Datum::Int4(tablespace_oid),
                text(database_definition.collate.as_str(), arena)?,
                text(database_definition.ctype.as_str(), arena)?,
                if database_definition.locale.as_str().is_empty() {
                    Datum::Null
                } else {
                    text(database_definition.locale.as_str(), arena)?
                },
                Datum::Null,
                if database_definition.collation_version.as_str().is_empty() {
                    Datum::Null
                } else {
                    text(database_definition.collation_version.as_str(), arena)?
                },
                acl(storage, object, txid, arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn pg_settings<'a>(arena: &'a Arena) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_settings",
        &[
            ("name", ColType::Text),
            ("setting", ColType::Text),
            ("unit", ColType::Text),
            ("category", ColType::Text),
            ("short_desc", ColType::Text),
            ("extra_desc", ColType::Text),
            ("context", ColType::Text),
            ("vartype", ColType::Text),
            ("source", ColType::Text),
            ("min_val", ColType::Text),
            ("max_val", ColType::Text),
            ("enumvals", ColType::Array(super::types::ArrElem::Text)),
            ("boot_val", ColType::Text),
            ("reset_val", ColType::Text),
            ("sourcefile", ColType::Text),
            ("sourceline", ColType::Int4),
            ("pending_restart", ColType::Bool),
        ],
    );
    let output = arena
        .alloc_slice_with(crate::sql::SETTING_NAMES.len() + 2, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    fn metadata(name: &str) -> (&'static str, &'static str, &'static str, &'static str) {
        let boot = match name {
            "application_name" | "default_tablespace" => "",
            "bytea_output" => "hex",
            "check_function_bodies"
            | "integer_datetimes"
            | "row_security"
            | "standard_conforming_strings" => "on",
            "client_encoding" | "server_encoding" => "UTF8",
            "client_min_messages" => "notice",
            "DateStyle" => "ISO, MDY",
            "default_transaction_deferrable" | "default_transaction_read_only" => "off",
            "default_transaction_isolation" => "read committed",
            "default_table_access_method" => "heap",
            "extra_float_digits" => "1",
            "idle_in_transaction_session_timeout"
            | "lock_timeout"
            | "statement_timeout"
            | "transaction_timeout" => "0",
            "IntervalStyle" => "postgres",
            "is_superuser" => "on",
            "max_connections" => "100",
            "max_prepared_transactions" => "0",
            "search_path" => "\"$user\", public",
            "server_version" => crate::pg::REPORTED_SERVER_VERSION,
            "server_version_num" => crate::pg::REPORTED_SERVER_VERSION_NUM,
            "synchronize_seqscans" => "on",
            "TimeZone" => "UTC",
            "transaction_isolation" => "read committed",
            "transaction_deferrable" | "transaction_read_only" => "off",
            "xmloption" => "content",
            _ => "",
        };
        let vartype = if matches!(
            name,
            "check_function_bodies"
                | "integer_datetimes"
                | "is_superuser"
                | "row_security"
                | "standard_conforming_strings"
                | "synchronize_seqscans"
                | "default_transaction_deferrable"
                | "default_transaction_read_only"
                | "transaction_deferrable"
                | "transaction_read_only"
        ) {
            "bool"
        } else if matches!(
            name,
            "DateStyle" | "IntervalStyle" | "bytea_output" | "xmloption"
        ) {
            "enum"
        } else if matches!(
            name,
            "extra_float_digits" | "max_connections" | "max_prepared_transactions"
        ) {
            "integer"
        } else {
            "string"
        };
        let context = if matches!(
            name,
            "integer_datetimes"
                | "is_superuser"
                | "server_encoding"
                | "server_version"
                | "server_version_num"
        ) {
            "internal"
        } else if matches!(name, "max_connections" | "max_prepared_transactions") {
            "postmaster"
        } else {
            "user"
        };
        let unit = if name.ends_with("timeout") { "ms" } else { "" };
        (boot, vartype, context, unit)
    }
    let mut count = 0;
    for &(name, value, vartype, context) in &[
        ("max_identifier_length", "63", "integer", "internal"),
        ("max_index_keys", "32", "integer", "internal"),
    ] {
        output[count] = row(
            &[
                text(name, arena)?,
                text(value, arena)?,
                Datum::Null,
                text("Preset Options", arena)?,
                text("", arena)?,
                Datum::Null,
                text(context, arena)?,
                text(vartype, arena)?,
                text("default", arena)?,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                text(value, arena)?,
                text(value, arena)?,
                Datum::Null,
                Datum::Null,
                Datum::Bool(false),
            ],
            arena,
        )?;
        count += 1;
    }
    for &name in crate::sql::SETTING_NAMES {
        let Some(value) = crate::sql::eval::funcs::system::session_setting(name) else {
            continue;
        };
        let (reset_value, source) = crate::sql::eval::funcs::system::session_setting_metadata(name)
            .unwrap_or((value, "default"));
        let (boot, vartype, context, unit) = metadata(name);
        output[count] = row(
            &[
                text(name, arena)?,
                text(value.as_str(), arena)?,
                if unit.is_empty() {
                    Datum::Null
                } else {
                    text(unit, arena)?
                },
                text("Client Connection Defaults", arena)?,
                text("", arena)?,
                Datum::Null,
                text(context, arena)?,
                text(vartype, arena)?,
                text(source, arena)?,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                text(boot, arena)?,
                text(reset_value.as_str(), arena)?,
                Datum::Null,
                Datum::Null,
                Datum::Bool(false),
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn pg_prepared_xacts<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "pg_prepared_xacts",
        &[
            ("transaction", ColType::Xid),
            ("gid", ColType::Text),
            ("prepared", ColType::Timestamptz),
            ("owner", ColType::Name),
            ("database", ColType::Name),
        ],
    );
    let entries = storage.prepared_transaction_catalog();
    let output = arena
        .alloc_slice_with(entries.len(), |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, entry) in entries.iter().enumerate() {
        let database = storage
            .database_slot_by_oid(entry.database, txid)
            .map(|slot| storage.database_definition(slot, txid).name);
        output[index] = row(
            &[
                Datum::Oid(entry.transaction_id),
                text(entry.gid.as_str(), arena)?,
                Datum::Timestamptz(entry.prepared_at),
                text(
                    storage.role_name(usize::from(entry.owner), txid).as_str(),
                    arena,
                )?,
                match database {
                    Some(name) => text(name.as_str(), arena)?,
                    None => Datum::Null,
                },
            ],
            arena,
        )?;
    }
    finish(definition, output, arena)
}

fn pg_views<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_views",
        &[
            ("schemaname", ColType::Name),
            ("viewname", ColType::Name),
            ("viewowner", ColType::Name),
            ("definition", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.view_count() {
        let view = storage.view(slot);
        if !storage.view_slot_visible_to(slot, txid) || n == out.len() {
            continue;
        }
        out[n] = row(
            &[
                text(view.schema_for(txid).as_str(), arena)?,
                text(view.name_for(txid).as_str(), arena)?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::View,
                    slot,
                    txid,
                    arena,
                )?,
                text(storage.view_sql(slot), arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_rules<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_rules",
        &[
            ("schemaname", ColType::Name),
            ("tablename", ColType::Name),
            ("rulename", ColType::Name),
            ("definition", ColType::Text),
        ],
    );
    let count = storage.rules_visible_to(txid).count();
    let rows = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (_, rule)) in storage.rules_visible_to(txid).enumerate() {
        let definition = rule.definition_for(txid);
        let (schema, relation) = match definition.target {
            crate::storage::RuleTarget::Table(slot) => {
                let table = storage.table_def(usize::from(slot), txid);
                (table.schema, table.name)
            }
            crate::storage::RuleTarget::View(slot) => {
                let view = storage.view(usize::from(slot));
                (view.schema_for(txid), view.name)
            }
        };
        rows[index] = row(
            &[
                text(schema.as_str(), arena)?,
                text(relation.as_str(), arena)?,
                text(definition.name.as_str(), arena)?,
                rule_def_text(storage, txid, rule.oid(), arena)?
                    .map(Datum::Text)
                    .unwrap_or(Datum::Null),
            ],
            arena,
        )?;
    }
    finish(def, rows, arena)
}

fn pg_matviews<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_matviews",
        &[
            ("schemaname", ColType::Name),
            ("matviewname", ColType::Name),
            ("matviewowner", ColType::Name),
            ("tablespace", ColType::Name),
            ("hasindexes", ColType::Bool),
            ("ispopulated", ColType::Bool),
            ("definition", ColType::Text),
        ],
    );
    let indexes = collect_indexes(storage, txid, arena)?;
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for (slot, mv) in storage.matviews_visible_to(txid) {
        if n == out.len() {
            continue;
        }
        let backing = storage.table_def(storage.matview_table(slot), txid);
        out[n] = row(
            &[
                text(
                    arena
                        .alloc_str(backing.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(
                    arena
                        .alloc_str(backing.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::MaterializedView,
                    slot,
                    txid,
                    arena,
                )?,
                match backing.tablespace {
                    0 => Datum::Null,
                    tablespace => storage
                        .tablespace_name(tablespace, txid)
                        .map_or(Ok(Datum::Null), |name| text(name.as_str(), arena))?,
                },
                Datum::Bool(
                    indexes
                        .iter()
                        .any(|index| index.table_slot == storage.matview_table(slot)),
                ),
                Datum::Bool(mv.populated),
                text(
                    arena
                        .alloc_str(mv.sql.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_sequences<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_sequences",
        &[
            ("schemaname", ColType::Text),
            ("sequencename", ColType::Text),
            ("sequenceowner", ColType::Text),
            ("data_type", ColType::Text),
            ("start_value", ColType::Int8),
            ("min_value", ColType::Int8),
            ("max_value", ColType::Int8),
            ("increment_by", ColType::Int8),
            ("cycle", ColType::Bool),
            ("cache_size", ColType::Int8),
            ("last_value", ColType::Int8),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.sequence_count() {
        let seq = storage.sequence_for(slot, txid);
        if !storage.sequence_slot_visible_to(slot, txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_sequences"));
        }
        // last_value is NULL until the sequence has been advanced at least once,
        // exactly as PostgreSQL reports it.
        let (sequence_last_value, sequence_is_called) = storage.sequence_value_for(slot, txid);
        let last_value = if sequence_is_called {
            Datum::Int8(sequence_last_value)
        } else {
            Datum::Null
        };
        out[n] = row(
            &[
                text(seq.schema.as_str(), arena)?,
                text(seq.name.as_str(), arena)?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::Sequence,
                    slot,
                    txid,
                    arena,
                )?,
                text(seq.data_type.sql_name(), arena)?,
                Datum::Int8(seq.start_value),
                Datum::Int8(seq.min_value),
                Datum::Int8(seq.max_value),
                Datum::Int8(seq.increment),
                Datum::Bool(seq.cycle),
                Datum::Int8(seq.cache),
                last_value,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn pg_sequence<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "pg_sequence",
        &[
            ("seqrelid", ColType::Int4),
            ("seqtypid", ColType::Int4),
            ("seqstart", ColType::Int8),
            ("seqincrement", ColType::Int8),
            ("seqmax", ColType::Int8),
            ("seqmin", ColType::Int8),
            ("seqcache", ColType::Int8),
            ("seqcycle", ColType::Bool),
        ],
    );
    let mut out: [&[Datum]; 256] = [&[]; 256];
    let mut n = 0;
    for slot in 0..storage.sequence_count() {
        let seq = storage.sequence_for(slot, txid);
        if !storage.sequence_slot_visible_to(slot, txid) {
            continue;
        }
        if n == out.len() {
            return Err(catalog_capacity_exceeded("pg_sequence"));
        }
        out[n] = row(
            &[
                Datum::Int4(sequence_oid(slot)),
                Datum::Int4(seq.data_type.oid()),
                Datum::Int8(seq.start_value),
                Datum::Int8(seq.increment),
                Datum::Int8(seq.max_value),
                Datum::Int8(seq.min_value),
                Datum::Int8(seq.cache),
                Datum::Bool(seq.cycle),
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn info_foreign_data_wrappers<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "foreign_data_wrappers",
        &[
            ("foreign_data_wrapper_catalog", ColType::Text),
            ("foreign_data_wrapper_name", ColType::Text),
            ("authorization_identifier", ColType::Text),
            ("library_name", ColType::Text),
            ("foreign_data_wrapper_language", ColType::Text),
        ],
    );
    let capacity = storage.foreign_wrappers(txid).count();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (slot, entry) in storage.foreign_wrappers(txid) {
        if !foreign_object_visible(
            storage,
            crate::storage::AccessClass::ForeignDataWrapper,
            slot,
            txid,
        ) {
            continue;
        }
        let wrapper = entry.definition_for(txid);
        rows[count] = row(
            &[
                text(catalog.as_str(), arena)?,
                text(wrapper.name.as_str(), arena)?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::ForeignDataWrapper,
                    slot,
                    txid,
                    arena,
                )?,
                Datum::Null,
                match wrapper.handler {
                    crate::storage::foreign::ForeignDataHandler::None => Datum::Null,
                    crate::storage::foreign::ForeignDataHandler::Postgres => {
                        text("internal", arena)?
                    }
                },
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn info_foreign_data_wrapper_options<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "foreign_data_wrapper_options",
        &[
            ("foreign_data_wrapper_catalog", ColType::Text),
            ("foreign_data_wrapper_name", ColType::Text),
            ("option_name", ColType::Text),
            ("option_value", ColType::Text),
        ],
    );
    let capacity = storage
        .foreign_wrappers(txid)
        .map(|(_, entry)| entry.definition_for(txid).options.entries().len())
        .sum();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (slot, entry) in storage.foreign_wrappers(txid) {
        if !foreign_object_visible(
            storage,
            crate::storage::AccessClass::ForeignDataWrapper,
            slot,
            txid,
        ) {
            continue;
        }
        let wrapper = entry.definition_for(txid);
        for option in wrapper.options.entries() {
            rows[count] = row(
                &[
                    text(catalog.as_str(), arena)?,
                    text(wrapper.name.as_str(), arena)?,
                    text(option.name.as_str(), arena)?,
                    text(option.value.as_str(), arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn info_foreign_servers<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "foreign_servers",
        &[
            ("foreign_server_catalog", ColType::Text),
            ("foreign_server_name", ColType::Text),
            ("foreign_data_wrapper_catalog", ColType::Text),
            ("foreign_data_wrapper_name", ColType::Text),
            ("foreign_server_type", ColType::Text),
            ("foreign_server_version", ColType::Text),
            ("authorization_identifier", ColType::Text),
        ],
    );
    let capacity = storage.foreign_servers(txid).count();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (slot, entry) in storage.foreign_servers(txid) {
        if !foreign_object_visible(
            storage,
            crate::storage::AccessClass::ForeignServer,
            slot,
            txid,
        ) {
            continue;
        }
        let server = entry.definition_for(txid);
        let wrapper = storage
            .foreign_wrapper_by_slot(server.wrapper as usize, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "foreign server references a missing foreign-data wrapper"
                )
            })?;
        rows[count] = row(
            &[
                text(catalog.as_str(), arena)?,
                text(server.name.as_str(), arena)?,
                text(catalog.as_str(), arena)?,
                text(wrapper.name.as_str(), arena)?,
                server
                    .server_type
                    .map_or(Ok(Datum::Null), |value| text(value.as_str(), arena))?,
                server
                    .version
                    .map_or(Ok(Datum::Null), |value| text(value.as_str(), arena))?,
                owner_name(
                    storage,
                    crate::storage::AccessClass::ForeignServer,
                    slot,
                    txid,
                    arena,
                )?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn info_foreign_server_options<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "foreign_server_options",
        &[
            ("foreign_server_catalog", ColType::Text),
            ("foreign_server_name", ColType::Text),
            ("option_name", ColType::Text),
            ("option_value", ColType::Text),
        ],
    );
    let capacity = storage
        .foreign_servers(txid)
        .map(|(_, entry)| entry.definition_for(txid).options.entries().len())
        .sum();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (slot, entry) in storage.foreign_servers(txid) {
        if !foreign_object_visible(
            storage,
            crate::storage::AccessClass::ForeignServer,
            slot,
            txid,
        ) {
            continue;
        }
        let server = entry.definition_for(txid);
        for option in server.options.entries() {
            rows[count] = row(
                &[
                    text(catalog.as_str(), arena)?,
                    text(server.name.as_str(), arena)?,
                    text(option.name.as_str(), arena)?,
                    text(option.value.as_str(), arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn info_foreign_tables<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "foreign_tables",
        &[
            ("foreign_table_catalog", ColType::Text),
            ("foreign_table_schema", ColType::Text),
            ("foreign_table_name", ColType::Text),
            ("foreign_server_catalog", ColType::Text),
            ("foreign_server_name", ColType::Text),
        ],
    );
    let capacity = storage.foreign_tables(txid).count();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (_, entry) in storage.foreign_tables(txid) {
        let foreign = entry.definition_for(txid);
        if !foreign_table_visible(storage, foreign.table as usize, txid) {
            continue;
        }
        let table = storage.table_def(foreign.table as usize, txid);
        let server = storage
            .foreign_server_by_slot(foreign.server as usize, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "foreign table references a missing foreign server"
                )
            })?;
        rows[count] = row(
            &[
                text(catalog.as_str(), arena)?,
                text(table.schema.as_str(), arena)?,
                text(table.name.as_str(), arena)?,
                text(catalog.as_str(), arena)?,
                text(server.name.as_str(), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn info_foreign_table_options<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "foreign_table_options",
        &[
            ("foreign_table_catalog", ColType::Text),
            ("foreign_table_schema", ColType::Text),
            ("foreign_table_name", ColType::Text),
            ("option_name", ColType::Text),
            ("option_value", ColType::Text),
        ],
    );
    let capacity = storage
        .foreign_tables(txid)
        .map(|(_, entry)| entry.definition_for(txid).options.entries().len())
        .sum();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (_, entry) in storage.foreign_tables(txid) {
        let foreign = entry.definition_for(txid);
        if !foreign_table_visible(storage, foreign.table as usize, txid) {
            continue;
        }
        let table = storage.table_def(foreign.table as usize, txid);
        for option in foreign.options.entries() {
            rows[count] = row(
                &[
                    text(catalog.as_str(), arena)?,
                    text(table.schema.as_str(), arena)?,
                    text(table.name.as_str(), arena)?,
                    text(option.name.as_str(), arena)?,
                    text(option.value.as_str(), arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn info_column_options<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "column_options",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("option_name", ColType::Text),
            ("option_value", ColType::Text),
        ],
    );
    let capacity = storage
        .foreign_tables(txid)
        .map(|(_, entry)| entry.definition_for(txid).column_options.entries().len())
        .sum();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (_, entry) in storage.foreign_tables(txid) {
        let foreign = entry.definition_for(txid);
        if !foreign_table_visible(storage, foreign.table as usize, txid) {
            continue;
        }
        let table = storage.table_def(foreign.table as usize, txid);
        for option in foreign.column_options.entries() {
            let column = table.columns.get(option.column as usize).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "foreign column option references a missing column"
                )
            })?;
            rows[count] = row(
                &[
                    text(catalog.as_str(), arena)?,
                    text(table.schema.as_str(), arena)?,
                    text(table.name.as_str(), arena)?,
                    text(column.name.as_str(), arena)?,
                    text(option.option.name.as_str(), arena)?,
                    text(option.option.value.as_str(), arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn info_user_mappings<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "user_mappings",
        &[
            ("authorization_identifier", ColType::Text),
            ("foreign_server_catalog", ColType::Text),
            ("foreign_server_name", ColType::Text),
        ],
    );
    let capacity = storage.foreign_user_mappings(txid).count();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (_, entry) in storage.foreign_user_mappings(txid) {
        let mapping = entry.definition_for(txid);
        if !foreign_object_visible(
            storage,
            crate::storage::AccessClass::ForeignServer,
            mapping.server as usize,
            txid,
        ) {
            continue;
        }
        let server = storage
            .foreign_server_by_slot(mapping.server as usize, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "user mapping references a missing foreign server"
                )
            })?;
        let authorization = match mapping.user {
            crate::storage::foreign::ForeignMappingUser::Public => text("PUBLIC", arena)?,
            crate::storage::foreign::ForeignMappingUser::Role(role) => {
                text(storage.role_name(role as usize, txid).as_str(), arena)?
            }
        };
        rows[count] = row(
            &[
                authorization,
                text(catalog.as_str(), arena)?,
                text(server.name.as_str(), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &rows[..count], arena)
}

fn info_user_mapping_options<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "user_mapping_options",
        &[
            ("authorization_identifier", ColType::Text),
            ("foreign_server_catalog", ColType::Text),
            ("foreign_server_name", ColType::Text),
            ("option_name", ColType::Text),
            ("option_value", ColType::Text),
        ],
    );
    let capacity = storage
        .foreign_user_mappings(txid)
        .map(|(_, entry)| entry.definition_for(txid).options.entries().len())
        .sum();
    let rows = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let catalog = storage.current_database_name(txid);
    let mut count = 0;
    for (_, entry) in storage.foreign_user_mappings(txid) {
        let mapping = entry.definition_for(txid);
        if !foreign_object_visible(
            storage,
            crate::storage::AccessClass::ForeignServer,
            mapping.server as usize,
            txid,
        ) {
            continue;
        }
        let server = storage
            .foreign_server_by_slot(mapping.server as usize, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "user mapping references a missing foreign server"
                )
            })?;
        for option in mapping.options.entries() {
            let authorization = match mapping.user {
                crate::storage::foreign::ForeignMappingUser::Public => text("PUBLIC", arena)?,
                crate::storage::foreign::ForeignMappingUser::Role(role) => {
                    text(storage.role_name(role as usize, txid).as_str(), arena)?
                }
            };
            rows[count] = row(
                &[
                    authorization,
                    text(catalog.as_str(), arena)?,
                    text(server.name.as_str(), arena)?,
                    text(option.name.as_str(), arena)?,
                    if foreign_mapping_options_visible(storage, mapping, txid) {
                        text(option.value.as_str(), arena)?
                    } else {
                        Datum::Null
                    },
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &rows[..count], arena)
}

fn info_tables<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
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
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        if n == out.len() {
            return Err(catalog_capacity_exceeded("information_schema.tables"));
        }
        out[n] = row(
            &[
                text("postgres", arena)?,
                text(
                    arena
                        .alloc_str(table.schema.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(table.name.as_str(), arena)?,
                text(
                    if table.kind == crate::storage::TableKind::Foreign {
                        "FOREIGN"
                    } else {
                        "BASE TABLE"
                    },
                    arena,
                )?,
            ],
            arena,
        )?;
        n += 1;
    }
    for (_, view) in storage.views_visible_to(txid) {
        if n == out.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "information_schema.tables exceeds static capacity"
            ));
        }
        out[n] = row(
            &[
                text("postgres", arena)?,
                text(
                    arena
                        .alloc_str(view.schema_for(txid).as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(
                    arena
                        .alloc_str(view.name_for(txid).as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text("VIEW", arena)?,
            ],
            arena,
        )?;
        n += 1;
    }
    finish(def, &out[..n], arena)
}

fn routine_specific_name(
    routine: &crate::storage::RoutineDef,
    txid: u32,
) -> crate::util::StackStr<96> {
    use core::fmt::Write;
    let mut name = crate::util::StackStr::new();
    let _ = write!(
        name,
        "{}_{}",
        routine.name_for(txid).as_str(),
        crate::storage::routine_oid(routine)
    );
    name
}

fn info_routines<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "routines",
        &[
            ("specific_catalog", ColType::Text),
            ("specific_schema", ColType::Text),
            ("specific_name", ColType::Text),
            ("routine_catalog", ColType::Text),
            ("routine_schema", ColType::Text),
            ("routine_name", ColType::Text),
            ("routine_type", ColType::Text),
            ("data_type", ColType::Text),
            ("external_language", ColType::Text),
            ("routine_definition", ColType::Text),
        ],
    );
    let count = (0..storage.routine_count())
        .filter(|slot| {
            storage.routine_slot_visible_to(*slot, txid)
                && !matches!(
                    storage.routine_for(*slot, txid).kind,
                    crate::storage::RoutineKind::Aggregate(_)
                )
        })
        .count();
    let output = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut row_index = 0;
    for slot in 0..storage.routine_count() {
        let routine = storage.routine_for(slot, txid);
        if !storage.routine_slot_visible_to(slot, txid)
            || matches!(routine.kind, crate::storage::RoutineKind::Aggregate(_))
        {
            continue;
        }
        let specific_name = routine_specific_name(&routine, txid);
        output[row_index] = row(
            &[
                text("postgres", arena)?,
                text(routine.schema_for(txid).as_str(), arena)?,
                text(specific_name.as_str(), arena)?,
                text("postgres", arena)?,
                text(routine.schema_for(txid).as_str(), arena)?,
                text(routine.name_for(txid).as_str(), arena)?,
                text(
                    if matches!(routine.kind, crate::storage::RoutineKind::Procedure) {
                        "PROCEDURE"
                    } else {
                        "FUNCTION"
                    },
                    arena,
                )?,
                match routine.kind.function_result() {
                    Some(result) => text(result.name(), arena)?,
                    None => Datum::Null,
                },
                text("SQL", arena)?,
                text(routine.body.as_str(), arena)?,
            ],
            arena,
        )?;
        row_index += 1;
    }
    finish(definition, output, arena)
}

fn info_routine_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    include_public: bool,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        if include_public {
            "routine_privileges"
        } else {
            "role_routine_grants"
        },
        &[
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("specific_catalog", ColType::Text),
            ("specific_schema", ColType::Text),
            ("specific_name", ColType::Text),
            ("routine_catalog", ColType::Text),
            ("routine_schema", ColType::Text),
            ("routine_name", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
        ],
    );
    let capacity = storage.routine_count() + crate::storage::MAX_ACL_ENTRIES;
    let output = arena
        .alloc_slice_with(capacity, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    let mut append =
        |slot: usize, grantor: u16, grantee: u16, grantable: bool| -> Result<(), SqlError> {
            if (!include_public && grantee == crate::storage::PUBLIC_ROLE)
                || (!storage.role_is_enabled(grantor, txid)
                    && !storage.role_is_enabled(grantee, txid))
            {
                return Ok(());
            }
            if count == output.len() {
                return Err(catalog_capacity_exceeded(
                    "information_schema.routine_privileges",
                ));
            }
            let routine = storage.routine_for(slot, txid);
            let specific_name = routine_specific_name(&routine, txid);
            let grantor_name = storage.role_name(grantor as usize, txid);
            let grantee_name = if grantee == crate::storage::PUBLIC_ROLE {
                SqlName::parse("PUBLIC").expect("PUBLIC fits a SQL name")
            } else {
                storage.role_name(grantee as usize, txid)
            };
            output[count] = row(
                &[
                    text(grantor_name.as_str(), arena)?,
                    text(grantee_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(routine.schema_for(txid).as_str(), arena)?,
                    text(specific_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(routine.schema_for(txid).as_str(), arena)?,
                    text(routine.name_for(txid).as_str(), arena)?,
                    text("EXECUTE", arena)?,
                    text(if grantable { "YES" } else { "NO" }, arena)?,
                ],
                arena,
            )?;
            count += 1;
            Ok(())
        };
    for slot in 0..storage.routine_count() {
        if !storage.routine_slot_visible_to(slot, txid) {
            continue;
        }
        let object = Storage::routine_access_object(slot);
        let owner = storage.object_owner(object, txid) as u16;
        append(slot, owner, owner, true)?;
        let public_defined = storage.acl_entries().any(|(acl_slot, entry)| {
            entry.object == object
                && storage.acl_identity(acl_slot, txid).0 == crate::storage::PUBLIC_ROLE
        });
        if !public_defined {
            append(slot, owner, crate::storage::PUBLIC_ROLE, false)?;
        }
        for (acl_slot, entry) in storage.acl_entries() {
            if entry.object != object {
                continue;
            }
            let (privileges, options) = storage.acl_state(acl_slot, txid);
            if !privileges.contains(crate::storage::PrivilegeSet::EXECUTE) {
                continue;
            }
            let (grantee, grantor) = storage.acl_identity(acl_slot, txid);
            if grantee == owner && grantor == owner {
                continue;
            }
            append(
                slot,
                grantor,
                grantee,
                options.contains(crate::storage::PrivilegeSet::EXECUTE),
            )?;
        }
    }
    finish(definition, &output[..count], arena)
}

fn info_parameters<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "parameters",
        &[
            ("specific_catalog", ColType::Text),
            ("specific_schema", ColType::Text),
            ("specific_name", ColType::Text),
            ("ordinal_position", ColType::Int4),
            ("parameter_mode", ColType::Text),
            ("parameter_name", ColType::Text),
            ("data_type", ColType::Text),
            ("udt_catalog", ColType::Text),
            ("udt_schema", ColType::Text),
            ("udt_name", ColType::Text),
        ],
    );
    let count = (0..storage.routine_count())
        .filter(|slot| storage.routine_slot_visible_to(*slot, txid))
        .map(|slot| storage.routine_for(slot, txid).parameter_count)
        .sum();
    let output = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut row_index = 0;
    for slot in 0..storage.routine_count() {
        let routine = storage.routine_for(slot, txid);
        if !storage.routine_slot_visible_to(slot, txid) {
            continue;
        }
        let specific_name = routine_specific_name(&routine, txid);
        for (argument_index, argument) in routine.parameters().iter().enumerate() {
            let mode = match argument.mode {
                crate::storage::RoutineParameterMode::In { .. }
                | crate::storage::RoutineParameterMode::Variadic { .. } => "IN",
                crate::storage::RoutineParameterMode::Out => "OUT",
                crate::storage::RoutineParameterMode::InOut { .. } => "INOUT",
            };
            output[row_index] = row(
                &[
                    text("postgres", arena)?,
                    text(routine.schema_for(txid).as_str(), arena)?,
                    text(specific_name.as_str(), arena)?,
                    Datum::Int4((argument_index + 1) as i32),
                    text(mode, arena)?,
                    if argument.name.as_str().is_empty() {
                        Datum::Null
                    } else {
                        text(argument.name.as_str(), arena)?
                    },
                    text(argument.ctype.name(), arena)?,
                    text("postgres", arena)?,
                    text("pg_catalog", arena)?,
                    text(argument.ctype.name(), arena)?,
                ],
                arena,
            )?;
            row_index += 1;
        }
    }
    finish(definition, output, arena)
}

fn info_views<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "views",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("view_definition", ColType::Text),
            ("check_option", ColType::Text),
            ("is_updatable", ColType::Text),
            ("is_insertable_into", ColType::Text),
            ("is_trigger_updatable", ColType::Text),
            ("is_trigger_deletable", ColType::Text),
            ("is_trigger_insertable_into", ColType::Text),
        ],
    );
    let count = storage.views_visible_to(txid).count();
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    for (index, (_, view)) in storage.views_visible_to(txid).enumerate() {
        let is_updatable = super::query::view_is_auto_updatable(
            storage,
            view.schema_for(txid).as_str(),
            view.name_for(txid).as_str(),
            txid,
            arena,
        )?;
        let writable = if is_updatable { "YES" } else { "NO" };
        out[index] = row(
            &[
                text("postgres", arena)?,
                text(view.schema_for(txid).as_str(), arena)?,
                text(view.name_for(txid).as_str(), arena)?,
                text(storage.view_sql_for(view), arena)?,
                text(
                    match view.check_option_for(txid) {
                        None => "NONE",
                        Some(crate::storage::ViewCheckOption::Local) => "LOCAL",
                        Some(crate::storage::ViewCheckOption::Cascaded) => "CASCADED",
                    },
                    arena,
                )?,
                text(writable, arena)?,
                text(writable, arena)?,
                text("NO", arena)?,
                text("NO", arena)?,
                text("NO", arena)?,
            ],
            arena,
        )?;
    }
    finish(def, out, arena)
}

fn info_view_table_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "view_table_usage",
        &[
            ("view_catalog", ColType::Text),
            ("view_schema", ColType::Text),
            ("view_name", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
        ],
    );
    let mut count = 0usize;
    for (view_slot, _) in storage.views_visible_to(txid) {
        for dependency in storage.view_dependencies(view_slot).entries() {
            if matches!(
                dependency.class,
                crate::storage::DependencyClass::Table | crate::storage::DependencyClass::View
            ) {
                count = count.checked_add(1).ok_or_else(|| {
                    catalog_capacity_exceeded("information_schema.view_table_usage")
                })?;
            }
        }
    }
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut index = 0usize;
    for (view_slot, view) in storage.views_visible_to(txid) {
        for dependency in storage.view_dependencies(view_slot).entries() {
            let (schema, name): (SqlName, SqlName) = match dependency.class {
                crate::storage::DependencyClass::Table => {
                    let slot = dependency.slot as usize;
                    if !storage.table_slot_visible_to(slot, txid) {
                        return Err(sql_err!(
                            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            "view \"{}\" has a stale table dependency",
                            view.name_for(txid).as_str()
                        ));
                    }
                    let table = storage.table_def(slot, txid);
                    (table.schema, table.name)
                }
                crate::storage::DependencyClass::View => {
                    let slot = dependency.slot as usize;
                    let source = storage.view(slot);
                    if !storage.view_slot_visible_to(slot, txid) {
                        return Err(sql_err!(
                            sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            "view \"{}\" has a stale view dependency",
                            view.name_for(txid).as_str()
                        ));
                    }
                    (source.schema_for(txid), source.name_for(txid))
                }
                _ => continue,
            };
            out[index] = row(
                &[
                    text("postgres", arena)?,
                    text(view.schema_for(txid).as_str(), arena)?,
                    text(view.name_for(txid).as_str(), arena)?,
                    text("postgres", arena)?,
                    text(schema.as_str(), arena)?,
                    text(name.as_str(), arena)?,
                ],
                arena,
            )?;
            index += 1;
        }
    }
    debug_assert_eq!(index, out.len());
    finish(def, out, arena)
}

fn info_view_column_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "view_column_usage",
        &[
            ("view_catalog", ColType::Text),
            ("view_schema", ColType::Text),
            ("view_name", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
        ],
    );
    let mut count = 0usize;
    for (view_slot, view) in storage.views_visible_to(txid) {
        for dependency in storage.view_dependencies(view_slot).entries() {
            if dependency.class == crate::storage::DependencyClass::Table {
                let table_slot = dependency.slot as usize;
                if !storage.table_slot_visible_to(table_slot, txid) {
                    return Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "view \"{}\" has a stale table dependency",
                        view.name_for(txid).as_str()
                    ));
                }
                let columns = storage.table_def(table_slot, txid).columns().len();
                let valid_mask = if columns == u64::BITS as usize {
                    u64::MAX
                } else {
                    (1u64 << columns) - 1
                };
                if dependency.referenced_columns & !valid_mask != 0 {
                    return Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "view \"{}\" has a stale column dependency",
                        view.name_for(txid).as_str()
                    ));
                }
                count = count
                    .checked_add(dependency.referenced_columns.count_ones() as usize)
                    .ok_or_else(|| {
                        catalog_capacity_exceeded("information_schema.view_column_usage")
                    })?;
            } else if dependency.class == crate::storage::DependencyClass::View {
                let source_slot = dependency.slot as usize;
                if !storage.view_slot_visible_to(source_slot, txid) {
                    return Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "view \"{}\" has a stale view dependency",
                        view.name_for(txid).as_str()
                    ));
                }
                let mut columns =
                    [super::types::ColDesc::new("", 0, 0); crate::storage::MAX_COLUMNS];
                let n_columns =
                    describe_stored_view(storage, txid, source_slot, arena, &mut columns)?;
                let valid_mask = if n_columns == u64::BITS as usize {
                    u64::MAX
                } else {
                    (1u64 << n_columns) - 1
                };
                if dependency.referenced_columns & !valid_mask != 0 {
                    return Err(sql_err!(
                        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "view \"{}\" has a stale column dependency",
                        view.name_for(txid).as_str()
                    ));
                }
                count = count
                    .checked_add(dependency.referenced_columns.count_ones() as usize)
                    .ok_or_else(|| {
                        catalog_capacity_exceeded("information_schema.view_column_usage")
                    })?;
            }
        }
    }
    let out = arena
        .alloc_slice_with(count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut index = 0usize;
    for (view_slot, view) in storage.views_visible_to(txid) {
        for dependency in storage.view_dependencies(view_slot).entries() {
            match dependency.class {
                crate::storage::DependencyClass::Table => {
                    let table = storage.table_def(dependency.slot as usize, txid);
                    for column in 0..table.columns().len() {
                        if dependency.referenced_columns & (1u64 << column) != 0 {
                            out[index] = row(
                                &[
                                    text("postgres", arena)?,
                                    text(view.schema_for(txid).as_str(), arena)?,
                                    text(view.name_for(txid).as_str(), arena)?,
                                    text("postgres", arena)?,
                                    text(table.schema.as_str(), arena)?,
                                    text(table.name.as_str(), arena)?,
                                    text(table.columns()[column].name.as_str(), arena)?,
                                ],
                                arena,
                            )?;
                            index += 1;
                        }
                    }
                }
                crate::storage::DependencyClass::View => {
                    let source_slot = dependency.slot as usize;
                    let source = storage.view(source_slot);
                    let mut columns =
                        [super::types::ColDesc::new("", 0, 0); crate::storage::MAX_COLUMNS];
                    let n_columns =
                        describe_stored_view(storage, txid, source_slot, arena, &mut columns)?;
                    for (column, descriptor) in columns.iter().enumerate().take(n_columns) {
                        if dependency.referenced_columns & (1u64 << column) != 0 {
                            out[index] = row(
                                &[
                                    text("postgres", arena)?,
                                    text(view.schema_for(txid).as_str(), arena)?,
                                    text(view.name_for(txid).as_str(), arena)?,
                                    text("postgres", arena)?,
                                    text(source.schema_for(txid).as_str(), arena)?,
                                    text(source.name_for(txid).as_str(), arena)?,
                                    text(descriptor.name, arena)?,
                                ],
                                arena,
                            )?;
                            index += 1;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    debug_assert_eq!(index, out.len());
    finish(def, out, arena)
}

fn info_columns<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "columns",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("ordinal_position", ColType::Int4),
            ("column_default", ColType::Text),
            ("is_nullable", ColType::Text),
            ("data_type", ColType::Text),
            ("character_maximum_length", ColType::Int4),
            ("character_octet_length", ColType::Int4),
            ("numeric_precision", ColType::Int4),
            ("numeric_precision_radix", ColType::Int4),
            ("numeric_scale", ColType::Int4),
            ("datetime_precision", ColType::Int4),
            ("interval_type", ColType::Text),
            ("interval_precision", ColType::Int4),
            ("character_set_catalog", ColType::Text),
            ("character_set_schema", ColType::Text),
            ("character_set_name", ColType::Text),
            ("collation_catalog", ColType::Text),
            ("collation_schema", ColType::Text),
            ("collation_name", ColType::Text),
            ("domain_catalog", ColType::Text),
            ("domain_schema", ColType::Text),
            ("domain_name", ColType::Text),
            ("udt_catalog", ColType::Text),
            ("udt_schema", ColType::Text),
            ("udt_name", ColType::Text),
            ("scope_catalog", ColType::Text),
            ("scope_schema", ColType::Text),
            ("scope_name", ColType::Text),
            ("maximum_cardinality", ColType::Int4),
            ("dtd_identifier", ColType::Text),
            ("is_self_referencing", ColType::Text),
            ("is_identity", ColType::Text),
            ("identity_generation", ColType::Text),
            ("identity_start", ColType::Text),
            ("identity_increment", ColType::Text),
            ("identity_maximum", ColType::Text),
            ("identity_minimum", ColType::Text),
            ("identity_cycle", ColType::Text),
            ("is_generated", ColType::Text),
            ("generation_expression", ColType::Text),
            ("is_updatable", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 1024] = [&[]; 1024];
    let mut n = 0;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let foreign_updatable = storage
            .foreign_table(slot as u16, txid)
            .map(|(_, binding)| {
                if let Some(value) = binding.options.get("updatable") {
                    return super::eval::parse_bool(value);
                }
                let server = storage
                    .foreign_server_by_slot(binding.server as usize, txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "foreign table references a missing foreign server"
                        )
                    })?;
                server
                    .options
                    .get("updatable")
                    .map_or(Ok(true), super::eval::parse_bool)
            })
            .transpose()?
            .unwrap_or(true);
        for (i, c) in table.columns().iter().enumerate() {
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "information_schema.columns exceeds static capacity"
                ));
            }
            out[n] = info_column_row(
                storage,
                txid,
                InformationSchemaColumnSource {
                    schema: table.schema.as_str(),
                    table: table.name.as_str(),
                    name: c.name.as_str(),
                    position: i + 1,
                    column: c,
                    updatable: foreign_updatable,
                },
                arena,
            )?;
            n += 1;
        }
    }
    // A view's row type is derived from its SELECT under the path captured at
    // CREATE VIEW time. Reusing that resolver keeps information_schema,
    // pg_attribute, Describe, and execution on one source of truth.
    for (_, view) in storage.views_visible_to(txid) {
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let count = describe_view(storage, txid, view, arena, &mut columns)?;
        for (index, column) in columns[..count].iter().enumerate() {
            if n == out.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "information_schema.columns exceeds static capacity"
                ));
            }
            let (ctype, user_type) = view_column_catalog_type(storage, txid, column.type_oid)?;
            let column_meta = ColumnMeta {
                name: SqlName::EMPTY,
                ctype,
                type_mod: column.type_mod,
                collation: if ctype.is_collatable() {
                    crate::sql::ast::Collation::Default
                } else {
                    crate::sql::ast::Collation::None
                },
                not_null: crate::storage::NotNullOrigin::Nullable,
                unique: false,
                primary: false,
                auto_increment: false,
                default: crate::storage::ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type,
                statistics_target: -1,
            };
            out[n] = info_column_row(
                storage,
                txid,
                InformationSchemaColumnSource {
                    schema: view.schema_for(txid).as_str(),
                    table: view.name_for(txid).as_str(),
                    name: column.name,
                    position: index + 1,
                    column: &column_meta,
                    updatable: false,
                },
                arena,
            )?;
            n += 1;
        }
    }
    finish(def, &out[..n], arena)
}

fn view_column_catalog_type(
    storage: &Storage,
    txid: u32,
    oid: i32,
) -> Result<(ColType, Option<crate::storage::UserTypeName>), SqlError> {
    super::exec::catalog_column_type(storage, txid, oid).ok_or_else(|| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "view column has unsupported type oid {}",
            oid
        )
    })
}

/// The type-dependent portion of `information_schema.columns`.  `TypeMod`
/// keeps the several PostgreSQL wire encodings from leaking into catalog code.
struct InformationSchemaColumnSource<'a> {
    schema: &'a str,
    table: &'a str,
    name: &'a str,
    position: usize,
    column: &'a ColumnMeta,
    updatable: bool,
}

fn info_column_row<'a>(
    storage: &Storage,
    txid: u32,
    source: InformationSchemaColumnSource<'_>,
    arena: &'a Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    use crate::sql::types::oid;
    let InformationSchemaColumnSource {
        schema,
        table,
        name,
        position,
        column,
        updatable,
    } = source;
    let declared_oid = catalog_column_type_oid(storage, column, txid)?;
    let is_domain = (oid::FIRST_DOMAIN..oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&declared_oid);
    let domain =
        is_domain.then(|| storage.domain_for((declared_oid - oid::FIRST_DOMAIN) as usize, txid));
    let type_mod = TypeMod::decode(column.ctype, column.type_mod);
    let data_type = information_schema_data_type(storage, txid, declared_oid)?;
    let (character_length, numeric_precision, numeric_radix, numeric_scale, datetime_precision) =
        match type_mod {
            TypeMod::Length(length)
                if matches!(column.ctype, ColType::Varchar | ColType::Bpchar) =>
            {
                (Some(length as i32), None, None, None, None)
            }
            TypeMod::NumericPS { precision, scale } => (
                None,
                Some(precision as i32),
                Some(10),
                Some(scale as i32),
                None,
            ),
            TypeMod::TemporalPrecision(precision) => {
                (None, None, None, None, Some(precision as i32))
            }
            TypeMod::IntervalMod { precision, .. } => {
                (None, None, None, None, Some(precision.map_or(6, i32::from)))
            }
            _ => match column.ctype {
                ColType::Int2 => (None, Some(16), Some(2), Some(0), None),
                ColType::Int4 => (None, Some(32), Some(2), Some(0), None),
                ColType::Int8 => (None, Some(64), Some(2), Some(0), None),
                ColType::Float4 => (None, Some(24), Some(2), None, None),
                ColType::Float8 => (None, Some(53), Some(2), None, None),
                ColType::Interval => (None, None, None, None, Some(6)),
                _ => (None, None, None, None, None),
            },
        };
    let interval_type = information_schema_interval_type(type_mod);
    let user_type = column.user_type;
    let (udt_schema, udt_name) = if let Some(identity) = user_type {
        let mut type_name = StackStr::<64>::new();
        if matches!(column.ctype, ColType::Array(_)) {
            use core::fmt::Write as _;
            let _ = write!(type_name, "_{}", identity.name.as_str());
        } else {
            use core::fmt::Write as _;
            let _ = write!(type_name, "{}", identity.name.as_str());
        }
        (identity.schema, type_name)
    } else {
        (
            SqlName::parse("pg_catalog").expect("catalog schema fits"),
            StackStr::from_str(column.ctype.catalog_name()),
        )
    };
    let default = (!column.default.is_generated())
        .then(|| column.default.expression())
        .flatten();
    let generated = column.default.is_generated();
    let generated_expression = generated.then(|| column.default.expression()).flatten();
    let nullable =
        !column.not_null.is_required() && !domain.is_some_and(|definition| definition.not_null);
    let identity = column.is_identity;
    let identity_sequence = identity
        .then(|| {
            (0..storage.sequence_count())
                .map(|slot| storage.sequence_for(slot, txid))
                .find(|sequence| {
                    sequence.generator_for.is_some_and(|owner| {
                        owner.table_schema.as_str() == schema
                            && owner.table.as_str() == table
                            && owner.column.as_str() == name
                    })
                })
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROTOCOL_VIOLATION,
                        "identity column \"{}.{}.{}\" has no generator sequence",
                        schema,
                        table,
                        name
                    )
                })
        })
        .transpose()?;
    let identity_start = identity_sequence
        .as_ref()
        .map(|sequence| stack_format!(32, "{}", sequence.start_value));
    let identity_increment = identity_sequence
        .as_ref()
        .map(|sequence| stack_format!(32, "{}", sequence.increment));
    let identity_maximum = identity_sequence
        .as_ref()
        .map(|sequence| stack_format!(32, "{}", sequence.max_value));
    let identity_minimum = identity_sequence
        .as_ref()
        .map(|sequence| stack_format!(32, "{}", sequence.min_value));
    let identity_cycle = identity_sequence.as_ref().map(|sequence| sequence.cycle);
    let default_datum = match default {
        Some(value) => text(value.as_str(), arena)?,
        None => Datum::Null,
    };
    let domain_catalog = match domain {
        Some(_) => text("postgres", arena)?,
        None => Datum::Null,
    };
    let domain_schema = match domain {
        Some(value) => text(value.schema.as_str(), arena)?,
        None => Datum::Null,
    };
    let domain_name = match domain {
        Some(value) => text(value.name.as_str(), arena)?,
        None => Datum::Null,
    };
    let generated_expression = match generated_expression {
        Some(value) => text(value.as_str(), arena)?,
        None => Datum::Null,
    };
    row(
        &[
            text("postgres", arena)?,
            text(schema, arena)?,
            text(table, arena)?,
            text(name, arena)?,
            Datum::Int4(position as i32),
            default_datum,
            text(if nullable { "YES" } else { "NO" }, arena)?,
            text(data_type.as_str(), arena)?,
            character_length.map_or(Datum::Null, Datum::Int4),
            character_length.map_or(Datum::Null, Datum::Int4),
            numeric_precision.map_or(Datum::Null, Datum::Int4),
            numeric_radix.map_or(Datum::Null, Datum::Int4),
            numeric_scale.map_or(Datum::Null, Datum::Int4),
            datetime_precision.map_or(Datum::Null, Datum::Int4),
            match interval_type {
                Some(value) => text(value.as_str(), arena)?,
                None => Datum::Null,
            },
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            domain_catalog,
            domain_schema,
            domain_name,
            text("postgres", arena)?,
            text(udt_schema.as_str(), arena)?,
            text(udt_name.as_str(), arena)?,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            text(stack_format!(32, "{}", position).as_str(), arena)?,
            text("NO", arena)?,
            text(if identity { "YES" } else { "NO" }, arena)?,
            if identity {
                text(
                    if column.identity_always {
                        "ALWAYS"
                    } else {
                        "BY DEFAULT"
                    },
                    arena,
                )?
            } else {
                Datum::Null
            },
            if identity {
                text(
                    identity_start.expect("identity has a sequence").as_str(),
                    arena,
                )?
            } else {
                Datum::Null
            },
            if identity {
                text(
                    identity_increment
                        .expect("identity has a sequence")
                        .as_str(),
                    arena,
                )?
            } else {
                Datum::Null
            },
            if identity {
                text(
                    identity_maximum.expect("identity has a sequence").as_str(),
                    arena,
                )?
            } else {
                Datum::Null
            },
            if identity {
                text(
                    identity_minimum.expect("identity has a sequence").as_str(),
                    arena,
                )?
            } else {
                Datum::Null
            },
            text(
                if identity_cycle == Some(true) {
                    "YES"
                } else {
                    "NO"
                },
                arena,
            )?,
            text(if generated { "ALWAYS" } else { "NEVER" }, arena)?,
            generated_expression,
            text(if updatable { "YES" } else { "NO" }, arena)?,
        ],
        arena,
    )
}

/// The SQL-standard `data_type` spelling is intentionally less specific than
/// PostgreSQL's OID. Domains report their base type, named enums and
/// composites report `USER-DEFINED`, and arrays report `ARRAY`.
fn information_schema_data_type(
    storage: &Storage,
    txid: u32,
    oid: i32,
) -> Result<StackStr<64>, SqlError> {
    use crate::sql::types::oid as type_oid;
    if (type_oid::FIRST_DOMAIN..type_oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
    {
        return Ok(StackStr::from_str(
            storage
                .domain_for((oid - type_oid::FIRST_DOMAIN) as usize, txid)
                .base
                .name(),
        ));
    }
    if (type_oid::FIRST_DOMAIN_ARRAY
        ..type_oid::FIRST_DOMAIN_ARRAY + crate::storage::MAX_DOMAINS as i32)
        .contains(&oid)
        || (type_oid::FIRST_ENUM_ARRAY
            ..type_oid::FIRST_ENUM_ARRAY + crate::storage::MAX_ENUMS as i32)
            .contains(&oid)
        || (type_oid::FIRST_COMPOSITE_ARRAY
            ..type_oid::FIRST_COMPOSITE_ARRAY + crate::storage::MAX_COMPOSITES as i32)
            .contains(&oid)
    {
        return Ok(StackStr::from_str("ARRAY"));
    }
    if (type_oid::FIRST_ENUM..type_oid::FIRST_ENUM + crate::storage::MAX_ENUMS as i32)
        .contains(&oid)
        || (type_oid::FIRST_COMPOSITE
            ..type_oid::FIRST_COMPOSITE + crate::storage::MAX_COMPOSITES as i32)
            .contains(&oid)
    {
        return Ok(StackStr::from_str("USER-DEFINED"));
    }
    if let Some(ctype) = super::exec::coltype_of_oid(oid) {
        if matches!(ctype, ColType::Array(_)) {
            return Ok(StackStr::from_str("ARRAY"));
        }
        return Ok(StackStr::from_str(ctype.name()));
    }
    Err(sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "information_schema.columns cannot describe type oid {}",
        oid
    ))
}

fn info_table_constraints<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "table_constraints",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("constraint_type", ColType::Text),
            ("is_deferrable", ColType::Text),
            ("initially_deferred", ColType::Text),
            ("enforced", ColType::Text),
            ("nulls_distinct", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::sql::query::MAX_JOIN_TABLES
        * (crate::storage::MAX_COLUMNS * 2
            + crate::storage::MAX_UNIQUES
            + crate::storage::MAX_CHECKS
            + crate::storage::MAX_FKEYS);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    let mut append = |name: &str,
                      kind: &str,
                      nulls_distinct: Option<&str>,
                      timing: crate::storage::ConstraintTiming,
                      validation: crate::storage::ConstraintValidation,
                      table: &TableDef|
     -> Result<(), SqlError> {
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "table_constraints exceeds static capacity"
            ));
        }
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(table.schema.as_str(), arena)?,
                text(name, arena)?,
                text("postgres", arena)?,
                text(table.schema.as_str(), arena)?,
                text(table.name.as_str(), arena)?,
                text(kind, arena)?,
                text(if timing.is_deferrable() { "YES" } else { "NO" }, arena)?,
                text(
                    if timing.initially_deferred() {
                        "YES"
                    } else {
                        "NO"
                    },
                    arena,
                )?,
                text(if validation.enforced() { "YES" } else { "NO" }, arena)?,
                nulls_distinct.map_or(Ok(Datum::Null), |value| text(value, arena))?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for column in table.columns() {
            if column.primary {
                let name = inline_primary_constraint_name(table);
                append(
                    name.as_str(),
                    "PRIMARY KEY",
                    None,
                    crate::storage::ConstraintTiming::NotDeferrable,
                    crate::storage::ConstraintValidation::EnforcedValidated,
                    table,
                )?;
            } else if column.unique {
                let name = inline_unique_constraint_name(table, column);
                append(
                    name.as_str(),
                    "UNIQUE",
                    Some("YES"),
                    crate::storage::ConstraintTiming::NotDeferrable,
                    crate::storage::ConstraintValidation::EnforcedValidated,
                    table,
                )?;
            }
            if column.not_null.is_required() {
                let name = not_null_constraint_name(table, column);
                append(
                    name.as_str(),
                    "CHECK",
                    None,
                    crate::storage::ConstraintTiming::NotDeferrable,
                    crate::storage::ConstraintValidation::EnforcedValidated,
                    table,
                )?;
            }
        }
        for unique in table.uniques() {
            append(
                unique.name.as_str(),
                if unique.is_primary {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                },
                (!unique.is_primary).then_some("YES"),
                unique.timing,
                crate::storage::ConstraintValidation::EnforcedValidated,
                table,
            )?;
        }
        for check in table.checks() {
            append(
                check.name.as_str(),
                "CHECK",
                None,
                crate::storage::ConstraintTiming::NotDeferrable,
                check.validation,
                table,
            )?;
        }
        for foreign_key in table.fkeys() {
            append(
                foreign_key.name.as_str(),
                "FOREIGN KEY",
                None,
                foreign_key.timing,
                foreign_key.validation,
                table,
            )?;
        }
    }
    finish(definition, &output[..count], arena)
}

fn inline_primary_constraint_name(table: &TableDef) -> StackStr<128> {
    stack_format!(128, "{}_pkey", table.name.as_str())
}

fn inline_unique_constraint_name(table: &TableDef, column: &ColumnMeta) -> StackStr<128> {
    stack_format!(128, "{}_{}_key", table.name.as_str(), column.name.as_str())
}

fn not_null_constraint_name(table: &TableDef, column: &ColumnMeta) -> StackStr<128> {
    stack_format!(
        128,
        "{}_{}_not_null",
        table.name.as_str(),
        column.name.as_str()
    )
}

struct KeyConstraint {
    name: StackStr<128>,
    columns: [u16; crate::storage::MAX_INDEX_COLS],
    count: usize,
}

impl KeyConstraint {
    fn columns(&self) -> &[u16] {
        &self.columns[..self.count]
    }
}

fn matching_key_constraint(table: &TableDef, columns: &[u16]) -> Option<KeyConstraint> {
    if columns.len() == 1 {
        let column = &table.columns()[columns[0] as usize];
        if column.primary || column.unique {
            return Some(KeyConstraint {
                name: if column.primary {
                    inline_primary_constraint_name(table)
                } else {
                    inline_unique_constraint_name(table, column)
                },
                columns: [columns[0]; crate::storage::MAX_INDEX_COLS],
                count: 1,
            });
        }
    }
    table.uniques().iter().find_map(|key| {
        (key.n_cols == columns.len()
            && columns.iter().all(|column| key.columns().contains(column))
            && key.columns().iter().all(|column| columns.contains(column)))
        .then(|| KeyConstraint {
            name: stack_format!(128, "{}", key.name.as_str()),
            columns: key.columns,
            count: key.n_cols,
        })
    })
}

fn require_parent_key(
    storage: &Storage,
    txid: u32,
    foreign_key: &crate::storage::ForeignKey,
) -> Result<(TableDef, KeyConstraint), SqlError> {
    let parent_slot = storage
        .find_visible(
            foreign_key.parent_schema.as_str(),
            foreign_key.parent.as_str(),
            txid,
        )
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "foreign key \"{}\" has no visible referenced table",
                foreign_key.name.as_str()
            )
        })?;
    let parent = *storage.table_def(parent_slot, txid);
    let key = matching_key_constraint(&parent, foreign_key.parent_cols()).ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "foreign key \"{}\" has no referenced key",
            foreign_key.name.as_str()
        )
    })?;
    Ok((parent, key))
}

fn info_key_column_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "key_column_usage",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("ordinal_position", ColType::Int4),
            ("position_in_unique_constraint", ColType::Int4),
        ],
    );
    const MAX_ROWS: usize = crate::sql::query::MAX_JOIN_TABLES
        * (crate::storage::MAX_COLUMNS
            + crate::storage::MAX_UNIQUES * crate::storage::MAX_INDEX_COLS
            + crate::storage::MAX_FKEYS * crate::storage::MAX_INDEX_COLS);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    let mut append = |table: &TableDef,
                      name: &str,
                      column: u16,
                      position: usize,
                      parent_position: Option<usize>|
     -> Result<(), SqlError> {
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "key_column_usage exceeds static capacity"
            ));
        }
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(table.schema.as_str(), arena)?,
                text(name, arena)?,
                text("postgres", arena)?,
                text(table.schema.as_str(), arena)?,
                text(table.name.as_str(), arena)?,
                text(table.columns()[column as usize].name.as_str(), arena)?,
                Datum::Int4(position as i32),
                parent_position.map_or(Datum::Null, |value| Datum::Int4(value as i32)),
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for (column_index, column) in table.columns().iter().enumerate() {
            if column.primary || column.unique {
                let name = if column.primary {
                    inline_primary_constraint_name(table)
                } else {
                    inline_unique_constraint_name(table, column)
                };
                append(table, name.as_str(), column_index as u16, 1, None)?;
            }
        }
        for key in table.uniques() {
            for (position, &column) in key.columns().iter().enumerate() {
                append(table, key.name.as_str(), column, position + 1, None)?;
            }
        }
        for foreign_key in table.fkeys() {
            let (_, parent_key) = require_parent_key(storage, txid, foreign_key)?;
            for (position, (&column, &parent_column)) in foreign_key
                .columns()
                .iter()
                .zip(foreign_key.parent_cols())
                .enumerate()
            {
                let parent_position = parent_key
                    .columns()
                    .iter()
                    .position(|candidate| *candidate == parent_column)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "foreign key \"{}\" does not map to its referenced key",
                            foreign_key.name.as_str()
                        )
                    })?;
                append(
                    table,
                    foreign_key.name.as_str(),
                    column,
                    position + 1,
                    Some(parent_position + 1),
                )?;
            }
        }
    }
    finish(definition, &output[..count], arena)
}

fn info_constraint_column_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "constraint_column_usage",
        &[
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
        ],
    );
    let mut total = 0usize;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        total += table
            .columns()
            .iter()
            .filter(|column| column.primary || column.unique)
            .count();
        total += table
            .columns()
            .iter()
            .filter(|column| column.not_null.is_required())
            .count();
        total += table.uniques().iter().map(|key| key.n_cols).sum::<usize>();
        total += table
            .fkeys()
            .iter()
            .map(|key| key.n_parent_cols)
            .sum::<usize>();
        for check in table.checks() {
            let expression = crate::sql::parser::parse_expr(check.expression.as_str(), arena)?;
            total += crate::sql::exec::check_referenced_columns(expression, table)?.count_ones()
                as usize;
        }
    }
    let output = arena
        .alloc_slice_with(total, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    let mut append = |referenced_table: &TableDef,
                      constraint_table: &TableDef,
                      referenced_column: u16,
                      constraint_name: &str|
     -> Result<(), SqlError> {
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(referenced_table.schema.as_str(), arena)?,
                text(referenced_table.name.as_str(), arena)?,
                text(
                    referenced_table.columns()[referenced_column as usize]
                        .name
                        .as_str(),
                    arena,
                )?,
                text("postgres", arena)?,
                text(constraint_table.schema.as_str(), arena)?,
                text(constraint_name, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for (column_index, column) in table.columns().iter().enumerate() {
            if column.primary || column.unique {
                let name = if column.primary {
                    inline_primary_constraint_name(table)
                } else {
                    inline_unique_constraint_name(table, column)
                };
                append(table, table, column_index as u16, name.as_str())?;
            }
            if column.not_null.is_required() {
                let name = not_null_constraint_name(table, column);
                append(table, table, column_index as u16, name.as_str())?;
            }
        }
        for key in table.uniques() {
            for &column in key.columns() {
                append(table, table, column, key.name.as_str())?;
            }
        }
        for check in table.checks() {
            let expression = crate::sql::parser::parse_expr(check.expression.as_str(), arena)?;
            let columns = crate::sql::exec::check_referenced_columns(expression, table)?;
            for (index, _) in table.columns().iter().enumerate() {
                if columns & (1u64 << index) != 0 {
                    append(table, table, index as u16, check.name.as_str())?;
                }
            }
        }
        for foreign_key in table.fkeys() {
            let (parent, _) = require_parent_key(storage, txid, foreign_key)?;
            for &column in foreign_key.parent_cols() {
                append(&parent, table, column, foreign_key.name.as_str())?;
            }
        }
    }
    debug_assert_eq!(count, output.len());
    finish(definition, output, arena)
}

fn info_table_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    info_relation_privileges(storage, txid, arena, "table_privileges", true)
}

fn info_role_table_grants<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    info_relation_privileges(storage, txid, arena, "role_table_grants", false)
}

fn info_sequences<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "sequences",
        &[
            ("sequence_catalog", ColType::Text),
            ("sequence_schema", ColType::Text),
            ("sequence_name", ColType::Text),
            ("data_type", ColType::Text),
            ("numeric_precision", ColType::Int4),
            ("numeric_precision_radix", ColType::Int4),
            ("numeric_scale", ColType::Int4),
            ("start_value", ColType::Text),
            ("minimum_value", ColType::Text),
            ("maximum_value", ColType::Text),
            ("increment", ColType::Text),
            ("cycle_option", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; 256] = [&[]; 256];
    let mut count = 0usize;
    for slot in 0..storage.sequence_count() {
        let sequence = storage.sequence_for(slot, txid);
        if !storage.sequence_slot_visible_to(slot, txid) {
            continue;
        }
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "information_schema.sequences exceeds static capacity"
            ));
        }
        let precision = match sequence.data_type {
            crate::storage::SeqType::Smallint => 16,
            crate::storage::SeqType::Integer => 32,
            crate::storage::SeqType::Bigint => 64,
        };
        let start = stack_format!(32, "{}", sequence.start_value);
        let minimum = stack_format!(32, "{}", sequence.min_value);
        let maximum = stack_format!(32, "{}", sequence.max_value);
        let increment = stack_format!(32, "{}", sequence.increment);
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(sequence.schema.as_str(), arena)?,
                text(sequence.name.as_str(), arena)?,
                text(sequence.data_type.sql_name(), arena)?,
                Datum::Int4(precision),
                Datum::Int4(2),
                Datum::Int4(0),
                text(start.as_str(), arena)?,
                text(minimum.as_str(), arena)?,
                text(maximum.as_str(), arena)?,
                text(increment.as_str(), arena)?,
                text(if sequence.cycle { "YES" } else { "NO" }, arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn info_usage_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "usage_privileges",
        &[
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("object_catalog", ColType::Text),
            ("object_schema", ColType::Text),
            ("object_name", ColType::Text),
            ("object_type", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = 512 + crate::storage::MAX_ACL_ENTRIES;
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0usize;
    let mut append = |object: crate::storage::AccessObject,
                      object_type: &str,
                      grantor: u16,
                      grantee: u16,
                      grantable: bool|
     -> Result<(), SqlError> {
        if !storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid) {
            return Ok(());
        }
        if count == output.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "usage_privileges exceeds static capacity"
            ));
        }
        let (schema, name) = storage.access_object_name_to(object, txid);
        let grantor_name = storage.role_name(grantor as usize, txid);
        let grantee_name = if grantee == crate::storage::PUBLIC_ROLE {
            SqlName::parse("PUBLIC").expect("PUBLIC fits a SQL name")
        } else {
            storage.role_name(grantee as usize, txid)
        };
        output[count] = row(
            &[
                text(grantor_name.as_str(), arena)?,
                text(grantee_name.as_str(), arena)?,
                text("postgres", arena)?,
                text(schema.as_str(), arena)?,
                text(name.as_str(), arena)?,
                text(object_type, arena)?,
                text("USAGE", arena)?,
                text(if grantable { "YES" } else { "NO" }, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    let mut append_object = |object: crate::storage::AccessObject,
                             object_type: &str,
                             public_default: bool|
     -> Result<(), SqlError> {
        let owner = storage.object_owner(object, txid) as u16;
        append(object, object_type, owner, owner, true)?;
        let public_defined = storage.acl_entries().any(|(slot, entry)| {
            entry.object == object
                && storage.acl_identity(slot, txid).0 == crate::storage::PUBLIC_ROLE
        });
        if public_default && !public_defined {
            append(
                object,
                object_type,
                owner,
                crate::storage::PUBLIC_ROLE,
                false,
            )?;
        }
        for (slot, entry) in storage.acl_entries() {
            if entry.object != object {
                continue;
            }
            let (privileges, options) = storage.acl_state(slot, txid);
            if !privileges.contains(crate::storage::PrivilegeSet::USAGE) {
                continue;
            }
            let (grantee, grantor) = storage.acl_identity(slot, txid);
            if grantee == owner && grantor == owner {
                continue;
            }
            append(
                object,
                object_type,
                grantor,
                grantee,
                options.contains(crate::storage::PrivilegeSet::USAGE),
            )?;
        }
        Ok(())
    };
    for slot in 0..storage.sequence_count() {
        if !storage.sequence_slot_visible_to(slot, txid) {
            continue;
        }
        append_object(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Sequence,
                slot: slot as u16,
            },
            "SEQUENCE",
            false,
        )?;
    }
    for slot in 0..storage.domain_count() {
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        append_object(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Domain,
                slot: slot as u16,
            },
            "DOMAIN",
            true,
        )?;
    }
    finish(definition, &output[..count], arena)
}

fn info_relation_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    relation_name: &str,
    include_public: bool,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        relation_name,
        &[
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
            ("with_hierarchy", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = 8 * (512 + crate::storage::MAX_ACL_ENTRIES);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0usize;
    let mut append = |object: crate::storage::AccessObject,
                      grantor: u16,
                      grantee: u16,
                      privilege_mask: crate::storage::PrivilegeSet,
                      grant_options: crate::storage::PrivilegeSet|
     -> Result<(), SqlError> {
        if !include_public && grantee == crate::storage::PUBLIC_ROLE {
            return Ok(());
        }
        if !storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid) {
            return Ok(());
        }
        let (schema, name) = storage.access_object_name_to(object, txid);
        let privilege_names = [
            (crate::storage::PrivilegeSet::SELECT, "SELECT"),
            (crate::storage::PrivilegeSet::INSERT, "INSERT"),
            (crate::storage::PrivilegeSet::UPDATE, "UPDATE"),
            (crate::storage::PrivilegeSet::DELETE, "DELETE"),
            (crate::storage::PrivilegeSet::TRUNCATE, "TRUNCATE"),
            (crate::storage::PrivilegeSet::REFERENCES, "REFERENCES"),
            (crate::storage::PrivilegeSet::TRIGGER, "TRIGGER"),
        ];
        for (privilege, privilege_name) in privilege_names {
            if !privilege_mask.contains(privilege) {
                continue;
            }
            if count == output.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table_privileges exceeds static capacity"
                ));
            }
            let grantor_name = storage.role_name(grantor as usize, txid);
            let grantee_name = if grantee == crate::storage::PUBLIC_ROLE {
                SqlName::parse("PUBLIC").expect("PUBLIC fits a SQL name")
            } else {
                storage.role_name(grantee as usize, txid)
            };
            output[count] = row(
                &[
                    text(grantor_name.as_str(), arena)?,
                    text(grantee_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(schema.as_str(), arena)?,
                    text(name.as_str(), arena)?,
                    text(privilege_name, arena)?,
                    text(
                        if grant_options.contains(privilege) {
                            "YES"
                        } else {
                            "NO"
                        },
                        arena,
                    )?,
                    text(
                        if privilege == crate::storage::PrivilegeSet::SELECT {
                            "YES"
                        } else {
                            "NO"
                        },
                        arena,
                    )?,
                ],
                arena,
            )?;
            count += 1;
        }
        Ok(())
    };
    let mut append_object = |object: crate::storage::AccessObject| -> Result<(), SqlError> {
        let owner = storage.object_owner(object, txid) as u16;
        append(
            object,
            owner,
            owner,
            crate::storage::PrivilegeSet::TABLE_ALL,
            crate::storage::PrivilegeSet::TABLE_ALL,
        )?;
        for (slot, entry) in storage.acl_entries() {
            if entry.object != object {
                continue;
            }
            let (privileges, options) = storage.acl_state(slot, txid);
            if privileges.0 == 0 {
                continue;
            }
            let (grantee, grantor) = storage.acl_identity(slot, txid);
            if grantee == owner && grantor == owner {
                continue;
            }
            append(object, grantor, grantee, privileges, options)?;
        }
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let (class, object_slot) =
            match storage.matview_slot(table.schema.as_str(), table.name.as_str(), txid) {
                Some(matview) => (crate::storage::AccessClass::MaterializedView, matview),
                None => (crate::storage::AccessClass::Table, slot),
            };
        append_object(crate::storage::AccessObject {
            class,
            slot: object_slot as u16,
        })?;
    }
    for (slot, _) in storage.views_visible_to(txid) {
        append_object(crate::storage::AccessObject {
            class: crate::storage::AccessClass::View,
            slot: slot as u16,
        })?;
    }
    finish(definition, &output[..count], arena)
}

fn column_privilege_count(
    storage: &Storage,
    txid: u32,
    object: crate::storage::AccessObject,
    include_public: bool,
) -> usize {
    let visible_privileges = [
        crate::storage::PrivilegeSet::SELECT,
        crate::storage::PrivilegeSet::INSERT,
        crate::storage::PrivilegeSet::UPDATE,
        crate::storage::PrivilegeSet::REFERENCES,
    ];
    let owner = storage.object_owner(object, txid) as u16;
    let mut count = storage.role_is_enabled(owner, txid) as usize * visible_privileges.len();
    for (slot, entry) in storage.acl_entries() {
        if entry.object != object {
            continue;
        }
        let (grantee, grantor) = storage.acl_identity(slot, txid);
        if (!include_public && grantee == crate::storage::PUBLIC_ROLE)
            || (!storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid))
            || (grantee == owner && grantor == owner)
        {
            continue;
        }
        let (privileges, _) = storage.acl_state(slot, txid);
        count += visible_privileges
            .iter()
            .filter(|privilege| privileges.contains(**privilege))
            .count();
    }
    count
}

fn info_column_privileges<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    include_public: bool,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        if include_public {
            "column_privileges"
        } else {
            "role_column_grants"
        },
        &[
            ("grantor", ColType::Text),
            ("grantee", ColType::Text),
            ("table_catalog", ColType::Text),
            ("table_schema", ColType::Text),
            ("table_name", ColType::Text),
            ("column_name", ColType::Text),
            ("privilege_type", ColType::Text),
            ("is_grantable", ColType::Text),
        ],
    );
    let mut output_count = 0usize;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let object = crate::storage::AccessObject {
            class: match storage.matview_slot(table.schema.as_str(), table.name.as_str(), txid) {
                Some(_) => crate::storage::AccessClass::MaterializedView,
                None => crate::storage::AccessClass::Table,
            },
            slot: storage
                .matview_slot(table.schema.as_str(), table.name.as_str(), txid)
                .unwrap_or(slot) as u16,
        };
        output_count = output_count
            .checked_add(
                table
                    .columns()
                    .len()
                    .checked_mul(column_privilege_count(
                        storage,
                        txid,
                        object,
                        include_public,
                    ))
                    .ok_or_else(|| {
                        catalog_capacity_exceeded("information_schema.column_privileges")
                    })?,
            )
            .ok_or_else(|| catalog_capacity_exceeded("information_schema.column_privileges"))?;
    }
    for (slot, _) in storage.column_acl_entries() {
        let (grantee, grantor) = storage.column_acl_identity(slot, txid);
        if (!include_public && grantee == crate::storage::PUBLIC_ROLE)
            || (!storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid))
        {
            continue;
        }
        let (privileges, _) = storage.column_acl_state(slot, txid);
        output_count = output_count
            .checked_add(
                [
                    crate::storage::PrivilegeSet::SELECT,
                    crate::storage::PrivilegeSet::INSERT,
                    crate::storage::PrivilegeSet::UPDATE,
                    crate::storage::PrivilegeSet::REFERENCES,
                ]
                .iter()
                .filter(|privilege| privileges.contains(**privilege))
                .count(),
            )
            .ok_or_else(|| catalog_capacity_exceeded("information_schema.column_privileges"))?;
    }
    for (view_slot, view) in storage.views_visible_to(txid) {
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let column_count = describe_view(storage, txid, view, arena, &mut columns)?;
        output_count = output_count
            .checked_add(
                column_count
                    .checked_mul(column_privilege_count(
                        storage,
                        txid,
                        crate::storage::AccessObject {
                            class: crate::storage::AccessClass::View,
                            slot: view_slot as u16,
                        },
                        include_public,
                    ))
                    .ok_or_else(|| {
                        catalog_capacity_exceeded("information_schema.column_privileges")
                    })?,
            )
            .ok_or_else(|| catalog_capacity_exceeded("information_schema.column_privileges"))?;
    }
    let output = arena
        .alloc_slice_with(output_count, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    let mut append = |schema: &str,
                      table: &str,
                      column: &str,
                      grantor: u16,
                      grantee: u16,
                      privileges: crate::storage::PrivilegeSet,
                      grant_options: crate::storage::PrivilegeSet|
     -> Result<(), SqlError> {
        if (!include_public && grantee == crate::storage::PUBLIC_ROLE)
            || (!storage.role_is_enabled(grantor, txid) && !storage.role_is_enabled(grantee, txid))
        {
            return Ok(());
        }
        let grantor_name = storage.role_name(grantor as usize, txid);
        let grantee_name = if grantee == crate::storage::PUBLIC_ROLE {
            SqlName::parse("PUBLIC").expect("PUBLIC fits a SQL name")
        } else {
            storage.role_name(grantee as usize, txid)
        };
        for (privilege, privilege_name) in [
            (crate::storage::PrivilegeSet::SELECT, "SELECT"),
            (crate::storage::PrivilegeSet::INSERT, "INSERT"),
            (crate::storage::PrivilegeSet::UPDATE, "UPDATE"),
            (crate::storage::PrivilegeSet::REFERENCES, "REFERENCES"),
        ] {
            if !privileges.contains(privilege) {
                continue;
            }
            debug_assert!(count < output.len());
            output[count] = row(
                &[
                    text(grantor_name.as_str(), arena)?,
                    text(grantee_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(schema, arena)?,
                    text(table, arena)?,
                    text(column, arena)?,
                    text(privilege_name, arena)?,
                    text(
                        if grant_options.contains(privilege) {
                            "YES"
                        } else {
                            "NO"
                        },
                        arena,
                    )?,
                ],
                arena,
            )?;
            count += 1;
        }
        Ok(())
    };
    let mut append_relation = |object: crate::storage::AccessObject,
                               schema: &str,
                               table: &str,
                               columns: &[ColumnMeta]|
     -> Result<(), SqlError> {
        let owner = storage.object_owner(object, txid) as u16;
        for column in columns {
            append(
                schema,
                table,
                column.name.as_str(),
                owner,
                owner,
                crate::storage::PrivilegeSet::SELECT
                    .union(crate::storage::PrivilegeSet::INSERT)
                    .union(crate::storage::PrivilegeSet::UPDATE)
                    .union(crate::storage::PrivilegeSet::REFERENCES),
                crate::storage::PrivilegeSet::SELECT
                    .union(crate::storage::PrivilegeSet::INSERT)
                    .union(crate::storage::PrivilegeSet::UPDATE)
                    .union(crate::storage::PrivilegeSet::REFERENCES),
            )?;
            for (slot, entry) in storage.acl_entries() {
                if entry.object != object {
                    continue;
                }
                let (grantee, grantor) = storage.acl_identity(slot, txid);
                if grantee == owner && grantor == owner {
                    continue;
                }
                let (privileges, grant_options) = storage.acl_state(slot, txid);
                append(
                    schema,
                    table,
                    column.name.as_str(),
                    grantor,
                    grantee,
                    privileges,
                    grant_options,
                )?;
            }
        }
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        let object = match storage.matview_slot(table.schema.as_str(), table.name.as_str(), txid) {
            Some(matview_slot) => crate::storage::AccessObject {
                class: crate::storage::AccessClass::MaterializedView,
                slot: matview_slot as u16,
            },
            None => crate::storage::AccessObject {
                class: crate::storage::AccessClass::Table,
                slot: slot as u16,
            },
        };
        append_relation(
            object,
            table.schema.as_str(),
            table.name.as_str(),
            table.columns(),
        )?;
    }
    for (slot, view) in storage.views_visible_to(txid) {
        let mut descriptions = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let description_count = describe_view(storage, txid, view, arena, &mut descriptions)?;
        let mut columns = [ColumnMeta::EMPTY; super::exec::MAX_PROJ];
        for (index, description) in descriptions[..description_count].iter().enumerate() {
            let (ctype, user_type) = view_column_catalog_type(storage, txid, description.type_oid)?;
            columns[index] = ColumnMeta {
                name: SqlName::parse(description.name)?,
                ctype,
                type_mod: description.type_mod,
                collation: if ctype.is_collatable() {
                    crate::sql::ast::Collation::Default
                } else {
                    crate::sql::ast::Collation::None
                },
                not_null: crate::storage::NotNullOrigin::Nullable,
                unique: false,
                primary: false,
                auto_increment: false,
                default: crate::storage::ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type,
                statistics_target: -1,
            };
        }
        append_relation(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::View,
                slot: slot as u16,
            },
            view.schema_for(txid).as_str(),
            view.name_for(txid).as_str(),
            &columns[..description_count],
        )?;
    }
    for (slot, entry) in storage.column_acl_entries() {
        let relation = entry.target.relation();
        if !storage.access_object_visible_to(relation, txid) {
            continue;
        }
        let (grantee, grantor) = storage.column_acl_identity(slot, txid);
        let (privileges, grant_options) = storage.column_acl_state(slot, txid);
        if privileges.0 == 0 {
            continue;
        }
        let (schema, table) = storage.access_object_name_to(relation, txid);
        let column = match relation.class {
            crate::storage::AccessClass::Table => {
                storage.table_def(relation.slot as usize, txid).columns()
                    [entry.target.column() as usize]
                    .name
            }
            crate::storage::AccessClass::MaterializedView => {
                let table_slot = storage
                    .find_table(schema.as_str(), table.as_str())
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "materialized view column privilege has no table row"
                        )
                    })?;
                storage.table_def(table_slot, txid).columns()[entry.target.column() as usize].name
            }
            crate::storage::AccessClass::View => {
                let view = storage.view(relation.slot as usize);
                let mut descriptions =
                    [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
                let described = describe_view(storage, txid, view, arena, &mut descriptions)?;
                if entry.target.column() as usize >= described {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "view column privilege refers to an absent column"
                    ));
                }
                SqlName::parse(descriptions[entry.target.column() as usize].name)?
            }
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid column privilege target"
                ));
            }
        };
        append(
            schema.as_str(),
            table.as_str(),
            column.as_str(),
            grantor,
            grantee,
            privileges,
            grant_options,
        )?;
    }
    debug_assert_eq!(count, output.len());
    finish(definition, &output[..count], arena)
}

fn fk_action_name(action: crate::storage::FkAction) -> &'static str {
    match action {
        crate::storage::FkAction::NoAction => "NO ACTION",
        crate::storage::FkAction::Restrict => "RESTRICT",
        crate::storage::FkAction::Cascade => "CASCADE",
        crate::storage::FkAction::SetNull => "SET NULL",
        crate::storage::FkAction::SetDefault => "SET DEFAULT",
    }
}

fn info_referential_constraints<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "referential_constraints",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("unique_constraint_catalog", ColType::Text),
            ("unique_constraint_schema", ColType::Text),
            ("unique_constraint_name", ColType::Text),
            ("match_option", ColType::Text),
            ("update_rule", ColType::Text),
            ("delete_rule", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::sql::query::MAX_JOIN_TABLES * crate::storage::MAX_FKEYS;
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for foreign_key in table.fkeys() {
            if count == output.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "referential_constraints exceeds static capacity"
                ));
            }
            let (parent, parent_key) = require_parent_key(storage, txid, foreign_key)?;
            output[count] = row(
                &[
                    text("postgres", arena)?,
                    text(table.schema.as_str(), arena)?,
                    text(foreign_key.name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(parent.schema.as_str(), arena)?,
                    text(parent_key.name.as_str(), arena)?,
                    text("NONE", arena)?,
                    text(fk_action_name(foreign_key.on_update), arena)?,
                    text(fk_action_name(foreign_key.on_delete), arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &output[..count], arena)
}

fn info_domains<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "domains",
        &[
            ("domain_catalog", ColType::Text),
            ("domain_schema", ColType::Text),
            ("domain_name", ColType::Text),
            ("data_type", ColType::Text),
            ("character_maximum_length", ColType::Int4),
            ("character_octet_length", ColType::Int4),
            ("character_set_catalog", ColType::Text),
            ("character_set_schema", ColType::Text),
            ("character_set_name", ColType::Text),
            ("collation_catalog", ColType::Text),
            ("collation_schema", ColType::Text),
            ("collation_name", ColType::Text),
            ("numeric_precision", ColType::Int4),
            ("numeric_precision_radix", ColType::Int4),
            ("numeric_scale", ColType::Int4),
            ("datetime_precision", ColType::Int4),
            ("interval_type", ColType::Text),
            ("interval_precision", ColType::Int4),
            ("domain_default", ColType::Text),
            ("udt_catalog", ColType::Text),
            ("udt_schema", ColType::Text),
            ("udt_name", ColType::Text),
            ("scope_catalog", ColType::Text),
            ("scope_schema", ColType::Text),
            ("scope_name", ColType::Text),
            ("maximum_cardinality", ColType::Int4),
            ("dtd_identifier", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_DOMAINS] = [&[]; crate::storage::MAX_DOMAINS];
    let mut count = 0;
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        let type_mod = TypeMod::decode(domain.base, domain.base_type_mod);
        let (character_length, numeric_precision, numeric_radix, numeric_scale, datetime_precision) =
            information_schema_scalar_metadata(domain.base, type_mod);
        let interval_type = information_schema_interval_type(type_mod);
        let (data_type, udt_schema, udt_name) = match domain.base_domain {
            Some(parent) => (
                "USER-DEFINED",
                parent.schema,
                StackStr::<64>::from_str(parent.name.as_str()),
            ),
            None => (
                domain.base.name(),
                SqlName::parse("pg_catalog").expect("catalog schema fits"),
                StackStr::<64>::from_str(domain.base.catalog_name()),
            ),
        };
        let default = match domain.default_expr {
            Some(value) => text(value.as_str(), arena)?,
            None => Datum::Null,
        };
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(domain.schema.as_str(), arena)?,
                text(domain.name.as_str(), arena)?,
                text(data_type, arena)?,
                character_length.map_or(Datum::Null, Datum::Int4),
                character_length.map_or(Datum::Null, Datum::Int4),
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                numeric_precision.map_or(Datum::Null, Datum::Int4),
                numeric_radix.map_or(Datum::Null, Datum::Int4),
                numeric_scale.map_or(Datum::Null, Datum::Int4),
                datetime_precision.map_or(Datum::Null, Datum::Int4),
                match interval_type {
                    Some(value) => text(value.as_str(), arena)?,
                    None => Datum::Null,
                },
                Datum::Null,
                default,
                text("postgres", arena)?,
                text(udt_schema.as_str(), arena)?,
                text(udt_name.as_str(), arena)?,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                text("1", arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn info_domain_constraints<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "domain_constraints",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("domain_catalog", ColType::Text),
            ("domain_schema", ColType::Text),
            ("domain_name", ColType::Text),
            ("is_deferrable", ColType::Text),
            ("initially_deferred", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::storage::MAX_DOMAINS * (crate::storage::MAX_DOMAIN_CHECKS + 1);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        for check in domain.checks() {
            output[count] = row(
                &[
                    text("postgres", arena)?,
                    text(domain.schema.as_str(), arena)?,
                    text(check.name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(domain.schema.as_str(), arena)?,
                    text(domain.name.as_str(), arena)?,
                    text("NO", arena)?,
                    text("NO", arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
        if domain.not_null {
            let name = stack_format!(128, "{}_not_null", domain.name.as_str());
            output[count] = row(
                &[
                    text("postgres", arena)?,
                    text(domain.schema.as_str(), arena)?,
                    text(name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(domain.schema.as_str(), arena)?,
                    text(domain.name.as_str(), arena)?,
                    text("NO", arena)?,
                    text("NO", arena)?,
                ],
                arena,
            )?;
            count += 1;
        }
    }
    finish(definition, &output[..count], arena)
}

fn info_check_constraints<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "check_constraints",
        &[
            ("constraint_catalog", ColType::Text),
            ("constraint_schema", ColType::Text),
            ("constraint_name", ColType::Text),
            ("check_clause", ColType::Text),
        ],
    );
    const MAX_ROWS: usize = crate::sql::query::MAX_JOIN_TABLES
        * (crate::storage::MAX_CHECKS + MAX_COLUMNS)
        + crate::storage::MAX_DOMAINS * (crate::storage::MAX_DOMAIN_CHECKS + 1);
    let mut output: [&[Datum]; MAX_ROWS] = [&[]; MAX_ROWS];
    let mut count = 0;
    let mut append = |schema: &str, name: &str, clause: &str| -> Result<(), SqlError> {
        if count == output.len() {
            return Err(catalog_capacity_exceeded(
                "information_schema.check_constraints",
            ));
        }
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(schema, arena)?,
                text(name, arena)?,
                text(clause, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for check in table.checks() {
            let clause = stack_format!(1024, "({})", check.expression.as_str());
            append(table.schema.as_str(), check.name.as_str(), clause.as_str())?;
        }
        for column in table.columns() {
            if column.not_null.is_required() {
                let name = not_null_constraint_name(table, column);
                let clause = stack_format!(256, "{} IS NOT NULL", column.name.as_str());
                append(table.schema.as_str(), name.as_str(), clause.as_str())?;
            }
        }
    }
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        for check in domain.checks() {
            let clause = stack_format!(1024, "({})", check.expression.as_str());
            append(domain.schema.as_str(), check.name.as_str(), clause.as_str())?;
        }
        if domain.not_null {
            let name = stack_format!(128, "{}_not_null", domain.name.as_str());
            append(domain.schema.as_str(), name.as_str(), "VALUE IS NOT NULL")?;
        }
    }
    finish(definition, &output[..count], arena)
}

#[derive(Clone, Copy)]
enum InformationSchemaTypeUsage {
    Domain,
    UnderlyingType,
}

fn info_column_domain_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    info_column_type_usage(storage, txid, arena, InformationSchemaTypeUsage::Domain)
}

fn info_column_udt_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    info_column_type_usage(
        storage,
        txid,
        arena,
        InformationSchemaTypeUsage::UnderlyingType,
    )
}

fn info_column_type_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    usage: InformationSchemaTypeUsage,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        match usage {
            InformationSchemaTypeUsage::Domain => "column_domain_usage",
            InformationSchemaTypeUsage::UnderlyingType => "column_udt_usage",
        },
        match usage {
            InformationSchemaTypeUsage::Domain => &[
                ("domain_catalog", ColType::Text),
                ("domain_schema", ColType::Text),
                ("domain_name", ColType::Text),
                ("table_catalog", ColType::Text),
                ("table_schema", ColType::Text),
                ("table_name", ColType::Text),
                ("column_name", ColType::Text),
            ],
            InformationSchemaTypeUsage::UnderlyingType => &[
                ("udt_catalog", ColType::Text),
                ("udt_schema", ColType::Text),
                ("udt_name", ColType::Text),
                ("table_catalog", ColType::Text),
                ("table_schema", ColType::Text),
                ("table_name", ColType::Text),
                ("column_name", ColType::Text),
            ],
        },
    );
    let table_capacity = storage
        .table_count()
        .checked_mul(MAX_COLUMNS)
        .ok_or_else(|| catalog_capacity_exceeded("information_schema column type usage"))?;
    let view_capacity = storage
        .view_count()
        .checked_mul(super::exec::MAX_PROJ)
        .ok_or_else(|| catalog_capacity_exceeded("information_schema column type usage"))?;
    let output = arena
        .alloc_slice_with(
            table_capacity
                .checked_add(view_capacity)
                .ok_or_else(|| catalog_capacity_exceeded("information_schema column type usage"))?,
            |_| &[] as &[Datum],
        )
        .map_err(|_| arena_full())?;
    let mut count = 0;
    let mut append =
        |schema: &str, table: &str, column: &str, metadata: &ColumnMeta| -> Result<(), SqlError> {
            let declared_oid = catalog_column_type_oid(storage, metadata, txid)?;
            let identity =
                information_schema_usage_type(storage, txid, metadata, declared_oid, usage)?;
            let Some((type_schema, type_name)) = identity else {
                return Ok(());
            };
            debug_assert!(count < output.len());
            output[count] = row(
                &[
                    text("postgres", arena)?,
                    text(type_schema.as_str(), arena)?,
                    text(type_name.as_str(), arena)?,
                    text("postgres", arena)?,
                    text(schema, arena)?,
                    text(table, arena)?,
                    text(column, arena)?,
                ],
                arena,
            )?;
            count += 1;
            Ok(())
        };
    for slot in 0..storage.table_count() {
        if !storage.table_slot_visible_to(slot, txid) {
            continue;
        }
        let table = storage.table_def(slot, txid);
        for column in table.columns() {
            append(
                table.schema.as_str(),
                table.name.as_str(),
                column.name.as_str(),
                column,
            )?;
        }
    }
    for (_, view) in storage.views_visible_to(txid) {
        let mut columns = [super::types::ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
        let column_count = describe_view(storage, txid, view, arena, &mut columns)?;
        for column in &columns[..column_count] {
            let (ctype, user_type) = view_column_catalog_type(storage, txid, column.type_oid)?;
            let metadata = ColumnMeta {
                name: SqlName::EMPTY,
                ctype,
                type_mod: column.type_mod,
                collation: if ctype.is_collatable() {
                    crate::sql::ast::Collation::Default
                } else {
                    crate::sql::ast::Collation::None
                },
                not_null: crate::storage::NotNullOrigin::Nullable,
                unique: false,
                primary: false,
                auto_increment: false,
                default: crate::storage::ColumnDefault::NONE,
                is_identity: false,
                identity_always: false,
                auto_increment_step: 1,
                user_type,
                statistics_target: -1,
            };
            append(
                view.schema_for(txid).as_str(),
                view.name_for(txid).as_str(),
                column.name,
                &metadata,
            )?;
        }
    }
    finish(definition, &output[..count], arena)
}

fn information_schema_usage_type(
    storage: &Storage,
    txid: u32,
    column: &ColumnMeta,
    declared_oid: i32,
    usage: InformationSchemaTypeUsage,
) -> Result<Option<(SqlName, StackStr<64>)>, SqlError> {
    use crate::sql::types::oid;
    let is_domain = (oid::FIRST_DOMAIN..oid::FIRST_DOMAIN + crate::storage::MAX_DOMAINS as i32)
        .contains(&declared_oid);
    match usage {
        InformationSchemaTypeUsage::Domain => Ok(is_domain.then(|| {
            let domain = storage.domain_for((declared_oid - oid::FIRST_DOMAIN) as usize, txid);
            (domain.schema, StackStr::from_str(domain.name.as_str()))
        })),
        InformationSchemaTypeUsage::UnderlyingType if is_domain => {
            let domain = storage.domain_for((declared_oid - oid::FIRST_DOMAIN) as usize, txid);
            Ok(Some(match domain.base_domain {
                Some(parent) => (parent.schema, StackStr::from_str(parent.name.as_str())),
                None => (
                    SqlName::parse("pg_catalog").expect("catalog schema fits"),
                    StackStr::from_str(domain.base.catalog_name()),
                ),
            }))
        }
        InformationSchemaTypeUsage::UnderlyingType => Ok(Some(match column.user_type {
            Some(identity) => {
                let mut name = StackStr::<64>::new();
                if matches!(column.ctype, ColType::Array(_)) {
                    use core::fmt::Write as _;
                    let _ = write!(name, "_{}", identity.name.as_str());
                } else {
                    name = StackStr::from_str(identity.name.as_str());
                }
                (identity.schema, name)
            }
            None => (
                SqlName::parse("pg_catalog").expect("catalog schema fits"),
                StackStr::from_str(column.ctype.catalog_name()),
            ),
        })),
    }
}

fn info_domain_udt_usage<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "domain_udt_usage",
        &[
            ("udt_catalog", ColType::Text),
            ("udt_schema", ColType::Text),
            ("udt_name", ColType::Text),
            ("domain_catalog", ColType::Text),
            ("domain_schema", ColType::Text),
            ("domain_name", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_DOMAINS] = [&[]; crate::storage::MAX_DOMAINS];
    let mut count = 0;
    for slot in 0..storage.domain_count() {
        let domain = storage.domain_for(slot, txid);
        if !storage.domain_slot_visible_to(slot, txid) {
            continue;
        }
        let (udt_schema, udt_name) = match domain.base_domain {
            Some(parent) => (
                parent.schema,
                StackStr::<64>::from_str(parent.name.as_str()),
            ),
            None => (
                SqlName::parse("pg_catalog").expect("catalog schema fits"),
                StackStr::<64>::from_str(domain.base.catalog_name()),
            ),
        };
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(udt_schema.as_str(), arena)?,
                text(udt_name.as_str(), arena)?,
                text("postgres", arena)?,
                text(domain.schema.as_str(), arena)?,
                text(domain.name.as_str(), arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

type InformationSchemaScalarMetadata = (
    Option<i32>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
);

fn information_schema_scalar_metadata(
    ctype: ColType,
    type_mod: TypeMod,
) -> InformationSchemaScalarMetadata {
    match type_mod {
        TypeMod::Length(length) if matches!(ctype, ColType::Varchar | ColType::Bpchar) => {
            (Some(length as i32), None, None, None, None)
        }
        TypeMod::NumericPS { precision, scale } => (
            None,
            Some(precision as i32),
            Some(10),
            Some(scale as i32),
            None,
        ),
        TypeMod::TemporalPrecision(precision) => (None, None, None, None, Some(precision as i32)),
        TypeMod::IntervalMod { precision, .. } => {
            (None, None, None, None, Some(precision.map_or(6, i32::from)))
        }
        _ => match ctype {
            ColType::Int2 => (None, Some(16), Some(2), Some(0), None),
            ColType::Int4 => (None, Some(32), Some(2), Some(0), None),
            ColType::Int8 => (None, Some(64), Some(2), Some(0), None),
            ColType::Float4 => (None, Some(24), Some(2), None, None),
            ColType::Float8 => (None, Some(53), Some(2), None, None),
            ColType::Interval => (None, None, None, None, Some(6)),
            _ => (None, None, None, None, None),
        },
    }
}

fn information_schema_interval_type(type_mod: TypeMod) -> Option<StackStr<48>> {
    use core::fmt::Write as _;
    let TypeMod::IntervalMod { range, precision } = type_mod else {
        return None;
    };
    let fields = range.information_schema_name()?;
    let mut output = StackStr::<48>::from_str(fields);
    if let Some(precision) = precision {
        let _ = write!(output, "({})", precision);
    }
    Some(output)
}

fn info_schemata<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let def = def_of(
        "schemata",
        &[
            ("catalog_name", ColType::Text),
            ("schema_name", ColType::Text),
            ("schema_owner", ColType::Text),
            ("default_character_set_catalog", ColType::Text),
            ("default_character_set_schema", ColType::Text),
            ("default_character_set_name", ColType::Text),
            ("sql_path", ColType::Text),
        ],
    );
    let mut out: [&[Datum]; 2 + crate::storage::MAX_SCHEMAS] =
        [&[]; 2 + crate::storage::MAX_SCHEMAS];
    let mut n = 0;
    for (slot, schema) in storage.visible_schemas(txid) {
        out[n] = row(
            &[
                text("postgres", arena)?,
                text(
                    arena
                        .alloc_str(schema.name.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    arena,
                )?,
                text(
                    catalog_owner_name(
                        storage,
                        catalog_owner(
                            storage,
                            crate::storage::AccessObject {
                                class: crate::storage::AccessClass::Schema,
                                slot: slot as u16,
                            },
                            txid,
                        ),
                        txid,
                    )
                    .as_str(),
                    arena,
                )?,
                Datum::Null,
                Datum::Null,
                Datum::Null,
                Datum::Null,
            ],
            arena,
        )?;
        n += 1;
    }
    out[n] = row(
        &[
            text("postgres", arena)?,
            text("pg_catalog", arena)?,
            text("postgres", arena)?,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
        ],
        arena,
    )?;
    n += 1;
    out[n] = row(
        &[
            text("postgres", arena)?,
            text("information_schema", arena)?,
            text("postgres", arena)?,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
        ],
        arena,
    )?;
    n += 1;
    finish(def, &out[..n], arena)
}

fn info_collations<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "collations",
        &[
            ("collation_catalog", ColType::Text),
            ("collation_schema", ColType::Text),
            ("collation_name", ColType::Text),
            ("pad_attribute", ColType::Text),
        ],
    );
    let output = arena
        .alloc_slice_with(4 + crate::storage::MAX_COLLATIONS, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (index, collation) in crate::sql::ast::Collation::BUILTIN.iter().enumerate() {
        output[index] = row(
            &[
                text("postgres", arena)?,
                text("pg_catalog", arena)?,
                text(collation.name(), arena)?,
                text("NO PAD", arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    for (_, collation) in storage.collations_visible_to(txid) {
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(collation.schema.as_str(), arena)?,
                text(collation.name.as_str(), arena)?,
                text("NO PAD", arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn info_collation_character_set_applicability<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        "collation_character_set_applicability",
        &[
            ("collation_catalog", ColType::Text),
            ("collation_schema", ColType::Text),
            ("collation_name", ColType::Text),
            ("character_set_catalog", ColType::Text),
            ("character_set_schema", ColType::Text),
            ("character_set_name", ColType::Text),
        ],
    );
    let output = arena
        .alloc_slice_with(4 + crate::storage::MAX_COLLATIONS, |_| &[] as &[Datum])
        .map_err(|_| arena_full())?;
    let mut count = 0;
    for (index, collation) in crate::sql::ast::Collation::BUILTIN.iter().enumerate() {
        output[index] = row(
            &[
                text("postgres", arena)?,
                text("pg_catalog", arena)?,
                text(collation.name(), arena)?,
                Datum::Null,
                Datum::Null,
                text("UTF8", arena)?,
            ],
            arena,
        )?;
        count += 1;
    }
    for (_, collation) in storage.collations_visible_to(txid) {
        output[count] = row(
            &[
                text("postgres", arena)?,
                text(collation.schema.as_str(), arena)?,
                text(collation.name.as_str(), arena)?,
                Datum::Null,
                Datum::Null,
                text(
                    collation
                        .encoding
                        .unwrap_or(crate::storage::PgEncoding::UTF8)
                        .name(),
                    arena,
                )?,
            ],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn info_enabled_roles<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of("enabled_roles", &[("role_name", ColType::Text)]);
    let mut output: [&[Datum]; crate::storage::MAX_ROLES] = [&[]; crate::storage::MAX_ROLES];
    let mut count = 0;
    for slot in 0..storage.role_count() {
        if !storage.role(slot).visible_to(txid) || !storage.role_is_enabled(slot as u16, txid) {
            continue;
        }
        output[count] = row(
            &[text(storage.role_name(slot, txid).as_str(), arena)?],
            arena,
        )?;
        count += 1;
    }
    finish(definition, &output[..count], arena)
}

fn info_applicable_roles<'a>(
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    administrators_only: bool,
) -> Result<SynthTable<'a>, SqlError> {
    let definition = def_of(
        if administrators_only {
            "administrable_role_authorizations"
        } else {
            "applicable_roles"
        },
        &[
            ("grantee", ColType::Text),
            ("role_name", ColType::Text),
            ("is_grantable", ColType::Text),
        ],
    );
    let mut output: [&[Datum]; crate::storage::MAX_ROLE_MEMBERSHIPS + 1] =
        [&[]; crate::storage::MAX_ROLE_MEMBERSHIPS + 1];
    let mut count = 0;
    let mut append = |grantee: &str, role: &str, admin: bool| -> Result<(), SqlError> {
        if administrators_only && !admin {
            return Ok(());
        }
        if count == output.len() {
            return Err(catalog_capacity_exceeded(
                "information_schema.applicable_roles",
            ));
        }
        output[count] = row(
            &[
                text(grantee, arena)?,
                text(role, arena)?,
                text(if admin { "YES" } else { "NO" }, arena)?,
            ],
            arena,
        )?;
        count += 1;
        Ok(())
    };
    for slot in 0..storage.role_membership_count() {
        let membership = storage.role_membership(slot);
        if !membership.visible_to(txid) || !storage.role_is_enabled(membership.member, txid) {
            continue;
        }
        append(
            storage.role_name(membership.member as usize, txid).as_str(),
            storage.role_name(membership.role as usize, txid).as_str(),
            membership.options_to(txid).admin,
        )?;
    }
    if storage.role_count() > 0 && storage.role_is_enabled(0, txid) {
        append(
            storage.role_name(0, txid).as_str(),
            "pg_database_owner",
            false,
        )?;
    }
    finish(definition, &output[..count], arena)
}

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "catalog relation exceeds the statement arena"
    )
}

fn alloc_rendered<'a, const N: usize>(
    rendered: &StackStr<N>,
    too_long: &'static str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    if rendered.is_truncated() {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", too_long));
    }
    arena.alloc_str(rendered.as_str()).map_err(|_| arena_full())
}

fn write_identifier(out: &mut impl core::fmt::Write, name: &str) {
    if !ident_needs_quotes(name) {
        let _ = out.write_str(name);
        return;
    }
    let _ = out.write_char('"');
    for character in name.chars() {
        if character == '"' {
            let _ = out.write_char('"');
        }
        let _ = out.write_char(character);
    }
    let _ = out.write_char('"');
}
