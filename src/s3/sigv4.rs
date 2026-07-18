//! AWS Signature Version 4 request signing, validated against the official
//! AWS SigV4 test suite (vendored in boto/botocore under
//! tests/unit/auth/aws4_testsuite, commit 5378504).
//!
//! The canonical request is streamed straight into the hash — it never
//! needs to exist as text, so signing allocates nothing.

use crate::util::StackStr;

use super::hmac::hmac_sha256;
use super::sha256::{HexDigest, Sha256};

/// Everything that goes into a signature. Headers must be presorted by
/// (lowercase) name and include `host`; values must already be trimmed.
pub struct SigningInput<'a> {
    pub method: &'a str,
    /// Canonical URI (path), already percent-encoded as needed.
    pub uri: &'a str,
    /// Canonical query string: sorted, percent-encoded, no leading '?'.
    pub query: &'a str,
    /// (lowercase-name, value), sorted by name.
    pub headers: &'a [(&'a str, &'a str)],
    /// Hex SHA-256 of the payload.
    pub payload_sha256_hex: &'a str,
    /// `YYYYMMDD'T'HHMMSS'Z'`.
    pub timestamp: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

/// The hex signature plus the pieces needed for the Authorization header.
pub struct Signature {
    pub hex: HexDigest,
}

pub fn sign(secret_key: &str, input: &SigningInput) -> Signature {
    // Canonical request, streamed into the hasher.
    let mut creq = Sha256::new();
    creq.update(input.method.as_bytes());
    creq.update(b"\n");
    creq.update(input.uri.as_bytes());
    creq.update(b"\n");
    creq.update(input.query.as_bytes());
    creq.update(b"\n");
    for (name, value) in input.headers {
        creq.update(name.as_bytes());
        creq.update(b":");
        creq.update(value.as_bytes());
        creq.update(b"\n");
    }
    creq.update(b"\n");
    update_signed_headers(&mut creq, input.headers);
    creq.update(b"\n");
    creq.update(input.payload_sha256_hex.as_bytes());
    let creq_hash = HexDigest::of(&creq.finish());

    let date = &input.timestamp[..8];

    // String to sign.
    let mut sts = StackStr::<256>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut sts,
        format_args!(
            "AWS4-HMAC-SHA256\n{}\n{}/{}/{}/aws4_request\n{}",
            input.timestamp,
            date,
            input.region,
            input.service,
            creq_hash.as_str()
        ),
    );
    debug_assert!(!sts.is_truncated());

    // Signing key chain.
    let mut seed = StackStr::<128>::new();
    let _ = core::fmt::Write::write_fmt(&mut seed, format_args!("AWS4{secret_key}"));
    debug_assert!(!seed.is_truncated());
    let k_date = hmac_sha256(seed.as_str().as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, input.region.as_bytes());
    let k_service = hmac_sha256(&k_region, input.service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");

    let mac = hmac_sha256(&k_signing, sts.as_str().as_bytes());
    Signature {
        hex: HexDigest::of(&mac),
    }
}

/// `host;x-amz-content-sha256;x-amz-date`-style list, streamed.
fn update_signed_headers(hasher: &mut Sha256, headers: &[(&str, &str)]) {
    for (i, (name, _)) in headers.iter().enumerate() {
        if i > 0 {
            hasher.update(b";");
        }
        hasher.update(name.as_bytes());
    }
}

/// Writes the signed-headers list into a formatter target.
pub fn write_signed_headers<W: core::fmt::Write>(
    out: &mut W,
    headers: &[(&str, &str)],
) -> core::fmt::Result {
    for (i, (name, _)) in headers.iter().enumerate() {
        if i > 0 {
            out.write_char(';')?;
        }
        out.write_str(name)?;
    }
    Ok(())
}

