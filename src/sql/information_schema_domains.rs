//! The domains PostgreSQL's `information_schema` declares for its view columns.
//!
//! Source: <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql>
//! (`CREATE DOMAIN cardinal_number ...` through `yes_or_no`). initdb numbers
//! them after the schema and its helper functions, from the counter
//! `INFORMATION_SCHEMA_NS_OID` documents.

use crate::sql::ast::Collation;
use crate::sql::catalog::INFORMATION_SCHEMA_NS_OID;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InformationSchemaDomain {
    CardinalNumber,
    CharacterData,
    SqlIdentifier,
    TimeStamp,
    YesOrNo,
}

/// A domain's single CHECK constraint.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DomainCheck {
    pub oid: i32,
    pub name: &'static str,
    /// The CHECK expression as information_schema.sql declares it.
    pub source: &'static str,
}

impl InformationSchemaDomain {
    pub(crate) const ALL: [Self; 5] = [
        Self::CardinalNumber,
        Self::CharacterData,
        Self::SqlIdentifier,
        Self::TimeStamp,
        Self::YesOrNo,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::CardinalNumber => "cardinal_number",
            Self::CharacterData => "character_data",
            Self::SqlIdentifier => "sql_identifier",
            Self::TimeStamp => "time_stamp",
            Self::YesOrNo => "yes_or_no",
        }
    }

    pub(crate) const fn oid(self) -> i32 {
        match self {
            Self::CardinalNumber => INFORMATION_SCHEMA_NS_OID + 14,
            Self::CharacterData => INFORMATION_SCHEMA_NS_OID + 17,
            Self::SqlIdentifier => INFORMATION_SCHEMA_NS_OID + 19,
            Self::TimeStamp => INFORMATION_SCHEMA_NS_OID + 25,
            Self::YesOrNo => INFORMATION_SCHEMA_NS_OID + 27,
        }
    }

    pub(crate) const fn array_oid(self) -> i32 {
        self.oid() - 1
    }

    pub(crate) const fn base(self) -> ColType {
        match self {
            Self::CardinalNumber => ColType::Int4,
            Self::CharacterData | Self::YesOrNo => ColType::Varchar,
            Self::SqlIdentifier => ColType::Name,
            Self::TimeStamp => ColType::Timestamptz,
        }
    }

    /// `pg_type.typtypmod`: `timestamp(2) with time zone` and `varchar(3)`.
    pub(crate) const fn base_type_mod(self) -> i32 {
        match self {
            Self::TimeStamp => 2,
            Self::YesOrNo => 3 + 4,
            Self::CardinalNumber | Self::CharacterData | Self::SqlIdentifier => -1,
        }
    }

    /// The character domains are declared `COLLATE "C"`.
    pub(crate) const fn collation(self) -> Collation {
        match self {
            Self::CharacterData | Self::SqlIdentifier | Self::YesOrNo => Collation::C,
            Self::CardinalNumber | Self::TimeStamp => Collation::None,
        }
    }

    pub(crate) const fn default_text(self) -> Option<&'static str> {
        match self {
            Self::TimeStamp => Some("CURRENT_TIMESTAMP(2)"),
            _ => None,
        }
    }

    pub(crate) const fn check(self) -> Option<DomainCheck> {
        match self {
            Self::CardinalNumber => Some(DomainCheck {
                oid: INFORMATION_SCHEMA_NS_OID + 15,
                name: "cardinal_number_domain_check",
                source: "VALUE >= 0",
            }),
            Self::YesOrNo => Some(DomainCheck {
                oid: INFORMATION_SCHEMA_NS_OID + 28,
                name: "yes_or_no_check",
                // PostgreSQL stores the declared `VALUE IN ('YES', 'NO')` in
                // this analyzed form.
                source: "(VALUE)::text = ANY ((ARRAY['YES'::character varying, 'NO'::character varying])::text[])",
            }),
            Self::CharacterData | Self::SqlIdentifier | Self::TimeStamp => None,
        }
    }

    /// `pg_type.typcategory` follows the base type.
    pub(crate) const fn category(self) -> &'static str {
        match self {
            Self::CardinalNumber => "N",
            Self::CharacterData | Self::SqlIdentifier | Self::YesOrNo => "S",
            Self::TimeStamp => "D",
        }
    }

    /// The domain and whether `oid` names its array type.
    pub(crate) fn from_oid(oid: i32) -> Option<(Self, bool)> {
        Self::ALL.into_iter().find_map(|domain| {
            if domain.oid() == oid {
                Some((domain, false))
            } else if domain.array_oid() == oid {
                Some((domain, true))
            } else {
                None
            }
        })
    }

    /// Resolves a written type name; unqualified names resolve only when
    /// `information_schema` is on the search path.
    pub(crate) fn from_name(written: &str, unqualified_visible: bool) -> Option<(Self, bool)> {
        let (base, array) = written
            .strip_suffix("[]")
            .map_or((written, false), |base| (base, true));
        let name = match base.split_once('.') {
            Some(("information_schema", name)) => name,
            Some(_) => return None,
            None if unqualified_visible => base,
            None => return None,
        };
        Self::ALL
            .into_iter()
            .find(|domain| domain.name() == name)
            .map(|domain| (domain, array))
    }

    /// Applies the domain's CHECK constraint to an already coerced base value.
    pub(crate) fn validate(self, value: &Datum<'_>) -> Result<(), SqlError> {
        let accepted = match (self, value) {
            (_, Datum::Null) => true,
            (Self::CardinalNumber, Datum::Int4(value)) => *value >= 0,
            (Self::YesOrNo, Datum::Text(value)) => {
                matches!(*value, "YES" | "NO")
            }
            (Self::CardinalNumber | Self::YesOrNo, _) => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "domain {} received a value of another base type",
                    self.name()
                ));
            }
            (Self::CharacterData | Self::SqlIdentifier | Self::TimeStamp, _) => true,
        };
        if accepted {
            return Ok(());
        }
        let check = self.check().expect("rejecting domain has a check");
        Err(sql_err!(
            sqlstate::CHECK_VIOLATION,
            "value for domain information_schema.{} violates check constraint \"{}\"",
            self.name(),
            check.name
        ))
    }
}

/// The declared domain of an `information_schema` view column, as PostgreSQL's
/// view definitions declare it. Columns of built-in types return `None`.
pub(crate) fn view_column_domain(view: &str, column: &str) -> Option<InformationSchemaDomain> {
    columns::COLUMNS
        .iter()
        .find(|(candidate, name, _)| *candidate == view && *name == column)
        .map(|(_, _, domain)| *domain)
}

mod columns;
