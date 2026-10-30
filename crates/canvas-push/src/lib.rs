//! Push notifications to the iPhone app through Apple's push service (APNs),
//! sent by the service (docs/IOS-PLAN.md, phase 5).
//!
//! - [`ApnsKey`]: the team's APNs auth key (a `.p8` file), turned into the
//!   short-lived ES256 tokens APNs wants (reused for 50 minutes).
//! - [`Sender`]: HTTP/2 requests to `api.push.apple.com` (or the sandbox for
//!   development builds).
//! - [`Devices`]: the phones that registered (`ClientMsg::PushRegister`),
//!   per paired client id, in `<home>/remote/push.json`.
//! - [`Attention`]: which sessions just started waiting for you or finished
//!   a turn, from the same status rules the clients use.
//! - [`alert_payload`], [`access_payload`]: the notifications. A renewed
//!   tunnel access token travels sealed with the phone's own pairing secret
//!   (Apple only sees ciphertext).
//!
//! The service only pushes to phones that are not connected at that moment:
//! a connected phone already shows it in the app.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use canvas_model::status::Status;
use serde::{Deserialize, Serialize};

/// APNs tokens are valid for an hour; renewed after this long.
const JWT_LIFETIME: Duration = Duration::from_secs(50 * 60);

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The team's APNs auth key (`AuthKey_KEYID.p8` from the Apple developer site).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApnsKey {
    pub key_id: String,
    pub team_id: String,
    /// The `.p8` file (PKCS#8 PEM).
    pub pem: String,
}

impl std::fmt::Debug for ApnsKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApnsKey({} of team {})", self.key_id, self.team_id)
    }
}

impl ApnsKey {
    /// Checks the fields and that the key parses.
    pub fn new(key_id: &str, team_id: &str, pem: &str) -> Result<Self> {
        let valid = |id: &str| id.len() == 10 && id.chars().all(|c| c.is_ascii_alphanumeric());
        anyhow::ensure!(valid(key_id.trim()), "the key id is the 10 characters in the key's name (AuthKey_XXXXXXXXXX.p8)");
        anyhow::ensure!(valid(team_id.trim()), "the team id has 10 characters (developer.apple.com › Membership)");
        let key = Self { key_id: key_id.trim().to_owned(), team_id: team_id.trim().to_owned(), pem: pem.trim().to_owned() };
        key.signing_key()?;
        Ok(key)
    }

    fn signing_key(&self) -> Result<p256::ecdsa::SigningKey> {
        use p256::pkcs8::DecodePrivateKey;
        p256::ecdsa::SigningKey::from_pkcs8_pem(&self.pem).map_err(|e| anyhow::anyhow!("not an APNs auth key (.p8): {e}"))
    }

    /// A provider token: an ES256 JWT `{alg, kid}.{iss, iat}`.
    pub fn jwt(&self, issued_secs: u64) -> Result<String> {
        use p256::ecdsa::signature::Signer;
        let header = b64url(serde_json::json!({ "alg": "ES256", "kid": self.key_id }).to_string().as_bytes());
        let claims = b64url(serde_json::json!({ "iss": self.team_id, "iat": issued_secs }).to_string().as_bytes());
        let input = format!("{header}.{claims}");
        let signature: p256::ecdsa::Signature = self.signing_key()?.sign(input.as_bytes());
        Ok(format!("{input}.{}", b64url(&signature.to_bytes())))
    }

    /// `<home>/remote/apns.json` (owner-only; written by `remote push-key`).
    pub fn path(home: &Path) -> PathBuf {
        canvas_common::remote::dir(home).join("apns.json")
    }

    pub fn load(home: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(Self::path(home)).ok()?).ok()
    }
}

/// Which APNs service a device token belongs to (development builds use the sandbox).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    #[default]
    Production,
    Sandbox,
}

impl Environment {
    pub fn parse(text: &str) -> Self {
        if text.eq_ignore_ascii_case("sandbox") || text.eq_ignore_ascii_case("development") { Environment::Sandbox } else { Environment::Production }
    }

    fn host(self) -> &'static str {
        match self {
            Environment::Production => "https://api.push.apple.com",
            Environment::Sandbox => "https://api.sandbox.push.apple.com",
        }
    }
}