/// Percent-encodes into `out` per SigV4 rules: unreserved bytes
/// (A–Z a–z 0–9 - . _ ~) pass through; `/` passes when `is_path`;
/// everything else becomes uppercase %XX.
pub fn uri_encode<W: core::fmt::Write>(out: &mut W, s: &str, is_path: bool) -> core::fmt::Result {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved || (is_path && b == b'/') {
            out.write_char(b as char)?;
        } else {
            out.write_char('%')?;
            out.write_char(HEX[(b >> 4) as usize] as char)?;
            out.write_char(HEX[(b & 0xf) as usize] as char)?;
        }
    }
    Ok(())
}

/// Formats a unix timestamp as (`YYYYMMDD'T'HHMMSS'Z'`, days handled via
/// the civil-from-days algorithm — exact for the whole u32 epoch range).
pub fn format_amz_timestamp(unix_secs: i64) -> StackStr<16> {
    let days = unix_secs.div_euclid(86400);
    let secs = unix_secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    let mut out = StackStr::<16>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60
        ),
    );
    out
}

/// Days-since-epoch → (year, month, day). Howard Hinnant's public-domain
/// `civil_from_days` (<https://howardhinnant.github.io/date_algorithms.html>).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Credentials/region/service used by the whole official suite, per
    /// botocore's tests/unit/auth/test_sigv4.py.
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    /// SHA-256 of an empty payload.
    const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn suite_input<'a>(
        method: &'a str,
        uri: &'a str,
        query: &'a str,
        headers: &'a [(&'a str, &'a str)],
    ) -> SigningInput<'a> {
        SigningInput {
            method,
            uri,
            query,
            headers,
            payload_sha256_hex: EMPTY_SHA,
            timestamp: "20150830T123600Z",
            region: "us-east-1",
            service: "service",
        }
    }

    /// aws4_testsuite/get-vanilla (expected values from get-vanilla.authz).
    #[test]
    fn get_vanilla() {
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let sig = sign(SECRET, &suite_input("GET", "/", "", &headers));
        assert_eq!(
            sig.hex.as_str(),
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// aws4_testsuite/get-vanilla-query — empty query signs identically.
    #[test]
    fn get_vanilla_query() {
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let sig = sign(SECRET, &suite_input("GET", "/", "", &headers));
        assert_eq!(
            sig.hex.as_str(),
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// aws4_testsuite/post-vanilla.
    #[test]
    fn post_vanilla() {
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let sig = sign(SECRET, &suite_input("POST", "/", "", &headers));
        assert_eq!(
            sig.hex.as_str(),
            "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
        );
    }

    /// aws4_testsuite/get-unreserved: unreserved characters stay raw.
    #[test]
    fn get_unreserved() {
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        let sig = sign(
            SECRET,
            &suite_input(
                "GET",
                "/-._~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz",
                "",
                &headers,
            ),
        );
        assert_eq!(
            sig.hex.as_str(),
            "07ef7494c76fa4850883e2b006601f940f8a34d404d0cfa977f52a65bbf5f24f"
        );
    }

    #[test]
    fn timestamp_formatting() {
        // 2015-08-30T12:36:00Z — the suite's own timestamp.
        assert_eq!(format_amz_timestamp(1_440_938_160).as_str(), "20150830T123600Z");
        assert_eq!(format_amz_timestamp(0).as_str(), "19700101T000000Z");
        // Leap-day check: 2024-02-29T23:59:59Z.
        assert_eq!(format_amz_timestamp(1_709_251_199).as_str(), "20240229T235959Z");
    }

    #[test]
    fn uri_encoding() {
        let mut out = StackStr::<128>::new();
        uri_encode(&mut out, "sst/0001 +&.dat", true).unwrap();
        assert_eq!(out.as_str(), "sst/0001%20%2B%26.dat");
        let mut out = StackStr::<128>::new();
        uri_encode(&mut out, "a/b", false).unwrap();
        assert_eq!(out.as_str(), "a%2Fb");
    }

    #[test]
    fn signing_does_not_allocate() {
        crate::mem::guard::forbid_alloc(|| {
            let headers = [
                ("host", "example.amazonaws.com"),
                ("x-amz-date", "20150830T123600Z"),
            ];
            let _ = sign(SECRET, &suite_input("GET", "/", "", &headers));
        });
    }
}
