//! System views compose selected catalog rows and current lookup functions.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/system_views.sql

use super::{
    Arena, Datum, SqlError, Storage, SynthTable, arena_full, catalog_rows, finish, row, schema,
    text,
};

#[derive(Clone, Copy)]
pub(super) enum Read {
    Statement,
    Current,
}

#[derive(Clone, Copy)]
pub(super) enum Base {
    Class,
    Namespace,
    Tablespace,
    RoleSettings,
    Type,
    Attribute,
    Defaults,
    Collation,
    Dependencies,
    Sequence,
}

impl Read {
    pub(super) fn base<'a>(
        self,
        storage: &Storage,
        base: Base,
        transaction: u32,
        arena: &'a Arena,
    ) -> Result<SynthTable<'a>, SqlError> {
        let name = match base {
            Base::Class => "pg_class",
            Base::Namespace => "pg_namespace",
            Base::Tablespace => "pg_tablespace",
            Base::RoleSettings => "pg_db_role_setting",
            Base::Type => "pg_type",
            Base::Attribute => "pg_attribute",
            Base::Defaults => "pg_attrdef",
            Base::Collation => "pg_collation",
            Base::Dependencies => "pg_depend",
            Base::Sequence => "pg_sequence",
        };
        match self {
            Self::Statement => super::synthesize(
                storage,
                Some("pg_catalog"),
                name,
                transaction,
                crate::sql::catalog_snapshot::CatalogScan::Sequential,
                arena,
            ),
            Self::Current => super::synthesize_current(
                storage,
                Some("pg_catalog"),
                name,
                transaction,
                super::Materialization::Query,
                arena,
            ),
        }
    }

    fn authid_for_view<'a>(
        self,
        storage: &Storage,
        transaction: u32,
        arena: &'a Arena,
    ) -> Result<SynthTable<'a>, SqlError> {
        // Only these built-in projections can read the protected source. No
        // session privilege changes or user function calls inherit this access.
        match self {
            Self::Statement => super::select_catalog_rows(
                storage,
                Some("pg_catalog"),
                "pg_authid",
                transaction,
                crate::sql::catalog_snapshot::CatalogScan::Sequential,
                arena,
                |at| super::pg_authid(storage, at, arena, false),
            ),
            Self::Current => super::pg_authid(storage, transaction, arena, false),
        }
    }
}

pub(super) fn columns<const N: usize>(table: &SynthTable<'_>, names: [&str; N]) -> [usize; N] {
    names.map(|name| {
        table
            .def
            .column_index(name)
            .expect("built-in view source column exists")
    })
}

fn left_rows<'a>(
    rows: &'a [&'a [Datum<'a>]],
    matches: impl Fn(&[Datum<'a>]) -> bool + 'a,
) -> impl Iterator<Item = Option<&'a [Datum<'a>]>> + 'a {
    let missing = !rows.iter().any(|row| matches(row));
    rows.iter()
        .copied()
        .filter(move |row| matches(row))
        .map(Some)
        .chain(missing.then_some(None))
}

pub(super) fn tables<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let classes = read.base(storage, Base::Class, transaction, arena)?;
    let namespaces = read.base(storage, Base::Namespace, transaction, arena)?;
    let tablespaces = read.base(storage, Base::Tablespace, transaction, arena)?;
    let [
        kind,
        namespace,
        name,
        owner,
        tablespace,
        indexes,
        rules,
        triggers,
        security,
    ] = columns(
        &classes,
        [
            "relkind",
            "relnamespace",
            "relname",
            "relowner",
            "reltablespace",
            "relhasindex",
            "relhasrules",
            "relhastriggers",
            "relrowsecurity",
        ],
    );
    let [namespace_oid, namespace_name] = columns(&namespaces, ["oid", "nspname"]);
    let [tablespace_oid, tablespace_name] = columns(&tablespaces, ["oid", "spcname"]);
    let selected = || {
        classes
            .rows
            .iter()
            .copied()
            .filter(move |class| matches!(class[kind], Datum::Char(b'r' | b'p')))
    };
    let mut count = 0usize;
    for class in selected() {
        let n = left_rows(namespaces.rows, move |row| {
            row[namespace_oid] == class[namespace]
        })
        .count();
        let t = left_rows(tablespaces.rows, move |row| {
            row[tablespace_oid] == class[tablespace]
        })
        .count();
        count = n
            .checked_mul(t)
            .and_then(|added| count.checked_add(added))
            .ok_or_else(arena_full)?;
    }
    let output = catalog_rows(arena, count)?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let mut at = 0;
    for class in selected() {
        let Datum::Oid(owner_oid) = class[owner] else {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "invalid pg_class owner identifier"
            ));
        };
        let owner_name = Datum::Text(super::user_name_by_oid(&catalog, owner_oid as i32, arena)?);
        for namespace in left_rows(namespaces.rows, move |row| {
            row[namespace_oid] == class[namespace]
        }) {
            for tablespace in left_rows(tablespaces.rows, move |row| {
                row[tablespace_oid] == class[tablespace]
            }) {
                output[at] = row(
                    &[
                        namespace.map_or(Datum::Null, |row| row[namespace_name]),
                        class[name],
                        owner_name,
                        tablespace.map_or(Datum::Null, |row| row[tablespace_name]),
                        class[indexes],
                        class[rules],
                        class[triggers],
                        class[security],
                    ],
                    arena,
                )?;
                at += 1;
            }
        }
    }
    finish(schema::require("pg_tables", false), output, arena)
}

pub(super) fn roles<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let roles = read.authid_for_view(storage, transaction, arena)?;
    let settings = read.base(storage, Base::RoleSettings, transaction, arena)?;
    let [
        oid,
        name,
        superuser,
        inherit,
        create_role,
        create_database,
        login,
        replication,
        connections,
        valid_until,
        bypass,
    ] = columns(
        &roles,
        [
            "oid",
            "rolname",
            "rolsuper",
            "rolinherit",
            "rolcreaterole",
            "rolcreatedb",
            "rolcanlogin",
            "rolreplication",
            "rolconnlimit",
            "rolvaliduntil",
            "rolbypassrls",
        ],
    );
    let [setting_role, setting_database, setting_config] =
        columns(&settings, ["setrole", "setdatabase", "setconfig"]);
    let mut count = 0usize;
    for role in roles.rows.iter().copied() {
        let joined = left_rows(settings.rows, move |row| {
            row[setting_role] == role[oid] && row[setting_database] == Datum::Oid(0)
        })
        .count();
        count = count.checked_add(joined).ok_or_else(arena_full)?;
    }
    let output = catalog_rows(arena, count)?;
    let password = text("********", arena)?;
    let mut at = 0;
    for role in roles.rows.iter().copied() {
        for setting in left_rows(settings.rows, move |row| {
            row[setting_role] == role[oid] && row[setting_database] == Datum::Oid(0)
        }) {
            output[at] = row(
                &[
                    role[name],
                    role[superuser],
                    role[inherit],
                    role[create_role],
                    role[create_database],
                    role[login],
                    role[replication],
                    role[connections],
                    password,
                    role[valid_until],
                    role[bypass],
                    setting.map_or(Datum::Null, |row| row[setting_config]),
                    role[oid],
                ],
                arena,
            )?;
            at += 1;
        }
    }
    finish(schema::require("pg_roles", false), output, arena)
}
