//! A pack's egress profile (`egress/<name>.yaml`) rendered into a box's
//! [`EgressPolicy`] at its claim. The format (the example pack's
//! `egress/forge.yaml`):
//!
//! ```yaml
//! default: deny                     # or allow; deny when missing
//! rules:                            # in order: the first that matches decides
//!   - name: forge-api               # optional (else <profile>-<N>)
//!     host: api.example.com         # exact, *.domain or *; an optional :port
//!     path: /repos/contoso/app/*    # optional: a prefix or a * glob
//!     methods: [GET, POST]          # optional
//!     action: transform             # allow (when missing), deny or transform
//!     headers:                      # a transform's, and only a transform's
//!       - name: Authorization
//!         value: "Bearer {{secret:forge-token}}"
//! ```
//!
//! Hosts and paths take `{{box.branch}}`, `{{box.repo}}`, `{{box.id}}`,
//! `{{sandbox}}` and `{{hub.host}}`. A header value takes exactly one
//! `{{secret:NAME}}` (a secret of `secrets.yaml`, or the tunnel's) and nothing
//! else, so no literal token can reach a policy. Whatever a rule could be read
//! as in two ways is refused: an unknown key (a `method:` would allow every
//! method), a placeholder whose value would widen the rule (`*`, `..`, an
//! empty value). Errors name the profile, the rule and the placeholder,
//! never a value.

use anyhow::{Result, anyhow, bail, ensure};
use yaml_rust2::{Yaml, YamlLoader};

use super::Vars;
use crate::vm::pack::Pack;
use crate::vm::provider::{EgressPolicy, EgressRule, HeaderFromSecret};

const RULE_KEYS: [&str; 6] = ["name", "host", "path", "methods", "action", "headers"];
/// Headers a transform may not set: they frame the request or belong to one hop.
const FRAMING: [&str; 10] = ["host", "content-length", "transfer-encoding", "connection", "keep-alive", "proxy-connection", "proxy-authorization", "te", "trailer", "upgrade"];
const PLACEHOLDERS: &str = "box.branch, box.repo, box.id, sandbox, hub.host";

/// The host of the pack's `tunnel.url` (what `{{hub.host}}` is when the
/// caller leaves [`Vars::hub_host`] empty).
pub fn hub_host(pack: &Pack) -> Option<String> {
    let url = pack.tunnel.url.trim();
    if url.is_empty() {
        return None;
    }
    crate::vm::satellite::dial::parse(url).ok().map(|target| target.host.to_ascii_lowercase())
}

fn text(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

fn missing(y: &Yaml) -> bool {
    matches!(y, Yaml::BadValue | Yaml::Null)
}

/// A key or placeholder of the pack as an error shows it: only when it looks
/// like a name (it could be a value written in the wrong place).
fn shown(name: &str) -> String {
    if !name.is_empty() && name.len() <= 40 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':')) { name.to_owned() } else { "(not a name)".to_owned() }
}

fn plain_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub(super) fn render(pack: &Pack, profile: &str, vars: &Vars) -> Result<EgressPolicy> {
    let text_of = pack.egress.get(profile).ok_or_else(|| anyhow!("egress profile {}: the pack has no egress/{}.yaml", shown(profile), shown(profile)))?;
    let docs = YamlLoader::load_from_str(text_of).map_err(|error| anyhow!("egress profile {profile}: not YAML (line {})", error.marker().line() + 1))?;
    let yaml = docs.into_iter().next().unwrap_or(Yaml::Null);
    match &yaml {
        Yaml::Hash(map) => {
            for key in map.keys() {
                let key = text(key).unwrap_or_default();
                ensure!(matches!(key.as_str(), "default" | "rules"), "egress profile {profile}: unknown key {} (default, rules)", shown(&key));
            }
        }
        Yaml::Null => {}
        _ => bail!("egress profile {profile}: not a map of default and rules"),
    }
    let default_action = match &yaml["default"] {
        y if missing(y) => "Deny",
        y => match text(y).map(|t| t.trim().to_ascii_lowercase()).as_deref() {
            Some("deny") => "Deny",
            Some("allow") => "Allow",
            _ => bail!("egress profile {profile}: default is deny or allow"),
        },
    };
    let hub = if vars.hub_host.is_empty() { hub_host(pack).unwrap_or_default() } else { vars.hub_host.clone() };
    let items = match &yaml["rules"] {
        Yaml::Array(items) => items.clone(),
        y if missing(y) => Vec::new(),
        _ => bail!("egress profile {profile}: rules is a list"),
    };
    let mut rules = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let named = text(&item["name"]).filter(|n| plain_name(n.trim())).map(|n| format!(" ({})", n.trim())).unwrap_or_default();
        let rule = render_rule(pack, profile, index, item, vars, &hub).map_err(|error| anyhow!("egress profile {profile}, rule {}{named}: {error}", index + 1))?;
        ensure!(!rules.iter().any(|r: &EgressRule| r.name == rule.name), "egress profile {profile}, rule {}{named}: another rule has the name {}", index + 1, rule.name);
        rules.push(rule);
    }
    Ok(EgressPolicy { default_action: default_action.into(), rules })
}

