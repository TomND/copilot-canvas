//! The rule engine every egress point applies ([`decide`](super::decide)): the first rule
//! that matches decides; none: the policy's default action (anything but
//! `Allow` denies).
//!
//! What a rule matches:
//! - **host**: exact (`example.com`), any host under a domain
//!   (`*.example.com`, not the domain itself) or any host (`*`), without case,
//!   a trailing dot ignored; the port only when the rule names one
//!   (`example.com:8443`);
//! - **path**: a prefix ending at a segment (`/repos/contoso` covers
//!   `/repos/contoso` and `/repos/contoso/x`, not `/repos/contoso-x`), or a glob
//!   whose `*` stands for anything, `/` included; the query is not part of it,
//!   and `%XX` of a letter, digit, `-`, `.`, `_` or `~` reads as that character;
//! - **methods**: one of them, when the rule lists any.
//!
//! Where the engine cannot see part of a request it takes the careful
//! reading: a `CONNECT` (its tunnel may carry any path and method) and a path
//! a server could read otherwise than the rule does (`.` or `..` segments,
//! `//`, an encoded `/` or `\`, a `;`) make a `Deny` rule with a path or
//! methods match and an `Allow` or `Transform` rule with one not match. So a
//! rule with a path or methods never allows a `CONNECT`, and no `Deny` rule can
//! be stepped around. For the same reason `Deny` rules compare paths and
//! methods without case, `Allow` and `Transform` rules exactly; an action
//! the engine does not know denies.

use super::{Decision, Request};
use crate::vm::provider::{EgressPolicy, EgressRule};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Allow,
    Deny,
    Transform,
}

/// A rule's action word (empty: `Allow`, as the platform reads it).
pub(super) fn action(text: &str) -> Option<Action> {
    match text.trim().to_ascii_lowercase().as_str() {
        "" | "allow" => Some(Action::Allow),
        "deny" => Some(Action::Deny),
        "transform" => Some(Action::Transform),
        _ => None,
    }
}

/// The name a rule is known by (the platform's `rule-N` when it has none).
pub(super) fn rule_name(rule: &EgressRule, index: usize) -> String {
    match rule.name.trim() {
        "" => format!("rule-{}", index + 1),
        name => name.to_owned(),
    }
}

pub(super) fn decide(policy: &EgressPolicy, request: &Request) -> Decision {
    let connect = request.method.eq_ignore_ascii_case("CONNECT");
    let path = if connect { None } else { request.path.as_deref().and_then(canonical_path) };
    let host = normal_host(&request.host);
    for (index, rule) in policy.rules.iter().enumerate() {
        let action = action(&rule.action).unwrap_or(Action::Deny);
        let deny = action == Action::Deny;
        if !host_matches(&rule.host, &host, request.port, deny) || !methods_match(&rule.methods, &request.method, connect, deny) || !path_matches(rule.path.as_deref(), path.as_deref(), deny) {
            continue;
        }
        let rule_name = Some(rule_name(rule, index));
        return match action {
            Action::Deny => Decision { allowed: false, rule: rule_name, headers: Vec::new() },
            Action::Allow => Decision { allowed: true, rule: rule_name, headers: Vec::new() },
            Action::Transform => Decision { allowed: true, rule: rule_name, headers: rule.headers.clone() },
        };
    }
    Decision { allowed: policy.default_action.trim().eq_ignore_ascii_case("allow"), rule: None, headers: Vec::new() }
}

/// A host as rules compare it: lower case, no brackets, no trailing dot.
fn normal_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&host);
    host.strip_suffix('.').unwrap_or(host).to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Port {
    Any,
    Is(u16),
    /// A port that does not parse: a `Deny` rule matches every port, any other none.
    Bad,
}

