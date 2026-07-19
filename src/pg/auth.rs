//! Authentication: cleartext password and SCRAM-SHA-256 (RFC 5802/7677),
//! built on the crate's own SHA-256/HMAC. Credentials are derived once at
//! startup (salted, 4096 iterations); per-connection flows use fixed
//! stack buffers and getentropy for nonces.

use crate::s3::hmac::hmac_sha256;
use crate::s3::sha256::sha256;
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
fn hi(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
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
    let bytes = input.trim_end_matches('=').as_bytes();
    let mut w = 0usize;
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in bytes {
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
    Some(w)
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
        // GS2 header: we accept only "n,," (no channel binding).
        let bare = client_first
            .strip_prefix("n,,")
            .or_else(|| client_first.strip_prefix("y,,"))
            .ok_or("unsupported SCRAM channel binding")?;
        self.client_first_bare.clear();
        let _ = core::fmt::Write::write_str(&mut self.client_first_bare, bare);

        let mut client_nonce = None;
        for field in bare.split(',') {
            if let Some(r) = field.strip_prefix("r=") {
                client_nonce = Some(r);
            }
        }
        let client_nonce = client_nonce.ok_or("missing nonce in client-first-message")?;

        self.nonce.clear();
        let _ = core::fmt::Write::write_str(&mut self.nonce, client_nonce);
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
        let mut channel = None;
        let mut nonce = None;
        let mut proof_b64 = None;
        let mut without_proof_len = client_final.len();
        for field in client_final.split(',') {
            if let Some(v) = field.strip_prefix("c=") {
                channel = Some(v);
            } else if let Some(v) = field.strip_prefix("r=") {
                nonce = Some(v);
            } else if let Some(v) = field.strip_prefix("p=") {
                proof_b64 = Some(v);
                without_proof_len = client_final.len() - field.len() - 1;
            }
        }
        let (Some(channel), Some(nonce), Some(proof_b64)) = (channel, nonce, proof_b64)
        else {
            return Err("malformed client-final-message");
        };
        if channel != "biws" && channel != "eSws" {
            return Err("unsupported channel binding in client-final-message");
        }
        if nonce != self.nonce.as_str() {
            return Err("SCRAM nonce mismatch");
        }
        let mut proof = [0u8; 32];
        if b64_decode(proof_b64, &mut proof) != Some(32) {
            return Err("malformed SCRAM proof");
        }

        // AuthMessage = client-first-bare , server-first , client-final-no-proof
        let mut auth_message = StackStr::<768>::new();
        let _ = core::fmt::Write::write_fmt(
            &mut auth_message,
            format_args!(
                "{},{},{}",
                self.client_first_bare.as_str(),
                self.server_first.as_str(),
                &client_final[..without_proof_len]
            ),
        );

        let client_signature =
            hmac_sha256(&server.stored_key, auth_message.as_str().as_bytes());
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

        let server_signature =
            hmac_sha256(&server.server_key, auth_message.as_str().as_bytes());
        let mut sig_b64 = StackStr::<512>::new();
        b64_encode(&server_signature, &mut sig_b64);
        let mut out = StackStr::<256>::new();
        let _ = core::fmt::Write::write_fmt(
            &mut out,
            format_args!("v={}", sig_b64.as_str()),
        );
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
    fn base64_roundtrip() {
        for input in [b"".as_slice(), b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
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

    /// RFC 7677 §3 example exchange: user "user", password "pencil".
    #[test]
    fn rfc7677_example_exchange() {
        let mut salt = [0u8; 16];
        assert_eq!(
            b64_decode("W22ZaJ0SNY7soEsUEjb6gQ==", &mut salt),
            Some(16)
        );
        let server = ScramServer::derive("pencil", salt, 4096);

        // Server nonce raw bytes chosen so the base64 matches the RFC's
        // server nonce suffix "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0".
        let rfc_server_nonce_b64 = "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
        // That string is not valid base64 — the RFC nonce is printable
        // text, not an encoding. Drive the flow with the RFC strings by
        // constructing the states directly instead.
        let mut flow = ScramFlow::new();
        flow.client_first_bare.clear();
        core::fmt::Write::write_str(
            &mut flow.client_first_bare,
            "n=user,r=rOprNGfwEbeRWgbNEkqO",
        )
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
                assert_eq!(
                    v.as_str(),
                    "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
                );
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
        let step = flow.first(&server, "n,,n=user,r=clientnonce123", &nonce_raw).unwrap();
        let ScramStep::Continue(server_first) = step else { panic!() };
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
