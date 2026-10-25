//! System views compose selected catalog rows and current lookup functions.
//! Source: https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/system_views.sql

use super::view_rows::{Index, Output, character, integer};
use super::{
    Arena, Datum, SqlError, Storage, SynthTable, arena_full, catalog_rows, finish, row, schema,
    text,
};
use crate::sql::eval::{CatalogAccess, PrivilegeRelation};

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
    Rewrite,
    Trigger,
    Constraint,
    Index,
    RoleMembers,
    Procedure,
    Language,
}

pub(super) fn indexes<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    use super::view_rows::CatalogRow;
    let indexes = read.base(storage, Base::Index, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let tablespaces = Index::oid(
        read.base(storage, Base::Tablespace, transaction, arena)?,
        arena,
    )?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let mut output = Output::new(arena);
    for values in indexes.rows {
        let index = CatalogRow {
            definition: indexes.def,
            values,
        };
        for relation in classes.matching((index.integer("indrelid")? as u32, 0)) {
            let relation = CatalogRow {
                definition: classes.table.def,
                values: relation,
            };
            if !matches!(relation.character("relkind")?, b'r' | b'm' | b'p') {
                continue;
            }
            for indexed in classes.matching((index.integer("indexrelid")? as u32, 0)) {
                let indexed = CatalogRow {
                    definition: classes.table.def,
                    values: indexed,
                };
                if !matches!(indexed.character("relkind")?, b'i' | b'I') {
                    continue;
                }
                let definition = catalog
                    .index_def(indexed.integer("oid")?, 0, false, arena)?
                    .map_or(Datum::Null, Datum::Text);
                for namespace in
                    namespaces.left(Some((relation.integer("relnamespace")? as u32, 0)))
                {
                    let namespace = namespace.map(|values| CatalogRow {
                        definition: namespaces.table.def,
                        values,
                    });
                    for tablespace in
                        tablespaces.left(Some((indexed.integer("reltablespace")? as u32, 0)))
                    {
                        let tablespace = tablespace.map(|values| CatalogRow {
                            definition: tablespaces.table.def,
                            values,
                        });
                        output.push(&[
                            namespace
                                .map(|row| row.get("nspname"))
                                .transpose()?
                                .unwrap_or(Datum::Null),
                            relation.get("relname")?,
                            indexed.get("relname")?,
                            tablespace
                                .map(|row| row.get("spcname"))
                                .transpose()?
                                .unwrap_or(Datum::Null),
                            definition,
                        ])?;
                    }
                }
            }
        }
    }
    output.finish("pg_indexes", false)
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
            Base::Rewrite => "pg_rewrite",
            Base::Trigger => "pg_trigger",
            Base::Constraint => "pg_constraint",
            Base::Index => "pg_index",
            Base::RoleMembers => "pg_auth_members",
            Base::Procedure => "pg_proc",
            Base::Language => "pg_language",
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

    pub(super) fn authid_for_view<'a>(
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

pub(super) fn groups<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    use super::view_rows::{CatalogRow, boolean};
    let roles = read.authid_for_view(storage, transaction, arena)?;
    let membership = read.base(storage, Base::RoleMembers, transaction, arena)?;
    let [role, member] = columns(&membership, ["roleid", "member"]);
    let membership = Index::new(membership, arena, |values| {
        Ok(Some((integer(values[role])? as u32, 0)))
    })?;
    let mut output = Output::new(arena);
    for values in roles.rows {
        let account = CatalogRow {
            definition: roles.def,
            values,
        };
        if boolean(account.get("rolcanlogin")?)? {
            continue;
        }
        let key = (account.integer("oid")? as u32, 0);
        let count = membership.matching(key).count();
        let mut matching = membership.matching(key);
        let members = arena
            .alloc_slice_with(count, |_| {
                matching.next().expect("counted selected membership")[member]
            })
            .map_err(|_| arena_full())?;
        output.push(&[
            account.get("rolname")?,
            account.get("oid")?,
            Datum::Array {
                element: crate::sql::types::ArrElem::Oid,
                raw: crate::sql::array::build(members, arena)?,
            },
        ])?;
    }
    output.finish("pg_group", false)
}

