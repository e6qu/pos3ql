//! Validated framing for retained catalog expressions, independent of SQL input.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogNode<'a> {
    encoded: &'a str,
}

pub(crate) const HEADER: &str = "pos3ql-catalog-expression-v1\n";

impl<'a> CatalogNode<'a> {
    pub(crate) fn from_encoded(encoded: &'a str) -> Option<Self> {
        let body = encoded.strip_prefix(HEADER)?;
        let (kind, rest) = body.split_once('\n')?;
        if !matches!(kind, "expression" | "list" | "partition" | "query") {
            return None;
        }
        let (count, mut rest) = rest.split_once('\n')?;
        let count = number(count)?;
        if count > rest.len() || (matches!(kind, "partition" | "query") && count != 0) {
            return None;
        }
        let sites = rest;
        for _ in 0..count {
            rest = rest.split_once('\n')?.1;
        }
        let source = rest.strip_prefix('\n')?;
        if source.is_empty() || source.contains('\0') {
            return None;
        }
        let mut sites = sites.lines();
        let mut previous_end = 0;
        for _ in 0..count {
            let mut fields = sites.next()?.split(',');
            let start = number(fields.next()?)?;
            let end = number(fields.next()?)?;
            let column = number(fields.next()?)?;
            if fields.next().is_some() || column == 0 || column > crate::storage::MAX_COLUMNS
                || start < previous_end || end <= start
                || !source.is_char_boundary(start) || !source.is_char_boundary(end) {
                return None;
            }
            previous_end = end;
        }
        Some(Self { encoded })
    }

    pub fn as_str(self) -> &'a str {
        self.encoded
    }

    pub(crate) const NULL: Self = Self {
        encoded: "pos3ql-catalog-expression-v1\nexpression\n0\n\nNULL",
    };
}

fn number(value: &str) -> Option<usize> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_snapshot_node_framing_rejects_corruption_without_allocating() {
        crate::mem::guard::forbid_alloc(|| {
            for encoded in [
                CatalogNode::NULL.as_str(),
                "pos3ql-catalog-expression-v1\nlist\n2\n0,1,1\n3,4,2\n\na, b",
                "pos3ql-catalog-expression-v1\npartition\n0\n\nDEFAULT",
                "pos3ql-catalog-expression-v1\nquery\n0\n\nSELECT 1",
                "pos3ql-catalog-expression-v1\nexpression\n1\n0,2,1\n\né > 0",
            ] {
                assert_eq!(CatalogNode::from_encoded(encoded).unwrap().as_str(), encoded);
            }
            for encoded in [
                "NULL",
                "pos3ql-catalog-expression-v0\nexpression\n0\n\nNULL",
                "pos3ql-catalog-expression-v1\nunknown\n0\n\nNULL",
                "pos3ql-catalog-expression-v1\nexpression\n00\n\nNULL",
                "pos3ql-catalog-expression-v1\nexpression\n+0\n\nNULL",
                "pos3ql-catalog-expression-v1\nexpression\n999999999999999999999999\n\nNULL",
                "pos3ql-catalog-expression-v1\nexpression\n0\nNULL",
                "pos3ql-catalog-expression-v1\nexpression\n0\n\n",
                "pos3ql-catalog-expression-v1\nexpression\n0\n\nNU\0LL",
                "pos3ql-catalog-expression-v1\nexpression\n1\n0,1,0\n\na",
                "pos3ql-catalog-expression-v1\nexpression\n1\n0,1,65535\n\na",
                "pos3ql-catalog-expression-v1\nexpression\n1\n0,1,1,2\n\na",
                "pos3ql-catalog-expression-v1\nexpression\n1\n0,2,1\n\na",
                "pos3ql-catalog-expression-v1\nexpression\n1\n1,2,1\n\né",
                "pos3ql-catalog-expression-v1\nexpression\n2\n0,1,1\n0,1,1\n\na",
                "pos3ql-catalog-expression-v1\nexpression\n2\n0,2,1\n1,2,2\n\nab",
                "pos3ql-catalog-expression-v1\nquery\n1\n0,1,1\n\na",
                "pos3ql-catalog-expression-v1\npartition\n1\n0,1,1\n\na",
            ] {
                assert!(CatalogNode::from_encoded(encoded).is_none(), "{encoded:?}");
            }
        });
    }
}
