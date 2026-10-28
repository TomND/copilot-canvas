//! The authentication handshake, identical on the pipe and on TCP.
//!
//! The client opens with `Hello8` (protocol 8) or `Hello` (protocol 7) and its
//! nonce; the service answers `Challenge` with the negotiated protocol, its
//! nonce and an HMAC proof over the client's nonce; the client checks it and
//! sends its own proof (`Auth`). The secret is the per-start service token, or
//! the paired client's secret when `ClientInfo::remote_id` names one.
//!
//! A service that refuses a client (an unknown or revoked client id, a wrong
//! pairing code) says why with `ServiceMsg::Error` before it closes. A TCP
//! connection may instead open with `Pair` to pair a new client (see
//! [`crate::pairing`]).

use anyhow::{Context, Result, anyhow, bail};

use crate::pairing;
use crate::protocol::{self, ClientInfo, ClientMsg, PROTOCOL, ServiceMsg};
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

/// The service refused this client and said why (a revoked or unknown client,
/// a wrong pairing code). Retrying does not help.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for Refused {}

/// A client the service authenticated.
#[derive(Debug)]
pub struct Accepted {
    /// 7 (`Hello`) or 8 (`Hello8`).
    pub protocol: u32,
    pub client: Option<ClientInfo>,
}

/// A pairing request (`ClientMsg::Pair`).
#[derive(Debug)]
pub struct PairRequest<'a> {
    pub code_proof: &'a [u8],
    pub name: &'a str,
    pub nonce: &'a [u8],
}

/// A client the service paired: its id and secret, and the code that matched
/// (the key the secret is sealed under).
#[derive(Debug)]
pub struct PairGrant {
    pub client_id: String,
    pub code: String,
    pub secret: Vec<u8>,
}

/// How a connection opened on the service.
#[derive(Debug)]
pub enum Opening {
    /// An authenticated client: serve it.
    Client(Accepted),
    /// A new client was paired; the connection ends.
    Paired { client_id: String },
}

/// Tells the client why it is refused, then fails with that reason.
fn refuse(writer: &mut FrameWriter, reason: String) -> anyhow::Error {
    let _ = writer.send(&ServiceMsg::Error { id: None, message: reason.clone() });
    anyhow!(reason)
}

/// Service side. `remote`: a TCP client (protocol 8 required; may pair).
/// `secret_for` returns the secret for a paired client id, or the token for
/// `None`, or why the client is refused; `pair` checks a pairing request.
pub fn serve(
    reader: &mut FrameReader,
    writer: &mut FrameWriter,
    remote: bool,
    build: &str,
    secret_for: impl FnOnce(Option<&str>) -> Result<Vec<u8>, String>,
    pair: impl FnOnce(&PairRequest) -> Result<PairGrant, String>,
) -> Result<Opening> {
    let (protocol, client_nonce, client) = match reader.recv::<ClientMsg>()? {
        Some(ClientMsg::Hello { .. }) if remote => bail!("remote clients need protocol 8"),
        Some(ClientMsg::Hello { nonce, .. }) => (7, nonce, None),
        Some(ClientMsg::Hello8 { nonce, client, .. }) => (PROTOCOL, nonce, Some(client)),
        Some(ClientMsg::Pair { code_proof, name, nonce }) => {
            if !remote {
                return Err(refuse(writer, "pairing is only possible over TCP".into()));
            }
            anyhow::ensure!(nonce.len() == 32 && code_proof.len() == 32 && name.len() <= 64, "malformed pairing request");
            let request = PairRequest { code_proof: &code_proof, name: &name, nonce: &nonce };
            let grant = pair(&request).map_err(|reason| refuse(writer, reason))?;
            let server_nonce = protocol::nonce();
            let secret = pairing::seal(&grant.code, &nonce, &server_nonce, &grant.client_id, &grant.secret);
            writer.send(&ServiceMsg::Paired { client_id: grant.client_id.clone(), nonce: server_nonce, secret })?;
            return Ok(Opening::Paired { client_id: grant.client_id });
        }
        _ => bail!("expected hello"),
    };
    anyhow::ensure!(client_nonce.len() == 32, "bad client nonce");
    if let Some(info) = &client {
        anyhow::ensure!(info.session.len() == 16, "bad client session id");
    }
    let remote_id = client.as_ref().and_then(|c| c.remote_id.clone());
    let secret = secret_for(remote_id.as_deref()).map_err(|reason| refuse(writer, reason))?;
    let server_nonce = protocol::nonce();
    writer.send(&ServiceMsg::Challenge {
        protocol,
        build: build.to_owned(),
        nonce: server_nonce.clone(),
        proof: protocol::prove(&secret, "server", &client_nonce),
    })?;
    let Some(ClientMsg::Auth { proof }) = reader.recv::<ClientMsg>()? else { bail!("expected auth") };
    if !protocol::verify(&secret, "client", &server_nonce, &proof) {
        bail!("client failed authentication");
    }
    Ok(Opening::Client(Accepted { protocol, client }))
}

