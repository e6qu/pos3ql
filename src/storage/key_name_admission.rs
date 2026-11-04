//! Generated index names reserve the same schema namespace as their relations.

use super::*;
use core::fmt::Write;

#[derive(Clone, Copy)]
pub(crate) enum GeneratedNameClass {
    IndexConstraint,
    Constraint,
    Relation,
}

// A key shares a name only with the same index and table lifetimes.
#[derive(Clone, Copy)]
struct BackingIndexAlias {
    source: super::catalog_sources::CatalogObjectIdentity,
    identity: RelationIdentity,
}

pub(super) fn dirty_exists(state: CatalogDdlState, transaction: u32) -> bool {
    state != CatalogDdlState::Absent && state.pending_drop_txid() != Some(transaction)
}

fn constraint_named<B: DefinitionBacking>(definition: &TableDefinition<B>, name: SqlName) -> bool {
    definition
        .inline_key_names()
        .iter()
        .any(|key| key.name == name)
        || definition.uniques().iter().any(|key| key.name == name)
        || definition.checks().iter().any(|key| key.name == name)
        || definition.fkeys().iter().any(|key| key.name == name)
        || definition.exclusions().iter().any(|key| key.name == name)
        || definition.not_null_names.as_ref()[..definition.n_columns].contains(&Some(name))
        || definition
            .partition
            .detached_bound
            .is_some_and(|key| key.name == name)
}

fn implicit_relation_named<B: DefinitionBacking>(
    definition: &TableDefinition<B>,
    name: SqlName,
) -> bool {
    definition
        .backing_index_names()
        .any(|(stored, _)| stored == name)
}

impl Storage {
    pub(crate) fn generated_name_taken<B: DefinitionBacking>(
        &self,
        definition: &TableDefinition<B>,
        name: SqlName,
        transaction: u32,
        exclude: Option<usize>,
        class: GeneratedNameClass,
    ) -> bool {
        let relation = !matches!(class, GeneratedNameClass::Constraint);
        if (!matches!(class, GeneratedNameClass::Relation) && constraint_named(definition, name))
            || (relation && definition.name == name)
        {
            return true;
        }
        self.schema_generated_name_taken(definition.schema, name, transaction, exclude, class, None)
    }

    fn schema_generated_name_taken(
        &self,
        schema: SqlName,
        name: SqlName,
        transaction: u32,
        exclude: Option<usize>,
        class: GeneratedNameClass,
        backing: Option<BackingIndexAlias>,
    ) -> bool {
        let relation = !matches!(class, GeneratedNameClass::Constraint);
        for (slot, table) in self.tables.iter().enumerate() {
            if exclude == Some(slot) {
                continue;
            }
            let current = TableDefinitionRead::current(&table.definition, &self.pending_table_defs);
            let identity = current.identity();
            if identity.database != current_database()
                || !dirty_exists(identity.existence, transaction)
            {
                continue;
            }
            // PostgreSQL chooses relation names with SnapshotDirty, but checks
            // pg_constraint through the caller's catalog snapshot.
            if !matches!(class, GeneratedNameClass::Relation)
                && identity.existence.visible_to(transaction)
            {
                let visible = if current
                    .state
                    .pending
                    .is_some_and(|pending| pending.transaction == transaction)
                {
                    &*current
                } else {
                    current.committed_definition()
                };
                if visible.schema == schema && constraint_named(visible, name) {
                    return true;
                }
            }
            if !relation {
                continue;
            }
            let matches = |image: &TableDef| {
                image.schema == schema
                    && (image.name == name || implicit_relation_named(image, name))
            };
            if matches(&current) {
                return true;
            }
            if current
                .state
                .pending
                .is_none_or(|pending| pending.transaction != transaction)
                && matches(current.committed_definition())
            {
                return true;
            }
        }
        if !matches!(class, GeneratedNameClass::Relation) {
            for slot in 0..self.domain_count() {
                let domain = self.domain(slot);
                if domain.database != current_database() || !domain.visible_to(transaction) {
                    continue;
                }
                let image = domain.definition_for(transaction);
                if image.schema == schema
                    && (image.not_null_name == Some(name)
                        || image.checks().iter().any(|key| key.name == name))
                {
                    return true;
                }
            }
        }
        if !relation {
            return false;
        }
        let (indexes, views, sequences, types) = row_ownership::acquire(|| {
            Ok((
                row_ownership::try_mutex(&self.index_catalog, "index catalog lock poisoned")?,
                row_ownership::try_mutex(&self.view_rule_catalog, "view catalog lock poisoned")?,
                row_ownership::try_mutex(&self.sequence_catalog, "sequence catalog lock poisoned")?,
                row_ownership::try_mutex(&self.type_catalog, "type catalog lock poisoned")?,
            ))
        });
        catalog_relation_name_taken(
            schema,
            name,
            transaction,
            backing,
            RelationNameSources {
                indexes: &indexes,
                views: &views.views,
                sequences: &sequences.definitions,
                composites: &types.composites,
            },
        )
    }

