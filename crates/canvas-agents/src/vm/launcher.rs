//! `vm::Launcher` (docs/VM-SESSIONS.md §4.4): every VM box's launch is a
//! durable intent (`<home>/vm/launches/<box>.json`, written by the service's
//! launch handler) that this state machine advances on the service's VM
//! thread, one idempotent step at a time:
//!
//! `queued` (a budget) → `claiming` (a pool member leased, or a new sandbox)
//! → `starting` (its satellite credential minted and written, its branch
//! prepared) → `running` (the box's terminal joins it).
//!
//! A step's result is written only when the intent's fence is unchanged (a
//! cancel or retry bumps it); a sandbox claimed for a launch that was
//! canceled meanwhile is deleted (rollback). Three failed steps in a row fail
//! the launch, and its sandbox goes. At start the service reconciles: its own
//! sandboxes (label `atelier.hub`) the provider no longer has make their
//! boxes *lost*; sandboxes it has but the store does not are listed as
//! orphans; builders left by a crash are deleted.

use std::path::Path;

use anyhow::{Context, Result};
use canvas_model::vm::VmState;

use super::launch::{self, Intent};
use super::pack::Pack;
use super::provider::Provider;
use super::store::{self, Role, SandboxRecord, Satellites};
use crate::paths::now_ms;

/// Failed steps in a row that fail a launch.
pub const MAX_ATTEMPTS: u32 = 3;

pub struct Ctx<'a> {
    pub home: &'a Path,
    pub pack: &'a Pack,
    pub provider: &'a dyn Provider,
    pub hub: &'a str,
    pub log: &'a dyn Fn(String),
}

/// Whether an intent has a step to take.
pub fn wants_work(intent: &Intent) -> bool {
    !intent.pending.is_empty() || matches!(intent.state, VmState::Queued | VmState::Claiming | VmState::Starting | VmState::Resuming | VmState::Suspending | VmState::Deleting)
}

/// Takes the next step of box `id`'s launch (`Ok(false)`: nothing to do).
pub fn advance(ctx: &Ctx, id: &str) -> Result<bool> {
    let Some(intent) = launch::load(ctx.home, id)? else { return Ok(false) };
    if !wants_work(&intent) {
        return Ok(false);
    }
    let fence = intent.fence;
    let result = step(ctx, &intent);
    let outcome = launch::update(ctx.home, id, |current| {
        if current.fence != fence {
            // Canceled or retried meanwhile: this step's result is not its.
            return Err(result.as_ref().ok().and_then(|change| change.claimed.clone()));
        }
        match &result {
            Ok(change) => {
                change.apply(current);
                current.attempts = 0;
            }
            Err(error) => {
                current.attempts += 1;
                current.error = format!("{error:#}");
                if current.attempts >= MAX_ATTEMPTS {
                    current.state = VmState::Failed;
                    current.pending.clear();
                }
            }
        }
        Ok(())
    })?;
    match outcome {
        Some(Err(Some(sandbox))) => {
            (ctx.log)(format!("VM launch of {id} was canceled while {sandbox} was claimed: deleting it"));
            release(ctx, &sandbox);
        }
        Some(Ok(())) => {
            if let Err(error) = &result {
                (ctx.log)(format!("VM launch of {id}: {}", super::hooks::first_line(&format!("{error:#}"))));
                // A failed launch gives its sandbox back.
                if let Some(failed) = launch::load(ctx.home, id)?.filter(|i| i.state == VmState::Failed && !i.sandbox.is_empty()) {
                    release(ctx, &failed.sandbox);
                    let _ = launch::update(ctx.home, id, |i| i.sandbox.clear());
                }
            }
        }
        _ => {}
    }
    Ok(true)
}

/// What one step changed.
#[derive(Default)]
struct Change {
    state: Option<VmState>,
    sandbox: Option<String>,
    /// A sandbox this step claimed (deleted when the step's result is dropped).
    claimed: Option<String>,
    satellite: Option<String>,
    cwd: Option<String>,
    note: Option<String>,
    queued: bool,
    pending_done: bool,
}

impl Change {
    fn apply(&self, intent: &mut Intent) {
        if let Some(state) = self.state {
            intent.state = state;
        }
        if let Some(sandbox) = &self.sandbox {
            intent.sandbox = sandbox.clone();
        }
        if let Some(satellite) = &self.satellite {
            intent.satellite = satellite.clone();
        }
        if let Some(cwd) = &self.cwd {
            intent.cwd = cwd.clone();
        }
        if let Some(note) = &self.note {
            intent.note = note.clone();
        }
        if self.queued && intent.queued_ms == 0 {
            intent.queued_ms = now_ms();
        }
        if self.pending_done {
            intent.pending.clear();
        }
        if intent.state != VmState::Failed {
            intent.error.clear();
        }
    }
}

