//! The shared canvas document. The service owns it and is its single writer;
//! clients send edits and apply the patches it broadcasts.
//!
//! An edit is a list of operations ([`Op`]): a node or group created, deleted
//! or some of its fields set, or one shared top-level field set. The service
//! applies edits in arrival order, so the last edit wins per field (and a
//! delete wins over concurrent edits of the deleted object): two
//! windows moving and renaming the same box both keep their change; two
//! windows renaming it keep the later name. View and UI preferences (`view`,
//! `ui`, `tiles`, `selected`) belong to each client ([`ClientPrefs`]) and are
//! never patched.
//!
//! A client keeps a [`Replica`]: the service's state (`synced`), that state
//! plus its own edits in flight (`shadow`), next to its working copy. Local
//! changes are `diff(shadow, doc)`. A patch from another window is applied
//! to the working copy except for fields this client changed itself and the
//! service has not applied yet (those land later, so they win there too).
//! Every client sees the same patches in the same order, so all working
//! copies converge on the service's document.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::doc::{Doc, Group, Node, UiPrefs, View};

/// Own edits younger than this make an overwrite by another window worth a notice.
const RECENT: Duration = Duration::from_secs(30);

macro_rules! shared_fields {
    ($($name:ident),* $(,)?) => {
        /// Top-level fields every client shares (patched with `Op::SetField`).
        pub const SHARED_FIELDS: &[&str] = &[$(stringify!($name)),*];

        fn same_field(a: &Doc, b: &Doc, name: &str) -> bool {
            match name {
                $(stringify!($name) => a.$name == b.$name,)*
                _ => true,
            }
        }

        /// A shared field as JSON (`None`: not a shared field).
        pub fn field(doc: &Doc, name: &str) -> Option<Value> {
            match name {
                $(stringify!($name) => serde_json::to_value(&doc.$name).ok(),)*
                _ => None,
            }
        }

        /// Sets a shared field from JSON.
        pub fn set_field(doc: &mut Doc, name: &str, value: Value) -> Result<()> {
            match name {
                $(stringify!($name) => doc.$name = serde_json::from_value(value).with_context(|| format!("bad value for {name}"))?,)*
                other => bail!("{other:?} is not a shared canvas field"),
            }
            Ok(())
        }
    };
}

shared_fields!(recent_folders, defaults, vscode, idle_stop_minutes, idle_stop_v2, mcp, automations_dir, doctor_created, default_agent, dust_after_days, subagents_shown, notebook_root, notebook_mounts, notebook_scripts);

/// Shared fields whose overwrite by another window is worth telling about.
const NOTICED_FIELDS: [(&str, &str); 10] = [
    ("defaults", "the new-session defaults"),
    ("vscode", "the VS Code settings"),
    ("idle_stop_minutes", "the idle-stop setting"),
    ("mcp", "the MCP presets"),
    ("automations_dir", "the automations folder"),
    ("default_agent", "the default agent"),
    ("dust_after_days", "the dust setting"),
    ("subagents_shown", "the subagents setting"),
    ("notebook_root", "the notebook folder"),
    ("notebook_mounts", "the notebook mounts"),
];

/// Box fields that are bookkeeping, not edits anyone made (no notice).
const QUIET_FIELDS: [&str; 3] = ["reviewed_ms", "last_used_ms", "accessed_ms"];

/// One change of an edit or patch.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Op {
    /// Inserts a node (appended) or replaces the whole node with its id.
    PutNode(Box<Node>),
    DelNode(String),
    PutGroup(Box<Group>),
    DelGroup(String),
    /// Sets a shared top-level field ([`SHARED_FIELDS`]).
    SetField(String, Value),
    /// Sets the listed fields of a node from this one (dropped when the
    /// node no longer exists).
    NodeFields(Box<Node>, Vec<String>),
    /// Sets the listed fields of a group (dropped when it no longer exists).
    GroupFields(Box<Group>, Vec<String>),
}

/// What an operation changes (the unit of "last edit wins").
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// A node or group as a whole (created, replaced or deleted).
    Node(String),
    Group(String),
    /// One field of a node or group.
    NodeField(String, String),
    GroupField(String, String),
    /// A shared top-level field.
    Field(String),
}

impl Op {
    pub fn targets(&self) -> Vec<Target> {
        match self {
            Op::PutNode(node) => vec![Target::Node(node.id.clone())],
            Op::DelNode(id) => vec![Target::Node(id.clone())],
            Op::PutGroup(group) => vec![Target::Group(group.id.clone())],
            Op::DelGroup(id) => vec![Target::Group(id.clone())],
            Op::SetField(name, _) => vec![Target::Field(name.clone())],
            Op::NodeFields(node, fields) => fields.iter().map(|f| Target::NodeField(node.id.clone(), f.clone())).collect(),
            Op::GroupFields(group, fields) => fields.iter().map(|f| Target::GroupField(group.id.clone(), f.clone())).collect(),
        }
    }
}

/// Nodes and groups: objects of the document with an id and named fields.
trait Object: Clone + PartialEq + Serialize + DeserializeOwned {
    fn id(&self) -> &String;
    fn list(doc: &Doc) -> &Vec<Self>;
    fn list_mut(doc: &mut Doc) -> &mut Vec<Self>;
    fn whole(id: &str) -> Target;
    fn field(id: &str, field: &str) -> Target;
    fn put(self) -> Op;
    fn del(id: String) -> Op;
    fn fields(self, fields: Vec<String>) -> Op;
    fn label(&self) -> String;

    fn find<'a>(doc: &'a Doc, id: &str) -> Option<&'a Self> {
        Self::list(doc).iter().find(|x| x.id() == id)
    }
}

impl Object for Node {
    fn id(&self) -> &String {
        &self.id
    }
    fn list(doc: &Doc) -> &Vec<Self> {
        &doc.nodes
    }
    fn list_mut(doc: &mut Doc) -> &mut Vec<Self> {
        &mut doc.nodes
    }
    fn whole(id: &str) -> Target {
        Target::Node(id.to_owned())
    }
    fn field(id: &str, field: &str) -> Target {
        Target::NodeField(id.to_owned(), field.to_owned())
    }
    fn put(self) -> Op {
        Op::PutNode(Box::new(self))
    }
    fn del(id: String) -> Op {
        Op::DelNode(id)
    }
    fn fields(self, fields: Vec<String>) -> Op {
        Op::NodeFields(Box::new(self), fields)
    }
    fn label(&self) -> String {
        format!("“{}”", self.name)
    }
}

impl Object for Group {
    fn id(&self) -> &String {
        &self.id
    }
    fn list(doc: &Doc) -> &Vec<Self> {
        &doc.groups
    }
    fn list_mut(doc: &mut Doc) -> &mut Vec<Self> {
        &mut doc.groups
    }
    fn whole(id: &str) -> Target {
        Target::Group(id.to_owned())
    }
    fn field(id: &str, field: &str) -> Target {
        Target::GroupField(id.to_owned(), field.to_owned())
    }
    fn put(self) -> Op {
        Op::PutGroup(Box::new(self))
    }
    fn del(id: String) -> Op {
        Op::DelGroup(id)
    }
    fn fields(self, fields: Vec<String>) -> Op {
        Op::GroupFields(Box::new(self), fields)
    }
    fn label(&self) -> String {
        format!("Group “{}”", self.name)
    }
}

