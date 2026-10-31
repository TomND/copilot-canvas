//! Dev Tunnels access for the iPhone client (docs/IOS-PLAN.md D3).
//!
//! The phone reaches the service through the tunnel's web forwarding, which
//! needs a `connect` access token for a private tunnel. Tokens last 24 hours
//! and only an owner identity can issue them: this machine's `devtunnel` CLI
//! (signed in as the owner, since it hosts the tunnel) does. The service
//! keeps one token, renews it before it expires while a phone is connected,
//! and sends it to clients that asked (`CAP_TUNNEL_ACCESS`); `remote pair
//! --qr` puts one into the pairing QR code.

use canvas_sys::spawn::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use canvas_common::remote::{self, TunnelToken};
use canvas_common::settings::Settings;

/// A token is renewed once it has less than this left.
pub const RENEW_BEFORE_MS: u64 = 12 * 3600 * 1000;

/// A token and the address it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    /// `wss://…devtunnels.ms` (empty when unknown).
    pub url: String,
    pub token: String,
    pub expires_ms: u64,
}

impl Access {
    /// Still good for a while (`now` in ms).
    pub fn fresh(&self, now: u64) -> bool {
        self.expires_ms == 0 || self.expires_ms > now + RENEW_BEFORE_MS
    }
}

/// The tunnel and port in `settings.json` (defaults: `copilot-canvas`, 47600).
pub fn tunnel_and_port(settings: &Settings) -> (String, u16) {
    let tunnel = settings.remote.tunnel.clone().unwrap_or_else(|| remote::DEFAULT_TUNNEL.into());
    let port = settings.remote.listen.as_deref().and_then(|a| a.rsplit_once(':').and_then(|(_, p)| p.parse().ok())).filter(|p| *p != 0).unwrap_or(remote::DEFAULT_PORT);
    (tunnel, port)
}

/// Runs `devtunnel ARGS` without a window; stdout and stderr together.
fn devtunnel(args: &[&str], timeout: Duration) -> Result<String> {
    let cli = remote::devtunnel().context("the Dev Tunnels CLI (devtunnel.exe) is not installed")?;
    let temp = std::env::temp_dir().join(format!("copilot-canvas-devtunnel-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp)?;
    let out_path = temp.join("out.txt");
    let mut child = Command::new(&cli.program)
        .args(&cli.prefix)
        .args(args)
        .current_dir(&temp)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out_path)?)
        .stderr(std::fs::File::create(temp.join("err.txt"))?)
        .no_window()
        .spawn()
        .context("cannot run devtunnel")?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = std::fs::remove_dir_all(&temp);
            bail!("devtunnel {} timed out", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut text = std::fs::read_to_string(&out_path).unwrap_or_default();
    text.push_str(&std::fs::read_to_string(temp.join("err.txt")).unwrap_or_default());
    let _ = std::fs::remove_dir_all(&temp);
    if !status.success() {
        let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim().to_owned();
        bail!("devtunnel {}: {last} (sign in on this machine with `devtunnel user login -g`, as the tunnel's owner)", args.first().unwrap_or(&""));
    }
    Ok(text)
}

/// The web-forwarding address of the remote port: the `remote.web_url`
/// setting, else what `devtunnel show` lists, else one built from the
/// token's cluster.
pub fn web_url(settings: &Settings, token: Option<&TunnelToken>) -> Option<String> {
    if let Some(url) = settings.remote.web_url.clone().filter(|u| u.starts_with("wss://") || u.starts_with("ws://")) {
        return Some(url);
    }
    let (tunnel, port) = tunnel_and_port(settings);
    if let Some(url) = devtunnel(&["show", &tunnel], Duration::from_secs(30)).ok().and_then(|text| remote::web_url_from_show(&text, port)) {
        return Some(url);
    }
    let claims = remote::jwt_claims(&token?.token)?;
    let cluster = claims.get("clusterId").and_then(|c| c.as_str())?;
    remote::web_url_for(&format!("{tunnel}.{cluster}"), port)
}

/// Issues a `connect` token for the tunnel in `settings.json` with this
/// machine's signed-in Dev Tunnels CLI, and finds the web address.
pub fn issue(home: &Path) -> Result<Access> {
    let settings = Settings::load(home);
    let (tunnel, _) = tunnel_and_port(&settings);
    anyhow::ensure!(remote::valid_tunnel_id(&tunnel), "the tunnel id {tunnel:?} in settings.json is not valid");
    let output = devtunnel(&["token", &tunnel, "--scopes", "connect"], Duration::from_secs(60))?;
    let token = remote::parse_token_output(&output).with_context(|| format!("`devtunnel token {tunnel} --scopes connect` printed no token"))?;
    let url = web_url(&settings, Some(&token)).unwrap_or_default();
    Ok(Access { url, token: token.token, expires_ms: token.expires_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_renewed_half_a_day_before_they_expire() {
        let now = 1_000_000_000_000;
        let access = |expires_ms| Access { url: String::new(), token: "t".into(), expires_ms };
        assert!(access(now + 24 * 3600 * 1000).fresh(now));
        assert!(!access(now + 11 * 3600 * 1000).fresh(now));
        assert!(access(0).fresh(now), "no expiry known");
    }

    #[test]
    fn the_tunnel_and_port_come_from_the_settings() {
        let mut settings = Settings::default();
        assert_eq!(tunnel_and_port(&settings), ("copilot-canvas".to_owned(), 47600));
        settings.remote.tunnel = Some("my-tunnel".into());
        settings.remote.listen = Some("127.0.0.1:47700".into());
        assert_eq!(tunnel_and_port(&settings), ("my-tunnel".to_owned(), 47700));
        settings.remote.web_url = Some("wss://my-tunnel-47700.euw.devtunnels.ms".into());
        assert_eq!(web_url(&settings, None).as_deref(), Some("wss://my-tunnel-47700.euw.devtunnels.ms"));
    }
}
