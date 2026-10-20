//! Captured references identify a type's lifetime independently of its name.

use super::type_names::NamedTypeSlot;
use super::{
    ArrElem, ColType, DatabaseOid, PolymorphicType, RoutineResult, SqlName, TypeCatalog,
    UserTypeName,
};
use crate::mem::budget::BudgetError;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub(crate) enum StorageSetupError {
    Budget(BudgetError),
    TypeIdentitySpaceExhausted,
}

impl From<BudgetError> for StorageSetupError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl std::fmt::Display for StorageSetupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(error) => write!(formatter, "{error}"),
            Self::TypeIdentitySpaceExhausted => {
                formatter.write_str("type catalog identity space exhausted")
            }
        }
    }
}

impl std::error::Error for StorageSetupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Budget(error) => Some(error),
            Self::TypeIdentitySpaceExhausted => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TypeOrigin(u64);

impl TypeOrigin {
    pub(super) fn new() -> Result<Self, StorageSetupError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self::reserve(&NEXT)
    }

    fn reserve(sequence: &AtomicU64) -> Result<Self, StorageSetupError> {
        let mut current = sequence.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_add(1)
                .ok_or(StorageSetupError::TypeIdentitySpaceExhausted)?;
            match sequence.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(value) => return Ok(Self(value)),
                Err(observed) => current = observed,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum CapturedTypeSlot {
    Domain([u8; 2]),
    Enum([u8; 2]),
    Composite([u8; 2]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TypeIdentity {
    // Byte arrays preserve the full identity without padding every copied
    // column or routine parameter to eight-byte alignment.
    origin: [u8; 8],
    database: [u8; 4],
    created_at: [u8; 8],
    source: CapturedTypeSlot,
}

impl TypeIdentity {
    pub(super) fn new(
        origin: TypeOrigin,
        database: DatabaseOid,
        slot: NamedTypeSlot,
        created_at: u64,
    ) -> Self {
        // Configuration caps each catalog within its PostgreSQL OID band.
        let index = u16::try_from(slot.index())
            .expect("configured type slot is representable")
            .to_le_bytes();
        let source = match slot {
            NamedTypeSlot::Domain(_) => CapturedTypeSlot::Domain(index),
            NamedTypeSlot::Enum(_) => CapturedTypeSlot::Enum(index),
            NamedTypeSlot::Composite(_) => CapturedTypeSlot::Composite(index),
        };
        Self {
            origin: origin.0.to_le_bytes(),
            database: database.get().to_le_bytes(),
            created_at: created_at.to_le_bytes(),
            source,
        }
    }

    fn origin(self) -> TypeOrigin {
        TypeOrigin(u64::from_le_bytes(self.origin))
    }

    pub(super) fn database(self) -> DatabaseOid {
        DatabaseOid::parse(i32::from_le_bytes(self.database)).expect("captured database identity")
    }

    pub(super) fn created_at(self) -> u64 {
        u64::from_le_bytes(self.created_at)
    }

    pub(super) fn slot(self) -> NamedTypeSlot {
        match self.source {
            CapturedTypeSlot::Domain(index) => {
                NamedTypeSlot::Domain(usize::from(u16::from_le_bytes(index)))
            }
            CapturedTypeSlot::Enum(index) => {
                NamedTypeSlot::Enum(usize::from(u16::from_le_bytes(index)))
            }
            CapturedTypeSlot::Composite(index) => {
                NamedTypeSlot::Composite(usize::from(u16::from_le_bytes(index)))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StoredTypeIdentity {
    source: CapturedTypeSlot,
    created_at: [u8; 8],
}

impl StoredTypeIdentity {
    pub(super) fn created_at(self) -> u64 {
        u64::from_le_bytes(self.created_at)
    }

    pub(super) fn slot(self) -> NamedTypeSlot {
        match self.source {
            CapturedTypeSlot::Domain(index) => {
                NamedTypeSlot::Domain(usize::from(u16::from_le_bytes(index)))
            }
            CapturedTypeSlot::Enum(index) => {
                NamedTypeSlot::Enum(usize::from(u16::from_le_bytes(index)))
            }
            CapturedTypeSlot::Composite(index) => {
                NamedTypeSlot::Composite(usize::from(u16::from_le_bytes(index)))
            }
        }
    }
}

impl UserTypeName {
    /// One encoding is shared by WAL, manifests and aggregate type fields.
    /// Prefixing every name makes quoted SQL names containing ':' unambiguous.
    pub(crate) fn storage_name(self) -> crate::util::StackStr<128> {
        use core::fmt::Write;
        let mut output = crate::util::StackStr::new();
        let stored = match self.binding {
            TypeBinding::Captured(key) => Some(StoredTypeIdentity {
                source: key.source,
                created_at: key.created_at,
            }),
            TypeBinding::Stored(key) => Some(key),
            TypeBinding::Declaration | TypeBinding::Polymorphic(_) => None,
        };
        if let Some(key) = stored {
            let (kind, slot) = match key.source {
                CapturedTypeSlot::Domain(slot) => ('d', u16::from_le_bytes(slot)),
                CapturedTypeSlot::Enum(slot) => ('e', u16::from_le_bytes(slot)),
                CapturedTypeSlot::Composite(slot) => ('c', u16::from_le_bytes(slot)),
            };
            write!(
                output,
                "r:{kind}:{slot}:{}:{}",
                key.created_at(),
                self.name.as_str()
            )
            .expect("bounded stored type name");
        } else {
            let prefix = if matches!(self.binding, TypeBinding::Polymorphic(_)) {
                'p'
            } else {
                'n'
            };
            write!(output, "{prefix}:{}", self.name.as_str()).expect("bounded stored type name");
        }
        output
    }

    pub(crate) fn from_storage(schema: SqlName, encoded: &str) -> Result<Self, SqlError> {
        let invalid = || sql_err!(sqlstate::PROTOCOL_VIOLATION, "invalid stored type identity");
        if let Some(name) = encoded.strip_prefix("n:") {
            return Ok(Self::declaration(schema, SqlName::parse(name)?));
        }
        if let Some(name) = encoded.strip_prefix("p:") {
            if schema.as_str() != "pg_catalog" {
                return Err(invalid());
            }
            return PolymorphicType::from_name(name)
                .map(Self::polymorphic)
                .ok_or_else(invalid);
        }
        let mut fields = encoded
            .strip_prefix("r:")
            .ok_or_else(invalid)?
            .splitn(4, ':');
        let kind = fields.next().ok_or_else(invalid)?;
        let slot: u16 = fields
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let created_at: u64 = fields
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let name = SqlName::parse(fields.next().ok_or_else(invalid)?)?;
        let source = match kind {
            "d" if usize::from(slot) < super::MAX_DOMAIN_CATALOG_SLOTS => {
                CapturedTypeSlot::Domain(slot.to_le_bytes())
            }
            "e" if usize::from(slot) < super::MAX_ENUM_CATALOG_SLOTS => {
                CapturedTypeSlot::Enum(slot.to_le_bytes())
            }
            "c" if usize::from(slot) < super::MAX_COMPOSITE_CATALOG_SLOTS => {
                CapturedTypeSlot::Composite(slot.to_le_bytes())
            }
            _ => return Err(invalid()),
        };
        if created_at == 0 {
            return Err(invalid());
        }
        Ok(Self {
            schema,
            name,
            binding: TypeBinding::Stored(StoredTypeIdentity {
                source,
                created_at: created_at.to_le_bytes(),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_type_reference_layout_keeps_slots_in_their_oid_width() {
        use core::mem::size_of;
        assert_eq!(size_of::<CapturedTypeSlot>(), 3);
        assert_eq!(size_of::<TypeIdentity>(), 23);
        assert!(size_of::<TypeBinding>() <= 24);
        assert!(size_of::<Option<UserTypeName>>() <= 152);
        for source in [
            NamedTypeSlot::Domain(0),
            NamedTypeSlot::Enum(65535),
            NamedTypeSlot::Composite(1600),
        ] {
            let origin = TypeOrigin(u64::MAX);
            let database = DatabaseOid::parse(i32::MAX).unwrap();
            let identity = TypeIdentity::new(origin, database, source, u64::MAX);
            assert_eq!(identity.slot(), source);
            assert_eq!(identity.origin(), origin);
            assert_eq!(identity.database(), database);
            assert_eq!(identity.created_at(), u64::MAX);
        }
    }

    #[test]
    fn stored_type_names_preserve_sql_prefixes_and_reject_invalid_keys() {
        let schema = SqlName::parse("public").unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for name in ["n:plain", "r:d:0:1:quoted:name", "p:anyelement", "é:名"] {
                let original = UserTypeName::declaration(schema, SqlName::parse(name).unwrap());
                let encoded = original.storage_name();
                assert_eq!(
                    UserTypeName::from_storage(schema, encoded.as_str()).unwrap(),
                    original
                );
            }
            for encoded in [
                "plain",
                "r:z:0:1:x",
                "r:d:65536:1:x",
                "r:d:0:0:x",
                "r:d:0:1",
                "p:anyelement",
            ] {
                assert!(
                    UserTypeName::from_storage(schema, encoded).is_err(),
                    "{encoded}"
                );
            }
        });
    }

    #[test]
    fn bound_type_origin_never_reuses_an_exhausted_identifier() {
        let sequence = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            TypeOrigin::reserve(&sequence).unwrap(),
            TypeOrigin(u64::MAX - 1)
        );
        for _ in 0..2 {
            assert!(matches!(
                TypeOrigin::reserve(&sequence),
                Err(StorageSetupError::TypeIdentitySpaceExhausted)
            ));
            assert_eq!(sequence.load(Ordering::Relaxed), u64::MAX);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypeBinding {
    Declaration,
    Captured(TypeIdentity),
    Stored(StoredTypeIdentity),
    Polymorphic(PolymorphicType),
}

impl UserTypeName {
    pub(crate) fn is_named(self, schema: SqlName, name: SqlName) -> bool {
        self.schema == schema && self.name == name
    }

    pub(crate) fn polymorphic(kind: PolymorphicType) -> Self {
        Self {
            schema: SqlName::parse("pg_catalog").expect("static schema name"),
            name: SqlName::parse(kind.name()).expect("static polymorphic name"),
            binding: TypeBinding::Polymorphic(kind),
        }
    }

    fn captured(identity: TypeIdentity, schema: SqlName, name: SqlName) -> Self {
        Self {
            schema,
            name,
            binding: TypeBinding::Captured(identity),
        }
    }
}

impl PartialEq for UserTypeName {
    fn eq(&self, other: &Self) -> bool {
        match (self.binding, other.binding) {
            (TypeBinding::Captured(left), TypeBinding::Captured(right)) => left == right,
            (TypeBinding::Polymorphic(left), TypeBinding::Polymorphic(right)) => left == right,
            (TypeBinding::Stored(left), TypeBinding::Stored(right)) => left == right,
            (TypeBinding::Declaration, TypeBinding::Declaration) => {
                self.schema == other.schema && self.name == other.name
            }
            _ => false,
        }
    }
}

impl Eq for UserTypeName {}

pub(super) struct TypeSnapshot {
    pub(super) identity: TypeIdentity,
    pub(super) name: UserTypeName,
    pub(super) base: ColType,
}

impl TypeSnapshot {
    pub(super) fn representation(&self, declared: ColType) -> Result<ColType, SqlError> {
        match self.identity.slot() {
            NamedTypeSlot::Domain(slot)
                if matches!(declared, ColType::Array(ArrElem::Domain { .. }))
                    && !matches!(self.base, ColType::Array(ArrElem::Domain { .. })) =>
            {
                let slot = u16::try_from(slot).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "domain representation slot space exhausted"
                    )
                })?;
                ArrElem::domain(slot, self.base)
                    .map(ColType::Array)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "arrays of domain {} require a supported element base type",
                            self.name.name.as_str()
                        )
                    })
            }
            NamedTypeSlot::Domain(_) => Ok(self.base),
            NamedTypeSlot::Enum(_) | NamedTypeSlot::Composite(_) => match (declared, self.base) {
                (ColType::Array(_), ColType::Enum(slot)) => Ok(ColType::Array(ArrElem::Enum(slot))),
                (ColType::Array(_), ColType::Composite(slot)) => {
                    Ok(ColType::Array(ArrElem::Composite(slot)))
                }
                _ => Ok(self.base),
            },
        }
    }
}

impl TypeCatalog {
    pub(super) fn resolve_type_reference(
        &self,
        identity: UserTypeName,
        database: DatabaseOid,
        transaction: u32,
    ) -> Result<Option<TypeSnapshot>, SqlError> {
        match identity.binding {
            TypeBinding::Captured(key) => {
                if key.origin() != self.origin || key.database() != database {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "captured type reference belongs to a different catalog"
                    ));
                }
                Ok(self
                    .type_snapshot(key.slot(), transaction)?
                    .filter(|snapshot| snapshot.identity == key))
            }
            TypeBinding::Stored(key) => {
                Ok(self
                    .type_snapshot(key.slot(), transaction)?
                    .filter(|snapshot| {
                        snapshot.identity.database() == database
                            && snapshot.identity.created_at() == key.created_at()
                    }))
            }
            TypeBinding::Declaration => self.lookup_reference_name(identity, database, transaction),
            TypeBinding::Polymorphic(_) => Ok(None),
        }
    }
    pub(super) fn type_snapshot(
        &self,
        slot: NamedTypeSlot,
        transaction: u32,
    ) -> Result<Option<TypeSnapshot>, SqlError> {
        let (database, created_at, schema, name, base) = match slot {
            NamedTypeSlot::Domain(index) => {
                let Some(definition) = self
                    .domains
                    .get(index)
                    .filter(|definition| definition.visible_to(transaction))
                else {
                    return Ok(None);
                };
                let pending = definition
                    .pending_definition
                    .as_ref()
                    .filter(|pending| pending.txid == transaction);
                let (schema, name) = pending
                    .and_then(|pending| pending.identity)
                    .map_or((definition.schema, definition.name), |identity| {
                        (identity.schema, identity.name)
                    });
                (
                    definition.database,
                    definition.created_at,
                    schema,
                    name,
                    pending.map_or(definition.base, |pending| pending.spec.base),
                )
            }
            NamedTypeSlot::Enum(index) => {
                let Some(definition) = self
                    .enums
                    .get(index)
                    .filter(|definition| definition.visible_to(transaction))
                else {
                    return Ok(None);
                };
                let pending = definition
                    .pending_definition
                    .as_ref()
                    .filter(|pending| pending.txid == transaction);
                let (schema, name) = pending
                    .map_or((definition.schema, definition.name), |pending| {
                        (pending.schema, pending.name)
                    });
                let index = u16::try_from(index).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "enum representation slot space exhausted"
                    )
                })?;
                (
                    definition.database,
                    definition.created_at,
                    schema,
                    name,
                    ColType::Enum(index),
                )
            }
            NamedTypeSlot::Composite(index) => {
                let Some(definition) = self
                    .composites
                    .get(index)
                    .filter(|definition| definition.visible_to(transaction))
                else {
                    return Ok(None);
                };
                let pending = definition
                    .pending_definition
                    .as_ref()
                    .filter(|pending| pending.txid == transaction);
                let (schema, name) = pending
                    .map_or((definition.schema, definition.name), |pending| {
                        (pending.schema, pending.name)
                    });
                let index = u16::try_from(index).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "composite representation slot space exhausted"
                    )
                })?;
                (
                    definition.database,
                    definition.created_at,
                    schema,
                    name,
                    ColType::Composite(index),
                )
            }
        };
        let identity = TypeIdentity::new(self.origin, database, slot, created_at);
        Ok(Some(TypeSnapshot {
            identity,
            name: UserTypeName::captured(identity, schema, name),
            base,
        }))
    }

    pub(super) fn lookup_reference_name(
        &self,
        identity: UserTypeName,
        database: DatabaseOid,
        transaction: u32,
    ) -> Result<Option<TypeSnapshot>, SqlError> {
        let slot = self.name_reservations().find_map(|entry| {
            if entry.database != database || !entry.state.visible_to(transaction) {
                return None;
            }
            let visible = entry
                .pending
                .filter(|(owner, _)| *owner == transaction)
                .map_or(entry.current, |(_, name)| name);
            let matches =
                |name: UserTypeName| name.schema == identity.schema && name.name == identity.name;
            (matches(visible) || matches(entry.current)).then_some(entry.slot)
        });
        match slot {
            Some(slot) => self.type_snapshot(slot, transaction),
            None => Ok(None),
        }
    }

    pub(super) fn bind_type_reference(
        &self,
        result: RoutineResult,
        database: DatabaseOid,
        transaction: u32,
    ) -> Result<RoutineResult, SqlError> {
        let Some(identity) = result.user_type else {
            if matches!(result.ctype, ColType::Enum(_) | ColType::Composite(_))
                || matches!(result.ctype, ColType::Array(element) if element.user_type_slot().is_some())
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "user-defined type reference has no identity"
                ));
            }
            return Ok(result);
        };
        if let Some(kind) = super::polymorphic_type(result.ctype, Some(identity)) {
            return Ok(RoutineResult {
                ctype: result.ctype,
                user_type: Some(UserTypeName::polymorphic(kind)),
            });
        }
        if matches!(identity.binding, TypeBinding::Polymorphic(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "polymorphic type reference has an invalid representation"
            ));
        }
        let snapshot = self
            .resolve_type_reference(identity, database, transaction)?
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "declared type \"{}.{}\" does not exist",
                    identity.schema.as_str(),
                    identity.name.as_str()
                )
            })?;
        let ctype = snapshot.representation(result.ctype)?;
        if matches!(identity.binding, TypeBinding::Stored(_)) && ctype.code() != result.ctype.code()
        {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "stored type reference has an invalid representation"
            ));
        }
        if matches!(identity.binding, TypeBinding::Captured(_)) && ctype != result.ctype {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "captured type reference has an invalid representation"
            ));
        }
        Ok(RoutineResult {
            ctype,
            user_type: Some(snapshot.name),
        })
    }
}

