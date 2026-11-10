//! VM sessions on the service's side (docs/VM-SESSIONS.md, the plan in
//! docs/VM-SESSIONS-PLAN.md): the VM pack, providers, the manager and the
//! launcher, the satellite link and the satellite itself.
//!
//! VM sessions are optional; local stays the default. Without a pack
//! (`settings.json` `vm.pack`, `ATELIER_VM_PACK`) nothing here runs and no
//! VM UI shows: the bus snapshot carries no `vm` view and the requests say
//! that no pack is configured.

pub mod aca;
pub mod egress;
#[cfg(test)]
mod fake;
pub mod flood;
pub mod guest;
pub mod hooks;
pub mod launch;
pub mod launcher;
pub mod link;
pub mod lite;
pub mod local;
pub mod manager;
pub mod mcp;
pub mod pack;
pub mod paths;
pub mod profile;
pub mod provider;
pub mod refusals;
pub mod satellite;
pub mod secrets;
pub mod session_store;
pub mod spike;
pub mod store;
pub mod sync;
pub mod target;
pub mod thin;
pub mod tls;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use canvas_model::rpc::{Request, Response};
use canvas_model::vm::{BudgetView, ImageRow, PackStatus, QueuedStart, SandboxRow, VmInfo, VmOp, VmState, VmView};

use crate::paths::now_ms;

/// A check of a sandbox's repositories now (`Links::git_check`; `None`: not
/// linked, or an older satellite): the delete barrier's.
pub type GitCheck<'a> = &'a dyn Fn(&str) -> Option<Vec<canvas_common::sat::RepoStats>>;

/// What the service answers a VM request (`Request::is_service_state`).
pub fn service_request(home: &Path, request: Request) -> Response {
    service_request_with(home, request, &|_| None)
}

