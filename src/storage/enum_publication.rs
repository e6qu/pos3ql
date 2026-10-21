//! Publish member images without changing a type identity or its references.

use super::{NamedTypeKind, Storage, TypeEditTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "type identity moves require exclusive publication"]
pub(crate) enum EnumMemberPublication {
    Complete,
    IdentityMoveRequired,
}

impl Storage {
    pub(crate) fn commit_enum_members(&self, target: TypeEditTarget) -> EnumMemberPublication {
        let slot = target
            .slot(NamedTypeKind::Enum)
            .expect("enum publication identifies an enum source");
        let txid = target.transaction();
        let mut catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        if !catalog.matches_type_edit(target) {
            return EnumMemberPublication::Complete;
        }
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
        let mut images = self
            .enum_member_images
            .lock()
            .expect("enum member-image lock poisoned");
        assert!(
            !Self::install_enum_alter_locked(&mut catalog, &mut images, slot, txid),
            "member publication cannot move a type identity"
        );
        EnumMemberPublication::Complete
    }
}
