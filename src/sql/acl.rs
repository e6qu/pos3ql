//! PostgreSQL `aclitem` parsing and canonical text representation.

use core::fmt::Write as _;

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::AclItem;
use crate::sql_err;

const PRIVILEGES: &[u8] = b"arwdDxtXUCTcsAm";
const PRIVILEGE_NAMES: &[&str] = &[
    "INSERT",
    "SELECT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "EXECUTE",
    "USAGE",
    "CREATE",
    "TEMPORARY",
    "CONNECT",
    "SET",
    "ALTER SYSTEM",
    "MAINTAIN",
];

enum RenderedRole<'a> {
    Public,
    Name(&'a str),
    Missing(u32),
}

impl core::fmt::Display for RenderedRole<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Public => Ok(()),
            Self::Name(name) => crate::sql::types::acl_identifier(name).fmt(f),
            Self::Missing(oid) => oid.fmt(f),
        }
    }
}

fn write_item(
    out: &mut impl core::fmt::Write,
    grantee: impl core::fmt::Display,
    grantor: impl core::fmt::Display,
    privileges: u16,
    grant_options: u16,
) -> core::fmt::Result {
    write!(out, "{grantee}=")?;
    for (index, privilege) in PRIVILEGES.iter().copied().enumerate() {
        let mask = 1u16 << index;
        if privileges & mask != 0 {
            out.write_char(privilege as char)?;
            if grant_options & mask != 0 {
                out.write_char('*')?;
            }
        }
    }
    write!(out, "/{grantor}")
}

pub fn from_text<'a>(
    value: &str,
    catalog: &dyn crate::sql::eval::CatalogAccess,
    arena: &'a Arena,
) -> Result<AclItem<'a>, SqlError> {
    let parsed = parse(value)?;
    validate_input_roles(parsed, catalog, arena)?;
    build_item(
        role_oid(parsed.grantee, catalog, arena)?,
        role_oid(parsed.grantor, catalog, arena)?,
        parsed.privilege_bits(),
        parsed.grant_option_bits(),
        canonicalize(value, arena)?,
        arena,
    )
}

pub fn with_identity<'a>(
    grantee: u32,
    grantor: u32,
    value: &str,
    arena: &'a Arena,
) -> Result<AclItem<'a>, SqlError> {
    let parsed = parse(value)?;
    build_item(
        grantee,
        grantor,
        parsed.privilege_bits(),
        parsed.grant_option_bits(),
        value,
        arena,
    )
}

/// Restores a durable item, checking that its cached spelling agrees with the
/// privilege masks. Role OIDs are authoritative; names are only a render cache.
pub fn from_stored(raw: &[u8]) -> Result<AclItem<'_>, SqlError> {
    let item = AclItem::from_raw(raw).ok_or_else(|| invalid("stored aclitem"))?;
    let parsed = parse(item.text())?;
    if parsed.privilege_bits() != item.privileges()
        || parsed.grant_option_bits() != item.grant_options()
    {
        return Err(invalid(item.text()));
    }
    Ok(item)
}

fn build_item<'a>(
    grantee: u32,
    grantor: u32,
    privileges: u16,
    grant_options: u16,
    text: &str,
    arena: &'a Arena,
) -> Result<AclItem<'a>, SqlError> {
    let len = AclItem::HEADER_LEN
        .checked_add(text.len())
        .ok_or_else(|| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "aclitem is too large"))?;
    let raw = arena
        .alloc_slice_with(len, |_| 0u8)
        .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "query arena exhausted"))?;
    raw[..4].copy_from_slice(&grantee.to_le_bytes());
    raw[4..8].copy_from_slice(&grantor.to_le_bytes());
    raw[8..10].copy_from_slice(&privileges.to_le_bytes());
    raw[10..12].copy_from_slice(&grant_options.to_le_bytes());
    raw[12..].copy_from_slice(text.as_bytes());
    AclItem::from_raw(raw).ok_or_else(|| invalid(text))
}

