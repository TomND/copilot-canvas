//! Bearer tokens for `vm::aca`: the pack's `auth.command` (run as you; its
//! standard output is the token) or `az account get-access-token`, cached
//! until 5 minutes before they expire and refreshed on a background thread
//! ahead of that, so a call waits for the command only when the cache is
//! empty. A token never appears in errors, logs or `Debug`.
//!
//! [`TokenSource`] is what fetches a token: [`CommandSource`] in the
//! service, a closure in tests (which never run `az`).

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::Value;

use crate::vm::pack::Auth;
use crate::vm::provider::Secret;

/// A token is used until this long before it expires.
pub const MARGIN_MS: u64 = 5 * 60_000;
/// A token in use is refreshed in the background from this long before it expires.
pub const AHEAD_MS: u64 = 15 * 60_000;
/// The lifetime assumed for a token that says nothing of its expiry.
pub const ASSUMED_LIFETIME_MS: u64 = 30 * 60_000;
/// After a failed fetch, its error is the answer for this long (no command storm).
const FAILED_FOR_MS: u64 = 10_000;
/// How long the token command may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_STDOUT: u64 = 1 << 20;
const MAX_STDERR: u64 = 64 << 10;
/// What replaces a token (or anything like one) in a message.
const HIDDEN: &str = "(hidden)";

/// A bearer token and when it expires. Zeroized when dropped; `Debug` shows
/// only the expiry.
pub struct Token {
    value: Secret,
    expires_ms: u64,
}

impl Token {
    pub fn new(value: impl Into<String>, expires_ms: u64) -> Token {
        Token { value: Secret::new(value.into().into_bytes()), expires_ms }
    }

    pub fn expires_ms(&self) -> u64 {
        self.expires_ms
    }

    /// The token itself, for the `Authorization` header only.
    pub(crate) fn reveal(&self) -> &str {
        std::str::from_utf8(self.value.expose()).unwrap_or_default()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token(… expires at {})", self.expires_ms)
    }
}

/// Where tokens come from. Errors never carry the token.
pub trait TokenSource: Send + Sync {
    fn fetch(&self) -> Result<Token>;
}

impl<F: Fn() -> Result<Token> + Send + Sync> TokenSource for F {
    fn fetch(&self) -> Result<Token> {
        self()
    }
}

/// The pack's `auth.command`, else the Azure CLI for `auth.audience`.
#[derive(Clone)]
pub struct CommandSource {
    command: Option<String>,
    audience: String,
    timeout: Duration,
}

impl std::fmt::Debug for CommandSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The command is the pack's: say only which kind runs.
        f.write_str(if self.command.is_some() { "CommandSource(auth.command)" } else { "CommandSource(az)" })
    }
}

impl CommandSource {
    pub fn of(auth: &Auth) -> CommandSource {
        let audience = if auth.audience.trim().is_empty() { crate::vm::pack::DEFAULT_AUDIENCE.to_owned() } else { auth.audience.trim().to_owned() };
        CommandSource { command: auth.command.clone().filter(|c| !c.trim().is_empty()), audience, timeout: COMMAND_TIMEOUT }
    }

    /// What runs, as it is named in errors (never the pack's command itself).
    fn what(&self) -> &'static str {
        if self.command.is_some() { "the pack's auth.command" } else { "az account get-access-token" }
    }

    /// The Azure CLI's arguments for the audience.
    fn az_args(&self) -> Result<Vec<String>> {
        let ok = !self.audience.is_empty() && self.audience.chars().all(|c| c.is_ascii_alphanumeric() || ":/._-".contains(c));
        anyhow::ensure!(ok, "auth.audience may hold only letters, digits and : / . _ -");
        Ok(["account", "get-access-token", "--resource", &self.audience, "-o", "json"].map(str::to_owned).to_vec())
    }

    /// The process to start: the command through the shell, or `az`.
    fn process(&self) -> Result<Command> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // `cmd /s /c "…"` runs the text between the outer quotes as written.
            let line = match &self.command {
                Some(command) => command.clone(),
                None => format!("az {}", self.az_args()?.join(" ")),
            };
            let mut c = Command::new("cmd");
            c.args(["/d", "/s", "/c"]).raw_arg(format!("\"{line}\""));
            Ok(c)
        }
        #[cfg(not(windows))]
        {
            match &self.command {
                Some(command) => {
                    let mut c = Command::new("/bin/sh");
                    c.args(["-c", command]);
                    Ok(c)
                }
                None => {
                    let mut c = Command::new("az");
                    c.args(self.az_args()?);
                    Ok(c)
                }
            }
        }
    }
}

