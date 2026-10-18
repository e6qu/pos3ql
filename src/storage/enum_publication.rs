//! Publish member images without changing a type identity or its references.

use super::{PendingEnumDefinition, Storage};

#[derive(Debug, Clone, Copy)]
pub(crate) struct EnumAlterUndo {
    pub(super) created_at: u64,
    pub(super) transaction: u32,
    pub(super) version: u64,
    pub(super) prior: Option<PendingEnumDefinition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "type identity moves require exclusive publication"]
pub(crate) enum EnumMemberPublication {
    Complete,
    IdentityMoveRequired,
}

impl Storage {
    pub(crate) fn commit_enum_members(&self, slot: usize, txid: u32) -> EnumMemberPublication {
        let mut catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let current = &catalog.enums[slot];
        let Some(pending) = current
            .pending_definition
            .filter(|pending| pending.txid == txid)
        else {
            return EnumMemberPublication::Complete;
        };
        // A name change must publish every dependent declaration together.
        // Declining it leaves both the pending image and its history intact.
        if current.schema != pending.schema || current.name != pending.name {
            return EnumMemberPublication::IdentityMoveRequired;
        }
        assert!(
            self.install_enum_alter_locked(&mut catalog, slot, txid)
                .is_none(),
            "member publication cannot move a type identity"
        );
        EnumMemberPublication::Complete
    }
}
