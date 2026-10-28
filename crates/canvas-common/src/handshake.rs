//! The authentication handshake, identical on the pipe and on TCP.
//!
//! The client opens with `Hello8` (protocol 8) or `Hello` (protocol 7) and its
//! nonce; the service answers `Challenge` with the negotiated protocol, its
//! nonce and an HMAC proof over the client's nonce; the client checks it and
//! sends its own proof (`Auth`). The secret is the per-start service token, or
//! the paired client's secret when `ClientInfo::remote_id` names one. Over TCP
//! the service tells its build only once the client authenticated (`Welcome`).
//!
//! Every field of the opening message is checked (sizes, the client id's
//! form) before the service looks anything up. A refused client gets
//! `Refusal { proof }`: random bytes, or, for a revoked paired client, an
//! HMAC with its revocation key that only that client can check. Only a
//! proven refusal stops a client from reconnecting; anything else that
//! answers on the port could have sent an unproven one. A TCP connection may
//! instead open with `PairStart` to pair a new client (see [`crate::pairing`]).

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::pairing::{self, Keys};
use crate::protocol::{self, ClientInfo, ClientMsg, MAX_HELLO_FIELD, PROTOCOL, ServiceMsg};
use crate::wire::{FrameReader, FrameWriter};

/// The service failed to prove it knows the secret (an impostor, or a
/// pairing secret the service no longer has).
#[derive(Debug)]
pub struct AuthError;

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the service failed authentication: it could not prove it knows this client's secret")
    }
}
impl std::error::Error for AuthError {}

/// The connection closed before the challenge: an older service that cannot
/// read `Hello8`, or a service that refused this client.
#[derive(Debug)]
pub struct NoChallenge;

impl std::fmt::Display for NoChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the service closed the connection during the handshake (unknown client or wrong credentials)")
    }
}
impl std::error::Error for NoChallenge {}

/// The service refused this client. `verified`: it proved it knows this
/// client's credential (a revoked client, a pairing code it knows), so
/// retrying cannot help. Unverified refusals may come from anything that
/// answers on the port; clients keep retrying and show a generic message.
#[derive(Debug)]
pub struct Refused {
    pub verified: bool,
    pub message: String,
}

impl Refused {
    /// The paired client was revoked (proven by the service).
    pub fn revoked() -> Self {
        Self { verified: true, message: "this client was revoked on the service; pair it again (`copilot-canvas remote pair` there, then `copilot-canvas remote add` here)".into() }
    }

