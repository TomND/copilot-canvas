//! Remote mode on the client's side: the services this machine paired with
//! (`<home>/remote/servers.json`, secrets DPAPI-protected), where the Dev
//! Tunnels CLI is, and the names the tunnel setup uses.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The service's remote port when `remote.listen` is not set.
pub const DEFAULT_PORT: u16 = 47600;
/// The persistent Dev Tunnel `remote tunnel-setup` suggests.
pub const DEFAULT_TUNNEL: &str = "copilot-canvas";
/// The automation that hosts the tunnel on the service's machine.
pub const TUNNEL_TASK: &str = "CopilotCanvas-Tunnel";
/// Overrides the Dev Tunnels CLI: a program, optionally followed by `|` and
/// arguments that go before the command's own (`C:\x\copilot-canvas.exe|devtunnel-standin`).
pub const DEVTUNNEL_ENV: &str = "COPILOT_CANVAS_DEVTUNNEL";

/// `<home>/remote`: paired servers (client side) and paired clients (service side).
pub fn dir(home: &Path) -> PathBuf {
    home.join("remote")
}

/// Names of servers and clients: 1–40 characters of `[A-Za-z0-9_.-]`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Dev Tunnel ids: 1–70 lower-case letters, digits, '-' and '.' (a cluster
/// suffix, `copilot-canvas.usw2`), starting with a letter or digit: an id
/// becomes an argument of `devtunnel`, so it can never read as an option.
pub fn valid_tunnel_id(id: &str) -> bool {
    id.len() <= 70
        && id.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.'))
}

/// This machine's name (what a service lists for a client it paired).
pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME").ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "client".into())
}

/// A service this machine paired with (`remote add`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct ServerRecord {
    /// What `--remote NAME` calls it.
    pub name: String,
    /// Dev Tunnel id (`devtunnel connect TUNNEL`); `None`: a direct address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    /// The tunnel's port: the service's `remote.listen` port.
    #[serde(default)]
    pub port: u16,
    /// `HOST:PORT` of a direct connection (loopback tests, an SSH forward).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// The VS Code tunnel on the service's machine (`code tunnel --name …`):
    /// its files open there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vscode_tunnel: Option<String>,
    /// The id the service gave this client.
    pub client_id: String,
    /// The client secret, DPAPI-protected (hex).
    pub secret: String,
    pub added_ms: u64,
}

impl ServerRecord {
    /// The client secret (only this Windows user can decrypt it).
    #[cfg(windows)]
    pub fn secret(&self) -> Result<Vec<u8>> {
        crate::dpapi::unprotect_hex(&self.secret).with_context(|| format!("cannot decrypt the secret of {:?} (paired by another Windows user?)", self.name))
    }

    /// Elsewhere secrets live in the platform's own store (the iPhone's Keychain), not in this file.
    #[cfg(not(windows))]
    pub fn secret(&self) -> Result<Vec<u8>> {
        anyhow::bail!("the secret of {:?} is DPAPI-protected (Windows only)", self.name)
    }

    /// `tunnel copilot-canvas, port 47600` or the direct address.
    pub fn describe(&self) -> String {
        match (&self.tunnel, &self.addr) {
            (Some(tunnel), _) => format!("tunnel {tunnel}, port {}", self.port),
            (None, Some(addr)) => addr.clone(),
            (None, None) => "no address".into(),
        }
    }
}

/// `<home>/remote/servers.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Servers {
    #[serde(default)]
    pub servers: Vec<ServerRecord>,
}

impl Servers {
    pub fn path(home: &Path) -> PathBuf {
        dir(home).join("servers.json")
    }

