//! The effects collection: named canvas effects a session (usually a group's
//! passive session) puts on a box or a group with `copilot-canvas effect`,
//! e.g. fireworks when a pull request is approved. The service keeps them
//! (in the bus state) until they are acknowledged with a click or their
//! kind's time runs out, so every client shows them and they survive client
//! restarts. Clients draw them as animations, static rings or badges
//! (Settings › Effects).

use serde::{Deserialize, Serialize};

use crate::bus::Routing;

/// The bus event `copilot-canvas effect` sends (`data`: effect, on, note, kind).
pub const EVENT: &str = "canvas.effect";
/// The bus event a click sends to acknowledge effects (`data.ids`).
pub const ACK: &str = "canvas.effect_ack";
/// Effects kept at most (the oldest go first).
pub const MAX_EFFECTS: usize = 50;
/// Longest note shown with an effect (characters).
pub const MAX_NOTE: usize = 80;
const WEEK_MS: u64 = 7 * 86_400_000;

/// One kind of effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectKind {
    pub name: &'static str,
    /// Badge glyph (the static form, also in the TUI).
    pub glyph: &'static str,
    /// Badge text when the effect has no note.
    pub label: &'static str,
    /// Runs until clicked (else it ends by itself after `ttl_ms`).
    pub until_ack: bool,
    /// Longest life (also for those that wait for a click).
    pub ttl_ms: u64,
    pub about: &'static str,
}

/// The registry, in the order `copilot-canvas effect` lists them.
pub const KINDS: [EffectKind; 4] = [
    EffectKind { name: "fireworks", glyph: "✦", label: "approved", until_ack: true, ttl_ms: WEEK_MS, about: "a celebration (a pull request approved); runs until clicked" },
    EffectKind {
        name: "attention",
        glyph: "◉",
        label: "attention",
        until_ack: true,
        ttl_ms: WEEK_MS,
        about: "a pulsing beacon for pull-request attention (review requested, changes requested, comments); runs until clicked",
    },
    EffectKind { name: "sparkle", glyph: "✧", label: "done", until_ack: false, ttl_ms: 6_000, about: "a short sparkle for a success; ends by itself" },
    EffectKind { name: "alert", glyph: "▲", label: "alert", until_ack: true, ttl_ms: WEEK_MS, about: "a warning flash for something that went wrong; runs until clicked" },
];

/// A kind by name (case-insensitive).
pub fn kind(name: &str) -> Option<&'static EffectKind> {
    KINDS.iter().find(|k| k.name.eq_ignore_ascii_case(name.trim()))
}

/// An effect on the canvas.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Effect {
    pub id: String,
    /// [`EffectKind::name`].
    pub kind: String,
    pub group: String,
    /// The box it is on (a session or passive session id); empty: the group.
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub note: String,
    /// Who put it there.
    #[serde(default)]
    pub from: String,
    pub ms: u64,
    pub until_ms: u64,
}

impl Effect {
    pub fn spec(&self) -> Option<&'static EffectKind> {
        kind(&self.kind)
    }

    /// The badge: glyph and note (or the kind's label).
    pub fn badge(&self) -> String {
        let spec = self.spec();
        let glyph = spec.map(|k| k.glyph).unwrap_or("✦");
        let text = if self.note.is_empty() { spec.map(|k| k.label).unwrap_or("effect").to_owned() } else { self.note.clone() };
        format!("{glyph} {text}")
    }

    pub fn alive(&self, now: u64) -> bool {
        now < self.until_ms
    }
}

/// Event data is untrusted: one line of printable text, at most [`MAX_NOTE`] characters.
pub fn clean_note(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let words = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.chars().count() > MAX_NOTE { format!("{}…", words.chars().take(MAX_NOTE - 1).collect::<String>()) } else { words }
}

/// Where an effect goes: a box of the group or the group itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anchor {
    Node(String),
    Group,
}

/// Resolves `--on NAME` strictly inside `group`: a passive session or a
/// session of the group (name or id), or the group itself (its name, `group`).
pub fn resolve(routing: &Routing, group: &str, on: &str) -> Option<Anchor> {
    let on = on.trim();
    if on.is_empty() || group.is_empty() {
        return None;
    }
    if let Some(p) = routing.passive.iter().find(|p| p.group == group && (p.key == on || p.session_id == on || p.name.eq_ignore_ascii_case(on))) {
        return Some(Anchor::Node(p.session_id.clone()));
    }
    let member = |id: &str| routing.members.get(id).is_some_and(|g| g == group);
    if let Some((id, _)) = routing.names.iter().find(|(id, name)| member(id) && (id.as_str() == on || name.eq_ignore_ascii_case(on))) {
        return Some(Anchor::Node(id.clone()));
    }
    let named = routing.group(group).is_some_and(|g| g.name.eq_ignore_ascii_case(on));
    (named || on.eq_ignore_ascii_case("group")).then_some(Anchor::Group)
}