#[derive(Clone, Copy)]
pub(super) enum AccountView {
    Users,
    Shadow,
}

pub(super) fn accounts<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    view: AccountView,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    use super::view_rows::CatalogRow;
    if matches!(view, AccountView::Shadow) {
        super::require_authid_read(storage, transaction)?;
    }
    let roles = read.authid_for_view(storage, transaction, arena)?;
    let settings = read.base(storage, Base::RoleSettings, transaction, arena)?;
    let [setting_role, setting_database, setting_config] =
        columns(&settings, ["setrole", "setdatabase", "setconfig"]);
    let password = text("********", arena)?;
    let mut output = Output::new(arena);
    for values in roles.rows {
        let account = CatalogRow {
            definition: roles.def,
            values,
        };
        if !account.boolean("rolcanlogin")? {
            continue;
        }
        let oid = account.integer("oid")?;
        for setting in left_rows(settings.rows, move |values| {
            integer(values[setting_role]).expect("built-in setting role OID") == oid
                && integer(values[setting_database]).expect("built-in database OID") == 0
        }) {
            output.push(&[
                account.get("rolname")?,
                account.get("oid")?,
                account.get("rolcreatedb")?,
                account.get("rolsuper")?,
                account.get("rolreplication")?,
                account.get("rolbypassrls")?,
                match view {
                    AccountView::Users => password,
                    AccountView::Shadow => account.get("rolpassword")?,
                },
                account.get("rolvaliduntil")?,
                setting.map_or(Datum::Null, |values| values[setting_config]),
            ])?;
        }
    }
    output.finish(
        match view {
            AccountView::Users => "pg_user",
            AccountView::Shadow => "pg_shadow",
        },
        false,
    )
}

#[derive(Clone, Copy)]
pub(super) enum RelationView {
    Views,
    MaterializedViews,
}

pub(super) fn relations<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    kind: RelationView,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let classes = read.base(storage, Base::Class, transaction, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let tablespaces = match kind {
        RelationView::Views => None,
        RelationView::MaterializedViews => Some(Index::oid(
            read.base(storage, Base::Tablespace, transaction, arena)?,
            arena,
        )?),
    };
    let [
        oid,
        namespace,
        name,
        owner,
        relation_kind,
        tablespace,
        indexes,
        populated,
    ] = columns(
        &classes,
        [
            "oid",
            "relnamespace",
            "relname",
            "relowner",
            "relkind",
            "reltablespace",
            "relhasindex",
            "relispopulated",
        ],
    );
    let [namespace_name] = columns(&namespaces.table, ["nspname"]);
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let mut output = Output::new(arena);
    for class in classes.rows {
        if character(class[relation_kind])?
            != match kind {
                RelationView::Views => b'v',
                RelationView::MaterializedViews => b'm',
            }
        {
            continue;
        }
        let owner = Datum::Text(super::user_name_by_oid(
            &catalog,
            integer(class[owner])?,
            arena,
        )?);
        let definition = super::view_def_text(storage, transaction, integer(class[oid])?, arena)?
            .map_or(Datum::Null, Datum::Text);
        for names in namespaces.left(Some((integer(class[namespace])? as u32, 0))) {
            let namespace = names.map_or(Datum::Null, |values| values[namespace_name]);
            match &tablespaces {
                None => output.push(&[namespace, class[name], owner, definition])?,
                Some(tablespaces) => {
                    let [tablespace_name] = columns(&tablespaces.table, ["spcname"]);
                    for space in tablespaces.left(Some((integer(class[tablespace])? as u32, 0))) {
                        output.push(&[
                            namespace,
                            class[name],
                            owner,
                            space.map_or(Datum::Null, |values| values[tablespace_name]),
                            class[indexes],
                            class[populated],
                            definition,
                        ])?;
                    }
                }
            }
        }
    }
    output.finish(
        match kind {
            RelationView::Views => "pg_views",
            RelationView::MaterializedViews => "pg_matviews",
        },
        false,
    )
}