fn step(ctx: &Ctx, intent: &Intent) -> Result<Change> {
    match (intent.pending.as_str(), intent.state) {
        ("delete", _) | (_, VmState::Deleting) => delete(ctx, intent),
        ("suspend", VmState::Running) | (_, VmState::Suspending) => {
            ctx.provider.suspend(&intent.sandbox)?;
            set_suspended(ctx.home, &intent.sandbox, true)?;
            Ok(Change { state: Some(VmState::Suspended), pending_done: true, ..Default::default() })
        }
        ("resume", VmState::Suspended) | (_, VmState::Resuming) => {
            ctx.provider.resume(&intent.sandbox)?;
            set_suspended(ctx.home, &intent.sandbox, false)?;
            Ok(Change { state: Some(VmState::Running), pending_done: true, ..Default::default() })
        }
        (_, VmState::Queued | VmState::Claiming) if intent.sandbox.is_empty() => claim(ctx, intent),
        (_, VmState::Claiming | VmState::Starting) => start(ctx, intent),
        // An operation that does not apply to this state.
        _ => Ok(Change { pending_done: true, ..Default::default() }),
    }
}

fn set_suspended(home: &Path, sandbox: &str, suspended: bool) -> Result<()> {
    let mut file = store::load_sandboxes(home)?;
    if let Some(record) = file.sandboxes.iter_mut().find(|s| s.id == sandbox) {
        record.suspended = suspended;
        record.used_ms = now_ms();
        if !suspended {
            record.running_since_ms = now_ms();
        }
        store::save_sandboxes(home, &mut file)?;
    }
    Ok(())
}

