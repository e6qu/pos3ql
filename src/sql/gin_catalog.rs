//! PostgreSQL 18.6 built-in GIN bootstrap catalog rows.
//!
//! Generated from the official PostgreSQL 18.6 source tarball at
//! `https://ftp.postgresql.org/pub/source/v18.6/postgresql-18.6.tar.bz2`
//! and an unmodified `initdb` catalog. Source tarball SHA-256:
//! `555610c24d53e4316da5b7d3fc25c279d96856d5e0e23ee308c328c5fa881d9f`.

pub(crate) const FAMILIES: [(i32, &str); 4] = [
    (2745, "array_ops"),
    (3659, "tsvector_ops"),
    (4036, "jsonb_ops"),
    (4037, "jsonb_path_ops"),
];

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::FAMILIES;

    fn mix(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }

    #[test]
    fn postgresql_18_6_gin_support_catalog_snapshot_is_stable() {
        assert_eq!(FAMILIES.len(), 4);

        let mut hash = 0xcbf2_9ce4_8422_2325;
        for row in FAMILIES {
            mix(&mut hash, &row.0.to_le_bytes());
            mix(&mut hash, &(row.1.len() as u64).to_le_bytes());
            mix(&mut hash, row.1.as_bytes());
        }
        assert_eq!(hash, 6_786_515_908_560_691_389);
    }
}
