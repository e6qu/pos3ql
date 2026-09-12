//! Allocation-free Signature Version 4 request signing for the S3-compatible
//! wire protocol.
//!
//! The wire retains its standardized `AWS4-HMAC-SHA256`, `x-amz-*`, and
//! `aws4_request` spellings. Those are protocol constants, not provider
//! selection: every endpoint receives the same request construction.

use crate::crypto::hmac::hmac_sha256;
use crate::crypto::sha256::{HexDigest, Sha256};
use crate::util::StackStr;

/// Inputs to one signature. Headers are lowercase, trimmed, and sorted.
pub(crate) struct SigningInput<'a> {
    pub method: &'a str,
    pub uri: &'a str,
    pub query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_sha256_hex: &'a str,
    pub timestamp: &'a str,
    pub region: &'a str,
}

pub(crate) struct Signature {
    pub hex: HexDigest,
}

pub(crate) fn sign(secret_key: &str, input: &SigningInput<'_>) -> Signature {
    let mut canonical_request = Sha256::new();
    canonical_request.update(input.method.as_bytes());
    canonical_request.update(b"\n");
    canonical_request.update(input.uri.as_bytes());
    canonical_request.update(b"\n");
    canonical_request.update(input.query.as_bytes());
    canonical_request.update(b"\n");
    for (name, value) in input.headers {
        canonical_request.update(name.as_bytes());
        canonical_request.update(b":");
        canonical_request.update(value.as_bytes());
        canonical_request.update(b"\n");
    }
    canonical_request.update(b"\n");
    update_signed_headers(&mut canonical_request, input.headers);
    canonical_request.update(b"\n");
    canonical_request.update(input.payload_sha256_hex.as_bytes());
    let canonical_hash = HexDigest::of(&canonical_request.finish());

    let date = &input.timestamp[..8];
    let mut string_to_sign = StackStr::<256>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut string_to_sign,
        format_args!(
            "AWS4-HMAC-SHA256\n{}\n{}/{}/s3/aws4_request\n{}",
            input.timestamp,
            date,
            input.region,
            canonical_hash.as_str()
        ),
    );
    debug_assert!(!string_to_sign.is_truncated());

    let mut seed = StackStr::<128>::new();
    let _ = core::fmt::Write::write_fmt(&mut seed, format_args!("AWS4{secret_key}"));
    debug_assert!(!seed.is_truncated());
    let key_date = hmac_sha256(seed.as_str().as_bytes(), date.as_bytes());
    let key_region = hmac_sha256(&key_date, input.region.as_bytes());
    let key_service = hmac_sha256(&key_region, b"s3");
    let key_signing = hmac_sha256(&key_service, b"aws4_request");
    let mac = hmac_sha256(&key_signing, string_to_sign.as_str().as_bytes());
    Signature {
        hex: HexDigest::of(&mac),
    }
}

pub(crate) fn signed_headers<const N: usize>(out: &mut StackStr<N>, headers: &[(&str, &str)]) {
    use core::fmt::Write;
    for (index, (name, _)) in headers.iter().enumerate() {
        if index != 0 {
            let _ = out.write_char(';');
        }
        let _ = out.write_str(name);
    }
}

fn update_signed_headers(hasher: &mut Sha256, headers: &[(&str, &str)]) {
    for (index, (name, _)) in headers.iter().enumerate() {
        if index != 0 {
            hasher.update(b";");
        }
        hasher.update(name.as_bytes());
    }
}

/// S3 canonical URI/query encoding. Input is UTF-8 bytes; unreserved ASCII is
/// retained and every other byte is uppercase percent-encoded.
pub(crate) fn uri_encode<W: core::fmt::Write>(
    out: &mut W,
    input: &str,
    preserve_slash: bool,
) -> core::fmt::Result {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in input.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved || (preserve_slash && byte == b'/') {
            out.write_char(byte as char)?;
        } else {
            out.write_char('%')?;
            out.write_char(HEX[(byte >> 4) as usize] as char)?;
            out.write_char(HEX[(byte & 0x0f) as usize] as char)?;
        }
    }
    Ok(())
}

/// Formats Unix seconds as `YYYYMMDDTHHMMSSZ` for the whole signed `i64`
/// civil-date range representable by the fixed output.
pub(crate) fn format_timestamp(unix_seconds: i64) -> StackStr<16> {
    let days = unix_seconds.div_euclid(86_400);
    let seconds = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let mut out = StackStr::<16>::new();
    let _ = core::fmt::Write::write_fmt(
        &mut out,
        format_args!(
            "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
            seconds / 3_600,
            (seconds / 60) % 60,
            seconds % 60
        ),
    );
    out
}

/// Howard Hinnant's public-domain civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn input<'a>(
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
            payload_sha256_hex: EMPTY_SHA256,
            timestamp: "20150830T123600Z",
            region: "us-east-1",
        }
    }

    #[test]
    fn s3_scoped_get_vanilla_vector() {
        // Canonical input from botocore aws4_testsuite/get-vanilla at commit
        // 5378504, with its placeholder service scope changed to `s3`.
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        assert_eq!(
            sign(SECRET, &input("GET", "/", "", &headers)).hex.as_str(),
            "c184979bf80f1fe1bf360d25dd65b9fdb27729f4e53df49ec7a3ca1865402cb1"
        );
    }

    #[test]
    fn timestamp_and_uri_encoding_are_stable() {
        assert_eq!(format_timestamp(0).as_str(), "19700101T000000Z");
        assert_eq!(format_timestamp(1_709_251_199).as_str(), "20240229T235959Z");
        let mut out = StackStr::<128>::new();
        uri_encode(&mut out, "sst/0001 +&.dat", true).unwrap();
        assert_eq!(out.as_str(), "sst/0001%20%2B%26.dat");
    }

    #[test]
    fn signing_allocates_nothing() {
        let headers = [
            ("host", "example.amazonaws.com"),
            ("x-amz-date", "20150830T123600Z"),
        ];
        crate::mem::guard::forbid_alloc(|| {
            let _ = sign(SECRET, &input("GET", "/", "", &headers));
        });
    }
}
