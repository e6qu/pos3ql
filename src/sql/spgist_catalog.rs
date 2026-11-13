//! PostgreSQL 18.6 built-in SP-GiST bootstrap catalog rows.
//!
//! Generated from the official PostgreSQL 18.6 source tarball at
//! `https://ftp.postgresql.org/pub/source/v18.6/postgresql-18.6.tar.bz2`
//! and an unmodified `initdb` catalog. Source tarball SHA-256:
//! `555610c24d53e4316da5b7d3fc25c279d96856d5e0e23ee308c328c5fa881d9f`.

pub(crate) const FAMILIES: [(i32, &str); 7] = [
    (3474, "range_ops"),
    (3794, "network_ops"),
    (4015, "quad_point_ops"),
    (4016, "kd_point_ops"),
    (4017, "text_ops"),
    (5000, "box_ops"),
    (5008, "poly_ops"),
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
    fn postgresql_18_6_spgist_support_catalog_snapshot_is_stable() {
        assert_eq!(FAMILIES.len(), 7);

        let mut hash = 0xcbf2_9ce4_8422_2325;
        for row in FAMILIES {
            mix(&mut hash, &row.0.to_le_bytes());
            mix(&mut hash, &(row.1.len() as u64).to_le_bytes());
            mix(&mut hash, row.1.as_bytes());
        }
        assert_eq!(hash, 3_316_621_876_794_099_510);
    }
}
