//! Authentication: cleartext password and SCRAM-SHA-256 (RFC 5802/7677),
//! built on the crate's own SHA-256/HMAC. Credentials are derived once at
//! startup (salted, 4096 iterations); per-connection flows use fixed
//! stack buffers and getentropy for nonces.

use crate::crypto::hmac::hmac_sha256;
use crate::crypto::sha256::sha256;
use crate::util::StackStr;

pub const SCRAM_ITERATIONS: u32 = 4096;
const NONCE_RAW: usize = 18; // 24 base64 chars

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Trust,
    Password,
    ScramSha256,
}

/// Server-side SCRAM verifier, derived from the configured password.
pub struct ScramServer {
    pub salt: [u8; 16],
    pub stored_key: [u8; 32],
    pub server_key: [u8; 32],
    pub iterations: u32,
}

impl ScramServer {
    pub fn derive(password: &str, salt: [u8; 16], iterations: u32) -> Self {
        let salted = hi(password.as_bytes(), &salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let server_key = hmac_sha256(&salted, b"Server Key");
        Self {
            salt,
            stored_key,
            server_key,
            iterations,
        }
    }
}

/// PBKDF2-HMAC-SHA256 with a single block (dkLen = 32), i.e. RFC 5802 Hi.
pub(crate) fn hi(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut msg = [0u8; 64];
    let n = salt.len().min(60);
    msg[..n].copy_from_slice(&salt[..n]);
    msg[n..n + 4].copy_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &msg[..n + 4]);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (o, b) in out.iter_mut().zip(u.iter()) {
            *o ^= b;
        }
    }
    out
}

/// Standard base64 with padding.
pub fn b64_encode(input: &[u8], out: &mut StackStr<512>) {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    use core::fmt::Write;
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let index = [
            b[0] >> 2,
            ((b[0] & 0x03) << 4) | (b[1] >> 4),
            ((b[1] & 0x0f) << 2) | (b[2] >> 6),
            b[2] & 0x3f,
        ];
        let n = chunk.len();
        let quad = [
            A[index[0] as usize],
            A[index[1] as usize],
            if n > 1 { A[index[2] as usize] } else { b'=' },
            if n > 2 { A[index[3] as usize] } else { b'=' },
        ];
        let _ = out.write_str(core::str::from_utf8(&quad).expect("base64 is ASCII"));
    }
}

pub fn b64_decode(input: &str, out: &mut [u8]) -> Option<usize> {
    fn value(c: u8) -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let padding = bytes.iter().rev().take_while(|&&byte| byte == b'=').count();
    if padding > 2 || bytes[..bytes.len() - padding].contains(&b'=') {
        return None;
    }
    if !bytes.is_empty() {
        let last = value(bytes[bytes.len() - padding - 1])?;
        if (padding == 1 && last & 0b11 != 0) || (padding == 2 && last & 0b1111 != 0) {
            return None;
        }
    }
    let mut w = 0usize;
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in &bytes[..bytes.len() - padding] {
        acc = (acc << 6) | u32::from(value(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            if w == out.len() {
                return None;
            }
            out[w] = (acc >> bits) as u8;
            w += 1;
        }
    }
    (w == bytes.len() / 4 * 3 - padding).then_some(w)
}

struct ScramClientFirst<'a> {
    bare: &'a str,
    nonce: &'a str,
}

struct ScramClientFinal<'a> {
    channel: &'a str,
    nonce: &'a str,
    proof: &'a str,
    without_proof: &'a str,
}

fn valid_scram_username(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let bytes = value.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b',' => return false,
            b'=' => {
                let Some(escape) = bytes.get(at + 1..at + 3) else {
                    return false;
                };
                if !matches!(escape, b"2C" | b"3D") {
                    return false;
                }
                at += 3;
            }
            byte if byte < 0x20 || byte == 0x7f => return false,
            _ => at += 1,
        }
    }
    true
}