/// Resolves `--on NAME` for a canvas-wide agent: any session or passive
/// session box of the canvas (id first, then name). Answers the box's group
/// (empty for an ungrouped session) and the box; groups themselves are not
/// named this way.
pub fn resolve_canvas(routing: &Routing, on: &str) -> Option<(String, Anchor)> {
    let on = on.trim();
    if on.is_empty() {
        return None;
    }
    let passive = |exact: bool| routing.passive.iter().find(|p| if exact { p.key == on || p.session_id == on } else { p.name.eq_ignore_ascii_case(on) });
    let session = |exact: bool| routing.names.iter().find(|(id, name)| routing.passive_by_session(id).is_none() && if exact { id.as_str() == on } else { name.eq_ignore_ascii_case(on) });
    for exact in [true, false] {
        if let Some((id, _)) = session(exact) {
            return Some((routing.members.get(id).cloned().unwrap_or_default(), Anchor::Node(id.clone())));
        }
        if let Some(p) = passive(exact) {
            return Some((p.group.clone(), Anchor::Node(p.session_id.clone())));
        }
    }
    None
}

/// Adds an effect (the same kind on the same anchor is replaced, so it
/// starts again), keeping at most [`MAX_EFFECTS`].
pub fn add(effects: &mut Vec<Effect>, effect: Effect) {
    effects.retain(|e| !(e.kind == effect.kind && e.group == effect.group && e.target == effect.target));
    effects.push(effect);
    while effects.len() > MAX_EFFECTS {
        effects.remove(0);
    }
}

/// Removes the effects on an anchor (`kind`: only that kind); answers how many.
pub fn clear(effects: &mut Vec<Effect>, group: &str, target: &str, kind: Option<&str>) -> usize {
    let before = effects.len();
    effects.retain(|e| !(e.group == group && e.target == target && kind.is_none_or(|k| e.kind == k)));
    before - effects.len()
}

/// A new effect of `kind` (a registry name) from an event at `now`.
pub fn new_effect(spec: &EffectKind, group: &str, anchor: &Anchor, note: &str, from: &str, now: u64) -> Effect {
    Effect {
        id: uuid::Uuid::new_v4().to_string(),
        kind: spec.name.to_owned(),
        group: group.to_owned(),
        target: match anchor {
            Anchor::Node(id) => id.clone(),
            Anchor::Group => String::new(),
        },
        note: clean_note(note),
        from: clean_note(from),
        ms: now,
        until_ms: now + spec.ttl_ms,
    }
}

/// The `effects:` section of a group's event catalog: event type → effect
/// kind (unknown kinds are left out).
pub fn catalog_effects(text: &str) -> Vec<(String, &'static EffectKind)> {
    let Ok(docs) = yaml_rust2::YamlLoader::load_from_str(text) else { return Vec::new() };
    let Some(map) = docs.first().and_then(|d| d["effects"].as_hash()) else { return Vec::new() };
    map.iter().filter_map(|(k, v)| Some((k.as_str()?.trim().to_owned(), kind(v.as_str()?)?))).collect()
}

