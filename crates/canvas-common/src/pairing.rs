//! Pairing a remote client with a service (PLAN.md D5 "App-level
//! authentication").
//!
//! `copilot-canvas remote pair` on the service's machine opens a one-time
//! code. The client proves it knows the code without sending it
//! (`ClientMsg::Pair::code_proof`, an HMAC over the client's nonce); the
//! service answers with a new client id and a 32-byte secret sealed under a
//! key derived from the code and both nonces (`ServiceMsg::Paired`). The
//! tunnel is TLS already; the seal keeps the secret out of anything that only
//! sees the frames. Both sides then store the secret with DPAPI and every
//! connection runs the usual HMAC challenge with it.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::protocol;

type HmacSha256 = Hmac<Sha256>;

/// Letters and digits that cannot be mistaken for one another (no 0/O, 1/I/L).
pub const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// Characters in a code (31^8: about 40 bits).
pub const CODE_LEN: usize = 8;
/// A code is valid this long.
pub const CODE_TTL_MS: u64 = 10 * 60 * 1000;
/// Failed pairing attempts before every open code is cancelled.
pub const MAX_ATTEMPTS: u32 = 5;
/// Bytes of a client secret.
pub const SECRET_LEN: usize = 32;
const TAG_LEN: usize = 32;
const CONTEXT: &[u8] = b"copilot-canvas pairing v1";

fn random(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).expect("operating system random generator failed");
    bytes
}

/// A new code (unbiased: bytes at or above the largest multiple of the
/// alphabet's size are drawn again).
pub fn new_code() -> String {
    let n = CODE_ALPHABET.len();
    let limit = 256 - 256 % n;
    let mut code = String::with_capacity(CODE_LEN);
    while code.len() < CODE_LEN {
        for byte in random(16) {
            if (byte as usize) < limit && code.len() < CODE_LEN {
                code.push(CODE_ALPHABET[byte as usize % n] as char);
            }
        }
    }
    code
}

/// `ABCDEFGH` as `ABCD-EFGH`.
pub fn format_code(code: &str) -> String {
    match code.len() {
        CODE_LEN => format!("{}-{}", &code[..4], &code[4..]),
        _ => code.to_owned(),
    }
}

