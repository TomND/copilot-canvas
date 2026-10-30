//! Pairing a remote client with a service (PLAN.md D5 "App-level
//! authentication").
//!
//! `copilot-canvas remote pair` on the service's machine opens a one-time
//! code of 12 characters (about 59 bits). Both sides derive keys from it with
//! PBKDF2-HMAC-SHA256 ([`KDF_ROUNDS`] rounds, salted with both nonces), so
//! testing guesses against a recorded exchange, or against a service that
//! answered, costs that much per guess:
//!
//! 1. client → `PairStart { name, nonce }`;
//! 2. service → `PairChallenge { nonce, proofs }`: a proof of each open code
//!    over the [`transcript`] (both nonces and the name), padded with random
//!    proofs to [`MAX_OPEN_CODES`]: the service proves it knows the code first;
//! 3. client → `PairProof { proof }`, only once one of them matches its code;
//! 4. service → `Paired { client_id, nonce, secret }`: the new client's secret
//!    sealed under keys from the same derivation, bound to the transcript.
//!
//! The tunnel is TLS already; the seal keeps the secret out of anything that
//! only sees the frames. Both sides then store the secret with DPAPI and
//! every connection runs the usual HMAC challenge with it.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Letters and digits that cannot be mistaken for one another (no 0/O, 1/I/L).
pub const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// Characters in a code (31^12: about 59 bits), shown as three groups of four.
pub const CODE_LEN: usize = 12;
/// A code is valid this long.
pub const CODE_TTL_MS: u64 = 10 * 60 * 1000;
/// Codes open at the same time (a new one replaces the oldest); a challenge
/// always carries this many proofs.
pub const MAX_OPEN_CODES: usize = 3;
/// PBKDF2 rounds of a pairing key derivation (about 0.1 s each).
pub const KDF_ROUNDS: u32 = 200_000;
/// Bytes of a client secret.
pub const SECRET_LEN: usize = 32;
/// Longest client name in a pairing request.
pub const MAX_NAME: usize = 64;
const TAG_LEN: usize = 32;
const CONTEXT: &[u8] = b"copilot-canvas pairing v2";

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

/// `ABCDEFGHJKMN` as `ABCD-EFGH-JKMN`.
pub fn format_code(code: &str) -> String {
    match code.len() {
        CODE_LEN => format!("{}-{}-{}", &code[..4], &code[4..8], &code[8..]),
        _ => code.to_owned(),
    }
}