    /// Refused without proof of who refused.
    pub fn unverified() -> Self {
        Self {
            verified: false,
            message: "the service refused this client without proving who it is (an unknown or revoked client, or a peer that may not use the service token)".into(),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Refused {}

/// What the service knows about the credential a client asks for.
pub enum Credential {
    /// The secret to run the challenge with.
    Secret(Vec<u8>),
    /// A paired client that was revoked: the refusal is proven with this key.
    Revoked(Vec<u8>),
    /// Unknown, or not allowed here: refused with a random proof.
    Refused,
}

/// A client the service authenticated.
#[derive(Debug)]
pub struct Accepted {
    /// 7 (`Hello`) or 8 (`Hello8`).
    pub protocol: u32,
    pub client: Option<ClientInfo>,
}

/// A client the service paired: its id and secret.
#[derive(Debug)]
pub struct PairGrant {
    pub client_id: String,
    pub secret: Vec<u8>,
}

/// The service's side of pairing: open codes, failed attempts, new clients.
pub trait Pairing {
    /// How long this attempt must wait (failed attempts slow the next ones down).
    fn wait(&self) -> Duration;
    /// Codes open now (canonical form).
    fn codes(&self) -> Vec<String>;
    /// Uses up `code` and stores a new client called `name`; `Err` says why not.
    fn grant(&self, code: &str, name: &str) -> Result<PairGrant, String>;
    /// The attempt failed: no open code matched, or the client gave up.
    fn failed(&self);
}

/// How a connection opened on the service.
#[derive(Debug)]
pub enum Opening {
    /// An authenticated client: serve it.
    Client(Accepted),
    /// A new client was paired (`name`: what it called itself); the connection ends.
    Paired { client_id: String, name: String },
}

/// Longest wait for a pairing attempt's turn; a longer one is answered as failed.
const PAIR_WAIT_MAX: Duration = Duration::from_secs(2);
/// Pairing key derivations run one at a time (they are slow by design).
static PAIRING_TURN: Mutex<()> = Mutex::new(());

/// Checks an opening message before anything is looked up.
fn check_hello(build: &str, nonce: &[u8], client: Option<&ClientInfo>) -> Result<()> {
    anyhow::ensure!(nonce.len() == 32, "bad client nonce");
    anyhow::ensure!(build.len() <= MAX_HELLO_FIELD, "build id too long");
    if let Some(info) = client {
        anyhow::ensure!(info.name.len() <= MAX_HELLO_FIELD, "client name too long");
        anyhow::ensure!(info.session.len() == 16, "bad client session id");
        if let Some(id) = &info.remote_id {
            anyhow::ensure!(crate::remote::valid_name(id), "malformed remote client id");
        }
    }
    Ok(())
}

/// Service side. `remote`: a TCP client (protocol 8 required; may pair).
/// `credential_for` says what to authenticate a paired client id (or `None`:
/// the service token) with; `pairing` pairs new clients (`None`: refused).
pub fn serve(
    reader: &mut FrameReader,
    writer: &mut FrameWriter,
    remote: bool,
    build: &str,
    credential_for: impl FnOnce(Option<&str>) -> Credential,
    pairing: Option<&dyn Pairing>,
) -> Result<Opening> {
    let (protocol, client_nonce, client) = match reader.recv::<ClientMsg>()? {
        Some(ClientMsg::Hello { .. }) if remote => bail!("remote clients need protocol 8"),
        Some(ClientMsg::Hello { build, nonce, .. }) => {
            check_hello(&build, &nonce, None)?;
            (7, nonce, None)
        }
        Some(ClientMsg::Hello8 { build, nonce, client, .. }) => {
            check_hello(&build, &nonce, Some(&client))?;
            (PROTOCOL, nonce, Some(client))
        }
        Some(ClientMsg::PairStart { name, nonce }) => return serve_pairing(reader, writer, remote, name, nonce, pairing),
        Some(ClientMsg::Pair { .. }) => {
            let _ = writer.send(&ServiceMsg::Error { id: None, message: "this client is too old to pair with this service; update it".into() });
            bail!("a pairing request of an older client")
        }
        _ => bail!("expected hello"),
    };
    let remote_id = client.as_ref().and_then(|c| c.remote_id.clone());
    let secret = match credential_for(remote_id.as_deref()) {
        Credential::Secret(secret) => secret,
        Credential::Revoked(key) => {
            let _ = writer.send(&ServiceMsg::Refusal { proof: protocol::prove(&key, "revoked", &client_nonce) });
            bail!("refused revoked remote client {remote_id:?}")
        }
        Credential::Refused => {
            let _ = writer.send(&ServiceMsg::Refusal { proof: protocol::nonce() });
            bail!("refused client {remote_id:?}")
        }
    };
    let server_nonce = protocol::nonce();
    // Over TCP only authenticated clients learn the build (`Welcome`).
    let told = if remote { String::new() } else { build.to_owned() };
    writer.send(&ServiceMsg::Challenge { protocol, build: told, nonce: server_nonce.clone(), proof: protocol::prove(&secret, "server", &client_nonce) })?;
    let Some(ClientMsg::Auth { proof }) = reader.recv::<ClientMsg>()? else { bail!("expected auth") };
    if !protocol::verify(&secret, "client", &server_nonce, &proof) {
        bail!("client failed authentication");
    }
    if remote {
        writer.send(&ServiceMsg::Welcome { build: build.to_owned() })?;
    }
    Ok(Opening::Client(Accepted { protocol, client }))
}

/// The service's side of [`pairing`]: proves the open codes first, then
/// checks the client's proof and sends the sealed secret.
fn serve_pairing(reader: &mut FrameReader, writer: &mut FrameWriter, remote: bool, name: String, client_nonce: Vec<u8>, pairing: Option<&dyn Pairing>) -> Result<Opening> {
    if !remote {
        let _ = writer.send(&ServiceMsg::Error { id: None, message: "pairing is only possible over TCP".into() });
        bail!("pairing over the pipe");
    }
    anyhow::ensure!(client_nonce.len() == 32 && name.len() <= pairing::MAX_NAME, "malformed pairing request");
    let Some(pairing) = pairing else {
        let _ = writer.send(&ServiceMsg::Error { id: None, message: "this service does not pair clients".into() });
        bail!("pairing is not available");
    };
    let server_nonce = protocol::nonce();
    let transcript = pairing::transcript(&client_nonce, &server_nonce, &name);
    // Waiting out the backoff and deriving happen one attempt at a time; an
    // attempt that would wait too long gets random proofs only.
    let (ran, keyed) = {
        let _turn = PAIRING_TURN.lock().unwrap_or_else(|e| e.into_inner());
        let wait = pairing.wait();
        if wait > PAIR_WAIT_MAX {
            (false, Vec::new())
        } else {
            std::thread::sleep(wait);
            let codes = pairing.codes();
            (true, codes.into_iter().take(pairing::MAX_OPEN_CODES).map(|code| {
                let keys = Keys::derive(&code, &client_nonce, &server_nonce);
                (code, keys)
            }).collect::<Vec<(String, Keys)>>())
        }
    };
    let mut proofs: Vec<Vec<u8>> = keyed.iter().map(|(_, keys)| keys.server_proof(&transcript)).collect();
    while proofs.len() < pairing::MAX_OPEN_CODES {
        proofs.push(protocol::nonce());
    }
    // Sorted: the position of a real proof says nothing.
    proofs.sort();
    writer.send(&ServiceMsg::PairChallenge { nonce: server_nonce.clone(), proofs })?;
    let matched = match reader.recv::<ClientMsg>() {
        Ok(Some(ClientMsg::PairProof { proof })) => keyed.iter().find(|(_, keys)| keys.check_client(&transcript, &proof)),
        _ => None,
    };
    let Some((code, keys)) = matched else {
        if ran {
            pairing.failed();
        }
        bail!("pairing failed: no open code matched");
    };
    match pairing.grant(code, &name) {
        Ok(grant) => {
            let sealed = keys.seal(&transcript, &grant.client_id, &grant.secret);
            writer.send(&ServiceMsg::Paired { client_id: grant.client_id.clone(), nonce: server_nonce, secret: sealed })?;
            Ok(Opening::Paired { client_id: grant.client_id, name })
        }
        // The client proved the code: it may know why.
        Err(reason) => {
            let _ = writer.send(&ServiceMsg::Error { id: None, message: reason.clone() });
            Err(anyhow!(reason))
        }
    }
}

/// [`serve`] without pairing; unknown client ids are refused.
pub fn accept(
    reader: &mut FrameReader,
    writer: &mut FrameWriter,
    remote: bool,
    build: &str,
    secret_for: impl FnOnce(Option<&str>) -> Option<Vec<u8>>,
) -> Result<Accepted> {
    let credential_for = |id: Option<&str>| secret_for(id).map_or(Credential::Refused, Credential::Secret);
    match serve(reader, writer, remote, build, credential_for, None)? {
        Opening::Client(accepted) => Ok(accepted),
        Opening::Paired { .. } => bail!("unexpected pairing"),
    }
}

/// What the service said in its challenge.
#[derive(Debug)]
pub struct Greeting {
    pub protocol: u32,
    pub build: String,
}

/// Client side: `Hello8` with `client`, or protocol-7 `Hello` when it is `None`.
/// Errors: [`Refused`] when the service refused this client (verified when
/// it proved the client is revoked), [`NoChallenge`] when it closed or sent
/// something else first, [`AuthError`] when its proof is wrong.
pub fn open(reader: &mut FrameReader, writer: &mut FrameWriter, secret: &[u8], build: &str, client: Option<ClientInfo>) -> Result<Greeting> {
    let nonce = protocol::nonce();
    let paired = client.as_ref().is_some_and(|c| c.remote_id.is_some());
    let hello = match client {
        Some(client) => ClientMsg::Hello8 { protocol: PROTOCOL, build: build.to_owned(), nonce: nonce.clone(), client },
        None => ClientMsg::Hello { protocol: 7, build: build.to_owned(), nonce: nonce.clone() },
    };
    writer.send(&hello)?;
    // An older service answers frames it cannot decode by closing the connection.
    let challenge = match reader.recv_lenient::<ServiceMsg>() {
        Ok(Some(Ok(message))) => message,
        Ok(_) | Err(_) => return Err(NoChallenge.into()),
    };
    let (service_protocol, build, server_nonce, proof) = match challenge {
        ServiceMsg::Challenge { protocol, build, nonce, proof } => (protocol, build, nonce, proof),
        ServiceMsg::Refusal { proof } => {
            let proven = paired && protocol::verify(&pairing::revocation_key(secret), "revoked", &nonce, &proof);
            return Err(if proven { Refused::revoked() } else { Refused::unverified() }.into());
        }
        // An older service's reason, unproven: not shown.
        ServiceMsg::Error { .. } => return Err(Refused::unverified().into()),
        _ => return Err(NoChallenge.into()),
    };
    if !protocol::verify(secret, "server", &nonce, &proof) {
        return Err(AuthError.into());
    }
    writer.send(&ClientMsg::Auth { proof: protocol::prove(secret, "client", &server_nonce) })?;
    // Over TCP the build follows the client's proof.
    let build = if build.is_empty() {
        match reader.recv::<ServiceMsg>() {
            Ok(Some(ServiceMsg::Welcome { build })) => build,
            _ => bail!("the service refused this client after the handshake"),
        }
    } else {
        build
    };
    Ok(Greeting { protocol: service_protocol, build })
}

/// The message a client shows when the service did not prove the code.
const NOT_PROVEN: &str = "wrong or expired pairing code (or too many recent attempts: wait a minute); open a new one with `copilot-canvas remote pair` on the service's machine";

/// Client side of pairing with `code` (canonical form): checks that the
/// service knows the code before proving it, and returns the new (client id,
/// secret). Errors: [`Refused`] (verified once the service proved the code).
pub fn pair(reader: &mut FrameReader, writer: &mut FrameWriter, code: &str, name: &str) -> Result<(String, Vec<u8>)> {
    let nonce = protocol::nonce();
    writer.send(&ClientMsg::PairStart { name: name.to_owned(), nonce: nonce.clone() })?;
    let (server_nonce, proofs) = match reader.recv_lenient::<ServiceMsg>() {
        Ok(Some(Ok(ServiceMsg::PairChallenge { nonce, proofs }))) => (nonce, proofs),
        Ok(Some(Ok(ServiceMsg::Error { message, .. }))) => return Err(Refused { verified: false, message: format!("the service refused pairing ({message})") }.into()),
        _ => bail!("the service closed the connection without pairing (is it older than this client? update it)"),
    };
    anyhow::ensure!(server_nonce.len() == 32 && proofs.len() <= 16, "the service sent a malformed pairing challenge");
    let transcript = pairing::transcript(&nonce, &server_nonce, name);
    let keys = Keys::derive(code, &nonce, &server_nonce);
    if !proofs.iter().any(|proof| keys.check_server(&transcript, proof)) {
        return Err(Refused { verified: false, message: NOT_PROVEN.into() }.into());
    }
    writer.send(&ClientMsg::PairProof { proof: keys.client_proof(&transcript) })?;
    match reader.recv_lenient::<ServiceMsg>() {
        Ok(Some(Ok(ServiceMsg::Paired { client_id, secret, .. }))) => {
            let secret = keys.open(&transcript, &client_id, &secret).context("the service's answer does not match the pairing code")?;
            anyhow::ensure!(secret.len() == pairing::SECRET_LEN && crate::remote::valid_name(&client_id), "the service sent a malformed pairing answer");
            Ok((client_id, secret))
        }
        // The service proved the code: its reason counts.
        Ok(Some(Ok(ServiceMsg::Error { message, .. }))) => Err(Refused { verified: true, message }.into()),
        _ => bail!("the service closed the connection without pairing"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tcp;
    use std::sync::atomic::{AtomicU32, Ordering};

    type End = (FrameReader, FrameWriter, tcp::TcpCloser);

    /// A connected pair; reads give up after 20 s, so a broken test fails
    /// instead of hanging. A stray connection (another process reaching a
    /// reused port) is not taken for the test's own.
    fn connected() -> (End, End) {
        let listener = tcp::listen_loopback("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let client = std::thread::spawn(move || tcp::connect(&addr, Duration::from_secs(2)).unwrap());
        let client = client.join().unwrap();
        let ours = client.local_addr().unwrap();
        let server = loop {
            let (server, peer) = listener.accept().unwrap();
            if peer == ours {
                break server;
            }
        };
        let wrap = |stream| {
            let (r, w, closer) = tcp::split(stream).unwrap();
            closer.set_deadline(Some(Duration::from_secs(20)));
            (FrameReader::new(Box::new(r)), FrameWriter::new(Box::new(w)), closer)
        };
        (wrap(server), wrap(client))
    }

    fn info(remote_id: Option<&str>) -> ClientInfo {
        ClientInfo { name: "test".into(), caps: protocol::CAP_FRAME8, session: vec![1; 16], remote_id: remote_id.map(str::to_owned) }
    }

    /// Runs both sides; the service knows `token`, the paired client "home"
    /// with `paired`, and "gone" was revoked (its secret was `paired`).
    fn run(remote: bool, client_secret: &[u8], client: Option<ClientInfo>) -> (Result<Opening>, Result<Greeting>) {
        let ((mut sr, mut sw, sc), (mut cr, mut cw, cc)) = connected();
        let service = std::thread::spawn(move || {
            let result = serve(&mut sr, &mut sw, remote, "svc-build", |id| match id {
                None => Credential::Secret(b"token".to_vec()),
                Some("home") => Credential::Secret(b"paired".to_vec()),
                Some("gone") => Credential::Revoked(pairing::revocation_key(b"paired")),
                Some(_) => Credential::Refused,
            }, None);
            sc.close();
            result
        });
        let greeting = open(&mut cr, &mut cw, client_secret, "client-build", client);
        cc.close();
        (service.join().unwrap(), greeting)
    }

    fn protocol_of(opening: Result<Opening>) -> u32 {
        match opening.unwrap() {
            Opening::Client(accepted) => accepted.protocol,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn protocol8_and_protocol7_clients_negotiate_their_protocol() {
        let (accepted, greeting) = run(false, b"token", Some(info(None)));
        assert_eq!(protocol_of(accepted), 8);
        let greeting = greeting.unwrap();
        assert_eq!((greeting.protocol, greeting.build.as_str()), (8, "svc-build"));
        let (accepted, greeting) = run(false, b"token", None);
        assert_eq!(protocol_of(accepted), 7);
        assert_eq!(greeting.unwrap().protocol, 7);
    }

    #[test]
    fn over_tcp_the_build_is_told_only_after_authentication() {
        let ((mut sr, mut sw, _sc), (mut cr, mut cw, _cc)) = connected();
        let service = std::thread::spawn(move || serve(&mut sr, &mut sw, true, "svc-build", |_| Credential::Secret(b"token".to_vec()), None).map(|_| ()));
        cw.send(&ClientMsg::Hello8 { protocol: PROTOCOL, build: "x".into(), nonce: protocol::nonce(), client: info(None) }).unwrap();
        let Some(ServiceMsg::Challenge { build, nonce, .. }) = cr.recv::<ServiceMsg>().unwrap() else { panic!("challenge") };
        assert!(build.is_empty(), "no build before the client's proof");
        cw.send(&ClientMsg::Auth { proof: protocol::prove(b"token", "client", &nonce) }).unwrap();
        assert_eq!(cr.recv::<ServiceMsg>().unwrap(), Some(ServiceMsg::Welcome { build: "svc-build".into() }));
        service.join().unwrap().unwrap();
        // And the client reads it as the service's build.
        let (_, greeting) = run(true, b"paired", Some(info(Some("home"))));
        assert_eq!(greeting.unwrap().build, "svc-build");
    }

    #[test]
    fn paired_clients_use_their_own_secret() {
        let (accepted, greeting) = run(true, b"paired", Some(info(Some("home"))));
        let Opening::Client(accepted) = accepted.unwrap() else { panic!("client") };
        assert_eq!(accepted.client.unwrap().remote_id.as_deref(), Some("home"));
        assert!(greeting.is_ok());
    }

    #[test]
    fn wrong_secrets_unknown_clients_and_protocol7_over_tcp_are_rejected() {
        // The service cannot prove a secret the client does not share: the client refuses it.
        let (accepted, greeting) = run(true, b"wrong", Some(info(None)));
        assert!(greeting.unwrap_err().is::<AuthError>());
        assert!(accepted.is_err(), "no proof from the client");
        // An unknown id gets an unproven refusal: generic, and not final.
        let (accepted, greeting) = run(true, b"paired", Some(info(Some("stranger"))));
        assert!(accepted.is_err());
        let refused = greeting.unwrap_err();
        let refused = refused.downcast_ref::<Refused>().expect("refused");
        assert!(!refused.verified && !refused.message.contains("stranger"), "{refused}");
        let (accepted, greeting) = run(true, b"token", None);
        assert!(accepted.unwrap_err().to_string().contains("protocol 8"));
        assert!(greeting.unwrap_err().is::<NoChallenge>());
    }

    #[test]
    fn only_the_revoked_client_itself_can_verify_its_revocation() {
        let (_, greeting) = run(true, b"paired", Some(info(Some("gone"))));
        let error = greeting.unwrap_err();
        let refused = error.downcast_ref::<Refused>().expect("refused");
        assert!(refused.verified && refused.message.contains("was revoked"), "{refused}");
        // Someone else claiming that id cannot tell a revocation from an unknown id.
        let (_, greeting) = run(true, b"guess", Some(info(Some("gone"))));
        assert!(!greeting.unwrap_err().downcast_ref::<Refused>().expect("refused").verified);
    }

    #[test]
    fn opening_fields_are_checked_before_any_lookup() {
        let looked_up = std::sync::Arc::new(AtomicU32::new(0));
        let attempt = |message: ClientMsg| {
            let ((mut sr, mut sw, _sc), (_cr, mut cw, _cc)) = connected();
            let seen = looked_up.clone();
            let service = std::thread::spawn(move || {
                serve(&mut sr, &mut sw, true, "b", |_| {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Credential::Refused
                }, None)
            });
            cw.send(&message).unwrap();
            service.join().unwrap().unwrap_err().to_string()
        };
        let hello = |build: String, name: String, remote_id: Option<String>| ClientMsg::Hello8 {
            protocol: PROTOCOL,
            build,
            nonce: protocol::nonce(),
            client: ClientInfo { name, caps: 0, session: vec![1; 16], remote_id },
        };
        let long = "x".repeat(MAX_HELLO_FIELD + 1);
        assert!(attempt(hello(long.clone(), "n".into(), None)).contains("build id too long"));
        assert!(attempt(hello("b".into(), long, None)).contains("client name too long"));
        assert!(attempt(hello("b".into(), "n".into(), Some("../etc".into()))).contains("malformed remote client id"));
        assert!(attempt(hello("b".into(), "n".into(), Some("x".repeat(41)))).contains("malformed remote client id"));
        assert!(attempt(ClientMsg::PairStart { name: "x".repeat(pairing::MAX_NAME + 1), nonce: protocol::nonce() }).contains("malformed pairing request"));
        assert_eq!(looked_up.load(Ordering::SeqCst), 0, "nothing was looked up");
        // At the limits the lookup happens (and refuses).
        assert!(attempt(hello("b".repeat(MAX_HELLO_FIELD), "n".repeat(MAX_HELLO_FIELD), Some("pc-1".into()))).contains("refused"));
        assert_eq!(looked_up.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_client_with_a_wrong_proof_is_refused_by_the_service() {
        let ((mut sr, mut sw, _sc), (mut cr, mut cw, _cc)) = connected();
        let service = std::thread::spawn(move || accept(&mut sr, &mut sw, true, "b", |_| Some(b"token".to_vec())));
        cw.send(&ClientMsg::Hello8 { protocol: PROTOCOL, build: "x".into(), nonce: protocol::nonce(), client: info(None) }).unwrap();
        let Some(ServiceMsg::Challenge { .. }) = cr.recv::<ServiceMsg>().unwrap() else { panic!("challenge") };
        cw.send(&ClientMsg::Auth { proof: vec![0; 32] }).unwrap();
        assert!(service.join().unwrap().unwrap_err().to_string().contains("failed authentication"));
    }

    /// A service with open codes, counting failures.
    #[derive(Default)]
    struct Codes {
        open: Mutex<Vec<String>>,
        failures: AtomicU32,
        wait: Duration,
    }

    impl Pairing for Codes {
        fn wait(&self) -> Duration {
            self.wait
        }
        fn codes(&self) -> Vec<String> {
            self.open.lock().unwrap().clone()
        }
        fn grant(&self, code: &str, name: &str) -> Result<PairGrant, String> {
            let mut open = self.open.lock().unwrap();
            let index = open.iter().position(|c| c == code).ok_or("expired")?;
            open.remove(index);
            assert_eq!(name, "home-pc");
            Ok(PairGrant { client_id: "pc-0001".into(), secret: vec![9u8; pairing::SECRET_LEN] })
        }
        fn failed(&self) {
            self.failures.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Pairs `typed` with a service that has `codes` open; returns both results.
    fn pairing_with(codes: &std::sync::Arc<Codes>, remote: bool, typed: &str) -> (Result<Opening>, Result<(String, Vec<u8>)>) {
        let ((mut sr, mut sw, sc), (mut cr, mut cw, cc)) = connected();
        let service_codes = codes.clone();
        let service = std::thread::spawn(move || {
            let result = serve(&mut sr, &mut sw, remote, "b", |_| Credential::Refused, Some(&*service_codes));
            sc.close();
            result
        });
        let client = super::pair(&mut cr, &mut cw, typed, "home-pc");
        cc.close();
        (service.join().unwrap(), client)
    }

    fn codes(open: &[&str]) -> std::sync::Arc<Codes> {
        std::sync::Arc::new(Codes { open: Mutex::new(open.iter().map(|c| c.to_string()).collect()), ..Default::default() })
    }

    #[test]
    fn pairing_exchanges_a_sealed_secret_for_a_valid_code_only() {
        let service = codes(&["ABCDEFGHJKMN", "PQRSTUVWXYZ2"]);
        let (opening, client) = pairing_with(&service, true, "PQRSTUVWXYZ2");
        assert!(matches!(opening.unwrap(), Opening::Paired { client_id, name } if client_id == "pc-0001" && name == "home-pc"));
        assert_eq!(client.unwrap(), ("pc-0001".to_owned(), vec![9u8; pairing::SECRET_LEN]));
        assert_eq!(service.codes(), ["ABCDEFGHJKMN"], "the code was used up");
        assert_eq!(service.failures.load(Ordering::SeqCst), 0);
        // A wrong code: the service did not prove it, so the client never sends a proof.
        let (opening, client) = pairing_with(&service, true, "ABCDEFGHJKMP");
        assert!(opening.is_err());
        let refused = client.unwrap_err();
        let refused = refused.downcast_ref::<Refused>().expect("refused");
        assert!(!refused.verified && refused.message.starts_with("wrong or expired pairing code"), "{refused}");
        assert_eq!(service.failures.load(Ordering::SeqCst), 1, "the attempt counts as failed");
        let (opening, client) = pairing_with(&service, false, "ABCDEFGHJKMN");
        assert!(opening.is_err(), "never over the pipe");
        assert!(client.unwrap_err().to_string().contains("only possible over TCP"));
    }

    #[test]
    fn no_open_code_and_a_wrong_code_look_the_same() {
        let none = codes(&[]);
        let (_, client) = pairing_with(&none, true, "ABCDEFGHJKMN");
        let wrong = codes(&["PQRSTUVWXYZ2"]);
        let (_, other) = pairing_with(&wrong, true, "ABCDEFGHJKMN");
        assert_eq!(client.unwrap_err().to_string(), other.unwrap_err().to_string());
        // Waiting longer than the limit: random proofs, and no derivation counted as a failure.
        let paused = std::sync::Arc::new(Codes { open: Mutex::new(vec!["ABCDEFGHJKMN".into()]), wait: Duration::from_secs(10), ..Default::default() });
        let (_, client) = pairing_with(&paused, true, "ABCDEFGHJKMN");
        assert!(client.unwrap_err().to_string().starts_with("wrong or expired pairing code"));
        assert_eq!(paused.failures.load(Ordering::SeqCst), 0);
        assert_eq!(paused.codes().len(), 1, "the code stays open");
    }

    #[test]
    fn the_service_proves_the_code_before_the_client_does() {
        let ((mut sr, mut sw, _sc), (mut cr, mut cw, _cc)) = connected();
        let service_codes = codes(&["ABCDEFGHJKMN"]);
        let service = std::thread::spawn(move || serve(&mut sr, &mut sw, true, "b", |_| Credential::Refused, Some(&*service_codes)).map(|_| ()));
        let nonce = protocol::nonce();
        cw.send(&ClientMsg::PairStart { name: "home-pc".into(), nonce: nonce.clone() }).unwrap();
        // The first answer proves the code; nothing about the code came from the client yet.
        let Some(ServiceMsg::PairChallenge { nonce: server_nonce, proofs }) = cr.recv::<ServiceMsg>().unwrap() else { panic!("challenge") };
        assert_eq!(proofs.len(), pairing::MAX_OPEN_CODES, "padded to a fixed count");
        let transcript = pairing::transcript(&nonce, &server_nonce, "home-pc");
        let keys = Keys::derive("ABCDEFGHJKMN", &nonce, &server_nonce);
        assert_eq!(proofs.iter().filter(|p| keys.check_server(&transcript, p)).count(), 1);
        // A proof for another name (another transcript) is refused.
        let other = pairing::transcript(&nonce, &server_nonce, "evil");
        cw.send(&ClientMsg::PairProof { proof: keys.client_proof(&other) }).unwrap();
        assert!(service.join().unwrap().is_err());
    }

    #[test]
    fn older_pairing_requests_are_refused() {
        let ((mut sr, mut sw, _sc), (mut cr, mut cw, _cc)) = connected();
        let service_codes = codes(&["ABCDEFGHJKMN"]);
        let service = std::thread::spawn(move || serve(&mut sr, &mut sw, true, "b", |_| Credential::Refused, Some(&*service_codes)).map(|_| ()));
        cw.send(&ClientMsg::Pair { code_proof: vec![0; 32], name: "x".into(), nonce: protocol::nonce() }).unwrap();
        assert!(matches!(cr.recv::<ServiceMsg>().unwrap(), Some(ServiceMsg::Error { .. })));
        assert!(service.join().unwrap().is_err());
    }
}