/// A rule's host pattern and port: `name`, `name:port`, `[v6]`, `[v6]:port`, `v6`.
fn split_port(pattern: &str) -> (String, Port) {
    let pattern = pattern.trim().to_ascii_lowercase();
    let port = |p: &str| p.parse::<u16>().ok().filter(|p| *p != 0).map_or(Port::Bad, Port::Is);
    if let Some(rest) = pattern.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((host, "")) => (host.to_owned(), Port::Any),
            Some((host, after)) => (host.to_owned(), after.strip_prefix(':').map_or(Port::Bad, port)),
            None => (rest.to_owned(), Port::Bad),
        };
    }
    match pattern.split_once(':') {
        Some((host, p)) if !p.contains(':') => (host.to_owned(), port(p)),
        _ => (pattern, Port::Any),
    }
}

fn host_matches(pattern: &str, host: &str, port: u16, deny: bool) -> bool {
    let (pattern, want) = split_port(pattern);
    let pattern = match pattern.strip_suffix('.') {
        Some(p) if !p.ends_with(['*', '.']) => p,
        _ => &pattern,
    };
    let named = if pattern == "*" {
        true
    } else if let Some(domain) = pattern.strip_prefix("*.") {
        host.len() > domain.len() + 1 && host.ends_with(domain) && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
    } else {
        !pattern.is_empty() && pattern == host
    };
    named
        && match want {
            Port::Any => true,
            Port::Is(want) => want == port,
            Port::Bad => deny,
        }
}

fn methods_match(methods: &[String], method: &str, connect: bool, deny: bool) -> bool {
    if methods.is_empty() {
        return true;
    }
    if connect {
        return deny;
    }
    if deny { methods.iter().any(|m| m.trim().eq_ignore_ascii_case(method)) } else { methods.iter().any(|m| m.trim().to_ascii_uppercase() == method) }
}

fn path_matches(pattern: Option<&str>, path: Option<&str>, deny: bool) -> bool {
    let Some(pattern) = pattern.map(str::trim).filter(|p| !p.is_empty()) else { return true };
    let Some(path) = path else { return deny };
    let Some(pattern) = decode_unreserved(pattern) else { return deny };
    if deny { pattern_matches(&pattern.to_ascii_lowercase(), &path.to_ascii_lowercase()) } else { pattern_matches(&pattern, path) }
}

/// A path pattern: a glob when it has a `*`, else a prefix ending at a segment.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    if pattern.contains('*') {
        return glob(pattern.as_bytes(), path.as_bytes());
    }
    path == pattern || (path.starts_with(pattern) && (pattern.ends_with('/') || path[pattern.len()..].starts_with('/')))
}

/// `*` stands for any run of bytes (`/` included); everything else for itself.
fn glob(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((at, from)) = star {
            p = at + 1;
            t = from + 1;
            star = Some((at, from + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|b| *b == b'*')
}

/// `%XX` of an unreserved character as that character, other escapes upper
/// case; `None` for an escape that does not parse or hides a `/`, `\`, `;`
/// or a control character.
pub(super) fn decode_unreserved(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let hex = text.get(i + 1..i + 3).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))?;
        let byte = u8::from_str_radix(hex, 16).ok()?;
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else if matches!(byte, b'/' | b'\\' | b';') || byte.is_ascii_control() {
            return None;
        } else {
            out.push('%');
            out.push_str(&hex.to_ascii_uppercase());
        }
        chars.next();
        chars.next();
    }
    Some(out)
}