impl super::Storage {
    /// A template copy creates new type objects in another database. Redeclare
    /// copied links deliberately, then bind them after the whole copy exists.
    pub(super) fn redeclare_template_type_references(
        &mut self,
        source: DatabaseOid,
        target: DatabaseOid,
    ) -> Result<(), SqlError> {
        let origin = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned")
            .origin;
        let redeclare = |name: &mut Option<UserTypeName>| -> Result<(), SqlError> {
            if let Some(identity) = name
                && let TypeBinding::Captured(key) = identity.binding
            {
                if key.origin() != origin || key.database() != source {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "template type reference belongs to a different catalog"
                    ));
                }
                *identity = UserTypeName::declaration(identity.schema, identity.name);
            }
            Ok(())
        };
        let mut catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        for domain in catalog.domains.iter_mut().filter(|domain| {
            domain.database == target && domain.ddl_state != super::CatalogDdlState::Absent
        }) {
            redeclare(&mut domain.base_domain)?;
            redeclare(&mut domain.base_user_type)?;
        }
        for composite in catalog.composites.iter_mut().filter(|definition| {
            definition.database == target && definition.ddl_state != super::CatalogDdlState::Absent
        }) {
            for field in composite.fields.iter_mut().take(composite.n_fields) {
                redeclare(&mut field.user_type)?;
            }
        }
        drop(catalog);
        for table in self
            .tables
            .iter_mut()
            .filter(|table| table.database() == target && !table.is_free())
        {
            let mut definition = table.definition_mut();
            let n_columns = definition.n_columns;
            for column in definition.columns.iter_mut().take(n_columns) {
                redeclare(&mut column.user_type)?;
            }
        }
        for routine in self
            .routines
            .lock()
            .expect("routine catalog lock poisoned")
            .iter_mut()
            .filter(|routine| {
                routine.database == target && routine.ddl_state != super::CatalogDdlState::Absent
            })
        {
            super::type_contracts::CatalogTypes::Routine(routine)
                .try_for_each(|result| redeclare(&mut result.user_type))?;
        }
        for cast in self
            .casts
            .lock()
            .expect("cast catalog lock poisoned")
            .iter_mut()
            .filter(|cast| {
                cast.database == target && cast.ddl_state != super::CatalogDdlState::Absent
            })
        {
            super::type_contracts::CatalogTypes::Cast(cast)
                .try_for_each(|result| redeclare(&mut result.user_type))?;
        }
        let mut operators = self
            .operator_catalog
            .lock()
            .expect("operator catalog lock poisoned");
        for operator in operators.operators.iter_mut().filter(|operator| {
            operator.database == target && operator.ddl_state != super::CatalogDdlState::Absent
        }) {
            super::type_contracts::CatalogTypes::Operator(&mut operator.definition)
                .try_for_each(|result| redeclare(&mut result.user_type))?;
        }
        for family in operators.families.iter_mut().filter(|family| {
            family.database == target && family.ddl_state != super::CatalogDdlState::Absent
        }) {
            super::type_contracts::CatalogTypes::Family(&mut family.definition)
                .try_for_each(|result| redeclare(&mut result.user_type))?;
        }
        for class in operators.classes.iter_mut().filter(|class| {
            class.database == target && class.ddl_state != super::CatalogDdlState::Absent
        }) {
            super::type_contracts::CatalogTypes::Class(&mut class.definition)
                .try_for_each(|result| redeclare(&mut result.user_type))?;
        }
        Ok(())
    }

    pub(super) fn rebind_cast_and_operator_types(&self, transaction: u32) -> Result<(), SqlError> {
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let bind = |result: &mut RoutineResult| -> Result<(), SqlError> {
            *result =
                catalog.bind_type_reference(*result, super::current_database(), transaction)?;
            Ok(())
        };
        for cast in self
            .casts
            .lock()
            .expect("cast catalog lock poisoned")
            .iter_mut()
            .filter(|cast| {
                cast.database == super::current_database() && cast.visible_to(transaction)
            })
        {
            super::type_contracts::CatalogTypes::Cast(cast).try_for_each(bind)?;
        }
        let mut operators = self
            .operator_catalog
            .lock()
            .expect("operator catalog lock poisoned");
        for operator in operators.operators.iter_mut().filter(|operator| {
            operator.database == super::current_database() && operator.visible_to(transaction)
        }) {
            super::type_contracts::CatalogTypes::Operator(&mut operator.definition)
                .try_for_each(bind)?;
        }
        for family in operators.families.iter_mut().filter(|family| {
            family.database == super::current_database() && family.visible_to(transaction)
        }) {
            super::type_contracts::CatalogTypes::Family(&mut family.definition)
                .try_for_each(bind)?;
        }
        for class in operators.classes.iter_mut().filter(|class| {
            class.database == super::current_database() && class.visible_to(transaction)
        }) {
            super::type_contracts::CatalogTypes::Class(&mut class.definition).try_for_each(bind)?;
        }
        Ok(())
    }
}