pub fn materialize<'a>(
    item: AclItem<'_>,
    catalog: &dyn crate::sql::eval::CatalogAccess,
    arena: &'a Arena,
) -> Result<AclItem<'a>, SqlError> {
    let mut out = crate::util::StackStr::<256>::new();
    let grantee = if item.grantee() == 0 {
        RenderedRole::Public
    } else {
        match catalog.role_name(item.grantee() as i32, arena)? {
            Some(name) => RenderedRole::Name(name),
            None => RenderedRole::Missing(item.grantee()),
        }
    };
    let grantor = match catalog.role_name(item.grantor() as i32, arena)? {
        Some(name) => RenderedRole::Name(name),
        None => RenderedRole::Missing(item.grantor()),
    };
    let _ = write_item(
        &mut out,
        grantee,
        grantor,
        item.privileges(),
        item.grant_options(),
    );
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "aclitem exceeds 256 bytes"
        ));
    }
    build_item(
        item.grantee(),
        item.grantor(),
        item.privileges(),
        item.grant_options(),
        out.as_str(),
        arena,
    )
}

fn invalid(value: &str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "malformed acl item: \"{}\"",
        value
    )
}

#[derive(Clone, Copy)]
pub struct Parsed<'a> {
    pub grantee: &'a str,
    pub grantor: &'a str,
    privileges: u16,
    grant_options: u16,
}

impl Parsed<'_> {
    pub fn contains(self, other: Self) -> bool {
        self.grantee == other.grantee
            && self.grantor == other.grantor
            && self.privileges & other.privileges == other.privileges
            && self.grant_options & other.grant_options == other.grant_options
    }

    pub fn equivalent(self, other: Self) -> bool {
        self.grantee == other.grantee
            && self.grantor == other.grantor
            && self.privileges == other.privileges
            && self.grant_options == other.grant_options
    }

    pub fn privilege(self, index: usize) -> Option<(&'static str, bool)> {
        let mask = 1u16.checked_shl(index as u32)?;
        (self.privileges & mask != 0)
            .then(|| (PRIVILEGE_NAMES[index], self.grant_options & mask != 0))
    }

    pub const fn privilege_count(self) -> usize {
        self.privileges.count_ones() as usize
    }

    pub const fn privilege_bits(self) -> u16 {
        self.privileges
    }

    pub const fn grant_option_bits(self) -> u16 {
        self.grant_options
    }
}

fn split_role(input: &str, delimiter: u8) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    let mut quoted = false;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' if quoted && bytes.get(index + 1) == Some(&b'"') => index += 2,
            b'"' => {
                quoted = !quoted;
                index += 1;
            }
            byte if byte == delimiter && !quoted => {
                return Some((&input[..index], &input[index + 1..]));
            }
            _ => index += 1,
        }
    }
    None
}

fn valid_role(role: &str, public_allowed: bool) -> bool {
    if role.is_empty() {
        return public_allowed;
    }
    if let Some(inner) = role.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        if inner.is_empty() || inner.as_bytes().contains(&0) {
            return false;
        }
        let bytes = inner.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'"' {
                if bytes.get(index + 1) != Some(&b'"') {
                    return false;
                }
                index += 2;
            } else {
                index += 1;
            }
        }
        true
    } else {
        !role
            .bytes()
            .any(|byte| matches!(byte, b'"' | b'=' | b'/') || byte == 0)
    }
}

pub fn role_name<'a>(token: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    if token.is_empty() {
        return Ok("");
    }
    let Some(inner) = token
        .strip_prefix('"')
        .and_then(|role| role.strip_suffix('"'))
    else {
        return arena
            .alloc_str(token)
            .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "query arena exhausted"));
    };
    let mut out = crate::util::StackStr::<256>::new();
    let mut rest = inner;
    while let Some((prefix, suffix)) = rest.split_once("\"\"") {
        let _ = out.write_str(prefix);
        let _ = out.write_char('"');
        rest = suffix;
    }
    let _ = out.write_str(rest);
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "role name exceeds 256 bytes"
        ));
    }
    arena
        .alloc_str(out.as_str())
        .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "query arena exhausted"))
}

