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

type OperatorCatalogRow = (i32, i32, i32, i32, i16, &'static str, i32, i32);

pub(crate) const OPERATORS: &[OperatorCatalogRow] = &[
    (10349, 2745, 2277, 2277, 1, "s", 2750, 0),
    (10350, 2745, 2277, 2277, 2, "s", 2751, 0),
    (10351, 2745, 2277, 2277, 3, "s", 2752, 0),
    (10352, 2745, 2277, 2277, 4, "s", 1070, 0),
    (10365, 3659, 3614, 3615, 1, "s", 3636, 0),
    (10366, 3659, 3614, 3615, 2, "s", 3660, 0),
    (10456, 4036, 3802, 3802, 7, "s", 3246, 0),
    (10457, 4036, 3802, 25, 9, "s", 3247, 0),
    (10458, 4036, 3802, 1009, 10, "s", 3248, 0),
    (10459, 4036, 3802, 1009, 11, "s", 3249, 0),
    (10460, 4036, 3802, 4072, 15, "s", 4012, 0),
    (10461, 4036, 3802, 4072, 16, "s", 4013, 0),
    (10462, 4037, 3802, 3802, 7, "s", 3246, 0),
    (10463, 4037, 3802, 4072, 15, "s", 4012, 0),
    (10464, 4037, 3802, 4072, 16, "s", 4013, 0),
];

pub(crate) const PROCEDURES: &[(i32, i32, i32, i32, i16, i32, &str)] = &[
    (
        10276,
        2745,
        2277,
        2277,
        2,
        2743,
        "pg_catalog.ginarrayextract",
    ),
    (10277, 2745, 2277, 2277, 3, 2774, "ginqueryarrayextract"),
    (10278, 2745, 2277, 2277, 4, 2744, "ginarrayconsistent"),
    (10279, 2745, 2277, 2277, 6, 3920, "ginarraytriconsistent"),
    (10280, 3659, 3614, 3614, 1, 3724, "gin_cmp_tslexeme"),
    (
        10281,
        3659,
        3614,
        3614,
        2,
        3656,
        "pg_catalog.gin_extract_tsvector",
    ),
    (
        10282,
        3659,
        3614,
        3614,
        3,
        3657,
        "pg_catalog.gin_extract_tsquery",
    ),
    (
        10283,
        3659,
        3614,
        3614,
        4,
        3658,
        "pg_catalog.gin_tsquery_consistent",
    ),
    (10284, 3659, 3614, 3614, 5, 2700, "gin_cmp_prefix"),
    (
        10285,
        3659,
        3614,
        3614,
        6,
        3921,
        "gin_tsquery_triconsistent",
    ),
    (10286, 4036, 3802, 3802, 1, 3480, "gin_compare_jsonb"),
    (10287, 4036, 3802, 3802, 2, 3482, "gin_extract_jsonb"),
    (10288, 4036, 3802, 3802, 3, 3483, "gin_extract_jsonb_query"),
    (10289, 4036, 3802, 3802, 4, 3484, "gin_consistent_jsonb"),
    (10290, 4036, 3802, 3802, 6, 3488, "gin_triconsistent_jsonb"),
    (10291, 4037, 3802, 3802, 1, 351, "btint4cmp"),
    (10292, 4037, 3802, 3802, 2, 3485, "gin_extract_jsonb_path"),
    (
        10293,
        4037,
        3802,
        3802,
        3,
        3486,
        "gin_extract_jsonb_query_path",
    ),
    (
        10294,
        4037,
        3802,
        3802,
        4,
        3487,
        "gin_consistent_jsonb_path",
    ),
    (
        10295,
        4037,
        3802,
        3802,
        6,
        3489,
        "gin_triconsistent_jsonb_path",
    ),
];

#[cfg(test)]
mod tests {
    use super::{FAMILIES, OPERATORS, PROCEDURES};

    fn mix(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }

    #[test]
    fn postgresql_18_6_gin_support_catalog_snapshot_is_stable() {
        assert_eq!(FAMILIES.len(), 4);
        assert_eq!(OPERATORS.len(), 15);
        assert_eq!(PROCEDURES.len(), 20);

        let mut hash = 0xcbf2_9ce4_8422_2325;
        for row in FAMILIES {
            mix(&mut hash, &row.0.to_le_bytes());
            mix(&mut hash, &(row.1.len() as u64).to_le_bytes());
            mix(&mut hash, row.1.as_bytes());
        }
        for row in OPERATORS {
            for value in [row.0, row.1, row.2, row.3, i32::from(row.4), row.6, row.7] {
                mix(&mut hash, &value.to_le_bytes());
            }
            mix(&mut hash, &(row.5.len() as u64).to_le_bytes());
            mix(&mut hash, row.5.as_bytes());
        }
        for row in PROCEDURES {
            for value in [row.0, row.1, row.2, row.3, i32::from(row.4), row.5] {
                mix(&mut hash, &value.to_le_bytes());
            }
            mix(&mut hash, &(row.6.len() as u64).to_le_bytes());
            mix(&mut hash, row.6.as_bytes());
        }
        assert_eq!(hash, 4_981_215_225_332_455_774);
    }
}