fn render_rule(pack: &Pack, profile: &str, index: usize, item: &Yaml, vars: &Vars, hub: &str) -> Result<EgressRule> {
    let Yaml::Hash(map) = item else { bail!("not a map of host, path, methods, action and headers") };
    for key in map.keys() {
        let key = text(key).unwrap_or_default();
        ensure!(RULE_KEYS.contains(&key.as_str()), "unknown key {} ({})", shown(&key), RULE_KEYS.join(", "));
    }
    let name = match &item["name"] {
        y if missing(y) => {
            if plain_name(profile) && profile.len() < 56 {
                format!("{profile}-{}", index + 1)
            } else {
                format!("rule-{}", index + 1)
            }
        }
        y => text(y).map(|n| n.trim().to_owned()).filter(|n| plain_name(n)).ok_or_else(|| anyhow!("name: letters, digits, '.', '-' and '_' (64 at most)"))?,
    };
    let host = text(&item["host"]).map(|h| h.trim().to_owned()).filter(|h| !h.is_empty()).ok_or_else(|| anyhow!("no host"))?;
    let host = fill(&host, Place::Host, vars, hub)?.to_ascii_lowercase();
    ensure!(host_ok(&host), "host is a name, *.domain or *, with an optional :port");
    let path = match &item["path"] {
        y if missing(y) => None,
        y => {
            let path = text(y).map(|p| p.trim().to_owned()).ok_or_else(|| anyhow!("path is text"))?;
            let path = fill(&path, Place::Path, vars, hub)?;
            ensure!(path_ok(&path), "path starts with / and has no spaces, '?', '#', ';', '\\', '.' or '..' segments or broken %-escapes");
            Some(path)
        }
    };
    let words = match &item["methods"] {
        Yaml::Array(items) => items.iter().map(|m| text(m).unwrap_or_default()).collect(),
        y if missing(y) => Vec::new(),
        y => vec![text(y).unwrap_or_default()],
    };
    let mut methods: Vec<String> = Vec::new();
    for method in words {
        let method = method.trim().to_ascii_uppercase();
        ensure!(!method.is_empty() && method.len() <= 20 && method.chars().all(|c| c.is_ascii_alphabetic()), "methods are words such as GET or POST");
        if !methods.contains(&method) {
            methods.push(method);
        }
    }
    let action = match text(&item["action"]).map(|a| a.trim().to_ascii_lowercase()).as_deref() {
        None if missing(&item["action"]) => "Allow",
        Some("allow") => "Allow",
        Some("deny") => "Deny",
        Some("transform") => "Transform",
        _ => bail!("action is allow, deny or transform"),
    };
    let headers = match &item["headers"] {
        Yaml::Array(items) => items.iter().map(|h| header(pack, h)).collect::<Result<Vec<_>>>()?,
        y if missing(y) => Vec::new(),
        _ => bail!("headers is a list of name and value"),
    };
    if action == "Transform" {
        ensure!(!headers.is_empty(), "a transform sets at least one header");
    } else {
        ensure!(headers.is_empty(), "only a transform sets headers");
    }
    for (i, h) in headers.iter().enumerate() {
        ensure!(!headers[..i].iter().any(|o| o.name.eq_ignore_ascii_case(&h.name)), "header {} is set twice", h.name);
    }
    Ok(EgressRule { name, host, path, methods, action: action.into(), headers })
}