/// [`service_request`] with the service's links (a delete checks the VM's
/// repositories now when it is linked).
pub fn service_request_with(home: &Path, request: Request, git: GitCheck) -> Response {
    let pack = pack::configured(home);
    match (request, pack) {
        (Request::VmPackStatus, None) => Response::VmPack(Box::default()),
        (Request::VmPackStatus, Some(folder)) => Response::VmPack(Box::new(pack::load(&folder).1)),
        (Request::VmList | Request::VmPool | Request::VmImages | Request::VmBudget, Some(_)) => Response::Vm(Box::new(view(home).unwrap_or_default())),
        (Request::VmAction { r#box, op }, Some(_)) => action_with(home, &r#box, op, git).map_or_else(|e| Response::Error(format!("{e:#}")), Response::Text),
        (Request::VmImageBuild { repo }, Some(_)) => request_image(home, &repo).map_or_else(|e| Response::Error(format!("{e:#}")), Response::Text),
        (request, None) => Response::Error(format!("{}: no VM pack is configured (settings.json vm.pack or ATELIER_VM_PACK); VM sessions are off", request.name())),
        (request, Some(_)) => Response::Error(format!("{} is not a VM request", request.name())),
    }
}

/// An operation on a box's VM (windows' bus actions, `Request::VmAction`,
/// `atelier vm suspend|resume|delete|cancel|retry`): recorded on its intent
/// and done by the service's VM loop.
pub fn action(home: &Path, id: &str, op: VmOp) -> anyhow::Result<String> {
    action_with(home, id, op, &|_| None)
}

/// [`action`] with a check of the VM's repositories now (the delete barrier).
pub fn action_with(home: &Path, id: &str, op: VmOp, git: GitCheck) -> anyhow::Result<String> {
    let (pack, _) = loaded(home).ok_or_else(|| anyhow::anyhow!("no VM pack is configured"))?;
    if op == VmOp::UpdateFromMain {
        return update_from_main(home, &pack, id);
    }
    // The quiescence barrier: what the VM's repositories hold and have not pushed, checked now when it is linked,
    // else as they last reported (a suspended VM's were checked as it was suspended).
    if let VmOp::Delete { confirm } = &op
        && let Some(intent) = launch::load(home, id)?
        && !intent.sandbox.is_empty()
    {
        let (repos, when) = match git(&intent.sandbox) {
            Some(repos) => (repos, "now".to_owned()),
            None => match link::load_record(home, &intent.sandbox).filter(|r| r.stats_ms > 0) {
                Some(r) => (r.stats.repos, format!("as it reported {} ago", canvas_common::paths::format_age(r.stats_ms, now_ms()))),
                None => (Vec::new(), String::new()),
            },
        };
        let findings = barrier_findings(&repos, repos.len() > 1);
        let accepted = confirm.iter().any(|c| c == canvas_model::vm::DISCARD_ALL) || findings.iter().all(|f| confirm.contains(f));
        if !accepted {
            anyhow::bail!("{}", canvas_model::vm::barrier_refusal(&findings, &when));
        }
    }
    let done = launch::update(home, id, |intent| -> anyhow::Result<String> {
        let busy = matches!(intent.state, VmState::Deleted | VmState::Deleting);
        Ok(match op {
            VmOp::Suspend if intent.state == VmState::Running => {
                intent.pending = "suspend".into();
                "Suspending its VM.".into()
            }
            VmOp::Resume if matches!(intent.state, VmState::Suspended | VmState::Suspending) => {
                intent.pending = "resume".into();
                "Resuming its VM.".into()
            }
            VmOp::Delete { .. } if !busy => {
                intent.pending = "delete".into();
                format!("Deleting its VM{}.", if intent.sandbox.is_empty() { "" } else { " (its sandbox and satellite credential)" })
            }
            VmOp::CancelStart if matches!(intent.state, VmState::Queued | VmState::Claiming | VmState::Starting) => {
                intent.fence += 1;
                if intent.sandbox.is_empty() {
                    intent.state = VmState::Deleted;
                } else {
                    intent.pending = "delete".into();
                }
                "Start canceled.".into()
            }
            VmOp::RetryStart if matches!(intent.state, VmState::Failed | VmState::Lost) => {
                // Its conversation comes along when the hub mirrored it (a resume on a new VM).
                let mirror = thin::mirror_dir(home, &intent.sandbox);
                let claude = std::fs::read_dir(mirror.join("claude").join("projects")).into_iter().flatten().flatten().any(|e| e.path().join(format!("{}.jsonl", intent.r#box)).is_file());
                let mirrored = claude || mirror.join("copilot").join(&intent.r#box).join("events.jsonl").is_file();
                (intent.resume_from, intent.resume_session) = if !intent.sandbox.is_empty() && mirrored { (intent.sandbox.clone(), intent.r#box.clone()) } else { Default::default() };
                intent.fence += 1;
                intent.state = VmState::Claiming;
                intent.sandbox.clear();
                intent.satellite.clear();
                intent.attempts = 0;
                intent.error.clear();
                intent.queued_ms = now_ms();
                if intent.resume_session.is_empty() { "Starting it again on a new VM.".into() } else { "Starting it again on a new VM, with its conversation from this machine's mirror.".into() }
            }
            VmOp::Shell => anyhow::bail!("a shell in the VM comes with a later phase (docs/VM-STATUS.md)"),
            other => anyhow::bail!("{} is not possible while its VM is {}", serde_json::to_value(&other).ok().and_then(|v| v["op"].as_str().map(str::to_owned)).unwrap_or_default(), intent.state.word()),
        })
    })?;
    done.ok_or_else(|| anyhow::anyhow!("box {id} does not run on a VM"))?
}

/// What deleting a VM would lose, over all its repositories (each finding
/// names its repository's folder when there are several).
fn barrier_findings(repos: &[canvas_common::sat::RepoStats], name_them: bool) -> Vec<String> {
    let mut out = Vec::new();
    for g in repos {
        let state = canvas_model::vm::GitState { branch: g.branch.clone(), ahead: g.ahead, behind: g.behind, dirty: g.dirty, untracked: g.untracked, stashes: g.stashes, unpushed: g.unpushed.clone() };
        let folder = g.cwd.rsplit(['/', '\\']).next().unwrap_or_default();
        out.extend(canvas_model::vm::delete_findings(&state).into_iter().map(|f| if name_them { format!("{folder}: {f}") } else { f }));
    }
    out
}

/// *Update from main*: asks the box's agent (a message typed in when it is
/// idle) to bring in its repository's default branch, never to push.
fn update_from_main(home: &Path, pack: &pack::Pack, id: &str) -> anyhow::Result<String> {
    let intent = launch::load(home, id)?.ok_or_else(|| anyhow::anyhow!("box {id} does not run on a VM"))?;
    anyhow::ensure!(matches!(intent.state, VmState::Running | VmState::Suspended | VmState::Suspending | VmState::Resuming), "its VM is {}", intent.state.word());
    let base = pack.repo(&intent.placement.repo).map(|r| r.default_branch.clone()).filter(|b| !b.is_empty()).unwrap_or_else(|| "main".into());
    let text = format!("Please fetch origin/{base} and bring it into this branch (merge or rebase, as you see fit for this branch); resolve conflicts if any. Do not push without asking me.");
    let event = canvas_model::bus::Event { kind: "message".into(), to: id.to_owned(), data: serde_json::json!({ "text": text }), ..Default::default() };
    canvas_model::bus::post(home, &event)?;
    if matches!(intent.state, VmState::Suspended | VmState::Suspending) {
        let _ = launch::update(home, id, |i| i.pending = "resume".into());
    }
    Ok(format!("Asked the agent to bring in origin/{base} when it is idle (it never pushes without asking)."))
}

/// Asks for `repo`'s image now (built by the VM loop).
pub fn request_image(home: &Path, repo: &str) -> anyhow::Result<String> {
    let (pack, _) = loaded(home).ok_or_else(|| anyhow::anyhow!("no VM pack is configured"))?;
    anyhow::ensure!(pack.repo(repo).is_some(), "the VM pack has no repository {repo:?}");
    let mut images = store::load_images(home)?;
    images.last_build_ms.remove(repo);
    if !images.images.iter().any(|i| i.repo == repo && i.state == "requested") {
        images.images.push(store::ImageRecord { repo: repo.to_owned(), state: "requested".into(), built_ms: now_ms(), ..Default::default() });
    }
    store::save_images(home, &mut images)?;
    Ok(format!("The image of {repo} is built next (the VMs view shows its progress)."))
}

/// The provider of a pack for the service in `home`: `local` keeps its
/// sandboxes in `local.root`, else `<home>/vm/local`; `aca` gets its tokens
/// from `auth.command` (else the Azure CLI) when a call first needs one.
pub fn open_provider(home: &Path, pack: &pack::Pack) -> anyhow::Result<Arc<dyn provider::Provider>> {
    Ok(match pack.provider {
        pack::ProviderKind::Hooks => Arc::new(hooks::HooksProvider { hooks: hooks::Hooks::of(pack) }),
        // Each local sandbox gets an egress stand-in enforcing its policy; the hub's own host goes direct.
        pack::ProviderKind::Local => Arc::new(local::Local::new(local_root(home, pack)).with_egress(&egress::hub_host(pack).unwrap_or_default())),
        pack::ProviderKind::Aca => Arc::new(aca::Aca::from_pack(pack)?),
    })
}

/// Where `vm::local` keeps the sandboxes of the service in `home`.
pub fn local_root(home: &Path, pack: &pack::Pack) -> PathBuf {
    pack.local_root.clone().unwrap_or_else(|| home.join("vm").join("local"))
}

/// What the box's satellite last said: since when it is away (a running
/// box whose satellite linked once), and its repository's state.
fn with_link(mut info: VmInfo, intent: &launch::Intent, record: Option<link::LinkRecord>) -> VmInfo {
    let Some(record) = record else { return info };
    if !record.linked && intent.state == VmState::Running {
        info.offline_ms = record.since_ms;
        info.state = VmState::IntegrationOffline;
    }
    if record.stats_ms > 0
        && let Some(repo) = record.stats.repos.iter().find(|r| r.term == intent.r#box).or(record.stats.repos.first())
    {
        info.git = Some(canvas_model::vm::GitState { branch: repo.branch.clone(), ahead: repo.ahead, behind: repo.behind, dirty: repo.dirty, untracked: repo.untracked, stashes: repo.stashes, unpushed: repo.unpushed.clone() });
    }
    info
}

/// `atelier doctor`'s VM lines (none without a pack): (passed, label, detail);
/// `None`: information.
pub fn doctor_lines(home: &Path) -> Vec<(Option<bool>, &'static str, String)> {
    let Some(folder) = pack::configured(home) else { return Vec::new() };
    let (pack, status) = pack::load(&folder);
    let mut out = vec![(Some(status.errors() == 0), "VM pack", format!("{} ({} error(s), {} warning(s); `atelier vm pack check`)", folder.display(), status.errors(), status.findings.iter().filter(|f| f.level == canvas_model::vm::Level::Warning).count()))];
    let provider = match pack.provider {
        pack::ProviderKind::Aca => "aca",
        pack::ProviderKind::Hooks => "hooks",
        pack::ProviderKind::Local => "local",
    };
    let opened = open_provider(home, &pack);
    out.push((Some(opened.is_ok()), "VM provider", format!("{provider}{}", opened.err().map(|e| format!(": {e:#}")).unwrap_or_default())));
    for (name, read) in [("sandboxes", store::load_sandboxes(home).err()), ("images", store::load_images(home).err())] {
        if let Some(error) = read {
            out.push((Some(false), "VM store", format!("{name}: {error:#}")));
        }
    }
    let intents = launch::all(home);
    let running = intents.iter().filter(|i| i.state == VmState::Running).count();
    let queued = intents.iter().filter(|i| i.state == VmState::Queued).count();
    let failed = intents.iter().filter(|i| i.state == VmState::Failed).count();
    out.push((None, "VM sessions", format!("{running} running, {queued} queued, {failed} failed")));
    let listen = canvas_common::settings::Settings::load(home).listen();
    if pack.provider != pack::ProviderKind::Hooks {
        out.push((Some(listen.is_some() && !pack.tunnel.url.is_empty()), "VM link", if listen.is_some() && !pack.tunnel.url.is_empty() { "remote listening on, tunnel address in the pack".into() } else { "satellites need remote listening (remote.listen) and tunnel.url in vm.yaml".into() }));
    }
    out
}

/// Whether VM launches have work to do (the service stays up for them).
pub fn has_work(home: &Path) -> bool {
    pack::configured(home).is_some() && launch::all(home).iter().any(launcher::wants_work)
}

/// The service's VM loop (thread `vm`): reconciles once per pack, advances
/// every launch that has a step to take, runs the manager and the secrets
/// refresher every 10 s (images built on threads of their own), drains the
/// thin lane's guests.
/// Without a pack it only waits.
pub fn run(home: PathBuf, log: impl Fn(String) + Send + Sync + 'static, wake: impl Fn() + Send + 'static, links: Arc<link::Links>, adopt: impl Fn(&str, Arc<link::Remote>)) {
    let log: Arc<dyn Fn(String) + Send + Sync> = Arc::new(log);
    // The VM boxes' terminals that ran in guests when this service last stopped.
    links.adopt(adopt);
    let mut opened: Option<(pack::Pack, Arc<dyn provider::Provider>)> = None;
    let mut refused: Option<String> = None;
    let mut reconciled = false;
    let mut drains = thin::Drains::default();
    let mut refresher = secrets::Refresher::default();
    let mut last_tick: Option<Instant> = None;
    let mut since_ms = now_ms();
    let building: Arc<Mutex<std::collections::HashSet<String>>> = Arc::default();
    let syncing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut last_sync: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Some((pack, status)) = loaded(&home) else {
            opened = None;
            continue;
        };
        if status.errors() > 0 {
            continue;
        }
        if opened.as_ref().is_none_or(|(p, _)| *p != pack) {
            match open_provider(&home, &pack) {
                Ok(provider) => {
                    opened = Some((pack.clone(), provider));
                    refused = None;
                    reconciled = false;
                }
                Err(error) => {
                    let text = format!("{error:#}");
                    if refused.as_ref() != Some(&text) {
                        log(format!("VM sessions: {text}"));
                        refused = Some(text);
                    }
                    continue;
                }
            }
        }
        let Some((pack, provider)) = opened.as_ref() else { continue };
        let Ok(hub) = store::hub_id(&home) else { continue };
        let ctx = launcher::Ctx { home: &home, pack, provider: provider.as_ref(), hub: &hub, log: log.as_ref() };
        if !reconciled {
            reconciled = true;
            if let Err(error) = launcher::reconcile(&ctx) {
                log(format!("VM reconciliation failed: {}", hooks::first_line(&format!("{error:#}"))));
            }
        }
        for intent in launch::all(&home).into_iter().filter(launcher::wants_work) {
            // Its repositories checked just before it freezes: the delete barrier of a suspended VM reads them.
            let suspending = intent.pending == "suspend" && intent.state == VmState::Running;
            if suspending && links.linked(&intent.sandbox) {
                let _ = links.git_check(&intent.sandbox);
            }
            if let Err(error) = launcher::advance(&ctx, &intent.r#box) {
                log(format!("VM launch of {}: {}", intent.r#box, hooks::first_line(&format!("{error:#}"))));
            }
            // Frozen now: its link goes at once (typing into it then resumes it).
            if suspending && launch::load(&home, &intent.r#box).ok().flatten().is_some_and(|i| i.state == VmState::Suspended) {
                links.close(&intent.sandbox);
            }
        }
        // The boxes' terminals: their launch's steps, their programs started once their satellites link.
        links.progress();
        // The canvas's files in the guests of linked VM boxes (one pass at a time, every 3 s).
        if pack.provider != pack::ProviderKind::Hooks && last_sync.is_none_or(|t| t.elapsed() >= Duration::from_secs(3)) && !syncing.load(std::sync::atomic::Ordering::Acquire) {
            last_sync = Some(Instant::now());
            let mut boxes: std::collections::BTreeMap<String, Vec<launch::Intent>> = std::collections::BTreeMap::new();
            for intent in launch::all(&home).into_iter().filter(|i| i.state == VmState::Running && !i.sandbox.is_empty()) {
                boxes.entry(intent.sandbox.clone()).or_default().push(intent);
            }
            boxes.retain(|sandbox, _| links.linked(sandbox));
            if !boxes.is_empty() {
                syncing.store(true, std::sync::atomic::Ordering::Release);
                let (home, pack, links, log, syncing) = (home.clone(), pack.clone(), links.clone(), log.clone(), syncing.clone());
                let _ = std::thread::Builder::new().name("vm-sync".into()).spawn(move || {
                    let user_home = dirs::home_dir().unwrap_or_default();
                    for (sandbox, intents) in boxes {
                        let Some(guest) = links.guest(&sandbox) else { continue };
                        let roots = sync::roots_for(&home, &pack, &user_home, &guest, &intents);
                        if let Err(error) = links.sync(&sandbox, &roots) {
                            log(format!("VM {sandbox}: the canvas's files were not synced: {}", hooks::first_line(&format!("{error:#}"))));
                        }
                    }
                    syncing.store(false, std::sync::atomic::Ordering::Release);
                });
            }
        }
        if last_tick.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
            last_tick = Some(Instant::now());
            let now = now_ms();
            if let Err(error) = manager::tick(&home, pack, provider.as_ref(), &hub, since_ms, now, log.as_ref()) {
                log(format!("VM manager: {}", hooks::first_line(&format!("{error:#}"))));
            }
            since_ms = now;
            refresher.run(pack, provider.as_ref(), log.as_ref());
            // The running boxes' egress refusals, for their details.
            if pack.provider != pack::ProviderKind::Hooks {
                let running: Vec<launch::Intent> = launch::all(&home).into_iter().filter(|i| i.state == VmState::Running).collect();
                refusals::collect_egress(&home, provider.as_ref(), &running);
            }
            let due = store::load_images(&home).map(|images| manager::images_due(pack, &images, now)).unwrap_or_default();
            for repo in due {
                if !building.lock().unwrap_or_else(|e| e.into_inner()).insert(repo.clone()) {
                    continue;
                }
                let (home, pack, provider, hub, log, building) = (home.clone(), pack.clone(), provider.clone(), hub.clone(), log.clone(), building.clone());
                let _ = std::thread::Builder::new().name("vm-image".into()).spawn(move || {
                    let _ = manager::build_image(&home, &pack, provider.as_ref(), &hub, &repo, log.as_ref());
                    if let Ok(mut images) = store::load_images(&home) {
                        images.images.retain(|i| !(i.repo == repo && i.state == "requested"));
                        let _ = store::save_images(&home, &mut images);
                    }
                    building.lock().unwrap_or_else(|e| e.into_inner()).remove(&repo);
                });
            }
        }
        if drains.run(&home, pack, provider.as_ref(), &links, log.as_ref()) {
            wake();
        }
    }
}

/// The pack's files' newest modification (a cache key for [`loaded`]).
fn pack_stamp(folder: &Path) -> Option<SystemTime> {
    let mut newest = std::fs::metadata(folder).ok()?.modified().ok();
    for dir in [folder.to_path_buf(), folder.join("egress"), folder.join("hooks")] {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                newest = newest.max(Some(modified));
            }
        }
    }
    newest
}

/// The configured pack, read again only when its files change.
pub fn loaded(home: &Path) -> Option<(pack::Pack, PackStatus)> {
    type Cached = Option<(PathBuf, Option<SystemTime>, pack::Pack, PackStatus)>;
    static CACHE: Mutex<Cached> = Mutex::new(None);
    let folder = pack::configured(home)?;
    let stamp = pack_stamp(&folder);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((f, s, pack, status)) = cache.as_ref()
        && *f == folder
        && *s == stamp
        && stamp.is_some()
    {
        return Some((pack.clone(), status.clone()));
    }
    let (pack, status) = pack::load(&folder);
    *cache = Some((folder, stamp, pack.clone(), status.clone()));
    Some((pack, status))
}

/// What clients see of VM sessions (`bus::Snapshot::vm`): `None` without a
/// pack, so no VM UI shows anywhere. Times and costs are whole minutes and
/// cents, so the view changes only when something does.
pub fn view(home: &Path) -> Option<VmView> {
    let (pack, status) = loaded(home)?;
    let now = now_ms();
    let mut view = VmView { pack: pack.summary(status.errors()), ..Default::default() };
    let sandboxes = store::load_sandboxes(home).unwrap_or_default();
    let images = store::load_images(home).unwrap_or_default();
    let budget = store::load_budget(home).unwrap_or_default();
    let (refusals, refused) = refusals::read(home);
    view.refusals = refusals;
    let minute = |ms: u64| ms - ms % 60_000;
    for intent in launch::all(home) {
        let record = sandboxes.sandboxes.iter().find(|s| s.id == intent.sandbox);
        let info = VmInfo {
            state: intent.state,
            sandbox: intent.sandbox.clone(),
            repo: intent.placement.repo.clone(),
            branch: intent.placement.branch.clone().unwrap_or_default(),
            size: record.map(|s| s.size.clone()).or_else(|| intent.placement.size.clone()).or_else(|| pack.sizes.first().map(|s| s.name.clone())).unwrap_or_default(),
            image_ms: record.and_then(|s| images.images.iter().find(|i| i.id == s.image)).map(|i| minute(i.built_ms)).unwrap_or(0),
            started_ms: record.map(|s| minute(s.created_ms)).unwrap_or(0),
            cost_cents: budget.by_sandbox.get(&intent.sandbox).copied().unwrap_or(0),
            note: if intent.error.is_empty() { intent.note.clone() } else { intent.error.clone() },
            ..Default::default()
        };
        let mut info = with_link(info, &intent, link::load_record(home, &intent.sandbox));
        info.refusals = refused.get(&intent.r#box).copied().unwrap_or(0);
        if intent.state == VmState::Queued {
            view.queued.push(QueuedStart { r#box: intent.r#box.clone(), since_ms: minute(intent.queued_ms), why: intent.note.clone() });
        }
        view.boxes.insert(intent.r#box.clone(), info);
    }
    view.sandboxes = sandboxes
        .sandboxes
        .iter()
        .map(|s| SandboxRow {
            id: s.id.clone(),
            role: serde_json::to_value(s.role).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default(),
            state: if s.suspended { VmState::Suspended } else { VmState::Running },
            repo: s.repo.clone(),
            size: s.size.clone(),
            boxes: s.boxes.clone(),
            created_ms: minute(s.created_ms),
        })
        .collect();
    view.images = images.images.iter().map(|i| ImageRow { repo: i.repo.clone(), id: i.id.clone(), built_ms: minute(i.built_ms), state: i.state.clone(), note: i.note.clone() }).collect();
    let running = manager::running(home, &pack);
    view.budget = BudgetView {
        running: running.len() as u32,
        max_running: pack.budget.max_running,
        cores: running.iter().filter_map(|(_, size)| pack.size(Some(size))).map(|z| z.cores()).sum(),
        max_cores: pack.budget.max_cores,
        today_cents: budget.today(now),
        daily_cents: pack.budget.daily_cents,
    };
    Some(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boxs_link_says_since_when_it_is_away_and_its_repository() {
        let intent = launch::Intent { r#box: "b1".into(), sandbox: "sbx-1".into(), state: VmState::Running, ..Default::default() };
        assert_eq!(with_link(VmInfo::default(), &intent, None), VmInfo::default(), "never linked: nothing to say (the thin lane)");
        let repo = canvas_common::sat::RepoStats { term: "b1".into(), branch: "atelier/x".into(), ahead: 2, untracked: 1, unpushed: vec!["atelier/x".into()], ..Default::default() };
        let record = link::LinkRecord { linked: false, since_ms: 1000, stats_ms: 900, stats: canvas_common::sat::StatsReport { repos: vec![repo], ..Default::default() } };
        let info = with_link(VmInfo::default(), &intent, Some(record.clone()));
        assert_eq!((info.offline_ms, info.state), (1000, VmState::IntegrationOffline));
        let git = info.git.unwrap();
        assert_eq!((git.branch.as_str(), git.ahead, git.untracked, git.unpushed.len()), ("atelier/x", 2, 1, 1));
        let suspended = launch::Intent { state: VmState::Suspended, ..intent };
        assert_eq!(with_link(VmInfo::default(), &suspended, Some(record)).offline_ms, 0, "a suspended VM is not offline");
    }

    #[test]
    fn the_view_and_actions_follow_the_launches() {
        let home = std::env::temp_dir().join(format!("atelier-vm-view-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/vm-pack");
        canvas_common::settings::Settings { vm: canvas_common::settings::VmSettings { pack: Some(example.display().to_string()) }, ..Default::default() }.save(&home).unwrap();
        if std::env::var_os("ATELIER_VM_PACK").is_some() {
            return;
        }
        let mut intent = launch::Intent { r#box: "b1".into(), state: VmState::Queued, note: "1 of 1 VMs are running".into(), queued_ms: 123_456_789, ..Default::default() };
        intent.placement.repo = "contoso-app".into();
        launch::store(&home, &mut intent).unwrap();
        let view = view(&home).unwrap();
        assert_eq!(view.boxes["b1"].state, VmState::Queued);
        assert_eq!(view.queued[0].since_ms % 60_000, 0, "whole minutes");
        assert_eq!(view.budget.max_running, 4);
        assert!(action(&home, "b1", VmOp::Suspend).unwrap_err().to_string().contains("not possible while its VM is queued"));
        assert_eq!(action(&home, "b1", VmOp::CancelStart).unwrap(), "Start canceled.");
        assert_eq!(launch::load(&home, "b1").unwrap().unwrap().state, VmState::Deleted, "nothing was claimed: nothing to release");
        assert!(action(&home, "nope", VmOp::Resume).unwrap_err().to_string().contains("does not run on a VM"));
        assert!(request_image(&home, "contoso-app").unwrap().contains("built next"));
        assert!(manager::images_due(&loaded(&home).unwrap().0, &store::load_images(&home).unwrap(), now_ms()).is_empty() || store::load_images(&home).unwrap().images.iter().any(|i| i.state == "requested"));
        assert!(has_work(&home) == launch::all(&home).iter().any(launcher::wants_work));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn deleting_stops_at_what_is_not_pushed_and_update_from_main_asks_the_agent() {
        let home = std::env::temp_dir().join(format!("atelier-vm-barrier-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/vm-pack");
        canvas_common::settings::Settings { vm: canvas_common::settings::VmSettings { pack: Some(example.display().to_string()) }, ..Default::default() }.save(&home).unwrap();
        if std::env::var_os("ATELIER_VM_PACK").is_some() {
            return;
        }
        let mut intent = launch::Intent { r#box: "b1".into(), sandbox: "sbx-1".into(), state: VmState::Running, ..Default::default() };
        intent.placement.repo = "contoso-app".into();
        launch::store(&home, &mut intent).unwrap();
        // Never reported: nothing is known to be lost.
        let repo = canvas_common::sat::RepoStats { term: "b1".into(), branch: "atelier/x".into(), untracked: 1, unpushed: vec!["atelier/x".into()], ..Default::default() };
        link::update_record(&home, "sbx-1", |r| {
            r.linked = true;
            r.stats_ms = now_ms();
            r.stats.repos = vec![repo];
        });
        let refused = action(&home, "b1", VmOp::Delete { confirm: Vec::new() }).unwrap_err().to_string();
        assert!(refused.contains("would lose: 1 branch not pushed: atelier/x; 1 new file not in git"), "{refused}");
        assert!(launch::load(&home, "b1").unwrap().unwrap().pending.is_empty(), "nothing was asked");
        let partly = VmOp::Delete { confirm: vec!["1 new file not in git".into()] };
        assert!(action(&home, "b1", partly).is_err(), "every finding is confirmed");
        // Checked now when linked: the fresh answer wins over the last report.
        let clean = |_: &str| Some(vec![canvas_common::sat::RepoStats { term: "b1".into(), branch: "atelier/x".into(), ..Default::default() }]);
        assert!(action_with(&home, "b1", VmOp::Delete { confirm: Vec::new() }, &clean).is_ok(), "pushed since: nothing to lose");
        launch::update(&home, "b1", |i| i.pending.clear()).unwrap();
        let two = |_: &str| Some(vec![canvas_common::sat::RepoStats { term: "b1".into(), cwd: "/home/agent/work/app".into(), stashes: 1, ..Default::default() }, canvas_common::sat::RepoStats { term: "b3".into(), cwd: "/home/agent/work/lib".into(), dirty: 1, ..Default::default() }]);
        let refused = action_with(&home, "b1", VmOp::Delete { confirm: Vec::new() }, &two).unwrap_err().to_string();
        assert!(refused.contains("would lose: app: 1 stash; lib: 1 file with uncommitted changes (now)"), "{refused}");
        let all = VmOp::Delete { confirm: vec!["1 branch not pushed: atelier/x".into(), "1 new file not in git".into()] };
        assert!(action(&home, "b1", all).unwrap().starts_with("Deleting its VM"));
        assert_eq!(launch::load(&home, "b1").unwrap().unwrap().pending, "delete");

        let mut other = launch::Intent { r#box: "b2".into(), sandbox: "sbx-2".into(), state: VmState::Suspended, ..Default::default() };
        other.placement.repo = "contoso-app".into();
        launch::store(&home, &mut other).unwrap();
        assert!(action(&home, "b2", VmOp::Delete { confirm: Vec::new() }).is_ok(), "no report: no barrier");
        launch::update(&home, "b2", |i| i.pending.clear()).unwrap();
        assert!(action(&home, "b2", VmOp::UpdateFromMain).unwrap().contains("origin/main"));
        assert_eq!(launch::load(&home, "b2").unwrap().unwrap().pending, "resume", "a suspended VM is resumed for it");
        let inbox: Vec<_> = std::fs::read_dir(canvas_model::bus::inbox_dir(&home)).unwrap().flatten().collect();
        let event: canvas_model::bus::Event = serde_json::from_slice(&std::fs::read(inbox[0].path()).unwrap()).unwrap();
        assert_eq!((event.kind.as_str(), event.to.as_str()), ("message", "b2"));
        assert!(event.data["text"].as_str().unwrap().contains("Do not push"));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn starting_a_lost_box_again_brings_its_mirrored_conversation() {
        let home = std::env::temp_dir().join(format!("atelier-vm-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/vm-pack");
        canvas_common::settings::Settings { vm: canvas_common::settings::VmSettings { pack: Some(example.display().to_string()) }, ..Default::default() }.save(&home).unwrap();
        if std::env::var_os("ATELIER_VM_PACK").is_some() {
            return;
        }
        for (id, sandbox) in [("5e55a1d0-0000-4000-8000-0000000000b1", "sbx-1"), ("5e55a1d0-0000-4000-8000-0000000000b2", "sbx-2")] {
            let mut intent = launch::Intent { r#box: id.into(), sandbox: sandbox.into(), state: VmState::Lost, ..Default::default() };
            launch::store(&home, &mut intent).unwrap();
        }
        let mirrored = thin::mirror_dir(&home, "sbx-1").join("copilot").join("5e55a1d0-0000-4000-8000-0000000000b1");
        std::fs::create_dir_all(&mirrored).unwrap();
        std::fs::write(mirrored.join("events.jsonl"), "{}\n").unwrap();
        assert!(action(&home, "5e55a1d0-0000-4000-8000-0000000000b1", VmOp::RetryStart).unwrap().contains("with its conversation"));
        let again = launch::load(&home, "5e55a1d0-0000-4000-8000-0000000000b1").unwrap().unwrap();
        assert_eq!((again.state, again.resume_from.as_str(), again.resume_session.as_str()), (VmState::Claiming, "sbx-1", "5e55a1d0-0000-4000-8000-0000000000b1"));
        assert_eq!(action(&home, "5e55a1d0-0000-4000-8000-0000000000b2", VmOp::RetryStart).unwrap(), "Starting it again on a new VM.", "nothing mirrored: a new start");
        assert!(launch::load(&home, "5e55a1d0-0000-4000-8000-0000000000b2").unwrap().unwrap().resume_session.is_empty());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn without_a_pack_vm_requests_say_so() {
        let home = std::env::temp_dir().join(format!("atelier-vm-nopack-{}", uuid::Uuid::new_v4()));
        if std::env::var_os("ATELIER_VM_PACK").is_some() {
            return;
        }
        assert_eq!(service_request(&home, Request::VmPackStatus), Response::VmPack(Box::default()));
        assert!(matches!(service_request(&home, Request::VmList), Response::Error(e) if e.contains("no VM pack is configured")));
    }
}