/// A typed code in canonical form: upper case, dashes, spaces and
/// underscores removed. `None` when it cannot be a code.
pub fn normalize_code(input: &str) -> Option<String> {
    let code: String = input.chars().filter(|c| !matches!(c, '-' | ' ' | '_')).map(|c| c.to_ascii_uppercase()).collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
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

/// Constant-time check of `tag` against the HMAC of `parts` under `key`.
fn verify(key: &[u8], parts: &[&[u8]], tag: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for part in parts {
        mac.update(part);
    }
    mac.verify_slice(tag).is_ok()
}

/// What a pairing's proofs and seal are bound to: both nonces and the name
/// the client gave (length-prefixed, so no two transcripts share bytes).
pub fn transcript(client_nonce: &[u8], server_nonce: &[u8], name: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(CONTEXT);
    for part in [client_nonce, server_nonce, name.as_bytes()] {
        hash.update((part.len() as u32).to_le_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}

/// PBKDF2-HMAC-SHA256 of `code`, salted with the context and both nonces
/// (each length-prefixed), `rounds` rounds, one 32-byte block.
fn master_key(code: &str, client_nonce: &[u8], server_nonce: &[u8], rounds: u32) -> [u8; 32] {
    let mut salt = Vec::with_capacity(CONTEXT.len() + client_nonce.len() + server_nonce.len() + 12);
    for part in [CONTEXT, client_nonce, server_nonce] {
        salt.extend_from_slice(&(part.len() as u32).to_le_bytes());
        salt.extend_from_slice(part);
    }
    let mut master = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(code.as_bytes(), &salt, rounds, &mut master);
    master
}

/// The keys of one pairing attempt, derived from the code.
pub struct Keys {
    auth: [u8; 32],
    enc: [u8; 32],
    seal: [u8; 32],
}

impl Keys {
    /// PBKDF2-HMAC-SHA256 of `code` (canonical form) salted with both
    /// nonces: deliberately slow, so guesses cannot be tested quickly. One
    /// 32-byte block is derived (what a guess costs an attacker is what it
    /// costs us); the three keys are expanded from it with HMAC.
    pub fn derive(code: &str, client_nonce: &[u8], server_nonce: &[u8]) -> Self {
        Self::derive_rounds(code, client_nonce, server_nonce, KDF_ROUNDS)
    }

    fn derive_rounds(code: &str, client_nonce: &[u8], server_nonce: &[u8], rounds: u32) -> Self {
        let master = master_key(code, client_nonce, server_nonce, rounds);
        Self { auth: mac(&master, &[b"auth"]), enc: mac(&master, &[b"enc"]), seal: mac(&master, &[b"seal"]) }
    }

    /// The service's proof that it knows the code.
    pub fn server_proof(&self, transcript: &[u8; 32]) -> Vec<u8> {
        mac(&self.auth, &[b"server", transcript]).to_vec()
    }

    /// The client's proof that it knows the code.
    pub fn client_proof(&self, transcript: &[u8; 32]) -> Vec<u8> {
        mac(&self.auth, &[b"client", transcript]).to_vec()
    }

    /// Constant-time check of a [`Keys::server_proof`].
    pub fn check_server(&self, transcript: &[u8; 32], proof: &[u8]) -> bool {
        verify(&self.auth, &[b"server", transcript], proof)
    }

    /// Constant-time check of a [`Keys::client_proof`].
    pub fn check_client(&self, transcript: &[u8; 32], proof: &[u8]) -> bool {
        verify(&self.auth, &[b"client", transcript], proof)
    }

    /// Encrypts `secret` for `client_id` (HMAC-SHA256 in counter mode, then a
    /// tag over the transcript, the id and the ciphertext).
    pub fn seal(&self, transcript: &[u8; 32], client_id: &str, secret: &[u8]) -> Vec<u8> {
        let mut sealed = secret.to_vec();
        xor_stream(&self.enc, &mut sealed);
        let tag = mac(&self.seal, &[transcript, client_id.as_bytes(), &[0], &sealed]);
        sealed.extend_from_slice(&tag);
        sealed
    }

    /// The secret of a [`Keys::seal`]ed answer; `None` when the keys, the
    /// transcript or the id differ, or it was changed.
    pub fn open(&self, transcript: &[u8; 32], client_id: &str, sealed: &[u8]) -> Option<Vec<u8>> {
        let body_len = sealed.len().checked_sub(TAG_LEN)?;
        let (body, tag) = sealed.split_at(body_len);
        if !verify(&self.seal, &[transcript, client_id.as_bytes(), &[0], body], tag) {
            return None;
        }
        let mut secret = body.to_vec();
        xor_stream(&self.enc, &mut secret);
        Some(secret)
    }
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

/// Seals a message for one paired client with its own secret, for channels
/// the service does not trust (a push notification goes through Apple):
/// a fresh 16-byte nonce, keys derived from the secret, `label` and the
/// nonce, HMAC-SHA256 in counter mode, a tag over the ciphertext.
/// Returns (nonce, ciphertext ‖ tag).
pub fn seal_for_client(secret: &[u8], label: &str, message: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let nonce = crate::protocol::nonce()[..16].to_vec();
    let (enc, seal) = client_keys(secret, label, &nonce);
    let mut sealed = message.to_vec();
    xor_stream(&enc, &mut sealed);
    let tag = mac(&seal, &[&nonce, &sealed]);
    sealed.extend_from_slice(&tag);
    (nonce, sealed)
}

/// Opens [`seal_for_client`]; `None` when the secret, label or nonce differ, or it was changed.
pub fn open_for_client(secret: &[u8], label: &str, nonce: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    let body_len = sealed.len().checked_sub(TAG_LEN)?;
    let (body, tag) = sealed.split_at(body_len);
    let (enc, seal) = client_keys(secret, label, nonce);
    if !verify(&seal, &[nonce, body], tag) {
        return None;
    }
    let mut message = body.to_vec();
    xor_stream(&enc, &mut message);
    Some(message)
}

fn client_keys(secret: &[u8], label: &str, nonce: &[u8]) -> ([u8; 32], [u8; 32]) {
    let base = mac(secret, &[b"copilot-canvas client seal v1", label.as_bytes(), &[0], nonce]);
    (mac(&base, &[b"enc"]), mac(&base, &[b"seal"]))
}

/// The key a revoked client's refusal is proven with. The service keeps it
/// (instead of the secret) once it revokes a client; the client derives it
/// from its secret, so only it can check the refusal.
pub fn revocation_key(secret: &[u8]) -> Vec<u8> {
    mac(secret, &[b"copilot-canvas revoked v1"]).to_vec()
}

/// How long the next pairing attempt waits after `failures` failed ones in a
/// row: 250 ms, doubling, at most 30 s.
pub fn backoff(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        n => Duration::from_millis(250u64 << (n - 1).min(7)).min(Duration::from_secs(30)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol;

    #[test]
    fn codes_are_twelve_unambiguous_characters_in_three_groups() {
        for _ in 0..200 {
            let code = new_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)), "{code}");
            assert_eq!(normalize_code(&format_code(&code).to_lowercase()).as_deref(), Some(code.as_str()));
        }
        assert_eq!(format_code("ABCDEFGHJKMN"), "ABCD-EFGH-JKMN");
        assert_eq!(normalize_code(" abcd-efgh-jkmn "), Some("ABCDEFGHJKMN".into()));
        assert_eq!(normalize_code("abcd efgh_jkmn"), Some("ABCDEFGHJKMN".into()), "spaces and underscores too");
        assert_eq!(normalize_code("ABCDEFGHJKMN"), Some("ABCDEFGHJKMN".into()), "without separators");
        assert_eq!(normalize_code("ABCD-EFGH-JKM0"), None, "0 is not in the alphabet");
        assert_eq!(normalize_code("ABCD-EFGH"), None, "the old 8-character format");
        assert_eq!(normalize_code("ABCD-EFGH-JKMNP"), None);
        assert!((CODE_ALPHABET.len() as f64).powi(CODE_LEN as i32).log2() > 59.0);
        assert_ne!(new_code(), new_code());
        assert!(new_client_id().starts_with("pc-") && new_client_id().len() == 19);
    }

    #[test]
    fn derived_proofs_bind_the_code_the_nonces_and_the_name() {
        let (client, server) = (protocol::nonce(), protocol::nonce());
        let t = transcript(&client, &server, "home-pc");
        let keys = Keys::derive("ABCDEFGHJKMN", &client, &server);
        let proof = keys.server_proof(&t);
        assert!(keys.check_server(&t, &proof));
        assert!(!keys.check_client(&t, &proof), "roles differ");
        assert!(keys.check_client(&t, &keys.client_proof(&t)));
        assert!(!Keys::derive("ABCDEFGHJKMP", &client, &server).check_server(&t, &proof), "another code");
        assert!(!keys.check_server(&transcript(&client, &server, "other"), &proof), "another name");
        assert!(!keys.check_server(&transcript(&server, &client, "home-pc"), &proof), "swapped nonces");
        assert!(!Keys::derive("ABCDEFGHJKMN", &server, &client).check_server(&t, &proof), "the nonces salt the derivation");
        assert!(!proof.windows(4).any(|w| w == b"ABCD"), "the code itself is not sent");
    }

    #[test]
    fn the_derivation_is_pbkdf2_with_many_rounds() {
        let (client, server) = ([1u8; 32], [2u8; 32]);
        let mut salt = Vec::new();
        for part in [CONTEXT, &client[..], &server[..]] {
            salt.extend_from_slice(&(part.len() as u32).to_le_bytes());
            salt.extend_from_slice(part);
        }
        let mut expected = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(b"ABCDEFGHJKMN", &salt, 1000, &mut expected);
        assert_eq!(master_key("ABCDEFGHJKMN", &client, &server, 1000), expected);
        let keys = Keys::derive_rounds("ABCDEFGHJKMN", &client, &server, 1000);
        assert_eq!((keys.auth, keys.enc, keys.seal), (mac(&expected, &[b"auth"]), mac(&expected, &[b"enc"]), mac(&expected, &[b"seal"])));
        assert!(keys.auth != keys.enc && keys.enc != keys.seal, "three different keys");
        // Fewer rounds give other keys: the round count is part of the scheme.
        assert_ne!(Keys::derive_rounds("ABCDEFGHJKMN", &client, &server, 999).auth, keys.auth);
        let started = std::time::Instant::now();
        let full = Keys::derive("ABCDEFGHJKMN", &client, &server);
        assert_ne!(full.auth, keys.auth);
        println!("{KDF_ROUNDS} rounds: {:?}", started.elapsed());
    }

    #[test]
    fn sealed_secrets_round_trip_only_with_the_same_keys_transcript_and_id() {
        let (client, server) = (protocol::nonce(), protocol::nonce());
        let t = transcript(&client, &server, "pc");
        let keys = Keys::derive_rounds("ABCDEFGHJKMN", &client, &server, 1000);
        let secret = new_secret();
        let sealed = keys.seal(&t, "pc-1", &secret);
        assert_eq!(sealed.len(), SECRET_LEN + TAG_LEN);
        assert!(!sealed.windows(8).any(|w| secret.windows(8).any(|s| s == w)), "the secret is not in the clear");
        assert_eq!(keys.open(&t, "pc-1", &sealed), Some(secret.clone()));
        let other = Keys::derive_rounds("ABCDEFGHJKMP", &client, &server, 1000);
        assert_eq!(other.open(&t, "pc-1", &sealed), None, "another code");
        assert_eq!(keys.open(&transcript(&client, &server, "x"), "pc-1", &sealed), None, "another transcript");
        assert_eq!(keys.open(&t, "pc-2", &sealed), None, "another client id");
        let mut flipped = sealed.clone();
        flipped[0] ^= 1;
        assert_eq!(keys.open(&t, "pc-1", &flipped), None, "a modified secret");
        assert_eq!(keys.open(&t, "pc-1", &sealed[..10]), None);
        let long = vec![7u8; 100];
        assert_eq!(keys.open(&t, "x", &keys.seal(&t, "x", &long)), Some(long), "more than one keystream block");
    }

    #[test]
    fn failures_back_off_exponentially_up_to_a_cap() {
        let delays: Vec<u64> = (0..12).map(|n| backoff(n).as_millis() as u64).collect();
        assert_eq!(delays, [0, 250, 500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000, 30000]);
        assert_ne!(revocation_key(&[1; 32]), revocation_key(&[2; 32]));
        assert_ne!(revocation_key(&[1; 32]), vec![1; 32]);
    }

    #[test]
    fn messages_sealed_for_a_client_open_only_with_its_secret_and_label() {
        let secret = new_secret();
        let (nonce, sealed) = seal_for_client(&secret, "tunnel-access", b"{\"token\":\"t\"}");
        assert_eq!(open_for_client(&secret, "tunnel-access", &nonce, &sealed).as_deref(), Some(&b"{\"token\":\"t\"}"[..]));
        assert_eq!(open_for_client(&new_secret(), "tunnel-access", &nonce, &sealed), None, "another client's secret");
        assert_eq!(open_for_client(&secret, "other", &nonce, &sealed), None, "another purpose");
        let mut changed = sealed.clone();
        changed[0] ^= 1;
        assert_eq!(open_for_client(&secret, "tunnel-access", &nonce, &changed), None, "changed in transit");
        assert!(!sealed.windows(5).any(|w| w == b"token"), "not in the clear");
    }
}