fn header(pack: &Pack, item: &Yaml) -> Result<HeaderFromSecret> {
    let Yaml::Hash(map) = item else { bail!("every header is a map of name and value") };
    for key in map.keys() {
        let key = text(key).unwrap_or_default();
        ensure!(matches!(key.as_str(), "name" | "value"), "a header has a name and a value, not {}", shown(&key));
    }
    let name = text(&item["name"]).map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).ok_or_else(|| anyhow!("a header without a name"))?;
    ensure!(name.len() <= 64 && name.bytes().all(token_byte), "header name {} is not a header name", shown(&name));
    ensure!(!FRAMING.contains(&name.to_ascii_lowercase().as_str()), "header {name} frames the request or belongs to one hop: a transform cannot set it");
    let value = text(&item["value"]).ok_or_else(|| anyhow!("header {name}: no value (\"Bearer {{{{secret:NAME}}}}\")"))?;
    let (secret, format) = secret_format(&value).map_err(|error| anyhow!("header {name}: {error}"))?;
    let key = secret_key(pack, &secret).ok_or_else(|| anyhow!("header {name}: {{{{secret:{secret}}}}} is not in secrets.yaml"))?;
    Ok(HeaderFromSecret { name, secret, key, format })
}

/// A `tchar` of RFC 9110 (a header name's characters).
pub(super) fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The key of secret `name`: from `secrets.yaml`, or `value` for the tunnel's.
fn secret_key(pack: &Pack, name: &str) -> Option<String> {
    pack.secrets.iter().find(|s| s.name == name).map(|s| s.key.clone()).or_else(|| (!pack.tunnel.secret.is_empty() && pack.tunnel.secret == name).then(|| "value".to_owned()))
}

/// A header value's one secret and its format (`Bearer {}`).
fn secret_format(value: &str) -> Result<(String, String)> {
    let literal = |text: &str, format: &mut String| -> Result<()> {
        ensure!(!text.contains(['{', '}']) && !text.chars().any(char::is_control), "the text around {{{{secret:NAME}}}} has no braces or control characters");
        format.push_str(text);
        Ok(())
    };
    let (mut secret, mut format, mut rest) = (None, String::new(), value);
    while let Some(start) = rest.find("{{") {
        literal(&rest[..start], &mut format)?;
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| anyhow!("a {{{{ without }}}}"))?;
        let inner = after[..end].trim();
        match inner.strip_prefix("secret:").map(str::trim) {
            Some(name) if secret.is_none() => {
                ensure!(plain_name(name), "{{{{secret:NAME}}}} names a secret of secrets.yaml");
                secret = Some(name.to_owned());
                format.push_str("{}");
            }
            Some(_) => bail!("one {{{{secret:NAME}}}} per header"),
            None => bail!("{{{{{}}}}} in a header value: header values take only {{{{secret:NAME}}}}", shown(inner)),
        }
        rest = &after[end + 2..];
    }
    literal(rest, &mut format)?;
    let secret = secret.ok_or_else(|| anyhow!("its value is written in the pack: header values come from {{{{secret:NAME}}}} (a literal value would reach the provider's policy in the clear)"))?;
    Ok((secret, format))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Host,
    Path,
}

/// `text` with its placeholders filled for a host or a path.
fn fill(text: &str, place: Place, vars: &Vars, hub: &str) -> Result<String> {
    let (mut out, mut rest) = (String::new(), text);
    let what = if place == Place::Host { "host" } else { "path" };
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| anyhow!("a {{{{ without }}}} in the {what}"))?;
        let inner = after[..end].trim();
        let value = match inner {
            "box.branch" => &vars.branch,
            "box.repo" => &vars.repo,
            "box.id" => &vars.r#box,
            "sandbox" => &vars.sandbox,
            "hub.host" => hub,
            _ if inner.starts_with("secret:") => bail!("{{{{secret:…}}}} is only for header values, not a {what}"),
            _ => bail!("{{{{{}}}}} is not a placeholder ({PLACEHOLDERS})", shown(inner)),
        };
        ensure!(!value.is_empty(), "{{{{{inner}}}}} has no value for this box");
        ensure!(fits(value, place), "the value of {{{{{inner}}}}} cannot be part of a {what}");
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Whether a placeholder's value stays one plain part of a host or a path
/// (never a wildcard, an escape, a `..` or an empty segment).
fn fits(value: &str, place: Place) -> bool {
    match place {
        Place::Host => value.len() <= 253 && value.split('.').all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))),
        Place::Path => {
            value.len() <= 256
                && value.chars().all(|c| c.is_ascii_graphic() && !matches!(c, '*' | '?' | '#' | '%' | '\\' | ';' | '{' | '}' | '"' | '<' | '>' | '^' | '`' | '|'))
                && value.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
        }
    }
}