impl super::Storage {
    pub(crate) fn catalog_identity_sequence(&self) -> u64 {
        self.catalog_sequence.current()
    }

    pub(crate) fn restore_catalog_identity_sequence(&self, value: u64) {
        self.catalog_sequence.observe(value);
    }

    /// Recovery creates a complete object under exclusive access, then installs
    /// its recorded slot and lifetime before any dependent declaration binds.
    pub(crate) fn restore_type_identity(
        &mut self,
        allocated: NamedTypeSlot,
        target: NamedTypeSlot,
        created_at: u64,
    ) -> Result<(), SqlError> {
        if created_at == 0 {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored type creation identity is zero"
            ));
        }
        let invalid = || {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored type catalog slot is unavailable"
            )
        };
        let mut catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let old = allocated.index();
        let new = target.index();
        match (allocated, target) {
            (NamedTypeSlot::Domain(_), NamedTypeSlot::Domain(_)) => {
                if new >= catalog.domains.len()
                    || old >= catalog.domains.len()
                    || old != new
                        && catalog.domains[new].ddl_state != super::CatalogDdlState::Absent
                {
                    return Err(invalid());
                }
                catalog.domains.swap(old, new);
                catalog.domains[new].created_at = created_at;
            }
            (NamedTypeSlot::Enum(_), NamedTypeSlot::Enum(_)) => {
                if new >= catalog.enums.len()
                    || old >= catalog.enums.len()
                    || old != new && catalog.enums[new].ddl_state != super::CatalogDdlState::Absent
                {
                    return Err(invalid());
                }
                catalog.enums.swap(old, new);
                catalog.enums[new].created_at = created_at;
            }
            (NamedTypeSlot::Composite(_), NamedTypeSlot::Composite(_)) => {
                if new >= catalog.composites.len()
                    || old >= catalog.composites.len()
                    || old != new
                        && catalog.composites[new].ddl_state != super::CatalogDdlState::Absent
                {
                    return Err(invalid());
                }
                catalog.composites.swap(old, new);
                catalog.composites[new].created_at = created_at;
            }
            _ => return Err(invalid()),
        }
        self.catalog_sequence.observe(created_at);
        Ok(())
    }
}

impl super::Storage {
    pub(crate) fn finish_recovered_type_sources(&mut self) -> Result<(), SqlError> {
        let selected = self.current_database_oid();
        let result = (|| {
            for slot in 0..self.database_count() {
                let database = self.database(slot);
                if !database.visible_to(0) {
                    continue;
                }
                self.select_database_for_recovery(database.oid)?;
                self.rebind_domain_base_types()?;
                self.rebind_user_type_declarations()?;
            }
            Ok(())
        })();
        self.select_database_for_recovery(selected)?;
        result
    }
}