fn valid_scram_nonce(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b',')
}

fn valid_scram_extension(field: &str) -> bool {
    let Some((name, value)) = field.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !value.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
        && !name.eq_ignore_ascii_case("c")
        && !name.eq_ignore_ascii_case("m")
        && !name.eq_ignore_ascii_case("n")
        && !name.eq_ignore_ascii_case("p")
        && !name.eq_ignore_ascii_case("r")
}

fn parse_scram_client_first(input: &str) -> Result<ScramClientFirst<'_>, &'static str> {
    let bare = input
        .strip_prefix("n,,")
        .or_else(|| input.strip_prefix("y,,"))
        .ok_or("unsupported SCRAM channel binding")?;
    let mut fields = bare.split(',');
    let username = fields
        .next()
        .and_then(|field| field.strip_prefix("n="))
        .ok_or("malformed client-first-message")?;
    let nonce = fields
        .next()
        .and_then(|field| field.strip_prefix("r="))
        .ok_or("missing nonce in client-first-message")?;
    if !valid_scram_username(username) || !valid_scram_nonce(nonce) {
        return Err("malformed client-first-message");
    }
    if !fields.all(valid_scram_extension) {
        return Err("malformed client-first-message");
    }
    Ok(ScramClientFirst { bare, nonce })
}

fn parse_scram_client_final(input: &str) -> Result<ScramClientFinal<'_>, &'static str> {
    let (without_proof, proof) = input
        .rsplit_once(",p=")
        .ok_or("malformed client-final-message")?;
    if proof.is_empty() || proof.contains(',') {
        return Err("malformed client-final-message");
    }
    let mut fields = without_proof.split(',');
    let channel = fields
        .next()
        .and_then(|field| field.strip_prefix("c="))
        .ok_or("malformed client-final-message")?;
    let nonce = fields
        .next()
        .and_then(|field| field.strip_prefix("r="))
        .ok_or("malformed client-final-message")?;
    if channel.is_empty() || !valid_scram_nonce(nonce) || !fields.all(valid_scram_extension) {
        return Err("malformed client-final-message");
    }
    Ok(ScramClientFinal {
        channel,
        nonce,
        proof,
        without_proof,
    })
}

/// Per-connection SCRAM exchange state.
pub struct ScramFlow {
    /// client-first-message-bare, kept verbatim for the AuthMessage.
    client_first_bare: StackStr<256>,
    /// Combined nonce (client + server).
    nonce: StackStr<96>,
    /// server-first-message, kept verbatim.
    server_first: StackStr<256>,
}

pub enum ScramStep {
    /// Send AuthenticationSASLContinue with this payload.
    Continue(StackStr<256>),
    /// Authentication succeeded; send AuthenticationSASLFinal with this
    /// payload (v=ServerSignature) then AuthenticationOk.
    Final(StackStr<256>),
}

impl ScramFlow {
    pub fn new() -> Self {
        Self {
            client_first_bare: StackStr::new(),
            nonce: StackStr::new(),
            server_first: StackStr::new(),
        }
    }

    /// Handles client-first-message; produces server-first-message.
    pub fn first(
        &mut self,
        server: &ScramServer,
        client_first: &str,
        server_nonce_raw: &[u8; NONCE_RAW],
    ) -> Result<ScramStep, &'static str> {
        let first = parse_scram_client_first(client_first)?;
        if first.bare.len() > 256 || first.nonce.len() + 24 > 96 {
            return Err("SCRAM client-first-message exceeds fixed capacity");
        }
        self.client_first_bare.clear();
        let _ = core::fmt::Write::write_str(&mut self.client_first_bare, first.bare);

        self.nonce.clear();
        let _ = core::fmt::Write::write_str(&mut self.nonce, first.nonce);
        {
            let mut b64 = StackStr::<512>::new();
            b64_encode(server_nonce_raw, &mut b64);
            let _ = core::fmt::Write::write_str(&mut self.nonce, b64.as_str());
        }