pub fn role_oid(
    token: &str,
    catalog: &dyn crate::sql::eval::CatalogAccess,
    arena: &Arena,
) -> Result<u32, SqlError> {
    if token.is_empty() {
        return Ok(0);
    }
    let name = role_name(token, arena)?;
    if let Some(oid) = catalog.role_oid(name) {
        return u32::try_from(oid)
            .map_err(|_| sql_err!(sqlstate::INTERNAL_ERROR, "role OID is invalid"));
    }
    // Values produced by makeaclitem can contain an OID whose role has since
    // disappeared. PostgreSQL retains and hashes that identity numerically.
    if !token.starts_with('"')
        && let Ok(oid) = token.parse::<u32>()
    {
        return Ok(oid);
    }
    Err(sql_err!(
        sqlstate::UNDEFINED_OBJECT,
        "role \"{}\" does not exist",
        name
    ))
}

pub fn validate_input_roles(
    parsed: Parsed<'_>,
    catalog: &dyn crate::sql::eval::CatalogAccess,
    arena: &Arena,
) -> Result<(), SqlError> {
    for token in [parsed.grantee, parsed.grantor] {
        if token.is_empty() {
            continue;
        }
        let name = role_name(token, arena)?;
        if catalog.role_oid(name).is_none() {
            return Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "role \"{}\" does not exist",
                name
            ));
        }
    }
    Ok(())
}

pub fn hash_value(
    parsed: Parsed<'_>,
    catalog: &dyn crate::sql::eval::CatalogAccess,
    arena: &Arena,
) -> Result<u32, SqlError> {
    let grantee = role_oid(parsed.grantee, catalog, arena)?;
    let grantor = role_oid(parsed.grantor, catalog, arena)?;
    // PostgreSQL deliberately hashes aclitem as this wrapping sum rather than
    // hashing its native struct, whose padding is not portable. Grant-option
    // bits occupy the high half of ai_privs and vanish in the uint32 result.
    Ok(grantee
        .wrapping_add(grantor)
        .wrapping_add(u32::from(parsed.privilege_bits())))
}

#[derive(Clone, Copy)]
pub struct Exploded {
    pub grantor: u32,
    pub grantee: u32,
    pub privilege: &'static str,
    pub grantable: bool,
}

pub fn explode_count(raw: &[u8]) -> Result<usize, SqlError> {
    let mut count = 0usize;
    for index in 0..crate::sql::array::len(raw) {
        let value = crate::sql::array::get(raw, crate::sql::types::ArrElem::AclItem, index)
            .unwrap_or(crate::sql::types::Datum::Null);
        let privileges = match value {
            crate::sql::types::Datum::Text(value) => parse(value)?.privilege_count(),
            crate::sql::types::Datum::AclItem(item) => item.privileges().count_ones() as usize,
            _ => {
                return Err(sql_err!(
                    sqlstate::NULL_VALUE_NOT_ALLOWED,
                    "ACL arrays must not contain null values"
                ));
            }
        };
        count = count.checked_add(privileges).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "ACL expansion exceeds configured capacity"
            )
        })?;
    }
    Ok(count)
}

pub fn explode_at(
    raw: &[u8],
    wanted: usize,
    catalog: &dyn crate::sql::eval::CatalogAccess,
    arena: &Arena,
) -> Result<Option<Exploded>, SqlError> {
    let mut current = 0usize;
    for index in 0..crate::sql::array::len(raw) {
        let value = crate::sql::array::get(raw, crate::sql::types::ArrElem::AclItem, index)
            .unwrap_or(crate::sql::types::Datum::Null);
        let (grantor, grantee, privileges, grant_options) = match value {
            crate::sql::types::Datum::Text(value) => {
                let parsed = parse(value)?;
                (
                    role_oid(parsed.grantor, catalog, arena)?,
                    role_oid(parsed.grantee, catalog, arena)?,
                    parsed.privilege_bits(),
                    parsed.grant_option_bits(),
                )
            }
            crate::sql::types::Datum::AclItem(item) => (
                item.grantor(),
                item.grantee(),
                item.privileges(),
                item.grant_options(),
            ),
            _ => {
                return Err(sql_err!(
                    sqlstate::NULL_VALUE_NOT_ALLOWED,
                    "ACL arrays must not contain null values"
                ));
            }
        };
        for (privilege_index, privilege) in PRIVILEGE_NAMES.iter().enumerate() {
            let mask = 1u16 << privilege_index;
            if privileges & mask == 0 {
                continue;
            }
            if current == wanted {
                return Ok(Some(Exploded {
                    grantor,
                    grantee,
                    privilege,
                    grantable: grant_options & mask != 0,
                }));
            }
            current += 1;
        }
    }
    Ok(None)
}