    pub(super) fn implicit_index_name_visible(
        &self,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> bool {
        for (slot, table) in self.tables.iter().enumerate() {
            let identity = table.identity();
            if identity.database != current_database()
                || !identity.existence.visible_to(transaction)
            {
                continue;
            }
            let definition = self.table_def(slot, transaction);
            if definition.schema.as_str() == schema
                && definition
                    .backing_index_names()
                    .any(|(stored, _)| stored.as_str() == name)
            {
                return true;
            }
        }
        false
    }

    pub(super) fn implicit_index_name_taken(
        &self,
        schema: SqlName,
        name: SqlName,
        transaction: u32,
        allowed: Option<RelationIdentity>,
    ) -> Result<bool, SqlError> {
        for table in self.tables.iter() {
            let current = TableDefinitionRead::current(&table.definition, &self.pending_table_defs);
            let identity = current.identity();
            if identity.database != current_database()
                || !dirty_exists(identity.existence, transaction)
            {
                continue;
            }
            let matches = |image: &TableDef| {
                image.schema == schema
                    && image.backing_index_names().any(|(stored, selected)| {
                        stored == name && !allowed.is_some_and(|allowed| selected == Some(allowed))
                    })
            };
            let pending = current.state.pending.map(|pending| pending.transaction);
            let current_matches = matches(&current);
            let committed_matches = matches(current.committed_definition());
            if identity
                .existence
                .pending_txid()
                .is_some_and(|owner| owner != transaction)
                && (current_matches || committed_matches)
            {
                let owner = identity
                    .existence
                    .pending_txid()
                    .expect("pending relation existence");
                drop(current);
                return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
            }
            if pending.is_some_and(|owner| owner != transaction)
                && current_matches != committed_matches
            {
                let owner = pending.expect("pending name change");
                drop(current);
                return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
            }
            if current_matches || (pending != Some(transaction) && committed_matches) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn prepare_inline_key_names(
        &self,
        definition: &mut TableDefBuilder,
        transaction: u32,
        exclude: Option<usize>,
    ) -> Result<(), SqlError> {
        definition.attributes.initialize(definition.n_columns)?;
        definition.retain_active_inline_key_names()?;
        for column in 0..definition.n_columns {
            let metadata = definition.columns[column];
            if !(metadata.primary || metadata.unique)
                || definition.column_key_name(column).is_some()
            {
                continue;
            }
            let mut suffix = 0u32;
            loop {
                let mut label =
                    StackStr::<32>::from_str(if metadata.primary { "pkey" } else { "key" });
                if suffix != 0 {
                    write!(label, "{suffix}").expect("u32 key suffix fits");
                }
                let name = constraint_names::generated(
                    definition.name.as_str(),
                    (!metadata.primary).then_some(metadata.name.as_str()),
                    label.as_str(),
                )?;
                if !self.generated_name_taken(
                    definition,
                    name,
                    transaction,
                    exclude,
                    GeneratedNameClass::IndexConstraint,
                ) {
                    definition.set_column_key_name(column, name)?;
                    break;
                }
                suffix = suffix.checked_add(1).ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "generated inline key names are exhausted"
                    )
                })?;
            }
        }
        Self::validate_inline_key_names(definition)
    }

    pub(crate) fn prepare_table_key_names(
        &self,
        target: TableDefinitionTarget,
        definition: &mut TableDefBuilder,
        mapping: &[Option<SqlName>; MAX_COLUMNS],
    ) -> Result<(), SqlError> {
        let _names = self
            .table_creation
            .lock()
            .expect("table creation lock poisoned");
        self.prepare_selected_key_names(target, definition, mapping)
    }