        let mut salt_b64 = StackStr::<512>::new();
        b64_encode(&server.salt, &mut salt_b64);
        self.server_first.clear();
        let _ = core::fmt::Write::write_fmt(
            &mut self.server_first,
            format_args!(
                "r={},s={},i={}",
                self.nonce.as_str(),
                salt_b64.as_str(),
                server.iterations
            ),
        );
        let mut out = StackStr::<256>::new();
        let _ = core::fmt::Write::write_str(&mut out, self.server_first.as_str());
        Ok(ScramStep::Continue(out))
    }

    /// Handles client-final-message; verifies the proof.
    pub fn finish(
        &mut self,
        server: &ScramServer,
        client_final: &str,
    ) -> Result<ScramStep, &'static str> {
        let final_message = parse_scram_client_final(client_final)?;
        if final_message.channel != "biws" && final_message.channel != "eSws" {
            return Err("unsupported channel binding in client-final-message");
        }
        if final_message.nonce != self.nonce.as_str() {
            return Err("SCRAM nonce mismatch");
        }
        let mut proof = [0u8; 32];
        if b64_decode(final_message.proof, &mut proof) != Some(32) {
            return Err("malformed SCRAM proof");
        }

        // AuthMessage = client-first-bare , server-first , client-final-no-proof
        let auth_message_len = self.client_first_bare.as_str().len()
            + self.server_first.as_str().len()
            + final_message.without_proof.len()
            + 2;
        if auth_message_len > 768 {
            return Err("SCRAM authentication message exceeds fixed capacity");
        }
        let mut auth_message = StackStr::<768>::new();
        let _ = core::fmt::Write::write_fmt(
            &mut auth_message,
            format_args!(
                "{},{},{}",
                self.client_first_bare.as_str(),
                self.server_first.as_str(),
                final_message.without_proof
            ),
        );

        let client_signature = hmac_sha256(&server.stored_key, auth_message.as_str().as_bytes());
        let mut client_key = [0u8; 32];
        for i in 0..32 {
            client_key[i] = proof[i] ^ client_signature[i];
        }
        // Constant-time-ish comparison (fixed length, full scan).
        let recomputed = sha256(&client_key);
        let mut diff = 0u8;
        for (a, b) in recomputed.iter().zip(server.stored_key.iter()) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return Err("password authentication failed");
        }

        let server_signature = hmac_sha256(&server.server_key, auth_message.as_str().as_bytes());
        let mut sig_b64 = StackStr::<512>::new();
        b64_encode(&server_signature, &mut sig_b64);
        let mut out = StackStr::<256>::new();
        let _ = core::fmt::Write::write_fmt(&mut out, format_args!("v={}", sig_b64.as_str()));
        Ok(ScramStep::Final(out))
    }
}