    /// A missing file is an empty list; a record with an invalid tunnel id
    /// makes the file an error (it would become a `devtunnel` argument).
    pub fn load(home: &Path) -> Result<Self> {
        let path = Self::path(home);
        let servers: Self = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("{} is not readable", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
        };
        if let Some(bad) = servers.servers.iter().find(|s| s.tunnel.as_deref().is_some_and(|t| !valid_tunnel_id(t))) {
            anyhow::bail!("{}: the server {:?} has an invalid tunnel id {:?}; add it again", path.display(), bad.name, bad.tunnel.as_deref().unwrap_or_default());
        }
        Ok(servers)
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        let path = Self::path(home);
        crate::paths::write_atomic(&path, serde_json::to_string_pretty(self)?.as_bytes()).with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn find(&self, name: &str) -> Option<&ServerRecord> {
        self.servers.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// Adds `record`, replacing a server of the same name.
    pub fn upsert(&mut self, record: ServerRecord) {
        self.servers.retain(|s| !s.name.eq_ignore_ascii_case(&record.name));
        self.servers.push(record);
    }

    pub fn remove(&mut self, name: &str) -> Option<ServerRecord> {
        let index = self.servers.iter().position(|s| s.name.eq_ignore_ascii_case(name))?;
        Some(self.servers.remove(index))
    }
}

/// How to run the Dev Tunnels CLI.
#[derive(Clone, Debug, PartialEq)]
pub struct DevTunnelCli {
    pub program: PathBuf,
    /// Arguments before the command's own (the stand-in's subcommand).
    pub prefix: Vec<String>,
}

/// The Dev Tunnels CLI: `COPILOT_CANVAS_DEVTUNNEL`, else [`find_devtunnel_exe`].
pub fn devtunnel() -> Option<DevTunnelCli> {
    if let Some(value) = std::env::var(DEVTUNNEL_ENV).ok().filter(|v| !v.trim().is_empty()) {
        let mut parts = value.split('|').map(str::trim);
        let program = PathBuf::from(parts.next().unwrap_or_default());
        return Some(DevTunnelCli { program, prefix: parts.filter(|p| !p.is_empty()).map(str::to_owned).collect() });
    }
    find_devtunnel_exe().map(|program| DevTunnelCli { program, prefix: Vec::new() })
}

/// `devtunnel.exe` on PATH, among WinGet's links, or in WinGet's package folder.
pub fn find_devtunnel_exe() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH")
        && let Some(found) = std::env::split_paths(&path).map(|d| d.join("devtunnel.exe")).find(|p| p.is_file())
    {
        return Some(found);
    }
    let local = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    let winget = local.join("Microsoft").join("WinGet");
    let link = winget.join("Links").join("devtunnel.exe");
    if link.is_file() {
        return Some(link);
    }
    std::fs::read_dir(winget.join("Packages"))
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with("microsoft.devtunnel"))
        .map(|e| e.path().join("devtunnel.exe"))
        .find(|p| p.is_file())
}

/// A Dev Tunnels access token (a JWT) and when it expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelToken {
    pub token: String,
    /// 0 when the token does not say.
    pub expires_ms: u64,
}

/// The claims of a JWT, unverified (only to read its expiry and tunnel).
pub fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Whether `word` looks like a JWT (three base64url parts, a JSON header).
fn looks_like_jwt(word: &str) -> bool {
    let parts: Vec<&str> = word.split('.').collect();
    parts.len() == 3 && word.starts_with("eyJ") && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=')))
}

/// Reads the output of `devtunnel token ID --scopes connect` (text with a
/// `Token: …` line, or JSON with a `token` field; any JWT in it otherwise).
pub fn parse_token_output(text: &str) -> Option<TunnelToken> {
    let from_json = serde_json::from_str::<serde_json::Value>(text.trim())
        .ok()
        .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(str::to_owned));
    let from_line = || {
        text.lines()
            .filter_map(|line| line.trim().strip_prefix("Token:").map(str::trim))
            .find(|t| looks_like_jwt(t))
            .map(str::to_owned)
    };
    let anywhere = || text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'').find(|w| looks_like_jwt(w)).map(str::to_owned);
    let token = from_json.filter(|t| looks_like_jwt(t)).or_else(from_line).or_else(anywhere)?;
    let expires_ms = jwt_claims(&token).and_then(|c| c.get("exp").and_then(|e| e.as_u64())).map_or(0, |secs| secs * 1000);
    Some(TunnelToken { token, expires_ms })
}

/// The web-forwarding address (`wss://ID-PORT.CLUSTER.devtunnels.ms`) of a
/// tunnel port, from the output of `devtunnel show ID` (or `host`): a
/// `https://…-PORT.….devtunnels.ms` address listed for the port, else one
/// built from a `Tunnel ID: NAME.CLUSTER` line.
pub fn web_url_from_show(text: &str, port: u16) -> Option<String> {
    let suffix = format!("-{port}.");
    let listed = text
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | ',' | '(' | ')' | '[' | ']'))
        .filter_map(|w| w.strip_prefix("https://"))
        .map(|w| w.trim_end_matches('/'))
        .find(|host| host.ends_with(".devtunnels.ms") && host.contains(&suffix))
        .map(|host| format!("wss://{host}"));
    listed.or_else(|| {
        let id = text.lines().find_map(|line| line.trim().strip_prefix("Tunnel ID").map(|rest| rest.trim_start_matches([' ', ':']).trim().to_owned()))?;
        web_url_for(&id, port)
    })
}