impl TokenSource for CommandSource {
    fn fetch(&self) -> Result<Token> {
        let what = self.what();
        let (status, stdout, stderr) = run(self.process()?, self.timeout, what)?;
        let stdout = Secret::new(stdout);
        let text = std::str::from_utf8(stdout.expose()).map_err(|_| anyhow!("{what} printed something that is not text"))?;
        if !status.success() {
            // Whatever it printed may hold a token: hide every longer word of it from stderr.
            let words: Vec<&str> = text.split(|c: char| c.is_whitespace() || "\"',:;{}".contains(c)).filter(|w| w.len() >= 8).collect();
            let reason = crate::vm::hooks::first_line(&scrub(&stderr, &words));
            bail!("{what} failed ({status}){}", if reason.is_empty() { String::new() } else { format!(": {reason}") });
        }
        parse(text, canvas_common::paths::now_ms()).with_context(|| format!("{what} printed no usable token"))
    }
}

/// Runs `command` without input, with a time limit; (status, stdout, stderr).
fn run(mut command: Command, limit: Duration, what: &str) -> Result<(ExitStatus, Vec<u8>, String)> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    canvas_sys::spawn::CommandExt::no_window(&mut command);
    let mut child = command.spawn().with_context(|| format!("cannot run {what}"))?;
    let mut stdout = child.stdout.take().context("token command stdout")?;
    let mut stderr = child.stderr.take().context("token command stderr")?;
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(MAX_STDOUT).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        Secret::new(bytes)
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stderr).take(MAX_STDERR).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        bytes
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = canvas_sys::process::kill_tree(child.id());
            let _ = child.kill();
            let _ = child.wait();
            bail!("{what} did not finish within {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out.join().map(|s| s.expose().to_vec()).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    Ok((status, stdout, stderr))
}

/// A token from a command's output: the Azure CLI's JSON (`accessToken`,
/// `expires_on`) or a token alone on one line. The expiry comes from the
/// JSON, else from the token's own `exp` claim (a JWT), else
/// [`ASSUMED_LIFETIME_MS`] from `now`.
pub fn parse(text: &str, now: u64) -> Result<Token> {
    let text = text.trim();
    let (token, expires) = if text.starts_with('{') {
        let json: Value = serde_json::from_str(text).map_err(|_| anyhow!("the output is not JSON"))?;
        let token = ["accessToken", "access_token", "token"].iter().find_map(|k| json[k].as_str()).context("the JSON has no accessToken")?;
        let expires = match &json["expires_on"] {
            Value::Number(n) => n.as_u64().map(|s| s * 1000),
            Value::String(s) => s.trim().parse::<u64>().ok().map(|s| s * 1000),
            _ => None,
        }
        .or_else(|| json["expiresOn"].as_str().and_then(canvas_common::paths::parse_rfc3339_ms))
        .or_else(|| json["expires_in"].as_u64().map(|s| now + s * 1000));
        (token.trim().to_owned(), expires)
    } else {
        anyhow::ensure!(text.lines().count() == 1, "the output is not one line");
        let token = text.strip_prefix("Bearer ").unwrap_or(text).trim();
        (token.to_owned(), None)
    };
    anyhow::ensure!(!token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()), "the output is not a bearer token");
    let expires = expires.or_else(|| jwt_expiry_ms(&token)).unwrap_or(now + ASSUMED_LIFETIME_MS);
    Ok(Token::new(token, expires))
}

/// A JWT's `exp` claim in milliseconds.
fn jwt_expiry_ms(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims["exp"].as_u64().map(|s| s * 1000)
}

/// `text` without the strings in `hide` and without anything that looks like
/// a JWT (`eyJ…`).
pub(crate) fn scrub(text: &str, hide: &[&str]) -> String {
    let mut out = text.to_owned();
    for value in hide.iter().map(|v| v.trim()).filter(|v| v.len() >= 4) {
        out = out.replace(value, HIDDEN);
    }
    let mut clean = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(start) = rest.find("eyJ") {
        let run = rest[start..].find(|c: char| !(c.is_ascii_alphanumeric() || "-_.=+/".contains(c))).unwrap_or(rest.len() - start);
        clean.push_str(&rest[..start]);
        clean.push_str(if run >= 16 { HIDDEN } else { &rest[start..start + run] });
        rest = &rest[start + run..];
    }
    clean.push_str(rest);
    clean
}

