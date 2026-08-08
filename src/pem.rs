//! Minimal PEM parsing, shared by the object-store TLS client and the TLS
//! server (`pg::tls`). The codebase refuses a base64 dependency, so the decoder
//! is hand-rolled; this all runs at startup, while allocation is still free.

/// One decoded PEM block: its label (the word after `BEGIN`) and DER payload.
pub(crate) struct Block {
    pub(crate) label: String,
    pub(crate) der: Vec<u8>,
}

/// Decodes every `-----BEGIN <label>-----`/`-----END <label>-----` block in a
/// PEM document, in order.
pub(crate) fn blocks(pem: &str) -> Result<Vec<Block>, String> {
    let mut out = Vec::new();
    let mut label: Option<String> = None;
    let mut b64 = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            let name = rest.trim_end_matches('-').trim();
            label = Some(name.to_string());
            b64.clear();
        } else if let Some(rest) = line.strip_prefix("-----END ") {
            let name = rest.trim_end_matches('-').trim();
            match label.take() {
                Some(open) if open == name => {
                    out.push(Block {
                        label: open,
                        der: base64_decode(&b64)?,
                    });
                }
                _ => return Err(format!("mismatched PEM END for {name}")),
            }
        } else if label.is_some() {
            b64.push_str(line);
        }
    }
    Ok(out)
}

/// The DER payloads of every `CERTIFICATE` block.
pub(crate) fn certificates(pem: &str) -> Result<Vec<Vec<u8>>, String> {
    Ok(blocks(pem)?
        .into_iter()
        .filter(|b| b.label == "CERTIFICATE")
        .map(|b| b.der)
        .collect())
}

/// Standard base64 (RFC 4648), decoded by hand.
pub(crate) fn base64_decode(text: &str) -> Result<Vec<u8>, String> {
    const BAD: u8 = 0xFF;
    fn value(c: u8) -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => BAD,
        }
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in text.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = value(c);
        if v == BAD {
            return Err("invalid base64 in PEM".to_string());
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("SGVsbG8=").unwrap(), b"Hello");
        assert_eq!(base64_decode("SGVsbG8h").unwrap(), b"Hello!");
        // Whitespace inside the payload is not valid; callers strip lines
        // first, so an embedded space is rejected.
        assert!(base64_decode("SGVs bG8=").is_err());
    }

    #[test]
    fn parses_labeled_blocks_in_order() {
        let pem = "\
-----BEGIN CERTIFICATE-----
SGVsbG8=
-----END CERTIFICATE-----
-----BEGIN PRIVATE KEY-----
SGVsbG8h
-----END PRIVATE KEY-----
";
        let parsed = blocks(pem).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].label, "CERTIFICATE");
        assert_eq!(parsed[0].der, b"Hello");
        assert_eq!(parsed[1].label, "PRIVATE KEY");
        assert_eq!(parsed[1].der, b"Hello!");
        // The certificate filter keeps only CERTIFICATE blocks.
        assert_eq!(certificates(pem).unwrap(), vec![b"Hello".to_vec()]);
    }

    #[test]
    fn rejects_mismatched_end() {
        let pem = "-----BEGIN CERTIFICATE-----\nSGk=\n-----END PRIVATE KEY-----\n";
        assert!(blocks(pem).is_err());
    }
}