/// A phone that wants notifications.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    /// The paired client id (`pc-…`).
    pub client_id: String,
    /// The APNs device token (hex).
    pub token: String,
    pub environment: Environment,
    /// The app's bundle id (the APNs topic).
    pub bundle: String,
    /// What the phone calls this machine ("work"): the notification's subtitle.
    pub label: String,
    pub registered_ms: u64,
}

impl Device {
    /// Checks a registration from a phone.
    pub fn new(client_id: &str, token: &str, environment: &str, bundle: &str, label: &str) -> Result<Self> {
        let token = token.trim().to_ascii_lowercase();
        anyhow::ensure!((32..=200).contains(&token.len()) && token.chars().all(|c| c.is_ascii_hexdigit()), "not an APNs device token");
        anyhow::ensure!(!bundle.is_empty() && bundle.len() <= 200 && bundle.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')), "not a bundle id");
        Ok(Self {
            client_id: client_id.to_owned(),
            token,
            environment: Environment::parse(environment),
            bundle: bundle.to_owned(),
            label: label.chars().filter(|c| !c.is_control()).take(40).collect(),
            registered_ms: canvas_common::paths::now_ms(),
        })
    }
}

/// The registered phones, one per paired client (`<home>/remote/push.json`).
pub struct Devices {
    path: PathBuf,
    devices: Mutex<Vec<Device>>,
}

impl Devices {
    pub fn path(home: &Path) -> PathBuf {
        canvas_common::remote::dir(home).join("push.json")
    }

    pub fn open(home: &Path) -> Self {
        let path = Self::path(home);
        let devices = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, devices: Mutex::new(devices) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Device>> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, devices: &[Device]) {
        let _ = canvas_common::paths::write_atomic(&self.path, &serde_json::to_vec_pretty(devices).unwrap_or_default());
    }

    pub fn list(&self) -> Vec<Device> {
        self.lock().clone()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Registers (or replaces) a client's phone; returns whether it changed.
    pub fn register(&self, device: Device) -> bool {
        let mut devices = self.lock();
        if devices.iter().any(|d| d.client_id == device.client_id && d.token == device.token && d.label == device.label && d.environment == device.environment) {
            return false;
        }
        devices.retain(|d| d.client_id != device.client_id && d.token != device.token);
        devices.push(device);
        self.save(&devices);
        true
    }

    /// Forgets a client's phone (unregistered, revoked) or a token APNs refused.
    pub fn remove(&self, client_id: Option<&str>, token: Option<&str>) {
        let mut devices = self.lock();
        let before = devices.len();
        devices.retain(|d| client_id.is_none_or(|c| c != d.client_id) && token.is_none_or(|t| t != d.token));
        if devices.len() != before {
            self.save(&devices);
        }
    }
}

/// What APNs said about one notification.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// The token is no longer valid (the app was removed): forget it.
    Unregistered,
    /// Anything else (the key, the topic, a network problem): logged.
    Failed(String),
}

/// Sends notifications over HTTP/2.
pub struct Sender {
    key: ApnsKey,
    http: reqwest::blocking::Client,
    jwt: Mutex<Option<(String, Instant)>>,
}

impl Sender {
    pub fn new(key: ApnsKey) -> Result<Self> {
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(15)).build().context("cannot start the HTTP client")?;
        Ok(Self { key, http, jwt: Mutex::new(None) })
    }

    pub fn key(&self) -> &ApnsKey {
        &self.key
    }