pub fn parse(value: &str) -> Result<Parsed<'_>, SqlError> {
    let (grantee, rest) = split_role(value, b'=').ok_or_else(|| invalid(value))?;
    let (written, grantor) = split_role(rest, b'/').ok_or_else(|| invalid(value))?;
    if !valid_role(grantee, true) || !valid_role(grantor, false) {
        return Err(invalid(value));
    }
    let mut privileges = 0u16;
    let mut grant_options = 0u16;
    let bytes = written.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let Some(bit) = PRIVILEGES
            .iter()
            .position(|candidate| *candidate == bytes[index])
        else {
            return Err(invalid(value));
        };
        let mask = 1u16 << bit;
        privileges |= mask;
        index += 1;
        if bytes.get(index) == Some(&b'*') {
            grant_options |= mask;
            index += 1;
        }
    }
    Ok(Parsed {
        grantee,
        grantor,
        privileges,
        grant_options,
    })
}

pub fn canonicalize<'a>(value: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let parsed = parse(value)?;
    let mut out = crate::util::StackStr::<256>::new();
    let _ = write!(out, "{}=", parsed.grantee);
    for (index, privilege) in PRIVILEGES.iter().copied().enumerate() {
        let mask = 1u16 << index;
        if parsed.privileges & mask != 0 {
            let _ = out.write_char(privilege as char);
            if parsed.grant_options & mask != 0 {
                let _ = out.write_char('*');
            }
        }
    }
    let _ = write!(out, "/{}", parsed.grantor);
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "aclitem exceeds 256 bytes"
        ));
    }
    arena
        .alloc_str(out.as_str())
        .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "query arena exhausted"))
}

pub fn make<'a>(
    grantee: &str,
    grantor: &str,
    privilege: &str,
    grantable: bool,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let code = if privilege.eq_ignore_ascii_case("insert") {
        'a'
    } else if privilege.eq_ignore_ascii_case("select") {
        'r'
    } else if privilege.eq_ignore_ascii_case("update") {
        'w'
    } else if privilege.eq_ignore_ascii_case("delete") {
        'd'
    } else if privilege.eq_ignore_ascii_case("truncate") {
        'D'
    } else if privilege.eq_ignore_ascii_case("references") {
        'x'
    } else if privilege.eq_ignore_ascii_case("trigger") {
        't'
    } else if privilege.eq_ignore_ascii_case("execute") {
        'X'
    } else if privilege.eq_ignore_ascii_case("usage") {
        'U'
    } else if privilege.eq_ignore_ascii_case("create") {
        'C'
    } else if privilege.eq_ignore_ascii_case("temporary") || privilege.eq_ignore_ascii_case("temp")
    {
        'T'
    } else if privilege.eq_ignore_ascii_case("connect") {
        'c'
    } else if privilege.eq_ignore_ascii_case("set") {
        's'
    } else if privilege.eq_ignore_ascii_case("alter system") {
        'A'
    } else if privilege.eq_ignore_ascii_case("maintain") {
        'm'
    } else {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "unrecognized privilege type: \"{}\"",
            privilege
        ));
    };
    let mut out = crate::util::StackStr::<256>::new();
    let _ = write!(
        out,
        "{}={}{}/{}",
        grantee,
        code,
        if grantable { "*" } else { "" },
        grantor
    );
    arena
        .alloc_str(out.as_str())
        .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "query arena exhausted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_canonicalizes_privileges() {
        let mut budget = crate::mem::budget::Budget::new(2048);
        let arena = Arena::new(&mut budget, "acl test", 1024).unwrap();
        assert_eq!(
            canonicalize("postgres=wr*r/postgres", &arena).unwrap(),
            "postgres=r*w/postgres"
        );
        assert!(parse("postgres=r").is_err());
        assert!(parse("postgres=?/postgres").is_err());
    }
}
