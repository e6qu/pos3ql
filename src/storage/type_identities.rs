//! Checked creation stamps for named SQL type lifetimes.

use core::num::NonZeroU8;

pub(crate) const MAX_TYPE_CREATION: u64 = 9_999_999;

/// A nonzero stamp with a three-byte optional representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub(crate) struct TypeCreation {
    low: NonZeroU8,
    upper: [u8; 2],
}

impl TypeCreation {
    /// Rejects zero and stamps outside the named-type identifier range.
    pub(crate) const fn parse(created_at: u64) -> Option<Self> {
        if created_at == 0 || created_at > MAX_TYPE_CREATION {
            return None;
        }
        let ordinal = created_at as u32 - 1;
        // Radix 255 leaves the first byte nonzero, so Option needs no tag byte.
        let low = match NonZeroU8::new((ordinal % 255 + 1) as u8) {
            Some(low) => low,
            None => return None,
        };
        Some(Self {
            low,
            upper: ((ordinal / 255) as u16).to_le_bytes(),
        })
    }

    pub(crate) const fn created_at(self) -> u64 {
        self.low.get() as u64 + 255 * u16::from_le_bytes(self.upper) as u64
    }
}

const _: [(); 3] = [(); core::mem::size_of::<TypeCreation>()];
const _: [(); 3] = [(); core::mem::size_of::<Option<TypeCreation>>()];
const _: [(); 1] = [(); core::mem::align_of::<TypeCreation>()];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_creation_identity_roundtrips_without_allocation() {
        crate::mem::guard::forbid_alloc(|| {
            for upper in [0, 1, 254, 255, 256, 32_768, 39_214, 39_215] {
                for low in [0, 1, 253, 254] {
                    let created_at = upper * 255 + low + 1;
                    if created_at <= MAX_TYPE_CREATION {
                        assert_eq!(
                            TypeCreation::parse(created_at).unwrap().created_at(),
                            created_at
                        );
                    }
                }
            }
            assert_eq!(
                TypeCreation::parse(MAX_TYPE_CREATION).unwrap().created_at(),
                MAX_TYPE_CREATION
            );
            for rejected in [0, MAX_TYPE_CREATION + 1, u64::MAX] {
                assert!(TypeCreation::parse(rejected).is_none());
            }
        });
    }
}