/// A VM for the box: within the budget, a leased pool member of its
/// repository's newest image when one is ready, else a new sandbox.
fn claim(ctx: &Ctx, intent: &Intent) -> Result<Change> {
    let pack = ctx.pack;
    let repo = pack.repo(&intent.placement.repo).with_context(|| format!("the VM pack has no repository {:?}", intent.placement.repo))?;
    let size = pack.size(intent.placement.size.as_deref()).context("the VM pack has no such size")?.name.clone();
    let branch = intent.placement.branch.clone().unwrap_or_else(|| canvas_model::vm::branch_for_session(&intent.r#box));
    anyhow::ensure!(canvas_model::vm::branch_ok(&branch), "{branch:?} is not a branch name");
    let mut sandboxes = store::load_sandboxes(ctx.home)?;
    let today = store::load_budget(ctx.home)?.today(now_ms());
    if let Err(why) = super::manager::admit(pack, &sandboxes, &size, today) {
        return Ok(Change { state: Some(VmState::Queued), note: Some(why), queued: true, ..Default::default() });
    }
    let images = store::load_images(ctx.home)?;
    let newest = images.newest(&repo.key).map(|i| i.id.clone());
    let leased = newest.as_ref().and_then(|image| sandboxes.sandboxes.iter_mut().find(|s| s.role == Role::Pool && s.repo == repo.key && s.size == size && s.image == *image));
    let record = match leased {
        Some(member) => {
            member.role = Role::Session;
            member.boxes = vec![intent.r#box.clone()];
            let record = member.clone();
            store::save_sandboxes(ctx.home, &mut sandboxes)?;
            (ctx.log)(format!("VM launch of {}: leased pool member {}", intent.r#box, record.id));
            if record.suspended {
                ctx.provider.resume(&record.id)?;
                set_suspended(ctx.home, &record.id, false)?;
            }
            record
        }
        None => {
            if ctx.provider.name() != "hooks" && newest.is_none() {
                // The pack's first image of this repository: the manager builds it.
                let mut images = store::load_images(ctx.home)?;
                images.last_build_ms.remove(&repo.key);
                if !images.images.iter().any(|i| i.repo == repo.key) {
                    images.images.push(store::ImageRecord { repo: repo.key.clone(), state: "requested".into(), ..Default::default() });
                }
                store::save_images(ctx.home, &mut images)?;
                return Ok(Change { state: Some(VmState::Queued), note: Some(format!("waiting for the first image of {}", repo.key)), queued: true, ..Default::default() });
            }
            let mut labels = super::manager::labels(ctx.hub, Role::Session, &repo.key);
            labels.insert("atelier.box".into(), intent.r#box.clone());
            if ctx.provider.name() == "hooks" {
                // The claim hook clones the repository on the branch itself.
                for (k, v) in [("atelier.url", &repo.url), ("atelier.branch", &branch), ("atelier.base", &repo.default_branch), ("atelier.size", &size)] {
                    labels.insert(k.into(), v.clone());
                }
            }
            let sandbox = ctx.provider.create(&super::manager::spec(pack, &size, newest.clone(), labels)?)?;
            let now = now_ms();
            let record = SandboxRecord { id: sandbox.id.clone(), role: Role::Session, repo: repo.key.clone(), size: size.clone(), image: newest.unwrap_or_default(), created_ms: now, boxes: vec![intent.r#box.clone()], running_since_ms: now, used_ms: now, ..Default::default() };
            let mut file = store::load_sandboxes(ctx.home)?;
            file.sandboxes.push(record.clone());
            store::save_sandboxes(ctx.home, &mut file)?;
            (ctx.log)(format!("VM launch of {}: sandbox {} ({}, {size})", intent.r#box, record.id, repo.key));
            if let Some(cwd) = sandbox.labels.get("atelier.cwd") {
                return Ok(Change { state: Some(VmState::Starting), sandbox: Some(record.id.clone()), claimed: Some(record.id), cwd: Some(cwd.clone()), note: Some(String::new()), ..Default::default() });
            }
            record
        }
    };
    Ok(Change { state: Some(VmState::Starting), sandbox: Some(record.id.clone()), claimed: Some(record.id), note: Some(String::new()), ..Default::default() })
}

/// A claimed sandbox made ready: its satellite credential (written to the
/// guest's `~/.atelier/satellite.json`) and, from an image, its branch.
fn start(ctx: &Ctx, intent: &Intent) -> Result<Change> {
    let mut change = Change { state: Some(VmState::Running), ..Default::default() };
    let hooks = ctx.provider.name() == "hooks";
    if intent.satellite.is_empty() {
        let minted = Satellites::new(ctx.home).mint(&intent.sandbox)?;
        let body = serde_json::json!({ "hub": ctx.pack.tunnel.url, "id": minted.id, "secret": minted.secret, "sandbox": intent.sandbox });
        match ctx.provider.put_file(&intent.sandbox, ".atelier/satellite.json", serde_json::to_string(&body)?.as_bytes()) {
            Ok(()) => {}
            // The thin lane does not need it: its guest may have no Atelier yet.
            Err(error) if hooks => (ctx.log)(format!("VM launch of {}: no satellite credential in the guest ({})", intent.r#box, super::hooks::first_line(&format!("{error:#}")))),
            Err(error) => return Err(error),
        }
        change.satellite = Some(minted.id);
    }
    if !hooks {
        let repo = ctx.pack.repo(&intent.placement.repo).context("the VM pack has no such repository")?;
        let branch = intent.placement.branch.clone().unwrap_or_else(|| canvas_model::vm::branch_for_session(&intent.r#box));
        let argv: Vec<String> = ["atelier", "satellite", "--prepare", "--repo", &repo.key, "--branch", &branch, "--base", &repo.default_branch].map(str::to_owned).to_vec();
        let out = ctx.provider.exec(&intent.sandbox, &argv, b"")?;
        let answer: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        anyhow::ensure!(out.code == 0 && answer["ok"] == true, "preparing {branch} failed: {}", super::hooks::first_line(&String::from_utf8_lossy(&out.stdout)));
        change.cwd = answer["path"].as_str().map(str::to_owned);
    }
    (ctx.log)(format!("VM launch of {}: ready in {}", intent.r#box, intent.sandbox));
    Ok(change)
}

/// Deletes a box's sandbox (Phase 4 puts the quiescence barrier before it):
/// the provider's, its credentials revoked, its record and mirror gone.
fn delete(ctx: &Ctx, intent: &Intent) -> Result<Change> {
    if !intent.sandbox.is_empty() {
        release(ctx, &intent.sandbox);
    }
    Ok(Change { state: Some(VmState::Deleted), pending_done: true, ..Default::default() })
}

/// A sandbox goes: deleted on the provider, its credentials revoked, its record dropped.
pub fn release(ctx: &Ctx, sandbox: &str) {
    if let Err(error) = ctx.provider.delete(sandbox) {
        (ctx.log)(format!("VM {sandbox}: delete failed: {}", super::hooks::first_line(&format!("{error:#}"))));
    }
    let _ = Satellites::new(ctx.home).revoke_sandbox(sandbox);
    if let Ok(mut file) = store::load_sandboxes(ctx.home) {
        file.sandboxes.retain(|s| s.id != sandbox);
        let _ = store::save_sandboxes(ctx.home, &mut file);
    }
}

/// At start: this hub's sandboxes as the provider has them against the
/// store. Returns (lost boxes, orphans found, builders deleted).
pub fn reconcile(ctx: &Ctx) -> Result<(usize, usize, usize)> {
    if ctx.provider.name() == "hooks" {
        // A hooks pack cannot list its sandboxes: the store is all there is.
        return Ok((0, 0, 0));
    }
    let hub_label = [("atelier.hub".to_owned(), ctx.hub.to_owned())].into_iter().collect();
    let live = ctx.provider.list(&hub_label)?;
    let mut file = store::load_sandboxes(ctx.home)?;
    let (mut lost, mut orphans, mut builders) = (0, 0, 0);
    let gone: Vec<SandboxRecord> = file.sandboxes.iter().filter(|s| !live.iter().any(|l| l.id == s.id)).cloned().collect();
    for record in &gone {
        for intent in launch::all(ctx.home).into_iter().filter(|i| i.sandbox == record.id && !matches!(i.state, VmState::Deleted | VmState::Failed)) {
            let _ = launch::update(ctx.home, &intent.r#box, |i| {
                i.state = VmState::Lost;
                i.note = "its VM is gone (its work is in its pushed branch)".into();
            });
            lost += 1;
        }
        let _ = Satellites::new(ctx.home).revoke_sandbox(&record.id);
    }
    file.sandboxes.retain(|s| !gone.iter().any(|g| g.id == s.id));
    for sandbox in &live {
        if file.sandboxes.iter().any(|s| s.id == sandbox.id) {
            continue;
        }
        if sandbox.labels.get("atelier.role").map(String::as_str) == Some("builder") {
            let _ = ctx.provider.delete(&sandbox.id);
            builders += 1;
            continue;
        }
        file.sandboxes.push(SandboxRecord { id: sandbox.id.clone(), role: Role::Orphan, repo: sandbox.labels.get("atelier.repo").cloned().unwrap_or_default(), suspended: sandbox.suspended(), created_ms: now_ms(), ..Default::default() });
        orphans += 1;
    }
    store::save_sandboxes(ctx.home, &mut file)?;
    if lost + orphans + builders > 0 {
        (ctx.log)(format!("VM reconciliation: {lost} box(es) lost their VM, {orphans} orphan sandbox(es) listed, {builders} builder(s) deleted"));
    }
    Ok((lost, orphans, builders))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::fake::FakeProvider;
    use super::super::pack::{Budget, Pack, ProviderKind, Repo, Size};

    fn pack(provider: ProviderKind) -> Pack {
        Pack {
            provider,
            sizes: vec![Size { name: "S".into(), cpu_milli: 1000, memory_mi: 2048, disk_gi: 20, cents_per_hour: 6 }],
            budget: Budget { max_running: 1, max_cores: 8.0, daily_cents: 0 },
            repos: vec![Repo { key: "app".into(), url: "https://example.com/app.git".into(), default_branch: "main".into(), ..Default::default() }],
            ..Default::default()
        }
    }

    fn intent(home: &Path, id: &str) {
        let mut intent = Intent { r#box: id.into(), agent: "copilot".into(), placement: canvas_model::vm::VmPlacement { repo: "app".into(), branch: Some(format!("atelier/{id}")), ..Default::default() }, state: VmState::Claiming, ..Default::default() };
        launch::store(home, &mut intent).unwrap();
    }

    fn run(ctx: &Ctx, id: &str) -> Intent {
        for _ in 0..10 {
            if !advance(ctx, id).unwrap() {
                break;
            }
        }
        launch::load(ctx.home, id).unwrap().unwrap()
    }

    #[test]
    fn launches_claim_start_queue_on_budget_and_delete() {
        let home = std::env::temp_dir().join(format!("atelier-launcher-{}", uuid::Uuid::new_v4()));
        let provider = FakeProvider::new("fake");
        let pack = pack(ProviderKind::Local);
        let log = |_: String| {};
        let ctx = Ctx { home: &home, pack: &pack, provider: &provider, hub: "hub1", log: &log };
        // The first image is asked for, then the launch goes on with it.
        intent(&home, "b1");
        let waiting = run(&ctx, "b1");
        assert_eq!((waiting.state, waiting.note.as_str()), (VmState::Queued, "waiting for the first image of app"));
        let mut images = store::load_images(&home).unwrap();
        images.images = vec![store::ImageRecord { repo: "app".into(), id: "img-1".into(), built_ms: 1, state: "ready".into(), ..Default::default() }];
        store::save_images(&home, &mut images).unwrap();
        let ready = run(&ctx, "b1");
        assert_eq!(ready.state, VmState::Running, "{ready:?}");
        assert!(ready.satellite.starts_with("sat-") && ready.cwd.ends_with("work/app"));
        assert!(provider.files(&ready.sandbox).contains(&".atelier/satellite.json".to_owned()), "the credential reaches the guest");
        assert!(provider.calls().iter().any(|c| c.contains("--prepare") && c.contains("atelier/b1")), "{:?}", provider.calls());
        // The budget (one running VM) queues a second launch and says why.
        intent(&home, "b2");
        let queued = run(&ctx, "b2");
        assert_eq!(queued.state, VmState::Queued);
        assert!(queued.note.contains("1 of 1 VMs are running"));
        // Suspending the first lets the second start.
        launch::update(&home, "b1", |i| i.pending = "suspend".into()).unwrap();
        assert_eq!(run(&ctx, "b1").state, VmState::Suspended);
        assert_eq!(run(&ctx, "b2").state, VmState::Running);
        // Delete: gone from the provider, credentials revoked.
        launch::update(&home, "b2", |i| i.pending = "delete".into()).unwrap();
        let sandbox = launch::load(&home, "b2").unwrap().unwrap().sandbox;
        let satellite = launch::load(&home, "b2").unwrap().unwrap().satellite;
        assert_eq!(run(&ctx, "b2").state, VmState::Deleted);
        assert!(!provider.exists(&sandbox));
        assert!(matches!(Satellites::new(&home).credential(&satellite), canvas_common::handshake::Credential::Revoked(_)));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn failures_retry_then_fail_and_cancels_roll_back() {
        let home = std::env::temp_dir().join(format!("atelier-launcher-{}", uuid::Uuid::new_v4()));
        let provider = FakeProvider::new("hooks");
        let pack = pack(ProviderKind::Hooks);
        let log = |_: String| {};
        let ctx = Ctx { home: &home, pack: &pack, provider: &provider, hub: "hub1", log: &log };
        provider.fail("create", 5);
        intent(&home, "b1");
        let failed = run(&ctx, "b1");
        assert_eq!((failed.state, failed.attempts), (VmState::Failed, MAX_ATTEMPTS));
        assert!(failed.error.contains("injected"));
        provider.fail("create", 0);
        // A cancel (the fence moves) while a sandbox is being claimed: the sandbox goes.
        intent(&home, "b2");
        provider.on_create(Box::new({
            let home = home.clone();
            move || {
                let _ = launch::update(&home, "b2", |i| i.fence += 1);
            }
        }));
        advance(&ctx, "b2").unwrap();
        assert!(provider.live().is_empty(), "the canceled claim's sandbox was deleted");
        assert_eq!(launch::load(&home, "b2").unwrap().unwrap().sandbox, "");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn reconciliation_marks_lost_boxes_lists_orphans_and_ends_builders() {
        let home = std::env::temp_dir().join(format!("atelier-reconcile-{}", uuid::Uuid::new_v4()));
        let provider = FakeProvider::new("fake");
        let pack = pack(ProviderKind::Local);
        let log = |_: String| {};
        let ctx = Ctx { home: &home, pack: &pack, provider: &provider, hub: "hub1", log: &log };
        let mut file = store::load_sandboxes(&home).unwrap();
        file.sandboxes.push(SandboxRecord { id: "gone".into(), role: Role::Session, ..Default::default() });
        store::save_sandboxes(&home, &mut file).unwrap();
        let mut gone = Intent { r#box: "b1".into(), sandbox: "gone".into(), state: VmState::Running, ..Default::default() };
        launch::store(&home, &mut gone).unwrap();
        let labels = |role: &str| -> super::super::provider::Labels { [("atelier.hub", "hub1"), ("atelier.role", role)].into_iter().map(|(k, v)| (k.into(), v.into())).collect() };
        provider.add("orphan", labels("session"));
        provider.add("builder", labels("builder"));
        provider.add("foreign", [("atelier.hub".to_owned(), "other".to_owned())].into_iter().collect());
        assert_eq!(reconcile(&ctx).unwrap(), (1, 1, 1));
        assert_eq!(launch::load(&home, "b1").unwrap().unwrap().state, VmState::Lost);
        let roles: Vec<(String, Role)> = store::load_sandboxes(&home).unwrap().sandboxes.into_iter().map(|s| (s.id, s.role)).collect();
        assert_eq!(roles, [("orphan".to_owned(), Role::Orphan)], "never another hub's");
        assert!(provider.exists("orphan") && !provider.exists("builder") && provider.exists("foreign"), "orphans are never deleted on their own");
        let _ = std::fs::remove_dir_all(home);
    }
}