fn to_map<T: Serialize>(value: &T) -> Map<String, Value> {
    match serde_json::to_value(value) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Names of the fields that differ (a field left out counts as its default).
fn changed_fields<T: Serialize>(a: &T, b: &T) -> Vec<String> {
    let (a, b) = (to_map(a), to_map(b));
    let names: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    names.into_iter().filter(|k| a.get(*k) != b.get(*k)).cloned().collect()
}

/// `base` with the fields `take` copied from `from` (`None`: every field),
/// except those in `keep`.
fn merge<T: Object>(base: &T, from: &T, take: Option<&[String]>, keep: &HashSet<String>) -> Result<T> {
    let (mut map, source) = (to_map(base), to_map(from));
    let all: Vec<String>;
    let take = match take {
        Some(fields) => fields,
        None => {
            all = map.keys().chain(source.keys()).cloned().collect::<BTreeSet<_>>().into_iter().collect();
            &all
        }
    };
    for name in take.iter().filter(|f| !keep.contains(*f) && f.as_str() != "id") {
        match source.get(name) {
            Some(value) => map.insert(name.clone(), value.clone()),
            None => map.remove(name),
        };
    }
    Ok(serde_json::from_value(Value::Object(map))?)
}

/// What of an object a client must not take from a patch.
enum Keep {
    Nothing,
    Fields(HashSet<String>),
    All,
}

impl Keep {
    fn with(self, other: Keep) -> Keep {
        match (self, other) {
            (Keep::All, _) | (_, Keep::All) => Keep::All,
            (Keep::Nothing, other) | (other, Keep::Nothing) => other,
            (Keep::Fields(mut a), Keep::Fields(b)) => {
                a.extend(b);
                Keep::Fields(a)
            }
        }
    }
}

/// An operation on one object, in a common shape.
enum Change<'a, T> {
    /// New values of the listed fields (`None`: the whole object, inserted when missing).
    Set(&'a T, Option<&'a [String]>),
    Delete,
}

/// Applies a change to `id`, leaving alone what `keep` says. Field changes
/// of an object that no longer exists are dropped (a delete wins over
/// concurrent field edits); a whole object is inserted or replaced.
fn change<T: Object>(doc: &mut Doc, id: &str, change: &Change<T>, keep: &Keep) -> Result<()> {
    let list = T::list_mut(doc);
    let at = list.iter().position(|x| x.id() == id);
    match (change, keep, at) {
        (_, Keep::All, _) => {}
        (Change::Delete, _, Some(at)) => {
            list.remove(at);
        }
        (Change::Delete, _, None) | (Change::Set(_, Some(_)), _, None) => {}
        (Change::Set(value, None), _, None) => list.push((*value).clone()),
        (Change::Set(value, None), Keep::Nothing, Some(at)) => list[at] = (*value).clone(),
        (Change::Set(value, take), keep, Some(at)) => {
            let none = HashSet::new();
            let keep = match keep {
                Keep::Fields(fields) => fields,
                _ => &none,
            };
            list[at] = merge(&list[at], value, *take, keep)?;
        }
    }
    Ok(())
}

/// Applies one operation as the service does (the last edit wins per field).
pub fn apply(doc: &mut Doc, op: &Op) -> Result<()> {
    match op {
        Op::PutNode(node) => change(doc, &node.id, &Change::Set(&**node, None), &Keep::Nothing),
        Op::DelNode(id) => change::<Node>(doc, id, &Change::Delete, &Keep::Nothing),
        Op::PutGroup(group) => change(doc, &group.id, &Change::Set(&**group, None), &Keep::Nothing),
        Op::DelGroup(id) => change::<Group>(doc, id, &Change::Delete, &Keep::Nothing),
        Op::NodeFields(node, fields) => change(doc, &node.id, &Change::Set(&**node, Some(fields)), &Keep::Nothing),
        Op::GroupFields(group, fields) => change(doc, &group.id, &Change::Set(&**group, Some(fields)), &Keep::Nothing),
        Op::SetField(name, value) => set_field(doc, name, value.clone()),
    }
}

/// Applies an edit; the document is unchanged when any operation fails.
pub fn apply_all(doc: &mut Doc, ops: &[Op]) -> Result<()> {
    let mut next = doc.clone();
    for op in ops {
        apply(&mut next, op)?;
    }
    *doc = next;
    Ok(())
}

/// The operations that turn the shared part of `from` into `to`.
pub fn diff(from: &Doc, to: &Doc) -> Vec<Op> {
    let mut ops = Vec::new();
    diff_list(&from.groups, &to.groups, &mut ops);
    diff_list(&from.nodes, &to.nodes, &mut ops);
    for name in SHARED_FIELDS {
        if !same_field(from, to, name)
            && let Some(value) = field(to, name)
        {
            ops.push(Op::SetField((*name).to_owned(), value));
        }
    }
    ops
}

fn diff_list<T: Object>(from: &[T], to: &[T], ops: &mut Vec<Op>) {
    let changed = |a: &T, b: &T| b.clone().fields(changed_fields(a, b));
    // Usually the same objects in the same order: compare pairwise.
    if from.len() == to.len() && from.iter().zip(to).all(|(a, b)| a.id() == b.id()) {
        ops.extend(from.iter().zip(to).filter(|(a, b)| a != b).map(|(a, b)| changed(a, b)));
        return;
    }
    let old: HashMap<&String, &T> = from.iter().map(|x| (x.id(), x)).collect();
    let new: HashSet<&String> = to.iter().map(|x| x.id()).collect();
    ops.extend(from.iter().filter(|x| !new.contains(x.id())).map(|x| T::del(x.id().clone())));
    for x in to {
        match old.get(x.id()) {
            None => ops.push(x.clone().put()),
            Some(before) if *before != x => ops.push(changed(before, x)),
            Some(_) => {}
        }
    }
}

/// The shared parts of two documents are equal (order aside).
pub fn same_shared(a: &Doc, b: &Doc) -> bool {
    diff(a, b).is_empty()
}

/// A local change kept for a later start of the client (it could not be
/// sent, or was sent and never confirmed), with what the service had where
/// it applies when it was made: replayed only where that is still so
/// ([`replay`], R3-C-02).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Kept {
    pub op: Op,
    /// The service's copy of what `op` changes, as this client knew it: the
    /// box or group (none: it did not exist), or the shared field's value
    /// (none counts as null).
    #[serde(default)]
    pub base: Option<Value>,
    /// Sent before it was kept and never confirmed: the service may have it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sent: bool,
}

/// What replaying kept changes did.
#[derive(Default, Debug, PartialEq)]
pub struct Replayed {
    /// Changes made to the document (they go to the service with the next edit).
    pub applied: usize,
    /// Changes the document has already (the service got them before).
    pub already: usize,
    /// Not made: what they change was changed elsewhere since (what, for the notice).
    pub changed: Vec<String>,
    /// Not made: the box or group is no longer on the canvas (never brought back).
    pub gone: Vec<String>,
    /// Not made: kept without a base by an older version, so not checkable.
    pub unchecked: usize,
    /// Box removals whose box is unchanged since: the caller makes them once
    /// it knows the box does not run again (a removal never stops anything).
    pub removals: Vec<Kept>,
}

impl Replayed {
    /// Changes not made, of every kind.
    pub fn dropped(&self) -> usize {
        self.changed.len() + self.gone.len() + self.unchecked
    }
}

/// A kept change's base as an object (`None`: it did not exist).
fn base_object<T: Object>(base: &Option<Value>) -> Option<T> {
    base.as_ref().and_then(|value| serde_json::from_value(value.clone()).ok())
}

/// How a shared field is named in notices.
fn field_label(name: &str) -> String {
    NOTICED_FIELDS.iter().find(|(f, _)| *f == name).map_or_else(|| name.replace('_', " "), |(_, label)| (*label).to_owned())
}