/// The web-forwarding address for a tunnel id with its cluster
/// (`copilot-canvas.usw2`) and a port.
pub fn web_url_for(id_with_cluster: &str, port: u16) -> Option<String> {
    let (name, cluster) = id_with_cluster.split_once('.')?;
    (valid_tunnel_id(name) && !cluster.is_empty() && cluster.chars().all(|c| c.is_ascii_alphanumeric())).then(|| format!("wss://{name}-{port}.{cluster}.devtunnels.ms"))
}

/// What the iPhone needs to pair (`copilot-canvas remote pair --qr`), as a
/// `ccanvas://pair?…` link shown in a QR code.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PairLink {
    /// What the phone calls this service (`cloudpc`).
    pub name: String,
    /// `wss://…devtunnels.ms` (or `ws://HOST:PORT` for the mock service and tests).
    pub url: String,
    /// The one-time pairing code (canonical form).
    pub code: String,
    /// A Dev Tunnels connect token (empty: none needed).
    pub token: String,
    pub token_expires_ms: u64,
}

impl PairLink {
    pub const SCHEME: &'static str = "ccanvas://pair";

    pub fn to_uri(&self) -> String {
        use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
        let enc = |v: &str| utf8_percent_encode(v, NON_ALPHANUMERIC).to_string();
        let mut uri = format!("{}?v=1&name={}&url={}&code={}", Self::SCHEME, enc(&self.name), enc(&self.url), enc(&self.code));
        if !self.token.is_empty() {
            uri.push_str(&format!("&token={}&exp={}", enc(&self.token), self.token_expires_ms));
        }
        uri
    }