/// A typed code in canonical form: upper case, dashes and spaces removed.
/// `None` when it cannot be a code.
pub fn normalize_code(input: &str) -> Option<String> {
    let code: String = input.chars().filter(|c| !matches!(c, '-' | ' ' | '_')).map(|c| c.to_ascii_uppercase()).collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

/// What the client sends instead of the code.
pub fn code_proof(code: &str, client_nonce: &[u8]) -> Vec<u8> {
    protocol::prove(code.as_bytes(), "pair", client_nonce)
}

/// Constant-time check of a [`code_proof`].
pub fn check_code_proof(code: &str, client_nonce: &[u8], proof: &[u8]) -> bool {
    protocol::verify(code.as_bytes(), "pair", client_nonce, proof)
}

/// A new client secret.
pub fn new_secret() -> Vec<u8> {
    random(SECRET_LEN)
}

/// A new client id, e.g. `pc-3f9a1c0b2d4e5f60`.
pub fn new_client_id() -> String {
    format!("pc-{}", crate::paths::hex(&random(8)))
}

fn mac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

/// (encryption key, authentication key) for one pairing.
fn keys(code: &str, client_nonce: &[u8], server_nonce: &[u8]) -> ([u8; 32], [u8; 32]) {
    let root = mac(code.as_bytes(), &[CONTEXT, client_nonce, server_nonce]);
    (mac(&root, &[b"enc"]), mac(&root, &[b"mac"]))
}

/// HMAC-SHA256 in counter mode as the keystream.
fn xor_stream(key: &[u8], data: &mut [u8]) {
    for (block, chunk) in data.chunks_mut(32).enumerate() {
        let stream = mac(key, &[&(block as u64).to_le_bytes()]);
        for (byte, k) in chunk.iter_mut().zip(stream) {
            *byte ^= k;
        }
    }
}

/// Encrypts `secret` for `client_id` (encrypt-then-MAC; the id is authenticated too).
pub fn seal(code: &str, client_nonce: &[u8], server_nonce: &[u8], client_id: &str, secret: &[u8]) -> Vec<u8> {
    let (enc, auth) = keys(code, client_nonce, server_nonce);
    let mut sealed = secret.to_vec();
    xor_stream(&enc, &mut sealed);
    let tag = mac(&auth, &[client_id.as_bytes(), &[0], &sealed]);
    sealed.extend_from_slice(&tag);
    sealed
}

/// The secret of a [`seal`]ed answer; `None` when the code, a nonce or the id differ.
pub fn open(code: &str, client_nonce: &[u8], server_nonce: &[u8], client_id: &str, sealed: &[u8]) -> Option<Vec<u8>> {
    let body_len = sealed.len().checked_sub(TAG_LEN)?;
    let (body, tag) = sealed.split_at(body_len);
    let (enc, auth) = keys(code, client_nonce, server_nonce);
    let mut check = HmacSha256::new_from_slice(&auth).expect("HMAC accepts any key length");
    check.update(client_id.as_bytes());
    check.update(&[0]);
    check.update(body);
    check.verify_slice(tag).ok()?;
    let mut secret = body.to_vec();
    xor_stream(&enc, &mut secret);
    Some(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_use_the_unambiguous_alphabet_and_normalize() {
        for _ in 0..200 {
            let code = new_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)), "{code}");
            assert_eq!(normalize_code(&format_code(&code).to_lowercase()).as_deref(), Some(code.as_str()));
        }
        assert_eq!(format_code("ABCDEFGH"), "ABCD-EFGH");
        assert_eq!(normalize_code(" abcd-efgh "), Some("ABCDEFGH".into()));
        assert_eq!(normalize_code("ABCD-EFG0"), None, "0 is not in the alphabet");
        assert_eq!(normalize_code("ABCDEFG"), None);
        assert_ne!(new_code(), new_code());
        assert!(new_client_id().starts_with("pc-") && new_client_id().len() == 19);
    }

    #[test]
    fn proofs_bind_the_code_and_the_nonce() {
        let nonce = protocol::nonce();
        let proof = code_proof("ABCDEFGH", &nonce);
        assert!(check_code_proof("ABCDEFGH", &nonce, &proof));
        assert!(!check_code_proof("ABCDEFGJ", &nonce, &proof));
        assert!(!check_code_proof("ABCDEFGH", &protocol::nonce(), &proof));
        assert!(!proof.windows(8).any(|w| w == b"ABCDEFGH"), "the code itself is not sent");
    }

    #[test]
    fn sealed_secrets_round_trip_only_with_the_same_code_nonces_and_id() {
        let (client, server) = (protocol::nonce(), protocol::nonce());
        let secret = new_secret();
        let sealed = seal("ABCDEFGH", &client, &server, "pc-1", &secret);
        assert_eq!(sealed.len(), SECRET_LEN + TAG_LEN);
        assert!(!sealed.windows(8).any(|w| secret.windows(8).any(|s| s == w)), "the secret is not in the clear");
        assert_eq!(open("ABCDEFGH", &client, &server, "pc-1", &sealed), Some(secret.clone()));
        assert_eq!(open("ABCDEFGJ", &client, &server, "pc-1", &sealed), None, "another code");
        assert_eq!(open("ABCDEFGH", &server, &client, "pc-1", &sealed), None, "swapped nonces");
        assert_eq!(open("ABCDEFGH", &client, &server, "pc-2", &sealed), None, "another client id");
        let mut flipped = sealed.clone();
        flipped[0] ^= 1;
        assert_eq!(open("ABCDEFGH", &client, &server, "pc-1", &flipped), None, "a modified secret");
        assert_eq!(open("ABCDEFGH", &client, &server, "pc-1", &sealed[..10]), None);
        // Longer payloads use more than one keystream block.
        let long = vec![7u8; 100];
        assert_eq!(open("ABCDEFGH", &client, &server, "x", &seal("ABCDEFGH", &client, &server, "x", &long)), Some(long));
    }
}