impl Default for ScramFlow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scram_messages_reject_duplicate_or_reordered_required_attributes() {
        for input in [
            "n,,n=user,r=nonce,r=again",
            "n,,r=nonce,n=user",
            "n,,n=user,r=",
            "n,,n=user,r=nonce,m=required",
        ] {
            assert!(parse_scram_client_first(input).is_err(), "{input}");
        }
        for input in [
            "r=nonce,c=biws,p=proof",
            "c=biws,r=nonce,p=proof,p=again",
            "c=biws,r=nonce,p=proof,a=late",
            "c=biws,r=nonce,m=required,p=proof",
        ] {
            assert!(parse_scram_client_final(input).is_err(), "{input}");
        }
    }

    #[test]
    fn base64_roundtrip() {
        for input in [
            b"".as_slice(),
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
        ] {
            let mut enc = StackStr::<512>::new();
            b64_encode(input, &mut enc);
            let mut dec = [0u8; 16];
            let n = b64_decode(enc.as_str(), &mut dec).unwrap();
            assert_eq!(&dec[..n], input);
        }
        let mut enc = StackStr::<512>::new();
        b64_encode(b"foobar", &mut enc);
        assert_eq!(enc.as_str(), "Zm9vYmFy");
        let mut enc = StackStr::<512>::new();
        b64_encode(b"foob", &mut enc);
        assert_eq!(enc.as_str(), "Zm9vYg==");
    }

    #[test]
    fn base64_rejects_malformed_padding() {
        let mut out = [0u8; 8];
        for input in ["A", "AAA", "A===", "A=AA", "Zh==", "Zg=", "Zg==="] {
            assert_eq!(b64_decode(input, &mut out), None, "{input}");
        }
        assert_eq!(b64_decode("Zg==", &mut out), Some(1));
    }

    /// RFC 7677 §3 example exchange: user "user", password "pencil".
    #[test]
    fn rfc7677_example_exchange() {
        let mut salt = [0u8; 16];
        assert_eq!(b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==", &mut salt), Some(16));
        let server = ScramServer::derive("pencil", salt, 4096);

        // Server nonce raw bytes chosen so the base64 matches the RFC's
        // server nonce suffix "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0".
        let rfc_server_nonce_b64 = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        // That string is not valid base64 — the RFC nonce is printable
        // text, not an encoding. Drive the flow with the RFC strings by
        // constructing the states directly instead.
        let mut flow = ScramFlow::new();
        flow.client_first_bare.clear();
        core::fmt::Write::write_str(&mut flow.client_first_bare, "n=user,r=rOprNGfwEbeRWgbNEkqO")
            .unwrap();
        flow.nonce.clear();
        core::fmt::Write::write_str(
            &mut flow.nonce,
            "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0",
        )
        .unwrap();
        flow.server_first.clear();
        core::fmt::Write::write_fmt(
            &mut flow.server_first,
            format_args!(
                "r=rOprNGfwEbeRWgbNEkqO{rfc_server_nonce_b64},s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096"
            ),
        )
        .unwrap();

        let client_final = "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=";
        match flow.finish(&server, client_final).unwrap() {
            ScramStep::Final(v) => {
                assert_eq!(v.as_str(), "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=");
            }
            ScramStep::Continue(_) => panic!("expected Final"),
        }
    }

    #[test]
    fn wrong_password_fails() {
        let server = ScramServer::derive("correct", [7u8; 16], 4096);
        let wrong = ScramServer::derive("wrong", [7u8; 16], 4096);
        let mut flow = ScramFlow::new();
        let nonce_raw = [1u8; NONCE_RAW];
        let step = flow
            .first(&server, "n,,n=user,r=clientnonce123", &nonce_raw)
            .unwrap();
        let ScramStep::Continue(server_first) = step else {
            panic!()
        };
        // Forge a client-final using the WRONG password's keys.
        let mut auth_message = StackStr::<768>::new();
        core::fmt::Write::write_fmt(
            &mut auth_message,
            format_args!(
                "n=user,r=clientnonce123,{},c=biws,r={}",
                server_first.as_str(),
                flow.nonce.as_str()
            ),
        )
        .unwrap();
        let salted_wrong = super::hi(b"wrong", &[7u8; 16], 4096);
        let client_key = hmac_sha256(&salted_wrong, b"Client Key");
        let stored_wrong = sha256(&client_key);
        let signature = hmac_sha256(&stored_wrong, auth_message.as_str().as_bytes());
        let mut proof = [0u8; 32];
        for i in 0..32 {
            proof[i] = client_key[i] ^ signature[i];
        }
        let mut proof_b64 = StackStr::<512>::new();
        b64_encode(&proof, &mut proof_b64);
        let mut client_final = StackStr::<768>::new();
        core::fmt::Write::write_fmt(
            &mut client_final,
            format_args!("c=biws,r={},p={}", flow.nonce.as_str(), proof_b64.as_str()),
        )
        .unwrap();
        assert!(flow.finish(&server, client_final.as_str()).is_err());
        let _ = wrong;
    }
}