/// [`serve`] without pairing; unknown client ids are refused.
pub fn accept(
    reader: &mut FrameReader,
    writer: &mut FrameWriter,
    remote: bool,
    build: &str,
    secret_for: impl FnOnce(Option<&str>) -> Option<Vec<u8>>,
) -> Result<Accepted> {
    let secret_for = |id: Option<&str>| secret_for(id).ok_or_else(|| format!("unknown remote client {:?}", id.unwrap_or_default()));
    match serve(reader, writer, remote, build, secret_for, |_| Err("this service does not pair clients".into()))? {
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
/// Errors: [`Refused`] when the service said why it refuses this client,
/// [`NoChallenge`] when it closed or sent something else first, [`AuthError`]
/// when its proof is wrong.
pub fn open(reader: &mut FrameReader, writer: &mut FrameWriter, secret: &[u8], build: &str, client: Option<ClientInfo>) -> Result<Greeting> {
    let nonce = protocol::nonce();
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
        ServiceMsg::Error { message, .. } => return Err(Refused(message).into()),
        _ => return Err(NoChallenge.into()),
    };
    if !protocol::verify(secret, "server", &nonce, &proof) {
        return Err(AuthError.into());
    }
    writer.send(&ClientMsg::Auth { proof: protocol::prove(secret, "client", &server_nonce) })?;
    Ok(Greeting { protocol: service_protocol, build })
}

/// Client side of pairing: proves `code` (canonical form) and returns the new
/// (client id, secret). Errors: [`Refused`] with the service's reason.
pub fn pair(reader: &mut FrameReader, writer: &mut FrameWriter, code: &str, name: &str) -> Result<(String, Vec<u8>)> {
    let nonce = protocol::nonce();
    writer.send(&ClientMsg::Pair { code_proof: pairing::code_proof(code, &nonce), name: name.to_owned(), nonce: nonce.clone() })?;
    match reader.recv_lenient::<ServiceMsg>() {
        Ok(Some(Ok(ServiceMsg::Paired { client_id, nonce: server_nonce, secret }))) => {
            let secret = pairing::open(code, &nonce, &server_nonce, &client_id, &secret).context("the service's answer does not match the pairing code")?;
            anyhow::ensure!(secret.len() == pairing::SECRET_LEN && crate::remote::valid_name(&client_id), "the service sent a malformed pairing answer");
            Ok((client_id, secret))
        }
        Ok(Some(Ok(ServiceMsg::Error { message, .. }))) => Err(Refused(message).into()),
        _ => bail!("the service closed the connection without pairing (is it older than pairing? update it)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tcp;
    use std::time::Duration;

    type End = (FrameReader, FrameWriter, tcp::TcpCloser);

    fn connected() -> (End, End) {
        let listener = tcp::listen_loopback("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let client = std::thread::spawn(move || tcp::connect(&addr, Duration::from_secs(2)).unwrap());
        let (server, _) = listener.accept().unwrap();
        let wrap = |stream| {
            let (r, w, closer) = tcp::split(stream).unwrap();
            (FrameReader::new(Box::new(r)), FrameWriter::new(Box::new(w)), closer)
        };
        (wrap(server), wrap(client.join().unwrap()))
    }

    fn info(remote_id: Option<&str>) -> ClientInfo {
        ClientInfo { name: "test".into(), caps: protocol::CAP_FRAME8, session: vec![1; 16], remote_id: remote_id.map(str::to_owned) }
    }

    /// Runs both sides; the service knows `token` and the paired client "home" with `paired`.
    fn run(remote: bool, client_secret: &[u8], client: Option<ClientInfo>) -> (Result<Accepted>, Result<Greeting>) {
        let ((mut sr, mut sw, sc), (mut cr, mut cw, cc)) = connected();
        let service = std::thread::spawn(move || {
            let result = accept(&mut sr, &mut sw, remote, "svc-build", |id| match id {
                None => Some(b"token".to_vec()),
                Some("home") => Some(b"paired".to_vec()),
                Some(_) => None,
            });
            sc.close();
            result
        });
        let greeting = open(&mut cr, &mut cw, client_secret, "client-build", client);
        cc.close();
        (service.join().unwrap(), greeting)
    }

    #[test]
    fn protocol8_and_protocol7_clients_negotiate_their_protocol() {
        let (accepted, greeting) = run(false, b"token", Some(info(None)));
        assert_eq!(accepted.unwrap().protocol, 8);
        let greeting = greeting.unwrap();
        assert_eq!((greeting.protocol, greeting.build.as_str()), (8, "svc-build"));
        let (accepted, greeting) = run(false, b"token", None);
        assert_eq!(accepted.unwrap().protocol, 7);
        assert_eq!(greeting.unwrap().protocol, 7);
    }

    #[test]
    fn paired_clients_use_their_own_secret() {
        let (accepted, greeting) = run(true, b"paired", Some(info(Some("home"))));
        assert_eq!(accepted.unwrap().client.unwrap().remote_id.as_deref(), Some("home"));
        assert!(greeting.is_ok());
    }

    #[test]
    fn wrong_secrets_unknown_clients_and_protocol7_over_tcp_are_rejected() {
        // The service cannot prove a secret the client does not share: the client refuses it.
        let (accepted, greeting) = run(true, b"wrong", Some(info(None)));
        assert!(greeting.unwrap_err().is::<AuthError>());
        assert!(accepted.is_err(), "no proof from the client");
        let (accepted, greeting) = run(true, b"paired", Some(info(Some("stranger"))));
        assert!(accepted.unwrap_err().to_string().contains("unknown remote client"));
        let refused = greeting.unwrap_err();
        assert!(refused.is::<Refused>() && refused.to_string().contains("unknown remote client"), "the client is told why: {refused}");
        let (accepted, greeting) = run(true, b"token", None);
        assert!(accepted.unwrap_err().to_string().contains("protocol 8"));
        assert!(greeting.unwrap_err().is::<NoChallenge>());
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

    /// A service with one open code; returns (service result, client result).
    fn pairing(remote: bool, typed: &str) -> (Result<Opening>, Result<(String, Vec<u8>)>) {
        let ((mut sr, mut sw, sc), (mut cr, mut cw, cc)) = connected();
        let secret = vec![9u8; pairing::SECRET_LEN];
        let granted = secret.clone();
        let service = std::thread::spawn(move || {
            let result = serve(&mut sr, &mut sw, remote, "b", |_| Err("unused".into()), |request: &PairRequest| {
                if request.name != "home-pc" {
                    return Err("wrong name".into());
                }
                match pairing::check_code_proof("ABCDEFGH", request.nonce, request.code_proof) {
                    true => Ok(PairGrant { client_id: "pc-0001".into(), code: "ABCDEFGH".into(), secret: granted }),
                    false => Err("wrong or expired pairing code".into()),
                }
            });
            sc.close();
            result
        });
        let client = super::pair(&mut cr, &mut cw, typed, "home-pc");
        cc.close();
        let result = service.join().unwrap();
        if let Ok((_, got)) = &client {
            assert_eq!(*got, secret);
        }
        (result, client)
    }

    #[test]
    fn pairing_exchanges_a_sealed_secret_for_a_valid_code_only() {
        let (opening, client) = pairing(true, "ABCDEFGH");
        assert!(matches!(opening.unwrap(), Opening::Paired { client_id } if client_id == "pc-0001"));
        assert_eq!(client.unwrap().0, "pc-0001");
        let (opening, client) = pairing(true, "ABCDEFGJ");
        assert!(opening.is_err());
        let refused = client.unwrap_err();
        assert!(refused.is::<Refused>() && refused.to_string() == "wrong or expired pairing code", "{refused}");
        let (opening, client) = pairing(false, "ABCDEFGH");
        assert!(opening.is_err(), "never over the pipe");
        assert!(client.unwrap_err().to_string().contains("only possible over TCP"));
    }
}