/// The effect a catalog maps an event type to (exact names or `prefix.*`).
pub fn effect_for_event<'a>(mapping: &'a [(String, &'static EffectKind)], event: &str) -> Option<&'a (String, &'static EffectKind)> {
    mapping.iter().find(|(pattern, _)| pattern.eq_ignore_ascii_case(event) || pattern.strip_suffix(".*").is_some_and(|p| event.starts_with(&format!("{p}."))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{PassiveSpec, RouteGroup};

    fn routing() -> Routing {
        Routing {
            groups: vec![RouteGroup { id: "g1".into(), name: "Payments".into(), folder: None }, RouteGroup { id: "g2".into(), name: "Other".into(), folder: None }],
            members: [("s1".to_owned(), "g1".to_owned()), ("s2".to_owned(), "g2".to_owned())].into(),
            names: [("s1".to_owned(), "Fix flaky test".to_owned()), ("s2".to_owned(), "Elsewhere".to_owned()), ("p1".to_owned(), "Rollout".to_owned())].into(),
            passive: vec![PassiveSpec { key: "k1".into(), session_id: "p1".into(), name: "Rollout".into(), group: "g1".into(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn the_registry_knows_its_kinds() {
        assert_eq!(KINDS.iter().map(|k| k.name).collect::<Vec<_>>(), ["fireworks", "attention", "sparkle", "alert"]);
        assert!(kind("Fireworks").is_some_and(|k| k.until_ack) && kind("sparkle").is_some_and(|k| !k.until_ack));
        assert_eq!(kind("confetti"), None);
    }

    #[test]
    fn targets_resolve_only_inside_the_senders_group() {
        let r = routing();
        assert_eq!(resolve(&r, "g1", "fix FLAKY test"), Some(Anchor::Node("s1".into())));
        assert_eq!(resolve(&r, "g1", "s1"), Some(Anchor::Node("s1".into())));
        assert_eq!(resolve(&r, "g1", "Rollout"), Some(Anchor::Node("p1".into())), "passive sessions by name");
        assert_eq!(resolve(&r, "g1", "payments"), Some(Anchor::Group));
        assert_eq!(resolve(&r, "g1", "group"), Some(Anchor::Group));
        assert_eq!(resolve(&r, "g1", "Elsewhere"), None, "a session of another group");
        assert_eq!(resolve(&r, "g1", "s2"), None);
        assert_eq!(resolve(&r, "", "s1"), None, "no group, no effects");
        assert_eq!(resolve(&r, "g1", " "), None);
    }

    #[test]
    fn canvas_wide_agents_reach_boxes_of_every_group() {
        let r = routing();
        assert_eq!(resolve_canvas(&r, "Elsewhere"), Some(("g2".into(), Anchor::Node("s2".into()))), "another group's session");
        assert_eq!(resolve_canvas(&r, "s1"), Some(("g1".into(), Anchor::Node("s1".into()))));
        assert_eq!(resolve_canvas(&r, "rollout"), Some(("g1".into(), Anchor::Node("p1".into()))), "passive sessions by name");
        assert_eq!(resolve_canvas(&r, "payments"), None, "groups are not boxes");
        assert_eq!(resolve_canvas(&r, "nobody"), None);
    }

    #[test]
    fn effects_replace_clear_expire_and_stay_bounded() {
        let mut effects = Vec::new();
        let fireworks = kind("fireworks").unwrap();
        let first = new_effect(fireworks, "g1", &Anchor::Node("s1".into()), "PR !12 approved\n\x1b[31m", "Rollout", 1_000);
        assert_eq!(first.note, "PR !12 approved [31m", "one printable line");
        assert_eq!(first.badge(), "✦ PR !12 approved [31m");
        assert!(first.alive(1_000) && !first.alive(1_000 + WEEK_MS));
        add(&mut effects, first.clone());
        add(&mut effects, new_effect(fireworks, "g1", &Anchor::Node("s1".into()), "", "Rollout", 2_000));
        assert_eq!(effects.len(), 1, "the same kind on the same box starts again");
        assert_eq!(effects[0].badge(), "✦ approved");
        add(&mut effects, new_effect(kind("attention").unwrap(), "g1", &Anchor::Node("s1".into()), "2 comments", "", 2_000));
        add(&mut effects, new_effect(kind("sparkle").unwrap(), "g1", &Anchor::Group, "", "", 2_000));
        assert_eq!(effects.len(), 3);
        assert!(!effects[2].alive(2_000 + 6_000), "a sparkle ends by itself");
        assert_eq!(clear(&mut effects, "g1", "s1", Some("attention")), 1);
        assert_eq!(clear(&mut effects, "g1", "s1", None), 1);
        assert_eq!(effects.len(), 1);
        let long = "x".repeat(200);
        assert_eq!(clean_note(&long).chars().count(), MAX_NOTE);
        for i in 0..(MAX_EFFECTS + 5) {
            add(&mut effects, new_effect(fireworks, "g1", &Anchor::Node(format!("n{i}")), "", "", 3_000));
        }
        assert_eq!(effects.len(), MAX_EFFECTS);
    }

    #[test]
    fn catalogs_map_events_to_effects() {
        let text = "events:\n  - name: pr.approved\neffects:\n  pr.approved: fireworks\n  pr.changes_requested: Attention\n  build.*: alert\n  pr.odd: confetti\n";
        let mapping = catalog_effects(text);
        assert_eq!(mapping.len(), 3, "unknown kinds are left out");
        assert_eq!(effect_for_event(&mapping, "pr.approved").map(|(_, k)| k.name), Some("fireworks"));
        assert_eq!(effect_for_event(&mapping, "pr.changes_requested").map(|(_, k)| k.name), Some("attention"));
        assert_eq!(effect_for_event(&mapping, "build.failed").map(|(_, k)| k.name), Some("alert"));
        assert_eq!(effect_for_event(&mapping, "pr.merged"), None);
        assert!(catalog_effects("events: []").is_empty() && catalog_effects("{{broken").is_empty());
    }
}