/// Adds `label` to a notice's list once.
fn note(list: &mut Vec<String>, label: String) {
    if !list.contains(&label) {
        list.push(label);
    }
}

/// Replays changes kept by an earlier run on top of the service's document
/// as it is now, as a three-way merge per field: a field is set only when
/// it still holds the value the change was made from (or holds the new one
/// already); a field changed elsewhere since keeps that change. A box or
/// group that is gone is never brought back, but for one created here and
/// never sent. Removals of boxes unchanged since are returned, not made.
/// `None` bases (`legacy`: an older version's file) set fields of objects
/// that exist and nothing else.
pub fn replay(doc: &mut Doc, kept: &[Kept], legacy: &[Op]) -> Replayed {
    let mut out = Replayed::default();
    // Removed boxes take their subagent boxes and their subsessions' links
    // along when the removal is made (`Doc::remove_node`), not before.
    let removing: HashSet<&str> = kept.iter().filter_map(|k| if let Op::DelNode(id) = &k.op { Some(id.as_str()) } else { None }).collect();
    for item in kept {
        let known = Some(&item.base);
        match &item.op {
            Op::PutNode(node) => replay_object(doc, &node.id, Change::Set(&**node, None), known, item.sent, &mut out),
            Op::PutGroup(group) => replay_object(doc, &group.id, Change::Set(&**group, None), known, item.sent, &mut out),
            Op::GroupFields(group, fields) => replay_object(doc, &group.id, Change::Set(&**group, Some(fields)), known, item.sent, &mut out),
            Op::DelGroup(id) => replay_object::<Group>(doc, id, Change::Delete, known, item.sent, &mut out),
            Op::NodeFields(node, fields) => {
                let base = base_object::<Node>(&item.base);
                let unlinked = |f: &String| f == "parent" && node.parent.is_none() && base.as_ref().and_then(|b| b.parent.as_deref()).is_some_and(|p| removing.contains(p));
                let fields: Vec<String> = fields.iter().filter(|f| !unlinked(f)).cloned().collect();
                if !fields.is_empty() {
                    replay_object(doc, &node.id, Change::Set(&**node, Some(&fields)), known, item.sent, &mut out);
                }
            }
            Op::DelNode(id) => match (Node::find(doc, id), base_object::<Node>(&item.base)) {
                (None, _) => out.already += 1,
                // A subagent box goes with its session's removal.
                (Some(now), _) if now.parent.as_deref().is_some_and(|p| removing.contains(p)) && now.kind == crate::doc::NodeKind::Agent => {}
                (Some(now), Some(base)) if *now == base => out.removals.push(item.clone()),
                (Some(now), _) => note(&mut out.changed, now.label()),
            },
            Op::SetField(name, value) => {
                let now = field(doc, name).unwrap_or(Value::Null);
                if now == *value {
                    out.already += 1;
                } else if now == item.base.clone().unwrap_or(Value::Null) && set_field(doc, name, value.clone()).is_ok() {
                    out.applied += 1;
                } else {
                    note(&mut out.changed, field_label(name));
                }
            }
        }
    }
    for op in legacy {
        match op {
            Op::NodeFields(node, fields) => replay_object(doc, &node.id, Change::Set(&**node, Some(fields)), None, true, &mut out),
            Op::GroupFields(group, fields) => replay_object(doc, &group.id, Change::Set(&**group, Some(fields)), None, true, &mut out),
            Op::SetField(name, value) if field(doc, name).as_ref() == Some(value) => out.already += 1,
            Op::PutNode(node) if Node::find(doc, &node.id).is_some_and(|n| n == &**node) => out.already += 1,
            Op::PutGroup(group) if Group::find(doc, &group.id).is_some_and(|g| g == &**group) => out.already += 1,
            Op::DelNode(id) if Node::find(doc, id).is_none() => out.already += 1,
            Op::DelGroup(id) if Group::find(doc, id).is_none() => out.already += 1,
            _ => out.unchecked += 1,
        }
    }
    out
}

/// One kept change of a box or group (see [`replay`]). `known`: its base
/// (`None`: not recorded).
fn replay_object<T: Object>(doc: &mut Doc, id: &str, op: Change<T>, known: Option<&Option<Value>>, sent: bool, out: &mut Replayed) {
    let base = known.map(base_object::<T>);
    let Some(now) = T::find(doc, id).cloned() else {
        match (&op, base) {
            // Created here and never sent: the service never had it.
            (Change::Set(value, None), Some(None)) if !sent => {
                T::list_mut(doc).push((*value).clone());
                out.applied += 1;
            }
            (Change::Delete, _) => out.already += 1,
            (Change::Set(value, _), _) => note(&mut out.gone, value.label()),
        }
        return;
    };
    let (value, fields) = match op {
        Change::Delete => {
            match base {
                Some(Some(base)) if base == now => {
                    T::list_mut(doc).retain(|x| x.id() != id);
                    out.applied += 1;
                }
                Some(_) => note(&mut out.changed, now.label()),
                None => out.unchecked += 1,
            }
            return;
        }
        Change::Set(value, fields) => (value, fields),
    };
    let (new, current) = (to_map(value), to_map(&now));
    let base = match base {
        Some(Some(base)) => Some(to_map(&base)),
        // Created here and the service has it: what differs was changed there since.
        Some(None) => Some(new.clone()),
        None => None,
    };
    let names: Vec<String> = match fields {
        Some(fields) => fields.to_vec(),
        None => new.keys().chain(current.keys()).cloned().collect::<BTreeSet<_>>().into_iter().collect(),
    };
    let (mut take, mut conflict) = (Vec::new(), false);
    for name in names.into_iter().filter(|f| f != "id") {
        let (now, new) = (current.get(&name), new.get(&name));
        if now == new {
            continue;
        }
        match &base {
            Some(base) if base.get(&name) != now => conflict = true,
            _ => take.push(name),
        }
    }
    if !take.is_empty()
        && let Ok(merged) = merge(&now, value, Some(&take), &HashSet::new())
        && let Some(at) = T::list(doc).iter().position(|x| x.id() == id)
    {
        T::list_mut(doc)[at] = merged;
        out.applied += 1;
    }
    if conflict {
        note(&mut out.changed, now.label());
    } else if take.is_empty() {
        out.already += 1;
    }
}

/// A client's own view of a canvas: kept in the client's home, never shared.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ClientPrefs {
    pub view: View,
    #[serde(default)]
    pub ui: UiPrefs,
    /// Most recently shown terminals, newest first.
    #[serde(default)]
    pub tiles: Vec<String>,
    #[serde(default)]
    pub selected: Option<String>,
    /// The space the window shows (`notebook`, `tasks`; none: the canvas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
}

impl Default for ClientPrefs {
    fn default() -> Self {
        Self::of(&Doc::default())
    }
}

impl ClientPrefs {
    /// The per-client fields of a document (the seed for a new client).
    pub fn of(doc: &Doc) -> Self {
        Self { view: doc.view.clone(), ui: doc.ui.clone(), tiles: doc.tiles.clone(), selected: doc.selected.clone(), space: None }
    }

    pub fn apply(&self, doc: &mut Doc) {
        doc.view = self.view.clone();
        doc.ui = self.ui.clone();
        doc.tiles = self.tiles.clone();
        doc.selected = self.selected.clone();
    }
}

/// 64-bit FNV-1a (stable across builds, unlike the std hasher).
pub fn fnv64(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3))
}

/// A stable, file-name-safe key of a canvas file: its name and a hash of its
/// full path (case-insensitive).
pub fn doc_key(path: &str) -> String {
    let stem: String = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(40).collect())
        .unwrap_or_default();
    let stem = if stem.is_empty() { "canvas".to_owned() } else { stem };
    format!("{stem}-{:016x}", fnv64(&path.replace('/', "\\").to_lowercase()))
}