    fn token(&self) -> Result<String> {
        let mut cached = self.jwt.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((jwt, at)) = cached.as_ref()
            && at.elapsed() < JWT_LIFETIME
        {
            return Ok(jwt.clone());
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let jwt = self.key.jwt(now)?;
        *cached = Some((jwt.clone(), Instant::now()));
        Ok(jwt)
    }

    /// Sends one notification. `background`: a silent one for the app (no alert).
    pub fn send(&self, device: &Device, payload: &serde_json::Value, background: bool, collapse: Option<&str>) -> Outcome {
        let jwt = match self.token() {
            Ok(jwt) => jwt,
            Err(error) => return Outcome::Failed(format!("{error:#}")),
        };
        let url = format!("{}/3/device/{}", device.environment.host(), device.token);
        let mut request = self
            .http
            .post(url)
            .header("authorization", format!("bearer {jwt}"))
            .header("apns-topic", &device.bundle)
            .header("apns-push-type", if background { "background" } else { "alert" })
            .header("apns-priority", if background { "5" } else { "10" })
            .header("content-type", "application/json")
            .body(payload.to_string());
        if let Some(collapse) = collapse {
            request = request.header("apns-collapse-id", collapse.chars().take(64).collect::<String>());
        }
        match request.send() {
            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.text().unwrap_or_default();
                let reason = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["reason"].as_str().map(str::to_owned)).unwrap_or_default();
                match status {
                    200 => Outcome::Sent,
                    410 => Outcome::Unregistered,
                    400 if reason == "BadDeviceToken" || reason == "DeviceTokenNotForTopic" => Outcome::Unregistered,
                    _ => Outcome::Failed(format!("APNs answered {status} {reason}")),
                }
            }
            Err(error) => Outcome::Failed(format!("cannot reach APNs: {error}")),
        }
    }
}

/// A session that just started waiting for you or finished a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// The canvas box id (what the app opens).
    pub id: String,
    pub name: String,
    /// `Status::Waiting` or `Status::JustFinished`.
    pub status: Status,
}

/// Remembers every session's last status and reports the transitions worth a
/// notification: into "needs you", and a working session finishing a turn.
#[derive(Default)]
pub struct Attention {
    seen: HashMap<String, Status>,
    primed: bool,
}

impl Attention {
    /// The current statuses (box id, name, status); the first call only primes.
    pub fn update(&mut self, now: impl IntoIterator<Item = (String, String, Status)>) -> Vec<Event> {
        let mut events = Vec::new();
        let mut seen = HashMap::new();
        for (id, name, status) in now {
            if self.primed
                && let Some(&before) = self.seen.get(&id)
                && before != status
            {
                let finished = status == Status::JustFinished && before == Status::Working;
                if status == Status::Waiting || finished {
                    events.push(Event { id: id.clone(), name, status });
                }
            }
            seen.insert(id, status);
        }
        self.seen = seen;
        self.primed = true;
        events
    }
}

/// The notification for a session: title the session, subtitle the machine
/// (as the phone calls it), the box and the paired client so a tap opens it.
pub fn alert_payload(device: &Device, event: &Event) -> serde_json::Value {
    let body = if event.status == Status::Waiting { "Needs you: a question or a permission." } else { "Finished — ready for review." };
    serde_json::json!({
        "aps": {
            "alert": { "title": event.name, "subtitle": device.label, "body": body },
            "sound": "default",
            "thread-id": device.client_id,
        },
        "cc": { "c": device.client_id, "s": event.id },
    })
}

