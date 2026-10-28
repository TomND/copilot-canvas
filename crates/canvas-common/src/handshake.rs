//! The authentication handshake, identical on the pipe and on TCP.
//!
//! The client opens with `Hello8` (protocol 8) or `Hello` (protocol 7) and its
//! nonce; the service answers `Challenge` with the negotiated protocol, its
//! nonce and an HMAC proof over the client's nonce; the client checks it and
//! sends its own proof (`Auth`). The secret is the per-start service token, or
//! the paired client's secret when `ClientInfo::remote_id` names one.

use anyhow::{Context, Result, bail};

use crate::protocol::{self, ClientInfo, ClientMsg, PROTOCOL, ServiceMsg};
use crate::wire::{FrameReader, FrameWriter};

/// The service failed to prove it knows the secret (an impostor).
#[derive(Debug)]
pub struct AuthError;

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the process on the service pipe failed authentication")
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

/// A client the service authenticated.
#[derive(Debug)]
pub struct Accepted {
    /// 7 (`Hello`) or 8 (`Hello8`).
    pub protocol: u32,
    pub client: Option<ClientInfo>,
}

/// Service side. `remote`: a TCP client (protocol 8 required). `secret_for`
/// returns the secret for a paired client id, or the token for `None`.
pub fn accept(
    reader: &mut FrameReader,
    writer: &mut FrameWriter,
    remote: bool,
    build: &str,
    secret_for: impl FnOnce(Option<&str>) -> Option<Vec<u8>>,
) -> Result<Accepted> {
    let (protocol, client_nonce, client) = match reader.recv::<ClientMsg>()? {
        Some(ClientMsg::Hello { .. }) if remote => bail!("remote clients need protocol 8"),
        Some(ClientMsg::Hello { nonce, .. }) => (7, nonce, None),
        Some(ClientMsg::Hello8 { nonce, client, .. }) => (PROTOCOL, nonce, Some(client)),
        _ => bail!("expected hello"),
    };
    anyhow::ensure!(client_nonce.len() == 32, "bad client nonce");
    if let Some(info) = &client {
        anyhow::ensure!(info.session.len() == 16, "bad client session id");
    }
    let remote_id = client.as_ref().and_then(|c| c.remote_id.clone());
    let secret = secret_for(remote_id.as_deref()).with_context(|| format!("unknown remote client {:?}", remote_id.unwrap_or_default()))?;
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
    Ok(Accepted { protocol, client })
}

/// What the service said in its challenge.
#[derive(Debug)]
pub struct Greeting {
    pub protocol: u32,
    pub build: String,
}

/// Client side: `Hello8` with `client`, or protocol-7 `Hello` when it is `None`.
/// Errors: [`NoChallenge`] when the service closed or sent something else
/// first, [`AuthError`] when its proof is wrong.
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
    let ServiceMsg::Challenge { protocol: service_protocol, build, nonce: server_nonce, proof } = challenge else {
        return Err(NoChallenge.into());
    };
    if !protocol::verify(secret, "server", &nonce, &proof) {
        return Err(AuthError.into());
    }
    writer.send(&ClientMsg::Auth { proof: protocol::prove(secret, "client", &server_nonce) })?;
    Ok(Greeting { protocol: service_protocol, build })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tcp;
    use std::time::Duration;

    type End = (FrameReader, FrameWriter, tcp::TcpCloser);

    fn pair() -> (End, End) {
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
        let ((mut sr, mut sw, sc), (mut cr, mut cw, cc)) = pair();
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
        assert!(greeting.unwrap_err().is::<NoChallenge>());
        let (accepted, greeting) = run(true, b"token", None);
        assert!(accepted.unwrap_err().to_string().contains("protocol 8"));
        assert!(greeting.unwrap_err().is::<NoChallenge>());
    }

    #[test]
    fn a_client_with_a_wrong_proof_is_refused_by_the_service() {
        let ((mut sr, mut sw, _sc), (mut cr, mut cw, _cc)) = pair();
        let service = std::thread::spawn(move || accept(&mut sr, &mut sw, true, "b", |_| Some(b"token".to_vec())));
        cw.send(&ClientMsg::Hello8 { protocol: PROTOCOL, build: "x".into(), nonce: protocol::nonce(), client: info(None) }).unwrap();
        let Some(ServiceMsg::Challenge { .. }) = cr.recv::<ServiceMsg>().unwrap() else { panic!("challenge") };
        cw.send(&ClientMsg::Auth { proof: vec![0; 32] }).unwrap();
        assert!(service.join().unwrap().unwrap_err().to_string().contains("failed authentication"));
    }
}