/// An id derived from a key, so windows creating the same thing at the same
/// time create the same object (`prefix-<hash>`).
pub fn stable_id(prefix: &str, key: &str) -> String {
    format!("{prefix}-{:016x}", fnv64(key))
}

/// Payload of `ServiceMsg::Doc`: a whole document (opened, or sent again to
/// resynchronize a client), or why it could not be opened.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DocReply {
    /// The canvas file on the service's machine.
    pub path: String,
    /// The receiving connection's id: patches with this origin are its own.
    pub client: u64,
    #[serde(default)]
    pub doc: Option<Doc>,
    #[serde(default)]
    pub error: Option<String>,
}

/// An opened document.
#[derive(Clone, Debug)]
pub struct Opened {
    pub key: String,
    pub version: u64,
    pub path: String,
    pub client: u64,
    pub doc: Doc,
}

impl Opened {
    /// From `ServiceMsg::Doc { key, version, json }`.
    pub fn from_message(key: String, version: u64, json: &str) -> Result<Self> {
        let reply: DocReply = serde_json::from_str(json).context("unreadable canvas from the service")?;
        if let Some(error) = reply.error {
            bail!(error);
        }
        let doc = reply.doc.context("the service sent no canvas")?;
        Ok(Self { key, version, path: reply.path, client: reply.client, doc })
    }

    /// A document that is not shared with a service (tests, benchmarks).
    pub fn detached(doc: Doc) -> Self {
        Self { key: "detached".into(), version: 1, path: String::new(), client: 0, doc }
    }
}

/// What applying a patch did.
#[derive(Default, Debug)]
pub struct Patched {
    /// The working copy changed.
    pub changed: bool,
    /// Things this client changed recently that another window overwrote.
    pub overwritten: Vec<String>,
    /// A patch is missing: ask for the whole document again.
    pub resync: bool,
}

/// A client's replica of a shared document (see the module documentation).
pub struct Replica {
    pub key: String,
    pub path: String,
    /// This connection's id at the service.
    pub client: u64,
    /// Version of `synced`.
    pub version: u64,
    synced: Doc,
    shadow: Doc,
    /// Targets of each edit sent and not yet seen back, oldest first.
    inflight: VecDeque<Vec<Target>>,
    inflight_count: HashMap<Target, usize>,
    /// When this client last edited each target.
    recent: HashMap<Target, Instant>,
}

impl Replica {
    pub fn new(opened: &Opened) -> Self {
        Self {
            key: opened.key.clone(),
            path: opened.path.clone(),
            client: opened.client,
            version: opened.version,
            synced: opened.doc.clone(),
            shadow: opened.doc.clone(),
            inflight: VecDeque::new(),
            inflight_count: HashMap::new(),
            recent: HashMap::new(),
        }
    }

    /// Edits sent and not yet confirmed.
    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// The service's document as of `version`.
    pub fn synced(&self) -> &Doc {
        &self.synced
    }

    /// The local changes the service may not have, with their bases, to
    /// keep for a later start: first those sent and not confirmed (their
    /// base is the service's copy), then those not sent (their base is that
    /// copy with this client's edits in flight), so a replay in this order
    /// checks each against what it was made from.
    pub fn kept(&self, doc: &Doc) -> Vec<Kept> {
        let based = |from: &Doc, to: &Doc, sent: bool| -> Vec<Kept> {
            diff(from, to)
                .into_iter()
                .map(|op| {
                    let base = match &op {
                        Op::PutNode(node) | Op::NodeFields(node, _) => Node::find(from, &node.id).and_then(|n| serde_json::to_value(n).ok()),
                        Op::DelNode(id) => Node::find(from, id).and_then(|n| serde_json::to_value(n).ok()),
                        Op::PutGroup(group) | Op::GroupFields(group, _) => Group::find(from, &group.id).and_then(|g| serde_json::to_value(g).ok()),
                        Op::DelGroup(id) => Group::find(from, id).and_then(|g| serde_json::to_value(g).ok()),
                        Op::SetField(name, _) => field(from, name),
                    };
                    Kept { op, base, sent }
                })
                .collect()
        };
        let mut kept = based(&self.synced, &self.shadow, true);
        kept.extend(based(&self.shadow, doc, false));
        kept
    }

    /// The local changes not sent yet, as an edit `(base version, ops)`;
    /// they count as in flight from now on.
    pub fn take_edit(&mut self, doc: &Doc) -> Option<(u64, Vec<Op>)> {
        let ops = diff(&self.shadow, doc);
        if ops.is_empty() {
            return None;
        }
        let now = Instant::now();
        self.recent.retain(|_, at| at.elapsed() < RECENT);
        let mut targets = Vec::new();
        for op in &ops {
            let _ = apply(&mut self.shadow, op);
            for target in op.targets() {
                *self.inflight_count.entry(target.clone()).or_default() += 1;
                self.recent.insert(target.clone(), now);
                targets.push(target);
            }
        }
        self.inflight.push_back(targets);
        Some((self.version, ops))
    }

    /// Applies a patch the service broadcast. `doc` is the working copy.
    pub fn apply_patch(&mut self, doc: &mut Doc, version: u64, origin: u64, ops: &[Op]) -> Patched {
        let mut patched = Patched::default();
        if version <= self.version {
            return patched;
        }
        if version != self.version + 1 {
            patched.resync = true;
            return patched;
        }
        self.version = version;
        for op in ops {
            let _ = apply(&mut self.synced, op);
        }
        // This client's own edit coming back is the oldest in flight (the same
        // ops, so the same targets). One that is not (an edit a resync after a
        // refusal dropped, applied by the service after that canvas was made)
        // is taken like another window's, so this copy gets it too (S8-05).
        let own = origin == self.client && self.inflight.front().is_some_and(|targets| *targets == ops.iter().flat_map(Op::targets).collect::<Vec<_>>());
        if own {
            for target in self.inflight.pop_front().unwrap_or_default() {
                if let Some(count) = self.inflight_count.get_mut(&target) {
                    *count -= 1;
                    if *count == 0 {
                        self.inflight_count.remove(&target);
                    }
                }
            }
        } else {
            for op in ops {
                match op {
                    Op::PutNode(node) => self.absorb(doc, &node.id, Change::Set(&**node, None), &mut patched),
                    Op::NodeFields(node, fields) => self.absorb(doc, &node.id, Change::Set(&**node, Some(fields)), &mut patched),
                    Op::DelNode(id) => self.absorb::<Node>(doc, id, Change::Delete, &mut patched),
                    Op::PutGroup(group) => self.absorb(doc, &group.id, Change::Set(&**group, None), &mut patched),
                    Op::GroupFields(group, fields) => self.absorb(doc, &group.id, Change::Set(&**group, Some(fields)), &mut patched),
                    Op::DelGroup(id) => self.absorb::<Group>(doc, id, Change::Delete, &mut patched),
                    Op::SetField(name, value) => self.absorb_field(doc, name, value, &mut patched),
                }
            }
        }
        if self.inflight.is_empty() {
            patched.changed |= reorder(&mut doc.nodes, &self.synced.nodes, |n| &n.id);
            patched.changed |= reorder(&mut doc.groups, &self.synced.groups, |g| &g.id);
        }
        patched
    }