pub(super) fn rules<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let rules = read.base(storage, Base::Rewrite, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let [oid, name, relation] = columns(&rules, ["oid", "rulename", "ev_class"]);
    let [class_name, namespace] = columns(&classes.table, ["relname", "relnamespace"]);
    let [namespace_name] = columns(&namespaces.table, ["nspname"]);
    let mut output = Output::new(arena);
    for rule in rules.rows {
        if rule[name] == Datum::Text("_RETURN") {
            continue;
        }
        let definition = super::rule_def_text(storage, transaction, integer(rule[oid])?, arena)?
            .map_or(Datum::Null, Datum::Text);
        for class in classes.matching((integer(rule[relation])? as u32, 0)) {
            for names in namespaces.left(Some((integer(class[namespace])? as u32, 0))) {
                output.push(&[
                    names.map_or(Datum::Null, |values| values[namespace_name]),
                    class[class_name],
                    rule[name],
                    definition,
                ])?;
            }
        }
    }
    output.finish("pg_rules", false)
}

pub(super) fn sequence_type(oid: i32) -> Result<&'static str, SqlError> {
    match oid {
        21 => Ok("smallint"),
        23 => Ok("integer"),
        20 => Ok("bigint"),
        _ => Err(crate::sql_err!(
            crate::sql::eval::sqlstate::INTERNAL_ERROR,
            "invalid sequence data type identifier {}",
            oid
        )),
    }
}

pub(super) fn sequences<'a>(
    storage: &Storage,
    transaction: u32,
    read: Read,
    arena: &'a Arena,
) -> Result<SynthTable<'a>, SqlError> {
    let sequences = read.base(storage, Base::Sequence, transaction, arena)?;
    let classes = Index::oid(read.base(storage, Base::Class, transaction, arena)?, arena)?;
    let namespaces = Index::oid(
        read.base(storage, Base::Namespace, transaction, arena)?,
        arena,
    )?;
    let [
        relation,
        data_type,
        start,
        minimum,
        maximum,
        increment,
        cycle,
        cache,
    ] = columns(
        &sequences,
        [
            "seqrelid",
            "seqtypid",
            "seqstart",
            "seqmin",
            "seqmax",
            "seqincrement",
            "seqcycle",
            "seqcache",
        ],
    );
    let [namespace, name, owner, kind] = columns(
        &classes.table,
        ["relnamespace", "relname", "relowner", "relkind"],
    );
    let [namespace_oid, namespace_name] = columns(&namespaces.table, ["oid", "nspname"]);
    let catalog = crate::sql::query::storage_catalog(storage, arena, transaction);
    let mut output = Output::new(arena);
    for sequence in sequences.rows {
        let oid = integer(sequence[relation])?;
        let type_oid = integer(sequence[data_type])?;
        let data_type = Datum::Regtype {
            referenced_oid: type_oid,
            name: sequence_type(type_oid)?,
        };
        // Settings are selected rows; values and access follow the live sequence.
        let last =
            if catalog.has_sequence_privilege(None, PrivilegeRelation::Oid(oid), "SELECT, USAGE")?
                == Some(true)
            {
                match catalog.sequence_state_by_oid(oid) {
                    Some((value, true)) => Datum::Int8(value),
                    _ => Datum::Null,
                }
            } else {
                Datum::Null
            };
        for class in classes.matching((oid as u32, 0)) {
            if character(class[kind])? != b'S' {
                continue;
            }
            let owner = Datum::Text(super::user_name_by_oid(
                &catalog,
                integer(class[owner])?,
                arena,
            )?);
            for names in namespaces.left(Some((integer(class[namespace])? as u32, 0))) {
                if let Some(names) = names
                    && catalog.is_other_temporary_namespace(integer(names[namespace_oid])?)
                {
                    continue;
                }
                output.push(&[
                    names.map_or(Datum::Null, |values| values[namespace_name]),
                    class[name],
                    owner,
                    data_type,
                    sequence[start],
                    sequence[minimum],
                    sequence[maximum],
                    sequence[increment],
                    sequence[cycle],
                    sequence[cache],
                    last,
                ])?;
            }
        }
    }
    output.finish("pg_sequences", false)
}
