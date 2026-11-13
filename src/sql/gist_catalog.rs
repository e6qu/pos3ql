//! PostgreSQL 18.6 built-in GiST bootstrap catalog rows.
//! Provenance: `pg_amop` and `pg_amproc` queried from an unmodified 18.6
//! server; stable OIDs preserve client catalog joins exactly.

pub(crate) const FAMILIES: [(i32, &str); 9] = [
    (1029, "point_ops"),
    (2593, "box_ops"),
    (2594, "poly_ops"),
    (2595, "circle_ops"),
    (3550, "network_ops"),
    (3655, "tsvector_ops"),
    (3702, "tsquery_ops"),
    (3919, "range_ops"),
    (6158, "multirange_ops"),
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
    fn postgresql_18_6_gist_support_catalog_snapshot_is_stable() {
        assert_eq!(FAMILIES.len(), 9);

        let mut hash = 0xcbf2_9ce4_8422_2325;
        for row in FAMILIES {
            mix(&mut hash, &row.0.to_le_bytes());
            mix(&mut hash, &(row.1.len() as u64).to_le_bytes());
            mix(&mut hash, row.1.as_bytes());
        }
        assert_eq!(hash, 12_102_468_264_755_534_889);
    }
}