/// Milliseconds since the epoch.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Tokens from a [`TokenSource`], kept until [`MARGIN_MS`] before they expire.
/// A token in use is refreshed in the background from [`AHEAD_MS`] before
/// its expiry (on the next call, and by a keeper thread when no call comes),
/// so calls wait for the source only when no usable token is cached.
pub struct Cache {
    inner: Arc<Inner>,
}

struct Inner {
    source: Box<dyn TokenSource>,
    clock: Clock,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    token: Option<Arc<Token>>,
    /// A fetch runs (on a caller's thread or in the background).
    fetching: bool,
    /// The token was handed out since it was fetched: only tokens in use are refreshed ahead.
    used: bool,
    /// The last failed fetch: when, and what it said.
    failed: Option<(u64, String)>,
    /// The keeper thread runs.
    keeper: bool,
    /// The cache was dropped: the keeper ends.
    stop: bool,
}

impl State {
    fn failed_recently(&self, now: u64) -> Option<&str> {
        self.failed.as_ref().filter(|(at, _)| now.saturating_sub(*at) < FAILED_FOR_MS).map(|(_, message)| message.as_str())
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs the source (`fetching` is already set) and records what came of
    /// it; `used`: the token goes to a caller now. A source that panics is a
    /// failed fetch (never a fetch that seems to run forever).
    fn fetch(self: &Arc<Self>, used: bool) -> Result<Arc<Token>> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.source.fetch())).unwrap_or_else(|_| Err(anyhow!("the token source failed")));
        let now = (self.clock)();
        let mut state = self.lock();
        state.fetching = false;
        let out = match result {
            Ok(token) => {
                let token = Arc::new(token);
                state.token = Some(token.clone());
                state.used = used;
                state.failed = None;
                if !state.keeper && !state.stop {
                    let weak = Arc::downgrade(self);
                    state.keeper = std::thread::Builder::new().name("aca-token".into()).spawn(move || keep(weak)).is_ok();
                }
                Ok(token)
            }
            Err(error) => {
                let message = format!("{error:#}");
                state.failed = Some((now, message.clone()));
                Err(anyhow!(message))
            }
        };
        self.changed.notify_all();
        out
    }

    /// Fetches on a thread of its own (`fetching` is already set).
    fn fetch_in_background(self: &Arc<Self>, state: &mut State) {
        let inner = self.clone();
        if std::thread::Builder::new().name("aca-token-fetch".into()).spawn(move || drop(inner.fetch(false))).is_err() {
            state.fetching = false;
        }
    }
}

/// The keeper: refreshes a token in use before it expires, and ends when the
/// token was not used since it was fetched (it lapses; the next call fetches).
fn keep(weak: Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else { return };
        let mut state = inner.lock();
        let now = (inner.clock)();
        let Some(token) = state.token.clone().filter(|t| now < t.expires_ms && !state.stop) else {
            state.keeper = false;
            return;
        };
        let due = token.expires_ms.saturating_sub(AHEAD_MS);
        let wait = if now < due {
            Duration::from_millis((due - now).min(10 * 60_000))
        } else if !state.used {
            state.keeper = false;
            return;
        } else if !state.fetching && state.failed_recently(now).is_none() {
            state.fetching = true;
            drop(state);
            let _ = inner.fetch(false);
            continue;
        } else {
            Duration::from_millis(FAILED_FOR_MS)
        };
        drop(inner.changed.wait_timeout(state, wait));
    }
}

impl Cache {
    pub fn new(source: Box<dyn TokenSource>) -> Cache {
        Cache::with_clock(source, Arc::new(canvas_common::paths::now_ms))
    }

    /// With a clock of its own (milliseconds since the epoch), for tests.
    pub fn with_clock(source: Box<dyn TokenSource>, clock: Clock) -> Cache {
        Cache { inner: Arc::new(Inner { source, clock, state: Mutex::new(State::default()), changed: Condvar::new() }) }
    }