/// A renewed tunnel access token for one phone, sealed with its pairing
/// secret ([`canvas_common::remote::seal_access`]; the phone opens it with
/// `open_access`).
pub fn access_payload(device: &Device, secret: &[u8], url: &str, token: &str, expires_ms: u64) -> serde_json::Value {
    let (nonce, sealed) = canvas_common::remote::seal_access(secret, url, token, expires_ms);
    serde_json::json!({
        "aps": { "content-available": 1 },
        "cc": { "c": device.client_id, "n": nonce, "t": sealed },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgupVmO4lHWipy6jwQ
TKnhSjBvWktN/OdHJn3c8GFl5XuhRANCAATh/CKGNLJE7HiUogwGZOQLRnDh1cb2
xIg5b32AvVgn4P6dw7B8py49iH9UZ7AQI5LDEdt1UIjEoEc2Sq4dZF82
-----END PRIVATE KEY-----";

    #[test]
    fn provider_tokens_are_es256_jwts_apple_can_verify() {
        use p256::ecdsa::signature::Verifier;
        let key = ApnsKey::new("ABC123DEFG", "8Z37ZVMJ4V", TEST_KEY).unwrap();
        let jwt = key.jwt(1_800_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let decode = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
        let header: serde_json::Value = serde_json::from_slice(&decode(parts[0])).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&decode(parts[1])).unwrap();
        assert_eq!((header["alg"].as_str(), header["kid"].as_str()), (Some("ES256"), Some("ABC123DEFG")));
        assert_eq!((claims["iss"].as_str(), claims["iat"].as_u64()), (Some("8Z37ZVMJ4V"), Some(1_800_000_000)));
        let signature = p256::ecdsa::Signature::from_slice(&decode(parts[2])).unwrap();
        let verifying = *key.signing_key().unwrap().verifying_key();
        assert!(verifying.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature).is_ok());
        assert!(ApnsKey::new("short", "8Z37ZVMJ4V", TEST_KEY).is_err());
        assert!(ApnsKey::new("ABC123DEFG", "8Z37ZVMJ4V", "not a key").is_err());
    }

    #[test]
    fn needs_you_and_finished_turns_are_reported_once() {
        let mut attention = Attention::default();
        let s = |id: &str, status| (id.to_owned(), format!("Session {id}"), status);
        assert!(attention.update([s("a", Status::Working), s("b", Status::Waiting)]).is_empty(), "the first look only primes");
        let events = attention.update([s("a", Status::JustFinished), s("b", Status::Waiting), s("c", Status::Idle)]);
        assert_eq!(events, [Event { id: "a".into(), name: "Session a".into(), status: Status::JustFinished }]);
        assert!(attention.update([s("a", Status::JustFinished)]).is_empty(), "no repeat");
        assert_eq!(attention.update([s("a", Status::Waiting)]).len(), 1, "now it needs you");
        assert!(attention.update([s("a", Status::Idle), s("x", Status::Waiting)]).is_empty(), "a new session is primed first");
        assert!(attention.update([s("a", Status::JustFinished)]).is_empty(), "finished without working first: reviewed state changes are not news");
    }

    #[test]
    fn registrations_replace_and_are_forgotten() {
        let home = std::env::temp_dir().join(format!("cc-push-{}", canvas_common::paths::now_ms()));
        let devices = Devices::open(&home);
        let token = "a".repeat(64);
        assert!(Device::new("pc-1", "xyz", "sandbox", "com.x.y", "work").is_err());
        let device = Device::new("pc-1", &token, "sandbox", "com.thomashorga.copilotcanvas", "work").unwrap();
        assert_eq!(device.environment, Environment::Sandbox);
        assert!(devices.register(device.clone()));
        assert!(!devices.register(device.clone()), "the same again changes nothing");
        assert!(devices.register(Device { label: "personal".into(), ..device.clone() }));
        assert_eq!(Devices::open(&home).list().len(), 1, "saved, one per client");
        devices.remove(Some("pc-1"), None);
        assert!(Devices::open(&home).is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn alerts_name_the_session_and_machine_and_access_is_sealed_for_one_phone() {
        let device = Device::new("pc-1", &"b".repeat(64), "production", "com.thomashorga.copilotcanvas", "work").unwrap();
        let event = Event { id: "box-1".into(), name: "Review PR #42".into(), status: Status::Waiting };
        let alert = alert_payload(&device, &event);
        assert_eq!(alert["aps"]["alert"]["title"], "Review PR #42");
        assert_eq!(alert["aps"]["alert"]["subtitle"], "work");
        assert_eq!((alert["cc"]["c"].as_str(), alert["cc"]["s"].as_str()), (Some("pc-1"), Some("box-1")));
        let secret = canvas_common::pairing::new_secret();
        let access = access_payload(&device, &secret, "wss://t-1.usw3.devtunnels.ms", "eyJ.a.b", 9);
        assert_eq!(access["aps"]["content-available"], 1);
        assert!(!access.to_string().contains("eyJ.a.b"), "the token is not in the clear");
        let open = canvas_common::remote::open_access;
        let opened = open(&secret, access["cc"]["n"].as_str().unwrap(), access["cc"]["t"].as_str().unwrap()).unwrap();
        assert_eq!(opened, ("wss://t-1.usw3.devtunnels.ms".to_owned(), "eyJ.a.b".to_owned(), 9));
        assert!(open(&canvas_common::pairing::new_secret(), access["cc"]["n"].as_str().unwrap(), access["cc"]["t"].as_str().unwrap()).is_err());
        assert!(access.to_string().len() < 4096, "fits a notification");
    }
}