fn port_ok(port: &str) -> bool {
    !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()) && port.parse::<u16>().is_ok_and(|p| p != 0)
}

/// `*`, `name`, `*.domain`, `[v6]`, each with an optional `:port`.
fn host_ok(host: &str) -> bool {
    if host == "*" {
        return true;
    }
    if let Some(rest) = host.strip_prefix('[') {
        let Some((v6, after)) = rest.split_once(']') else { return false };
        return v6.parse::<std::net::Ipv6Addr>().is_ok() && (after.is_empty() || after.strip_prefix(':').is_some_and(port_ok));
    }
    let (name, port) = match host.rsplit_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let name = name.strip_prefix("*.").unwrap_or(name);
    port.is_none_or(port_ok) && !name.is_empty() && name.len() <= 253 && name.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
}

fn path_ok(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 1024
        && path.chars().all(|c| c.is_ascii_graphic() && !matches!(c, '?' | '#' | ';' | '\\' | '"' | '<' | '>' | '{' | '}' | '|' | '^' | '`'))
        && !path.split('/').any(|s| s == "." || s == "..")
        && super::rules::decode_unreserved(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::pack::{SecretSpec, Tunnel};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn vars() -> Vars {
        Vars { branch: "atelier/dec-31-fix".into(), repo: "contoso-app".into(), r#box: "box-7f3a".into(), sandbox: "sbx-0123456789ab".into(), hub_host: String::new() }
    }

    fn pack(profile: &str) -> Pack {
        Pack {
            egress: BTreeMap::from([("p".to_owned(), profile.to_owned())]),
            secrets: vec![SecretSpec { name: "forge-token".into(), command: "example-cli auth token".into(), key: "token".into(), every_ms: 60_000 }],
            tunnel: Tunnel { url: "wss://contoso-hub-48600.example.devtunnels.ms".into(), secret: "tunnel-token".into() },
            ..Default::default()
        }
    }

    fn error(profile: &str) -> String {
        format!("{:#}", render(&pack(profile), "p", &vars()).unwrap_err())
    }

    #[test]
    fn the_example_profile_renders() {
        let folder = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/vm-pack");
        let (pack, _) = crate::vm::pack::load(&folder);
        let policy = render(&pack, "forge", &vars()).unwrap();
        let header = |name: &str, secret: &str, format: &str| HeaderFromSecret { name: name.into(), secret: secret.into(), key: "value".into(), format: format.into() };
        assert_eq!(
            policy,
            EgressPolicy {
                default_action: "Deny".into(),
                rules: vec![
                    EgressRule { name: "forge-1".into(), host: "example.com".into(), path: Some("/contoso/app.git/*".into()), methods: vec!["GET".into(), "POST".into()], action: "Allow".into(), headers: vec![] },
                    EgressRule { name: "forge-2".into(), host: "api.example.com".into(), path: Some("/repos/contoso/app/*".into()), methods: vec![], action: "Transform".into(), headers: vec![header("Authorization", "forge-token", "Bearer {}")] },
                    EgressRule {
                        name: "forge-3".into(),
                        host: "contoso-hub-48600.example.devtunnels.ms".into(),
                        path: None,
                        methods: vec![],
                        action: "Transform".into(),
                        headers: vec![header("X-Tunnel-Authorization", "tunnel-token", "tunnel {}")],
                    },
                ],
            }
        );
        // What the platform is sent: secrets by reference only.
        let body = crate::vm::aca::model::egress_body(&policy).unwrap().to_string();
        assert!(body.contains("\"secretId\":\"forge-token\"") && !body.contains("example-cli"), "{body}");
        assert!(render(&pack, "nope", &vars()).unwrap_err().to_string().contains("no egress/nope.yaml"));
    }

    #[test]
    fn placeholders_are_filled_in_hosts_and_paths() {
        let profile = "default: allow\nrules:\n  - name: branch-only\n    host: api.example.com:8443\n    path: /repos/{{box.repo}}/branches/{{ box.branch }}/*\n    methods: get\n    action: deny\n  - host: '{{sandbox}}.preview.example.com'\n    path: /boxes/{{box.id}}\n  - host: '{{hub.host}}'\n    action: Transform\n    headers:\n      - name: X-Tunnel-Authorization\n        value: 'tunnel {{secret:tunnel-token}}'\n  - host: '*.CDN.example.com'\n";
        let policy = render(&pack(profile), "p", &vars()).unwrap();
        assert_eq!(policy.default_action, "Allow");
        let rule = &policy.rules[0];
        assert_eq!((rule.name.as_str(), rule.host.as_str(), rule.path.as_deref(), rule.methods.as_slice(), rule.action.as_str()), ("branch-only", "api.example.com:8443", Some("/repos/contoso-app/branches/atelier/dec-31-fix/*"), &["GET".to_owned()][..], "Deny"));
        assert_eq!((policy.rules[1].name.as_str(), policy.rules[1].host.as_str(), policy.rules[1].path.as_deref()), ("p-2", "sbx-0123456789ab.preview.example.com", Some("/boxes/box-7f3a")));
        assert_eq!(policy.rules[2].host, "contoso-hub-48600.example.devtunnels.ms", "the tunnel's host when the caller gives none");
        assert_eq!(policy.rules[2].headers, vec![HeaderFromSecret { name: "X-Tunnel-Authorization".into(), secret: "tunnel-token".into(), key: "value".into(), format: "tunnel {}".into() }]);
        assert_eq!(policy.rules[3].host, "*.cdn.example.com");
        let given = Vars { hub_host: "hub.example.com".into(), ..vars() };
        assert_eq!(render(&pack(profile), "p", &given).unwrap().rules[2].host, "hub.example.com");
        assert_eq!(render(&pack(""), "p", &vars()).unwrap(), EgressPolicy { default_action: "Deny".into(), rules: vec![] }, "an empty profile denies everything");
        let forge = render(&pack("rules:\n  - host: api.example.com\n    action: transform\n    headers:\n      - name: Authorization\n        value: '{{secret:forge-token}}'\n"), "p", &vars()).unwrap();
        assert_eq!((forge.rules[0].headers[0].key.as_str(), forge.rules[0].headers[0].format.as_str()), ("token", "{}"), "the key of secrets.yaml; the value alone");
    }

    #[test]
    fn every_error_names_the_profile_and_rule_never_a_value() {
        let rule = |body: &str| format!("rules:\n  - host: example.com\n  - name: second\n{body}");
        let cases: &[(&str, String)] = &[
            ("unknown key method", "default: deny\nrules:\n  - host: example.com\n    method: GET\n".into()),
            ("unknown key Bearer", "rules: []\nBearer: 1\n".into()),
            ("default is deny or allow", "default: maybe\n".into()),
            ("rules is a list", "rules: example.com\n".into()),
            ("not YAML (line", "rules: [\n".into()),
            ("not a map", "- host: x\n".into()),
            ("rule 1: not a map", "rules:\n  - example.com\n".into()),
            ("rule 2 (second): no host", rule("    path: /x\n")),
            ("rule 2 (second): {{box.nope}} is not a placeholder", rule("    host: '{{box.nope}}.example.com'\n")),
            ("rule 2 (second): {{(not a name)}} is not a placeholder", rule("    host: example.com\n    path: '/{{Bearer s3cret-literal}}'\n")),
            ("{{secret:…}} is only for header values, not a host", rule("    host: '{{secret:forge-token}}.example.com'\n")),
            ("{{secret:…}} is only for header values, not a path", rule("    host: example.com\n    path: /x/{{secret:forge-token}}\n")),
            ("a {{ without }} in the path", rule("    host: example.com\n    path: /x/{{box.id\n")),
            ("the value of {{box.branch}} cannot be part of a host", rule("    host: '{{box.branch}}.example.com'\n")),
            ("host is a name", rule("    host: 'a.*.example.com'\n")),
            ("host is a name", rule("    host: 'example.com/x'\n")),
            ("path starts with /", rule("    host: example.com\n    path: x/y\n")),
            ("path starts with /", rule("    host: example.com\n    path: /x/../y\n")),
            ("path starts with /", rule("    host: example.com\n    path: /x%zz\n")),
            ("methods are words", rule("    host: example.com\n    methods: [GET, 'P OST']\n")),
            ("action is allow, deny or transform", rule("    host: example.com\n    action: rewrite\n")),
            ("a transform sets at least one header", rule("    host: example.com\n    action: transform\n")),
            ("only a transform sets headers", rule("    host: example.com\n    headers:\n      - name: Authorization\n        value: 'Bearer {{secret:forge-token}}'\n")),
            ("header Authorization: its value is written in the pack", rule("    host: example.com\n    action: transform\n    headers:\n      - name: Authorization\n        value: Bearer s3cret-literal\n")),
            ("header Authorization: one {{secret:NAME}} per header", rule("    host: example.com\n    action: transform\n    headers:\n      - name: Authorization\n        value: '{{secret:forge-token}}{{secret:tunnel-token}}'\n")),
            ("header Authorization: {{box.id}} in a header value", rule("    host: example.com\n    action: transform\n    headers:\n      - name: Authorization\n        value: '{{box.id}} {{secret:forge-token}}'\n")),
            ("header Authorization: the text around", rule("    host: example.com\n    action: transform\n    headers:\n      - name: Authorization\n        value: 'Bearer {} {{secret:forge-token}}'\n")),
            ("header Authorization: {{secret:nope}} is not in secrets.yaml", rule("    host: example.com\n    action: transform\n    headers:\n      - name: Authorization\n        value: 'Bearer {{secret:nope}}'\n")),
            ("header Content-Length frames the request", rule("    host: example.com\n    action: transform\n    headers:\n      - name: Content-Length\n        value: '{{secret:forge-token}}'\n")),
            ("header name (not a name) is not a header name", rule("    host: example.com\n    action: transform\n    headers:\n      - name: \"X-A\\r\\nX-B\"\n        value: '{{secret:forge-token}}'\n")),
            ("header x-a is set twice", rule("    host: example.com\n    action: transform\n    headers:\n      - name: X-A\n        value: '{{secret:forge-token}}'\n      - name: x-a\n        value: '{{secret:tunnel-token}}'\n")),
            ("another rule has the name p-1", "rules:\n  - host: example.com\n  - name: p-1\n    host: example.org\n".into()),
            ("name: letters", rule("    host: example.com\n").replace("name: second", "name: 'two words'")),
        ];
        for (want, profile) in cases {
            let error = error(profile);
            assert!(error.starts_with("egress profile p") && error.contains(want), "{want:?} in {error:?}");
            assert!(!error.contains("s3cret") && !error.contains("example-cli"), "never a value: {error}");
        }
        // A placeholder without a value, or one that would widen a path.
        let path = "rules:\n  - host: example.com\n    path: /b/{{box.branch}}/*\n";
        let empty = Vars { branch: String::new(), ..vars() };
        assert!(format!("{:#}", render(&pack(path), "p", &empty).unwrap_err()).contains("{{box.branch}} has no value"));
        for branch in ["a*b", "../main", "a//b", "x%2F", "sp ace", "s3cret?"] {
            let wide = Vars { branch: branch.into(), ..vars() };
            let error = format!("{:#}", render(&pack(path), "p", &wide).unwrap_err());
            assert!(error.contains("rule 1: the value of {{box.branch}} cannot be part of a path") && !error.contains(branch), "{branch}: {error}");
        }
        let no_hub = Pack { tunnel: Tunnel::default(), ..pack("rules:\n  - host: '{{hub.host}}'\n") };
        assert!(format!("{:#}", render(&no_hub, "p", &vars()).unwrap_err()).contains("{{hub.host}} has no value"));
    }

    #[test]
    fn hosts_and_paths_checked() {
        for host in ["*", "example.com", "*.example.com", "example.com:8443", "[::1]", "[::1]:80", "127.0.0.1", "a_b.example.com"] {
            assert!(host_ok(host), "{host}");
        }
        for host in ["", "*.", "**.example.com", "a.*.com", "example.com:", "example.com:0", "example.com:99999", "[::1", "[nope]", "exa mple.com", "a..b", ".example.com"] {
            assert!(!host_ok(host), "{host}");
        }
        assert!(path_ok("/a/*/b") && path_ok("/") && path_ok("/a%20b"));
        assert!(!path_ok("/a b") && !path_ok("/a/./b") && !path_ok("/a?x") && !path_ok("/a%2fb"));
        assert_eq!(shown("box.nope"), "box.nope");
        assert_eq!(shown("Bearer abc"), "(not a name)");
    }
}