    /// A usable token: the cached one, else a fresh one (waiting for the source).
    pub fn get(&self) -> Result<Arc<Token>> {
        let inner = &self.inner;
        let started = Instant::now();
        let mut state = inner.lock();
        loop {
            let now = (inner.clock)();
            if let Some(token) = state.token.clone()
                && now.saturating_add(MARGIN_MS) < token.expires_ms
            {
                state.used = true;
                if now.saturating_add(AHEAD_MS) >= token.expires_ms && !state.fetching && state.failed_recently(now).is_none() {
                    state.fetching = true;
                    inner.fetch_in_background(&mut state);
                }
                return Ok(token);
            }
            if !state.fetching {
                if let Some(message) = state.failed_recently(now) {
                    bail!("{message}");
                }
                state.fetching = true;
                drop(state);
                return inner.fetch(true);
            }
            // Another thread fetches: wait for it.
            let left = (COMMAND_TIMEOUT + Duration::from_secs(5)).saturating_sub(started.elapsed());
            if left.is_zero() {
                bail!("timed out waiting for an access token");
            }
            state = inner.changed.wait_timeout(state, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// The cached token, if any (never fetches): to hide it from messages.
    pub fn cached(&self) -> Option<Arc<Token>> {
        self.inner.lock().token.clone()
    }

    /// Starts a background fetch when no token is cached or it is due for a
    /// refresh (the manager calls this when it starts, so the first launch
    /// does not wait for the command).
    pub fn warm(&self) {
        let mut state = self.inner.lock();
        let now = (self.inner.clock)();
        let fresh = state.token.as_ref().is_some_and(|t| now.saturating_add(AHEAD_MS) < t.expires_ms);
        if !fresh && !state.fetching && state.failed_recently(now).is_none() {
            state.fetching = true;
            self.inner.fetch_in_background(&mut state);
        }
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.inner.lock().stop = true;
        self.inner.changed.notify_all();
    }
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.lock();
        f.debug_struct("Cache").field("expires_ms", &state.token.as_ref().map(|t| t.expires_ms)).field("fetching", &state.fetching).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const MIN: u64 = 60_000;

    /// A source that counts its calls and hands out `tok-<n>` expiring `life` after the clock.
    fn counting(clock: Arc<AtomicU64>, life: u64) -> (Arc<AtomicU64>, Box<dyn TokenSource>) {
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let source = move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Token::new(format!("tok-{n}"), clock.load(Ordering::SeqCst) + life))
        };
        (calls, Box::new(source))
    }

    fn fake_clock(start: u64) -> (Arc<AtomicU64>, Clock) {
        let now = Arc::new(AtomicU64::new(start));
        let read = now.clone();
        (now, Arc::new(move || read.load(Ordering::SeqCst)))
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn one_fetch_serves_many_calls_and_a_refresh_happens_ahead_in_the_background() {
        let (now, clock) = fake_clock(1_000_000_000);
        let (calls, source) = counting(now.clone(), 60 * MIN);
        let cache = Cache::with_clock(source, clock);
        for _ in 0..20 {
            assert_eq!(cache.get().unwrap().reveal(), "tok-1");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // 50 minutes later: still usable, handed out at once, refreshed behind.
        now.fetch_add(50 * MIN, Ordering::SeqCst);
        assert_eq!(cache.get().unwrap().reveal(), "tok-1");
        wait_for("the background refresh", || calls.load(Ordering::SeqCst) == 2 && !cache.inner.lock().fetching);
        assert_eq!(cache.get().unwrap().reveal(), "tok-2");
        // Within 5 minutes of its expiry a token is not used: the call waits for a new one.
        now.fetch_add(56 * MIN, Ordering::SeqCst);
        assert_eq!(cache.get().unwrap().reveal(), "tok-3");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_failed_fetch_is_not_repeated_at_once_and_never_shows_the_token() {
        let (now, clock) = fake_clock(1_000_000_000);
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        let source = move || -> Result<Token> {
            counter.fetch_add(1, Ordering::SeqCst);
            bail!("az: please run az login")
        };
        let cache = Cache::with_clock(Box::new(source), clock);
        assert!(cache.get().unwrap_err().to_string().contains("az login"));
        assert!(cache.get().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second call got the first one's error");
        now.fetch_add(FAILED_FOR_MS, Ordering::SeqCst);
        assert!(cache.get().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let token = Token::new("tok-very-secret", 5);
        assert_eq!(format!("{token:?}"), "Token(… expires at 5)");
        assert!(!format!("{cache:?}").contains("tok"));
        let panicking = Cache::new(Box::new(|| -> Result<Token> { panic!("source bug") }));
        assert_eq!(panicking.get().unwrap_err().to_string(), "the token source failed");
        assert!(!panicking.inner.lock().fetching);
    }

    #[test]
    fn the_keeper_refreshes_a_token_in_use_and_lets_an_unused_one_lapse() {
        let calls = Arc::new(AtomicU64::new(0));
        let counter = calls.clone();
        // Due for a refresh 300 ms after it is fetched (real clock).
        let source = move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Token::new(format!("tok-{n}"), canvas_common::paths::now_ms() + AHEAD_MS + 300))
        };
        let cache = Cache::new(Box::new(source));
        assert_eq!(cache.get().unwrap().reveal(), "tok-1");
        wait_for("the keeper's refresh", || calls.load(Ordering::SeqCst) == 2);
        // tok-2 was never used: the keeper lets it lapse instead of running the command again.
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        wait_for("the keeper to end", || !cache.inner.lock().keeper);
    }

    #[test]
    fn warming_fetches_in_the_background() {
        let (now, clock) = fake_clock(1_000_000_000);
        let (calls, source) = counting(now, 60 * MIN);
        let cache = Cache::with_clock(source, clock);
        cache.warm();
        wait_for("the warm fetch", || cache.cached().is_some());
        cache.warm();
        assert_eq!(cache.get().unwrap().reveal(), "tok-1");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    fn jwt(exp: u64) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}.sig", b64.encode(r#"{"alg":"none"}"#), b64.encode(format!(r#"{{"aud":"x","exp":{exp}}}"#)))
    }

    #[test]
    fn tokens_parse_from_the_cli_json_or_one_line() {
        let now = 1_700_000_000_000;
        let az = r#"{ "accessToken": "abc.def", "expiresOn": "2026-11-09 15:04:05.000000", "expires_on": 1700003600, "tokenType": "Bearer" }"#;
        let token = parse(az, now).unwrap();
        assert_eq!((token.reveal(), token.expires_ms()), ("abc.def", 1_700_003_600_000));
        let token = parse(&format!("Bearer {}\n", jwt(1_700_001_000)), now).unwrap();
        assert_eq!(token.expires_ms(), 1_700_001_000_000);
        assert_eq!(parse("opaque-token", now).unwrap().expires_ms(), now + ASSUMED_LIFETIME_MS);
        assert_eq!(parse(r#"{"accessToken": "t1", "expiresOn": "2023-11-14T22:13:20Z"}"#, now).unwrap().expires_ms(), now);
        for bad in ["", "two\nlines", "has space", "{\"other\": 1}", "{not json secret-x"] {
            let error = format!("{:#}", parse(bad, now).unwrap_err());
            assert!(!error.contains("secret-x") && !error.contains("lines"), "{error}");
        }
    }

    #[test]
    fn scrub_hides_tokens_and_jwts() {
        let token = jwt(1);
        assert_eq!(scrub(&format!("bad token {token}: refused"), &[]), "bad token (hidden): refused");
        assert_eq!(scrub("the key opaque-123 was refused", &["opaque-123"]), "the key (hidden) was refused");
        assert_eq!(scrub("eyJ short", &[]), "eyJ short");
        assert_eq!(scrub("x", &["", "ab"]), "x");
    }

    #[test]
    fn the_command_source_runs_the_pack_command_and_names_az_otherwise() {
        let az = CommandSource::of(&Auth { audience: "https://dynamicsessions.io".into(), command: None });
        assert_eq!(az.az_args().unwrap(), ["account", "get-access-token", "--resource", "https://dynamicsessions.io", "-o", "json"]);
        assert_eq!(format!("{az:?}"), "CommandSource(az)");
        let bad = CommandSource::of(&Auth { audience: "x & calc".into(), command: None });
        assert!(bad.az_args().is_err() && bad.process().is_err());
        let printing = if cfg!(windows) { "echo tok-from-command" } else { "printf '%s\\n' tok-from-command" };
        let source = CommandSource::of(&Auth { audience: String::new(), command: Some(printing.into()) });
        assert_eq!(format!("{source:?}"), "CommandSource(auth.command)");
        let token = source.fetch().unwrap();
        assert_eq!(token.reveal(), "tok-from-command");
        // A command that fails after printing the token: the error never shows it.
        let leaking = if cfg!(windows) { "echo tok-leaked-1234 & echo tok-leaked-1234 1>&2 & exit 3" } else { "echo tok-leaked-1234; echo tok-leaked-1234 >&2; exit 3" };
        let error = format!("{:#}", CommandSource::of(&Auth { audience: String::new(), command: Some(leaking.into()) }).fetch().unwrap_err());
        assert!(error.contains("failed") && !error.contains("tok-leaked"), "{error}");
        let empty = if cfg!(windows) { "exit 0" } else { "true" };
        let error = format!("{:#}", CommandSource::of(&Auth { audience: String::new(), command: Some(empty.into()) }).fetch().unwrap_err());
        assert!(error.contains("no usable token"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hanging_command_is_ended() {
        let source = CommandSource { command: Some("sleep 30".into()), audience: String::new(), timeout: Duration::from_millis(300) };
        let started = Instant::now();
        assert!(source.fetch().unwrap_err().to_string().contains("did not finish"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