/// The path of a request as a server reads it (no query), or `None` when a
/// server could read it otherwise than a rule would.
pub(super) fn canonical_path(raw: &str) -> Option<String> {
    let raw = raw.split(['?', '#']).next().unwrap_or_default();
    let raw = if raw.is_empty() { "/" } else { raw };
    if !raw.starts_with('/') || raw.chars().any(|c| c == '\\' || c == ';' || c.is_control() || c.is_whitespace()) {
        return None;
    }
    let path = decode_unreserved(raw)?;
    let segments: Vec<&str> = path[1..].split('/').collect();
    let last = segments.len() - 1;
    if segments.iter().enumerate().any(|(i, s)| *s == "." || *s == ".." || (s.is_empty() && i != last)) {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::provider::HeaderFromSecret;

    fn rule(name: &str, host: &str, path: Option<&str>, methods: &[&str], action: &str) -> EgressRule {
        EgressRule { name: name.into(), host: host.into(), path: path.map(str::to_owned), methods: methods.iter().map(|m| m.to_string()).collect(), action: action.into(), headers: Vec::new() }
    }

    fn request(method: &str, host: &str, port: u16, path: Option<&str>) -> Request {
        Request { host: host.into(), port, method: method.into(), path: path.map(str::to_owned) }
    }

    /// Allowed, and the deciding rule.
    type Verdict = (bool, Option<String>);

    /// The verdict for a request.
    fn verdict(policy: &EgressPolicy, method: &str, url: &str) -> Verdict {
        let (host, port, path) = match method {
            "CONNECT" => {
                let (host, port) = url.rsplit_once(':').unwrap();
                (host.to_owned(), port.parse().unwrap(), None)
            }
            _ => {
                let rest = url.strip_prefix("http://").unwrap();
                let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
                let (host, port) = authority.rsplit_once(':').map_or((authority, 80), |(h, p)| (h, p.parse().unwrap()));
                (host.to_owned(), port, Some(if path.is_empty() { "/".to_owned() } else { path.to_owned() }))
            }
        };
        let decision = decide(policy, &request(method, &host, port, path.as_deref()));
        (decision.allowed, decision.rule)
    }

    fn forge() -> EgressPolicy {
        EgressPolicy {
            default_action: "Deny".into(),
            rules: vec![
                rule("git-admin", "example.com", Some("/contoso/app.git/admin"), &[], "Deny"),
                rule("git", "example.com", Some("/contoso/app.git/*"), &["GET", "POST"], "Allow"),
                EgressRule {
                    headers: vec![HeaderFromSecret { name: "Authorization".into(), secret: "forge-token".into(), key: "value".into(), format: "Bearer {}".into() }],
                    ..rule("api", "api.example.com", Some("/repos/contoso/app"), &[], "Transform")
                },
                rule("cdn", "*.cdn.example.com", None, &["GET"], "Allow"),
                rule("mirror", "mirror.example.com:8443", None, &[], "Allow"),
                rule("", "registry.example.com", None, &[], "allow"),
            ],
        }
    }

    #[test]
    fn the_rules_table() {
        let policy = forge();
        let allowed = |rule: &str| (true, Some(rule.to_owned()));
        let denied = |rule: &str| (false, Some(rule.to_owned()));
        let by_default = (false, None);
        let table: &[(&str, &str, Verdict)] = &[
            // First match wins: the deny before the broader allow.
            ("GET", "http://example.com/contoso/app.git/info/refs?service=git-upload-pack", allowed("git")),
            ("POST", "http://example.com/contoso/app.git/git-upload-pack", allowed("git")),
            ("GET", "http://example.com/contoso/app.git/admin", denied("git-admin")),
            ("GET", "http://example.com/contoso/app.git/admin/users", denied("git-admin")),
            // Methods.
            ("DELETE", "http://example.com/contoso/app.git/x", by_default.clone()),
            ("get", "http://example.com/contoso/app.git/x", by_default.clone()),
            // Glob: `*` crosses `/`, but the folder itself is not under it.
            ("GET", "http://example.com/contoso/app.git", by_default.clone()),
            ("GET", "http://example.com/contoso/other.git/x", by_default.clone()),
            // Prefix at a segment: the path itself, below it, never a longer name.
            ("GET", "http://api.example.com/repos/contoso/app", allowed("api")),
            ("PATCH", "http://api.example.com/repos/contoso/app/pulls/1", allowed("api")),
            ("GET", "http://api.example.com/repos/contoso/app-evil/pulls", by_default.clone()),
            ("GET", "http://api.example.com/repos/contoso", by_default.clone()),
            // Paths compare exactly for an allow.
            ("GET", "http://api.example.com/REPOS/contoso/app", by_default.clone()),
            // `*.` hosts: below the domain, not the domain, not a look-alike.
            ("GET", "http://a.cdn.example.com/x.js", allowed("cdn")),
            ("GET", "http://a.b.cdn.example.com/x.js", allowed("cdn")),
            ("GET", "http://cdn.example.com/x.js", by_default.clone()),
            ("GET", "http://evilcdn.example.com/x.js", by_default.clone()),
            ("GET", "http://a.cdn.example.com.evil.test/x.js", by_default.clone()),
            // Hosts without case, a trailing dot ignored.
            ("GET", "http://EXAMPLE.com/contoso/app.git/x", allowed("git")),
            ("GET", "http://example.com./contoso/app.git/x", allowed("git")),
            // Ports: ignored unless the rule names one.
            ("GET", "http://example.com:8080/contoso/app.git/x", allowed("git")),
            ("GET", "http://mirror.example.com:8443/x", allowed("mirror")),
            ("GET", "http://mirror.example.com/x", by_default.clone()),
            // A rule without a name is known as the platform names it.
            ("GET", "http://registry.example.com/v2/", allowed("rule-6")),
            // Unknown hosts: the default.
            ("GET", "http://contoso.example.net/", by_default.clone()),
            // CONNECT: on host only; a rule with a path or methods never allows it.
            ("CONNECT", "registry.example.com:443", allowed("rule-6")),
            ("CONNECT", "mirror.example.com:8443", allowed("mirror")),
            ("CONNECT", "mirror.example.com:443", by_default.clone()),
            ("CONNECT", "api.example.com:443", by_default.clone()),
            ("CONNECT", "a.cdn.example.com:443", by_default.clone()),
            // ... and a deny with a path cannot be stepped around through a tunnel.
            ("CONNECT", "example.com:443", denied("git-admin")),
        ];
        for (method, url, want) in table {
            assert_eq!(&verdict(&policy, method, url), want, "{method} {url}");
        }
        let transform = decide(&policy, &request("GET", "api.example.com", 80, Some("/repos/contoso/app/x")));
        assert_eq!(transform.headers, policy.rules[2].headers, "a Transform carries its headers");
        assert!(decide(&policy, &request("GET", "example.com", 80, Some("/contoso/app.git/x"))).headers.is_empty());
    }

    #[test]
    fn the_default_action() {
        let mut policy = EgressPolicy { default_action: "Allow".into(), rules: vec![rule("no-admin", "example.com", Some("/admin"), &[], "Deny")] };
        assert_eq!(verdict(&policy, "GET", "http://contoso.example.net/"), (true, None));
        assert_eq!(verdict(&policy, "CONNECT", "contoso.example.net:443"), (true, None));
        assert_eq!(verdict(&policy, "GET", "http://example.com/admin"), (false, Some("no-admin".into())));
        assert_eq!(verdict(&policy, "GET", "http://example.com/public"), (true, None));
        for default in ["allow", " ALLOW "] {
            policy.default_action = default.into();
            assert!(verdict(&policy, "GET", "http://contoso.example.net/").0, "{default:?}");
        }
        for default in ["Deny", "deny", "", "Rewrite", "Allowed"] {
            policy.default_action = default.into();
            assert_eq!(verdict(&policy, "GET", "http://contoso.example.net/"), (false, None), "{default:?} denies");
        }
        assert!(!decide(&EgressPolicy::default(), &request("GET", "example.com", 80, Some("/"))).allowed, "an empty policy denies");
    }

    #[test]
    fn deny_rules_read_carefully() {
        let policy = EgressPolicy {
            default_action: "Allow".into(),
            rules: vec![rule("no-admin", "example.com", Some("/admin"), &[], "Deny"), rule("no-delete", "api.example.com", None, &["DELETE"], "Deny"), rule("odd", "odd.example.com", Some("/x"), &[], "Rewrite")],
        };
        let denied = |rule: &str| (false, Some(rule.to_owned()));
        for path in ["/ADMIN", "/Admin/x", "/%61dmin", "/public/../admin", "/./admin", "//admin", "/admin;x=1", "/public%2F..%2Fadmin", "/a\\..\\admin", "/x/%2e%2e/admin", "/%zz"] {
            assert_eq!(verdict(&policy, "GET", &format!("http://example.com{path}")), denied("no-admin"), "{path}");
        }
        assert_eq!(verdict(&policy, "GET", "http://example.com/admin-tools"), (true, None), "a longer name is not under /admin");
        assert_eq!(verdict(&policy, "delete", "http://api.example.com/x"), denied("no-delete"), "methods without case for a deny");
        assert_eq!(verdict(&policy, "CONNECT", "api.example.com:443"), denied("no-delete"), "a tunnel could carry a DELETE");
        assert_eq!(verdict(&policy, "GET", "http://api.example.com/x"), (true, None));
        assert_eq!(verdict(&policy, "GET", "http://odd.example.com/x"), denied("odd"), "an unknown action denies");
        assert_eq!(decide(&policy, &request("GET", "example.com", 80, None)).rule.as_deref(), Some("no-admin"), "no path: the careful reading");
        // An allow with a path never matches a path read otherwise.
        let allow = EgressPolicy { default_action: "Deny".into(), rules: vec![rule("pub", "example.com", Some("/public/*"), &[], "Allow")] };
        assert_eq!(verdict(&allow, "GET", "http://example.com/public/x"), (true, Some("pub".into())));
        assert_eq!(verdict(&allow, "GET", "http://example.com/public/%78"), (true, Some("pub".into())), "an escaped letter is the letter");
        for path in ["/public/../admin", "/public/x;y", "/public//x", "/public/%2e%2e/admin", "/public/a%2Fb"] {
            assert_eq!(verdict(&allow, "GET", &format!("http://example.com{path}")), (false, None), "{path}");
        }
        assert_eq!(verdict(&allow, "GET", "http://example.com/public/a%20b"), (true, Some("pub".into())), "other escapes stay as they are");
    }

    #[test]
    fn hosts_ports_and_globs() {
        assert_eq!(split_port("Example.com:8443"), ("example.com".into(), Port::Is(8443)));
        assert_eq!(split_port("[::1]:80"), ("::1".into(), Port::Is(80)));
        assert_eq!((split_port("[::1]").1, split_port("::1").1, split_port("x:0").1, split_port("x:http").1), (Port::Any, Port::Any, Port::Bad, Port::Bad));
        assert!(host_matches("[::1]", &normal_host("[::1]"), 80, false));
        assert!(host_matches("*", "anything.example", 1, false));
        assert!(!host_matches("", "", 80, false) && !host_matches("*.", "x", 80, false));
        assert!(host_matches("x:http", "x", 80, true) && !host_matches("x:http", "x", 80, false), "a port that does not parse: a deny matches, an allow does not");
        for (pattern, text, want) in [("/a/*", "/a/b/c", true), ("/a/*/c", "/a/b/x/c", true), ("/a/*/c", "/a/b/x", false), ("*", "/", true), ("/a*b", "/ab", true), ("/a*b*c", "/axbyc", true), ("/a*b*c", "/axbyb", false), ("/a/**", "/a/", true)] {
            assert_eq!(glob(pattern.as_bytes(), text.as_bytes()), want, "{pattern} {text}");
        }
        assert_eq!(canonical_path("/a/b/?x=1#f").as_deref(), Some("/a/b/"));
        assert_eq!(canonical_path("").as_deref(), Some("/"));
        assert_eq!(canonical_path("?q").as_deref(), Some("/"));
        assert_eq!(canonical_path("/%7Euser/%41%2b").as_deref(), Some("/~user/A%2B"));
        for odd in ["a/b", "/a/./b", "/a/..", "/a//b", "/a%2fb", "/a%5Cb", "/a%3bb", "/a%00", "/a%2", "/a%+1", "/a b"] {
            assert_eq!(canonical_path(odd), None, "{odd}");
        }
        assert_eq!(action(" Transform "), Some(Action::Transform));
        assert_eq!(action("rewrite"), None);
    }
}