    /// What of an object this client sent and the service has not applied
    /// yet: those changes land after this patch, so they win.
    fn flying<T: Object>(&self, id: &str) -> Keep {
        if self.inflight_count.contains_key(&T::whole(id)) {
            return Keep::All;
        }
        let fields: HashSet<String> = self
            .inflight_count
            .keys()
            .filter_map(|t| match t {
                Target::NodeField(i, f) | Target::GroupField(i, f) if i == id && *t == T::field(id, f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        if fields.is_empty() { Keep::Nothing } else { Keep::Fields(fields) }
    }

    /// What of an object this client changed and has not sent yet.
    fn unsent<T: Object>(&self, doc: &Doc, id: &str) -> Keep {
        match (T::find(doc, id), T::find(&self.shadow, id)) {
            (None, None) => Keep::Nothing,
            (Some(mine), Some(sent)) if mine == sent => Keep::Nothing,
            (Some(mine), Some(sent)) => Keep::Fields(changed_fields(sent, mine).into_iter().collect()),
            // Created or deleted here, not sent yet.
            _ => Keep::All,
        }
    }

    /// Another window's change to a node or group.
    fn absorb<T: Object>(&mut self, doc: &mut Doc, id: &str, op: Change<T>, patched: &mut Patched) {
        let unsent = self.unsent::<T>(doc, id);
        let flying = self.flying::<T>(id);
        let _ = change(&mut self.shadow, id, &op, &flying);
        let keep = flying.with(unsent);
        let before = T::find(doc, id).cloned();
        if change(doc, id, &op, &keep).is_err() {
            return;
        }
        let after = T::find(doc, id);
        if before.as_ref() == after {
            return;
        }
        patched.changed = true;
        // Tell when a change this client made recently was overwritten.
        let mine = |target: &Target| self.recent.get(target).is_some_and(|at| at.elapsed() < RECENT);
        let overwritten = match (&before, after) {
            (Some(a), Some(b)) => changed_fields(a, b).iter().any(|f| !QUIET_FIELDS.contains(&f.as_str()) && mine(&T::field(id, f))),
            _ => mine(&T::whole(id)) || self.recent.keys().any(|t| matches!(t, Target::NodeField(i, _) | Target::GroupField(i, _) if i == id) && mine(t)),
        };
        if overwritten && let Some(object) = before.as_ref().or(after) {
            patched.overwritten.push(object.label());
        }
    }

    /// Another window's change to a shared top-level field.
    fn absorb_field(&mut self, doc: &mut Doc, name: &str, value: &Value, patched: &mut Patched) {
        let target = Target::Field(name.to_owned());
        // Our edit in flight reaches the service after this one and wins.
        if self.inflight_count.contains_key(&target) {
            return;
        }
        let unsent = !same_field(doc, &self.shadow, name);
        let _ = set_field(&mut self.shadow, name, value.clone());
        // So does a change we have not sent yet.
        if unsent {
            return;
        }
        let before = field(doc, name);
        if set_field(doc, name, value.clone()).is_err() || before.as_ref() == Some(value) {
            return;
        }
        patched.changed = true;
        if self.recent.get(&target).is_some_and(|at| at.elapsed() < RECENT)
            && let Some((_, label)) = NOTICED_FIELDS.iter().find(|(f, _)| *f == name)
        {
            patched.overwritten.push(label.to_string());
        }
    }

    /// The whole document again (reconnected, or an edit was refused):
    /// local changes not known to have reached the service are kept on top
    /// of it and sent with the next edit.
    pub fn resync(&mut self, doc: &mut Doc, opened: Opened) {
        let local = diff(&self.synced, doc);
        self.resync_with(doc, opened, local);
    }

    /// The whole document the service sent when it refused an edit of this
    /// client: the changes it was sent go (the refused one is not sent
    /// again, and again, S8-05); those not sent yet are kept on top. An edit
    /// sent after the refused one that the service still applies comes back
    /// as a patch this copy takes like another window's.
    pub fn resync_refused(&mut self, doc: &mut Doc, opened: Opened) {
        let local = diff(&self.shadow, doc);
        self.resync_with(doc, opened, local);
    }

    fn resync_with(&mut self, doc: &mut Doc, opened: Opened, local: Vec<Op>) {
        let mut next = opened.doc.clone();
        for op in &local {
            let _ = apply(&mut next, op);
        }
        ClientPrefs::of(doc).apply(&mut next);
        *doc = next;
        reorder(&mut doc.nodes, &opened.doc.nodes, |n| &n.id);
        reorder(&mut doc.groups, &opened.doc.groups, |g| &g.id);
        self.key = opened.key;
        self.path = opened.path;
        self.client = opened.client;
        self.version = opened.version;
        self.shadow = opened.doc.clone();
        self.synced = opened.doc;
        self.inflight.clear();
        self.inflight_count.clear();
    }
}

/// Orders `items` like `by` (items `by` lacks stay last, in their order).
/// Returns true when anything moved.
fn reorder<T>(items: &mut [T], by: &[T], id: impl Fn(&T) -> &String) -> bool {
    let position: HashMap<&String, usize> = by.iter().enumerate().map(|(i, x)| (id(x), i)).collect();
    let rank = |x: &T| position.get(id(x)).copied().unwrap_or(usize::MAX);
    if items.windows(2).all(|w| rank(&w[0]) <= rank(&w[1])) {
        return false;
    }
    items.sort_by_cached_key(rank);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::NodeKind;

    fn node(id: &str, x: i32) -> Node {
        serde_json::from_value(serde_json::json!({ "id": id, "kind": "session", "name": id, "x": x, "y": 0 })).unwrap()
    }

    /// The service's side: applies edits in arrival order and logs patches.
    struct Server {
        doc: Doc,
        version: u64,
        patches: Vec<(u64, u64, Vec<Op>)>,
    }

    impl Server {
        fn new(doc: Doc) -> Self {
            Self { doc, version: 1, patches: Vec::new() }
        }

        fn edit(&mut self, origin: u64, ops: Vec<Op>) {
            apply_all(&mut self.doc, &ops).unwrap();
            self.version += 1;
            self.patches.push((self.version, origin, ops));
        }

        fn open(&self, client: u64) -> Opened {
            Opened { key: "k".into(), version: self.version, path: "p".into(), client, doc: self.doc.clone() }
        }
    }

    struct Window {
        replica: Replica,
        doc: Doc,
        seen: usize,
        notices: Vec<String>,
    }

    impl Window {
        fn open(server: &Server, client: u64) -> Self {
            let opened = server.open(client);
            Self { replica: Replica::new(&opened), doc: opened.doc, seen: server.patches.len(), notices: Vec::new() }
        }

        fn send(&mut self, server: &mut Server) {
            if let Some((_, ops)) = self.replica.take_edit(&self.doc) {
                server.edit(self.replica.client, ops);
            }
        }

        fn take(&mut self) -> Option<Vec<Op>> {
            self.replica.take_edit(&self.doc).map(|(_, ops)| ops)
        }

        fn receive(&mut self, server: &Server) {
            for (version, origin, ops) in &server.patches[self.seen..] {
                let patched = self.replica.apply_patch(&mut self.doc, *version, *origin, ops);
                assert!(!patched.resync);
                self.notices.extend(patched.overwritten);
            }
            self.seen = server.patches.len();
        }
    }

    fn ids(doc: &Doc) -> Vec<&str> {
        doc.nodes.iter().map(|n| n.id.as_str()).collect()
    }

    fn converged(server: &Server, windows: &[&Window]) {
        for window in windows {
            assert!(same_shared(&window.doc, &server.doc), "{:?}\nvs\n{:?}", diff(&server.doc, &window.doc), ids(&server.doc));
            assert_eq!(ids(&window.doc), ids(&server.doc), "same order");
            assert_eq!(window.replica.in_flight(), 0);
        }
    }

    #[test]
    fn concurrent_edits_converge_field_by_field_and_the_last_one_wins_with_a_notice() {
        let mut server = Server::new(Doc { nodes: vec![node("a", 0), node("b", 40)], ..Doc::default() });
        let mut a = Window::open(&server, 1);
        let mut b = Window::open(&server, 2);
        // Both rename "a"; A also moves it; each adds a box; A changes a shared setting.
        a.doc.node_mut("a").unwrap().x = 10;
        a.doc.node_mut("a").unwrap().name = "renamed in A".into();
        a.doc.nodes.push(node("from-a", 80));
        a.doc.defaults.set_model(crate::agent::AgentKind::OpenCode, Some("m-a"));
        b.doc.node_mut("a").unwrap().name = "renamed in B".into();
        b.doc.nodes.push(node("from-b", 120));
        let (ops_a, ops_b) = (a.take().unwrap(), b.take().unwrap());
        assert!(matches!(&ops_a[0], Op::NodeFields(n, f) if n.id == "a" && *f == ["name", "x"]), "only changed fields travel: {ops_a:?}");
        // The service gets A's edit first, then B's.
        server.edit(1, ops_a);
        server.edit(2, ops_b);
        a.receive(&server);
        b.receive(&server);
        converged(&server, &[&a, &b]);
        let n = server.doc.node("a").unwrap();
        assert_eq!((n.name.as_str(), n.x), ("renamed in B", 10), "the last edit wins per field; other fields keep both");
        assert_eq!(ids(&server.doc), ["a", "b", "from-a", "from-b"]);
        assert_eq!(server.doc.defaults.model(crate::agent::AgentKind::OpenCode), Some("m-a"), "the remembered model reaches every window");
        assert_eq!(a.notices, ["“renamed in A”"], "A's name was overwritten");
        assert!(b.notices.is_empty(), "B's edit won");
        // Per-client fields are never sent.
        a.doc.view.x += 50;
        a.doc.ui.tiled = true;
        a.doc.tiles.push("a".into());
        assert!(a.take().is_none());
    }

    #[test]
    fn a_bookkeeping_edit_never_reverts_another_windows_rename() {
        let mut server = Server::new(Doc { nodes: vec![node("a", 0)], ..Doc::default() });
        let mut a = Window::open(&server, 1);
        let mut b = Window::open(&server, 2);
        // B marks "a" reviewed while A renames it; B's edit reaches the service last.
        a.doc.node_mut("a").unwrap().name = "renamed".into();
        b.doc.node_mut("a").unwrap().reviewed_ms = Some(5);
        a.send(&mut server);
        b.send(&mut server);
        a.receive(&server);
        b.receive(&server);
        converged(&server, &[&a, &b]);
        let n = server.doc.node("a").unwrap();
        assert_eq!((n.name.as_str(), n.reviewed_ms), ("renamed", Some(5)));
        assert!(a.notices.is_empty() && b.notices.is_empty());
    }

    #[test]
    fn an_unsent_local_change_survives_a_remote_patch_and_wins_later() {
        let mut server = Server::new(Doc { nodes: vec![node("a", 0), node("c", 40)], ..Doc::default() });
        let mut a = Window::open(&server, 1);
        let mut b = Window::open(&server, 2);
        a.doc.node_mut("a").unwrap().y = 7;
        b.doc.node_mut("a").unwrap().y = 3;
        b.doc.node_mut("a").unwrap().name = "b's name".into();
        b.doc.nodes.retain(|n| n.id != "c");
        b.send(&mut server);
        a.receive(&server);
        let n = a.doc.node("a").unwrap();
        assert_eq!((n.y, n.name.as_str()), (7, "b's name"), "A's unsent field stays; the others follow the patch");
        assert!(a.doc.node("c").is_none(), "an untouched box follows the patch");
        a.send(&mut server);
        a.receive(&server);
        b.receive(&server);
        converged(&server, &[&a, &b]);
        assert_eq!(server.doc.node("a").unwrap().y, 7);
        assert_eq!(b.notices, ["“b's name”"]);
        // A box one window deletes while another changes it: the delete wins,
        // whichever order the edits arrive in, and the other window is told.
        for b_first in [false, true] {
            if server.doc.node("a").is_none() {
                server.edit(9, vec![Op::PutNode(Box::new(node("a", 0)))]);
                a.receive(&server);
                b.receive(&server);
            }
            b.notices.clear();
            b.doc.node_mut("a").unwrap().name = "changed".into();
            a.doc.nodes.retain(|n| n.id != "a");
            if b_first {
                b.send(&mut server);
                a.send(&mut server);
            } else {
                a.send(&mut server);
                b.send(&mut server);
            }
            a.receive(&server);
            b.receive(&server);
            converged(&server, &[&a, &b]);
            assert!(server.doc.node("a").is_none(), "b first: {b_first}");
            assert_eq!(b.notices, ["“changed”"], "b first: {b_first}");
        }
    }

    #[test]
    fn three_windows_with_interleaved_edits_converge() {
        let mut server = Server::new(Doc { nodes: (0..6).map(|i| node(&format!("n{i}"), i * 40)).collect(), ..Doc::default() });
        let mut windows: Vec<Window> = (1..=3).map(|c| Window::open(&server, c)).collect();
        for round in 0..20i32 {
            for (w, window) in windows.iter_mut().enumerate() {
                let target = format!("n{}", (round as usize + w) % 6);
                if let Some(n) = window.doc.node_mut(&target) {
                    n.x += w as i32 + 1;
                    n.name = format!("w{w} r{round}");
                }
                if round % 7 == w as i32 {
                    window.doc.nodes.push(node(&format!("new-{w}-{round}"), 0));
                }
                if round % 5 == w as i32 {
                    window.doc.nodes.retain(|n| n.id != format!("n{}", (round as usize + 3) % 6));
                }
                if round % 3 == 0 {
                    window.doc.recent_folders.insert(0, crate::doc::RecentFolder { path: format!("C:\\w{w}"), used_ms: round as u64 });
                }
            }
            // Edits leave in different orders; patches arrive late.
            let order: Vec<usize> = if round % 2 == 0 { vec![0, 1, 2] } else { vec![2, 0, 1] };
            for w in order {
                windows[w].send(&mut server);
            }
            if round % 3 != 1 {
                for window in windows.iter_mut() {
                    window.receive(&server);
                }
            }
        }
        for window in windows.iter_mut() {
            window.receive(&server);
        }
        // A last round without concurrency settles everything.
        for w in 0..3 {
            windows[w].send(&mut server);
            for window in windows.iter_mut() {
                window.receive(&server);
            }
        }
        converged(&server, &windows.iter().collect::<Vec<_>>());
        assert!(server.doc.nodes.iter().any(|n| n.id.starts_with("new-")));
    }

    #[test]
    fn random_interleavings_converge() {
        // A small deterministic generator (no external crates).
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for run in 0..200 {
            let mut server = Server::new(Doc {
                nodes: (0..5).map(|i| node(&format!("n{i}"), i * 40)).collect(),
                groups: vec![Group { id: "g".into(), name: "G".into(), w: 10, h: 10, ..Default::default() }],
                ..Doc::default()
            });
            let mut windows: Vec<Window> = (1..=3).map(|c| Window::open(&server, c)).collect();
            let mut connections = 10;
            for step in 0..80 {
                let w = next(3) as usize;
                let window = &mut windows[w];
                match next(11) {
                    0..=2 => {
                        let count = window.doc.nodes.len() as u64;
                        if count > 0 {
                            let i = next(count) as usize;
                            let n = &mut window.doc.nodes[i];
                            match next(3) {
                                0 => n.x += 1,
                                1 => n.name = format!("r{run}s{step}w{w}"),
                                _ => n.reviewed_ms = Some(step),
                            }
                        }
                    }
                    3 => window.doc.nodes.push(node(&format!("new{run}-{step}-{w}"), step as i32)),
                    4 => {
                        let count = window.doc.nodes.len() as u64;
                        if count > 0 {
                            let i = next(count) as usize;
                            window.doc.nodes.remove(i);
                        }
                    }
                    5 => window.doc.idle_stop_minutes = next(300) as u32,
                    6 => match window.doc.groups.first_mut() {
                        Some(g) if next(4) > 0 => g.x += 1,
                        Some(_) => window.doc.groups.clear(),
                        None => window.doc.groups.push(Group { id: "g".into(), name: format!("G{step}"), w: 10, h: 10, ..Default::default() }),
                    },
                    7 => {
                        // The connection drops with an edit in flight that never arrived.
                        let _lost = window.take();
                        connections += 1;
                        window.replica.resync(&mut window.doc, server.open(connections));
                        window.seen = server.patches.len();
                    }
                    8 => window.send(&mut server),
                    _ => window.receive(&server),
                }
            }
            for _ in 0..2 {
                for w in 0..3 {
                    windows[w].send(&mut server);
                    for window in windows.iter_mut() {
                        window.receive(&server);
                    }
                }
            }
            converged(&server, &windows.iter().collect::<Vec<_>>());
        }
    }

    #[test]
    fn resync_keeps_local_changes_on_top_of_the_service_document() {
        let mut server = Server::new(Doc { nodes: vec![node("a", 0), node("b", 40)], ..Doc::default() });
        let mut a = Window::open(&server, 1);
        let mut b = Window::open(&server, 2);
        a.doc.node_mut("a").unwrap().x = 5;
        // A's edit is lost with its connection; B deletes "b" meanwhile.
        let _lost = a.take().unwrap();
        a.doc.nodes.push(node("offline", 9));
        a.doc.view.zoom = 2;
        b.doc.nodes.retain(|n| n.id != "b");
        b.send(&mut server);
        b.receive(&server);
        a.replica.resync(&mut a.doc, server.open(3));
        a.seen = server.patches.len();
        assert_eq!((a.replica.client, a.replica.in_flight(), a.doc.view.zoom), (3, 0, 2));
        assert!(a.doc.node("b").is_none() && a.doc.node("offline").is_some() && a.doc.node("a").unwrap().x == 5);
        a.send(&mut server);
        a.receive(&server);
        b.receive(&server);
        converged(&server, &[&a, &b]);
        assert_eq!(server.doc.node("a").unwrap().x, 5);
    }

    #[test]
    fn a_refused_edit_answered_by_the_whole_canvas_leaves_nothing_in_flight() {
        let mut server = Server::new(Doc { nodes: vec![node("a", 0)], ..Doc::default() });
        let mut a = Window::open(&server, 1);
        let mut b = Window::open(&server, 2);
        // A's edit is refused: no patch for it ever comes.
        a.doc.node_mut("a").unwrap().x = 5;
        let _refused = a.take().unwrap();
        assert_eq!(a.replica.in_flight(), 1);
        // The service answers with the whole canvas, also when the refusal came within
        // its gap (later, R2-C-05): nothing stays in flight.
        a.replica.resync(&mut a.doc, server.open(1));
        a.seen = server.patches.len();
        assert_eq!(a.replica.in_flight(), 0);
        a.doc.node_mut("a").unwrap().x = 0;
        // Another window's later change of that box is taken, not held back as this one's own.
        b.doc.node_mut("a").unwrap().x = 40;
        b.send(&mut server);
        a.receive(&server);
        assert_eq!(a.doc.node("a").unwrap().x, 40);
    }

    #[test]
    fn a_refused_edit_is_not_sent_again_and_one_sent_after_it_still_arrives() {
        let mut server = Server::new(Doc { nodes: vec![node("a", 0), node("b", 0)], ..Doc::default() });
        let mut a = Window::open(&server, 1);
        // A's first edit is refused (no patch); its second, sent before A heard
        // of that, is applied after the service made the canvas it sends back.
        a.doc.node_mut("a").unwrap().x = 5;
        let _refused = a.take().unwrap();
        let refusal = server.open(1);
        a.doc.node_mut("b").unwrap().x = 7;
        a.send(&mut server);
        // A change not sent yet stays.
        a.doc.node_mut("b").unwrap().y = 3;
        a.replica.resync_refused(&mut a.doc, refusal);
        a.seen = server.patches.len() - 1;
        assert_eq!(a.doc.node("a").unwrap().x, 0, "the refused change is gone, not sent again");
        assert_eq!(a.replica.in_flight(), 0);
        assert_eq!(a.doc.node("b").unwrap().y, 3, "a change not sent yet is kept");
        // The second edit's patch comes back: taken as the service's, not lost here.
        a.receive(&server);
        assert_eq!(a.doc.node("b").unwrap().x, 7);
        let resent = a.take().unwrap();
        assert!(matches!(resent.as_slice(), [Op::NodeFields(n, f)] if n.id == "b" && *f == ["y"]), "only the unsent change goes: {resent:?}");
        server.edit(1, resent);
        a.receive(&server);
        converged(&server, &[&a]);
        // A plain resync (a reconnect) still keeps what was sent and not confirmed.
        a.doc.node_mut("a").unwrap().x = 9;
        let _lost = a.take().unwrap();
        a.replica.resync(&mut a.doc, server.open(2));
        assert_eq!(a.doc.node("a").unwrap().x, 9, "sent on a connection that is gone: kept and sent again");
        assert!(a.take().is_some());
    }

    #[test]
    fn missing_patches_ask_for_a_resync_and_old_ones_are_ignored() {
        let server = Server::new(Doc::default());
        let mut a = Window::open(&server, 1);
        let op = Op::PutNode(Box::new(node("x", 0)));
        assert!(a.replica.apply_patch(&mut a.doc, 3, 2, std::slice::from_ref(&op)).resync);
        assert!(a.doc.nodes.is_empty());
        assert!(!a.replica.apply_patch(&mut a.doc, 2, 2, std::slice::from_ref(&op)).resync);
        let stale = a.replica.apply_patch(&mut a.doc, 2, 2, &[Op::DelNode("x".into())]);
        assert!(!stale.changed && a.doc.node("x").is_some(), "a version seen before is ignored");
    }

    #[test]
    fn fields_ops_and_keys() {
        let mut doc = Doc::default();
        assert!(set_field(&mut doc, "view", serde_json::json!({"x": 1, "y": 2})).is_err(), "per-client fields are not shared");
        set_field(&mut doc, "idle_stop_minutes", serde_json::json!(30)).unwrap();
        assert_eq!(doc.idle_stop_minutes, 30);
        assert!(apply_all(&mut doc, &[Op::PutNode(Box::new(node("a", 0))), Op::SetField("nodes".into(), Value::Null)]).is_err());
        assert!(doc.nodes.is_empty(), "a failed edit changes nothing");
        // Field edits of a box that no longer exists are dropped; unset fields are cleared.
        let mut moved = node("a", 4);
        moved.agent = Some("opencode".into());
        apply(&mut doc, &Op::NodeFields(Box::new(moved.clone()), vec!["x".into()])).unwrap();
        assert!(doc.nodes.is_empty());
        apply(&mut doc, &Op::PutNode(Box::new(moved.clone()))).unwrap();
        assert_eq!(doc.node("a"), Some(&moved));
        let mut plain = moved.clone();
        plain.agent = None;
        plain.x = 0;
        apply(&mut doc, &Op::NodeFields(Box::new(plain), vec!["agent".into()])).unwrap();
        assert_eq!((doc.node("a").unwrap().agent.as_deref(), doc.node("a").unwrap().x), (None, 4));
        let mut agent = node("s:k", 0);
        agent.kind = NodeKind::Agent;
        let ops = diff(&Doc::default(), &Doc { nodes: vec![agent], groups: vec![Group { id: "g".into(), w: 2, h: 2, ..Default::default() }], ..Doc::default() });
        let json = serde_json::to_string(&ops).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Op>>(&json).unwrap(), ops);
        // Keys are made where canvases live: on the Windows service.
        if cfg!(windows) {
            let key = doc_key(r"C:\Users\Me\.atelier\canvas.json");
            assert!(key.starts_with("canvas-") && key.len() == "canvas-".len() + 16);
            assert_eq!(key, doc_key("c:/users/me/.atelier/CANVAS.json"));
            assert_ne!(key, doc_key(r"C:\other\canvas.json"));
        }
        assert_eq!(stable_id("auto", "wf:1"), stable_id("auto", "wf:1"));
        let prefs = ClientPrefs { tiles: vec!["t".into()], ..ClientPrefs::default() };
        let mut target = Doc::default();
        prefs.apply(&mut target);
        assert_eq!(ClientPrefs::of(&target), prefs);
    }

    /// Kept changes as a file holds them.
    fn through_a_file(kept: Vec<Kept>) -> Vec<Kept> {
        serde_json::from_str(&serde_json::to_string(&kept).unwrap()).unwrap()
    }

    #[test]
    fn kept_changes_replay_only_where_the_canvas_still_holds_what_they_were_made_from() {
        let base = Doc { nodes: vec![node("a", 0), node("b", 0), node("c", 0), node("d", 0)], idle_stop_minutes: 60, automations_dir: None, ..Doc::default() };
        let server = Server::new(base.clone());
        let mut w = Window::open(&server, 1);
        // Offline: rename a, move b, create e, two settings, remove c and d.
        w.doc.node_mut("a").unwrap().name = "mine".into();
        w.doc.node_mut("b").unwrap().x = 5;
        w.doc.nodes.push(node("e", 9));
        w.doc.idle_stop_minutes = 30;
        w.doc.automations_dir = Some("C:\\mine".into());
        w.doc.nodes.retain(|n| n.id != "c" && n.id != "d");
        let kept = through_a_file(w.replica.kept(&w.doc));
        assert!(kept.iter().all(|k| !k.sent), "nothing was sent");
        // Since then, elsewhere: a renamed, d moved, b removed, the folder set.
        let mut now = base.clone();
        now.node_mut("a").unwrap().name = "theirs".into();
        now.node_mut("d").unwrap().x = 3;
        now.nodes.retain(|n| n.id != "b");
        now.automations_dir = Some("C:\\theirs".into());
        let replayed = replay(&mut now, &kept, &[]);
        assert_eq!(now.node("a").unwrap().name, "theirs", "a newer rename wins");
        assert!(now.node("b").is_none(), "a box removed elsewhere is never brought back");
        assert_eq!(now.node("e").map(|n| n.x), Some(9), "a box created offline is added");
        assert_eq!(now.idle_stop_minutes, 30, "a setting unchanged since takes the change");
        assert_eq!(now.automations_dir.as_deref(), Some("C:\\theirs"), "one changed since keeps that change");
        assert!(now.node("c").is_some() && now.node("d").is_some(), "removals are the caller's to make");
        assert_eq!(replayed.applied, 2);
        assert_eq!(replayed.changed, ["“d”", "“theirs”", "the automations folder"]);
        assert_eq!(replayed.gone, ["“b”"]);
        assert_eq!(replayed.removals.iter().map(|k| k.op.clone()).collect::<Vec<_>>(), [Op::DelNode("c".into())], "d changed since: it stays");
        assert_eq!(replayed.dropped(), 4);
    }

    #[test]
    fn kept_changes_the_service_had_change_nothing_and_a_box_sent_then_removed_stays_gone() {
        let server = Server::new(Doc { nodes: vec![node("a", 0)], ..Doc::default() });
        let mut w = Window::open(&server, 1);
        // Sent, never confirmed (the tunnel died); then the new box moved offline.
        w.doc.nodes.push(node("new", 1));
        w.doc.node_mut("a").unwrap().x = 4;
        let lost = w.take().unwrap();
        w.doc.node_mut("new").unwrap().x = 2;
        let kept = through_a_file(w.replica.kept(&w.doc));
        assert!(kept[0].sent && !kept.last().unwrap().sent, "sent ones first: {kept:?}");
        // The service had applied the edit: only the later move is new.
        let mut applied = Server::new(server.doc.clone());
        applied.edit(1, lost);
        let mut now = applied.doc.clone();
        let replayed = replay(&mut now, &kept, &[]);
        assert_eq!((replayed.applied, replayed.already, replayed.dropped()), (1, 2, 0), "{replayed:?}");
        assert_eq!((now.node("new").unwrap().x, now.node("a").unwrap().x), (2, 4));
        // The box was removed elsewhere since: not brought back, nor moved.
        let mut now = applied.doc.clone();
        now.nodes.retain(|n| n.id != "new");
        let replayed = replay(&mut now, &kept, &[]);
        assert!(now.node("new").is_none());
        assert_eq!(replayed.gone, ["“new”"]);
        // The edit never arrived: the move of a is made; the box, which may as
        // well have been removed, is not brought back (the notice names it).
        let mut now = server.doc.clone();
        let replayed = replay(&mut now, &kept, &[]);
        assert_eq!(now.node("a").unwrap().x, 4);
        assert!(now.node("new").is_none() && replayed.gone == ["“new”"]);
    }

    #[test]
    fn a_kept_removal_takes_its_subagents_and_links_only_when_it_is_made() {
        let mut agent = node("p:x", 0);
        agent.kind = NodeKind::Agent;
        agent.parent = Some("p".into());
        let mut sub = node("s", 0);
        sub.parent = Some("p".into());
        let server = Server::new(Doc { nodes: vec![node("p", 0), agent, sub], ..Doc::default() });
        let mut w = Window::open(&server, 1);
        w.doc.remove_node("p");
        let kept = through_a_file(w.replica.kept(&w.doc));
        let mut now = server.doc.clone();
        let replayed = replay(&mut now, &kept, &[]);
        assert_eq!(replayed.removals.len(), 1);
        assert_eq!((now.nodes.len(), now.node("s").unwrap().parent.as_deref()), (3, Some("p")), "nothing is cut before the removal is made");
        assert_eq!((replayed.applied, replayed.dropped()), (0, 0));
    }

    #[test]
    fn an_older_versions_kept_changes_set_fields_of_boxes_that_exist_and_nothing_else() {
        let mut now = Doc { nodes: vec![node("a", 0), node("b", 0)], ..Doc::default() };
        let legacy =
            vec![Op::NodeFields(Box::new(node("a", 7)), vec!["x".into()]), Op::PutNode(Box::new(node("gone", 1))), Op::DelNode("b".into()), Op::SetField("idle_stop_minutes".into(), serde_json::json!(5))];
        let replayed = replay(&mut now, &[], &legacy);
        assert_eq!(now.node("a").unwrap().x, 7);
        assert!(now.node("gone").is_none() && now.node("b").is_some() && now.idle_stop_minutes != 5);
        assert_eq!((replayed.applied, replayed.unchecked), (1, 3));
    }
}