    pub(super) fn prepare_selected_key_names(
        &self,
        target: TableDefinitionTarget,
        definition: &mut TableDefBuilder,
        mapping: &[Option<SqlName>; MAX_COLUMNS],
    ) -> Result<(), SqlError> {
        target.validate(self)?;
        let previous = self.table_def(target.table(), target.transaction());
        definition.attributes =
            column_attributes::ColumnAttributes::rewrite(&previous, definition.columns(), mapping)?;
        constraint_names::remap_table(&previous, definition, mapping)?;
        self.prepare_inline_key_names(definition, target.transaction(), Some(target.table()))?;
        for key in definition.inline_key_names() {
            if previous.inline_key_names().iter().any(|prior| prior == key) {
                continue;
            }
            let backing = definition
                .attribute_column(key.attribute)
                .and_then(|column| definition.column_index_identity(column))
                .map(|identity| BackingIndexAlias {
                    source: target.source,
                    identity,
                });
            if self.schema_generated_name_taken(
                definition.schema,
                key.name,
                target.transaction(),
                Some(target.table()),
                GeneratedNameClass::Relation,
                backing,
            ) {
                return Err(sql_err!(
                    sqlstate::DUPLICATE_TABLE,
                    "relation \"{}\" already exists",
                    key.name.as_str()
                ));
            }
        }
        Ok(())
    }
}

/// Borrowed catalog images allow startup validation to reuse namespace rules
/// while it already owns these registry mutexes.
pub(crate) struct RelationNameSources<'a> {
    pub indexes: &'a [IndexDef],
    pub views: &'a [ViewDef],
    pub sequences: &'a [SequenceDef],
    pub composites: &'a [CompositeDef],
}

fn catalog_relation_name_taken(
    schema: SqlName,
    name: SqlName,
    transaction: u32,
    backing: Option<BackingIndexAlias>,
    sources: RelationNameSources<'_>,
) -> bool {
    for index in sources.indexes {
        if index.database != current_database() || !dirty_exists(index.ddl_state, transaction) {
            continue;
        }
        if backing.is_some_and(|backing| {
            matches!(index.relation, IndexRelationSource::Table)
                && index.created_at == backing.identity.created_at()
                && index
                    .table_binding
                    .is_some_and(|binding| binding.source == backing.source)
        }) {
            continue;
        }
        let owner = index
            .pending_table
            .map(|pending| pending.transaction)
            .or_else(|| index.pending_name.map(|pending| pending.txid));
        let matches = |schema_name: SqlName, relation_name: SqlName| {
            schema_name == schema && relation_name == name
        };
        if owner != Some(transaction) && matches(index.schema, index.name) {
            return true;
        }
        if let Some(owner) = owner
            && matches(index.schema_for(owner), index.name_for(owner))
        {
            return true;
        }
    }
    for sequence in sources.sequences {
        let sequence = *sequence;
        if sequence.database != current_database() || !dirty_exists(sequence.ddl_state, transaction)
        {
            continue;
        }
        let matches = |image: SequenceDef| image.schema == schema && image.name == name;
        if sequence
            .pending_definition
            .is_none_or(|pending| pending.txid != transaction)
            && matches(sequence)
        {
            return true;
        }
        if let Some(pending) = sequence.pending_definition
            && matches(sequence.definition_for(pending.txid))
        {
            return true;
        }
    }
    for view in sources.views {
        if view.database != current_database() || !dirty_exists(view.ddl_state, transaction) {
            continue;
        }
        if view
            .pending_definition
            .as_ref()
            .is_none_or(|pending| pending.txid != transaction)
            && view.schema == schema
            && view.name == name
        {
            return true;
        }
        if let Some(pending) = &view.pending_definition
            && pending.schema == schema
            && pending.name == name
        {
            return true;
        }
    }
    for composite in sources.composites {
        if composite.database != current_database()
            || !dirty_exists(composite.ddl_state, transaction)
        {
            continue;
        }
        if composite
            .pending_definition
            .is_none_or(|pending| pending.txid != transaction)
            && composite.schema == schema
            && composite.name == name
        {
            return true;
        }
        if let Some(pending) = composite.pending_definition
            && pending.schema == schema
            && pending.name == name
        {
            return true;
        }
    }
    false
}