    /// Reads a link (what the phone's camera scanned).
    pub fn parse(uri: &str) -> Result<Self> {
        use percent_encoding::percent_decode_str;
        let query = uri.trim().strip_prefix(Self::SCHEME).and_then(|q| q.strip_prefix('?')).context("not a Copilot Canvas pairing link")?;
        let mut link = PairLink::default();
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = percent_decode_str(value).decode_utf8().context("a pairing link field is not text")?.into_owned();
            match key {
                "name" => link.name = value,
                "url" => link.url = value,
                "code" => link.code = value,
                "token" => link.token = value,
                "exp" => link.token_expires_ms = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        link.code = crate::pairing::normalize_code(&link.code).context("the pairing link has no valid code")?;
        anyhow::ensure!(link.url.starts_with("wss://") || link.url.starts_with("ws://"), "the pairing link has no WebSocket address");
        if link.name.is_empty() || !valid_name(&link.name) {
            link.name = "cloudpc".into();
        }
        Ok(link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn servers_persist_with_protected_secrets() {
        let home = std::env::temp_dir().join(format!("cc-servers-{}", uuid::Uuid::new_v4()));
        assert_eq!(Servers::load(&home).unwrap(), Servers::default(), "no file yet");
        let secret = crate::pairing::new_secret();
        let record = ServerRecord {
            name: "CloudPC".into(),
            tunnel: Some("copilot-canvas".into()),
            port: 47600,
            client_id: "pc-1".into(),
            secret: crate::dpapi::protect_hex(&secret, "test").unwrap(),
            ..Default::default()
        };
        let mut servers = Servers::default();
        servers.upsert(record.clone());
        servers.upsert(ServerRecord { name: "direct".into(), tunnel: None, addr: Some("127.0.0.1:1".into()), ..record.clone() });
        servers.save(&home).unwrap();
        let text = std::fs::read_to_string(Servers::path(&home)).unwrap();
        assert!(!text.contains(&crate::paths::hex(&secret)), "the secret is not stored in the clear");
        let loaded = Servers::load(&home).unwrap();
        assert_eq!(loaded, servers);
        let found = loaded.find("cloudpc").unwrap();
        assert_eq!(found.secret().unwrap(), secret);
        assert_eq!(found.describe(), "tunnel copilot-canvas, port 47600");
        let mut changed = loaded.clone();
        changed.upsert(ServerRecord { client_id: "pc-2".into(), ..record });
        assert_eq!(changed.servers.len(), 2, "same name replaces");
        assert_eq!(changed.remove("DIRECT").unwrap().describe(), "127.0.0.1:1");
        assert!(changed.find("direct").is_none());
        std::fs::write(Servers::path(&home), "{broken").unwrap();
        assert!(Servers::load(&home).is_err(), "a broken file is an error, not an empty list");
        let mut injected = servers.clone();
        injected.servers[0].tunnel = Some("--help".into());
        std::fs::write(Servers::path(&home), serde_json::to_string(&injected).unwrap()).unwrap();
        assert!(Servers::load(&home).unwrap_err().to_string().contains("invalid tunnel id"));
        std::fs::remove_dir_all(home).unwrap();
        assert!(valid_name("cloud-pc_1.x") && !valid_name("a b") && !valid_name("") && !valid_name("host:1"));
    }

    #[test]
    fn tunnel_ids_follow_the_setup_rule() {
        for good in ["copilot-canvas", "a", "x1-2", "copilot-canvas.usw2", &"a".repeat(70)] {
            assert!(valid_tunnel_id(good), "{good}");
        }
        for bad in ["", "-x", "--port", ".x", "Copilot", "a b", "a;b", "a/b", &"a".repeat(71)] {
            assert!(!valid_tunnel_id(bad), "{bad}");
        }
    }

    #[test]
    fn the_devtunnel_override_takes_a_program_and_prefix_arguments() {
        // Only this test sets the variable.
        unsafe { std::env::set_var(DEVTUNNEL_ENV, r"C:\x\copilot-canvas.exe|devtunnel-standin") };
        let cli = devtunnel().unwrap();
        unsafe { std::env::remove_var(DEVTUNNEL_ENV) };
        assert_eq!(cli, DevTunnelCli { program: PathBuf::from(r"C:\x\copilot-canvas.exe"), prefix: vec!["devtunnel-standin".into()] });
    }

    fn jwt(claims: &str) -> String {
        use base64::Engine as _;
        let e = |t: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(t);
        format!("{}.{}.{}", e(r#"{"alg":"ES256"}"#), e(claims), e("sig"))
    }

    #[test]
    fn tunnel_tokens_are_read_from_text_or_json_with_their_expiry() {
        let token = jwt(r#"{"exp":1800000000,"scp":"connect","clusterId":"usw2","tunnelId":"copilot-canvas"}"#);
        let text = format!("Token tunnel ID: copilot-canvas.usw2\nToken scope: connect\nToken lifetime: 1.00:00:00\nToken expiration: 2027-01-15 08:00:00 UTC\nToken: {token}\n");
        let parsed = parse_token_output(&text).unwrap();
        assert_eq!(parsed, TunnelToken { token: token.clone(), expires_ms: 1_800_000_000_000 });
        assert_eq!(parse_token_output(&format!(r#"{{"token": "{token}", "scope": "connect"}}"#)).unwrap().token, token);
        assert_eq!(parse_token_output(&format!("something {token} else")).unwrap().token, token);
        assert_eq!(parse_token_output("Not logged in."), None);
        assert_eq!(jwt_claims(&token).unwrap()["clusterId"], "usw2");
    }

    #[test]
    fn web_addresses_come_from_the_listed_port_or_the_cluster() {
        let show = "Tunnel ID             : copilot-canvas.usw2\nPorts                 : 1\n  47600  auto  https://copilot-canvas-47600.usw2.devtunnels.ms/\n";
        assert_eq!(web_url_from_show(show, 47600).as_deref(), Some("wss://copilot-canvas-47600.usw2.devtunnels.ms"));
        assert_eq!(web_url_from_show("Tunnel ID: copilot-canvas.euw\n", 47600).as_deref(), Some("wss://copilot-canvas-47600.euw.devtunnels.ms"));
        assert_eq!(web_url_from_show("nothing here", 47600), None);
        assert_eq!(web_url_for("copilot-canvas", 1), None, "no cluster");
    }

    #[test]
    fn pairing_links_round_trip() {
        let link = PairLink { name: "cloudpc".into(), url: "wss://copilot-canvas-47600.usw2.devtunnels.ms".into(), code: "ABCDEFGHJKMN".into(), token: jwt("{}"), token_expires_ms: 5 };
        let uri = link.to_uri();
        assert!(uri.starts_with("ccanvas://pair?v=1&"));
        assert_eq!(PairLink::parse(&uri).unwrap(), link);
        let typed = PairLink::parse("ccanvas://pair?v=1&url=ws%3A%2F%2F127.0.0.1%3A47600&code=abcd-efgh-jkmn").unwrap();
        assert_eq!((typed.code.as_str(), typed.name.as_str(), typed.token.as_str()), ("ABCDEFGHJKMN", "cloudpc", ""));
        assert!(PairLink::parse("https://example.com").is_err());
        assert!(PairLink::parse("ccanvas://pair?v=1&url=http%3A%2F%2Fx&code=ABCDEFGHJKMN").is_err(), "WebSocket addresses only");
    }
}
