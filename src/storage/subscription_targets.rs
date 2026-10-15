//! Every subscription entry point accepts ordinary and partitioned tables.

use super::{SqlName, Storage, StoredRelKind};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

impl Storage {
    pub(crate) fn resolve_subscription_target(
        &self,
        schema: &str,
        name: &str,
        txid: u32,
    ) -> Result<usize, SqlError> {
        match self.relation_kind_in(schema, name, txid) {
            Some(StoredRelKind::Table) => self.find_visible(schema, name, txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "subscription table changed while resolving its identity"
                )
            }),
            Some(_) => Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "cannot use relation \"{}.{}\" as logical replication target",
                schema,
                name
            )),
            None => Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "logical replication target relation \"{}.{}\" does not exist",
                schema,
                name
            )),
        }
    }

    pub(crate) fn require_subscription_target(
        &self,
        table: usize,
        txid: u32,
    ) -> Result<(), SqlError> {
        let (schema, name): (SqlName, SqlName) = {
            let definition = self.table_def(table, txid);
            (definition.schema, definition.name)
        };
        if self.resolve_subscription_target(schema.as_str(), name.as_str(), txid)? != table {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "subscription table identity changed"
            ));
        }
        Ok(())
    }
}
