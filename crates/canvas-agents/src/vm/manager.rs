//! `vm::Manager` (docs/VM-SESSIONS.md §9): the decisions about images, the
//! warm pool and budgets, as pure functions of the pack, the stores and the
//! time (so they are tested without a provider), and [`tick`], which applies
//! them through a provider on the service's VM thread.
//!
//! Budgets queue starts and say why; they never stop running work. Pool
//! members are made from the newest image, suspended, leased once and never
//! returned; they are recycled after `pool.max_age` or when a newer image
//! exists. A hooks pack has no pool and no images of the canvas's own.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use super::pack::{Pack, ProviderKind};
use super::provider::{Labels, Provider, SandboxSpec};
use super::store::{self, ImageRecord, ImagesFile, Role, SandboxRecord, SandboxesFile};
use crate::paths::now_ms;

/// Whether a VM of `size` may start now (`Err`: why it waits).
pub fn admit(pack: &Pack, sandboxes: &SandboxesFile, size: &str, today_cents: u64) -> Result<(), String> {
    let running: Vec<&SandboxRecord> = sandboxes.sandboxes.iter().filter(|s| s.role == Role::Session && !s.suspended).collect();
    if pack.budget.max_running > 0 && running.len() as u32 >= pack.budget.max_running {
        return Err(format!("{} of {} VMs are running (budget.max_running); it starts when one is suspended or deleted", running.len(), pack.budget.max_running));
    }
    let cores = |name: &str| pack.size(Some(name)).map(|s| s.cores()).unwrap_or(0.0);
    let used: f32 = running.iter().map(|s| cores(&s.size)).sum();
    if pack.budget.max_cores > 0.0 && used + cores(size) > pack.budget.max_cores {
        return Err(format!("{used} of {} cores are in use (budget.max_cores)", pack.budget.max_cores));
    }
    if pack.budget.daily_cents > 0 && today_cents >= pack.budget.daily_cents {
        return Err(format!("today's estimated cost reached the daily budget ({:.2}); it starts tomorrow, or raise budget.daily_soft", pack.budget.daily_cents as f64 / 100.0));
    }
    Ok(())
}

/// When a schedule (`daily HH:MM` in UTC, `every Nh` or `Nm`, `off`) last
/// fell due at or before `now` (`None`: never).
pub fn last_slot(schedule: &str, now: u64) -> Option<u64> {
    let schedule = schedule.trim().to_ascii_lowercase();
    if let Some(at) = schedule.strip_prefix("daily ") {
        let minutes = canvas_model::bus::parse_hhmm(at.trim())? as u64;
        let day = now - now % 86_400_000;
        let slot = day + minutes * 60_000;
        return Some(if slot <= now { slot } else { slot - 86_400_000 });
    }
    if let Some(every) = schedule.strip_prefix("every ") {
        let period = canvas_model::bus::parse_duration(every.trim())?.max(60_000);
        return Some(now - now % period);
    }
    None
}

/// Whether a repository's image is due: asked for, never built, or the
/// schedule fell due since the last build.
pub fn image_due(schedule: &str, last_build_ms: Option<u64>, now: u64) -> bool {
    match (last_build_ms, last_slot(schedule, now)) {
        (None, _) => true,
        (Some(last), Some(slot)) => last < slot,
        (Some(_), None) => false,
    }
}

/// Images to delete: all but the newest `keep` ready ones of each
/// repository (an image a pool member or session still runs from stays).
pub fn images_to_delete(images: &ImagesFile, keep: u32, in_use: &[String]) -> Vec<ImageRecord> {
    let mut by_repo: BTreeMap<&str, Vec<&ImageRecord>> = BTreeMap::new();
    for image in images.images.iter().filter(|i| i.state == "ready") {
        by_repo.entry(&image.repo).or_default().push(image);
    }
    let mut out = Vec::new();
    for (_, mut list) in by_repo {
        list.sort_by_key(|i| std::cmp::Reverse(i.built_ms));
        out.extend(list.into_iter().skip(keep.max(1) as usize).filter(|i| !in_use.contains(&i.id)).cloned());
    }
    out
}

/// What the pool needs: members to make (repository, size) and members to recycle.
#[derive(Debug, Default, PartialEq)]
pub struct PoolPlan {
    pub create: Vec<(String, String)>,
    pub recycle: Vec<String>,
}

pub fn pool_plan(pack: &Pack, sandboxes: &SandboxesFile, images: &ImagesFile, now: u64) -> PoolPlan {
    let mut plan = PoolPlan::default();
    if pack.provider == ProviderKind::Hooks {
        return plan;
    }
    let size = pack.size(None).map(|s| s.name.clone()).unwrap_or_default();
    for member in sandboxes.sandboxes.iter().filter(|s| s.role == Role::Pool) {
        let newest = images.newest(&member.repo).map(|i| i.id.as_str());
        let old = pack.pool.max_age_ms > 0 && now.saturating_sub(member.created_ms) >= pack.pool.max_age_ms;
        if old || newest.is_some_and(|n| n != member.image) || !pack.pool.min.contains_key(&member.repo) {
            plan.recycle.push(member.id.clone());
        }
    }
    for (repo, min) in &pack.pool.min {
        let Some(newest) = images.newest(repo) else { continue };
        let ready = sandboxes.sandboxes.iter().filter(|s| s.role == Role::Pool && s.repo == *repo && s.image == newest.id && !plan.recycle.contains(&s.id)).count() as u32;
        for _ in ready..*min {
            plan.create.push((repo.clone(), size.clone()));
        }
    }
    plan
}

/// The labels every sandbox of this hub carries.
pub fn labels(hub: &str, role: Role, repo: &str) -> Labels {
    let role = match role {
        Role::Pool => "pool",
        Role::Session => "session",
        Role::Builder => "builder",
        Role::Orphan => "orphan",
    };
    [("atelier.hub", hub), ("atelier.role", role), ("atelier.repo", repo)].into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect()
}

/// A sandbox spec of `size` from `image`.
pub fn spec(pack: &Pack, size: &str, image: Option<String>, labels: Labels) -> Result<SandboxSpec> {
    let size = pack.size(Some(size)).or_else(|| pack.size(None)).context("the pack has no sizes")?;
    Ok(SandboxSpec { image, cpu_milli: size.cpu_milli, memory_mi: size.memory_mi, disk_gi: size.disk_gi, labels, env: Vec::new(), egress: None })
}

/// Builds `repo`'s image: a builder sandbox from the repository's base, the
/// satellite (the pack's Linux build) and the clone at the default branch,
/// committed, the builder deleted. Records it in `images.json`.
pub fn build_image(home: &Path, pack: &Pack, provider: &dyn Provider, hub: &str, repo_key: &str, log: &dyn Fn(String)) -> Result<ImageRecord> {
    let repo = pack.repo(repo_key).with_context(|| format!("the pack has no repository {repo_key:?}"))?;
    let name = format!("atelier-{}-{}", repo.key, store::day_of(now_ms()).replace('-', ""));
    let mut images = store::load_images(home)?;
    images.last_build_ms.insert(repo.key.clone(), now_ms());
    store::save_images(home, &mut images)?;
    log(format!("VM image of {}: building", repo.key));
    let result = (|| -> Result<super::provider::ImageRef> {
        if provider.name() == "hooks" {
            return provider.image_build(&repo.image_base, &name);
        }
        let base = provider.image_build(&repo.image_base, &format!("{name}-base"))?;
        // The smallest size: an image keeps its builder's disk as its minimum.
        let smallest = pack.sizes.iter().min_by_key(|s| s.disk_gi).map(|s| s.name.clone()).unwrap_or_default();
        let builder = provider.create(&spec(pack, &smallest, Some(base.id.clone()), labels(hub, Role::Builder, &repo.key))?)?;
        let built = (|| -> Result<super::provider::ImageRef> {
            if let Some(path) = &pack.satellite.path {
                let bytes = std::fs::read(path).context("cannot read the satellite binary (satellite.path)")?;
                provider.put_file(&builder.id, "/usr/local/bin/atelier", &bytes)?;
            }
            let argv: Vec<String> = ["atelier", "satellite", "--clone", "--repo", &repo.key, "--url", &repo.url, "--branch", &repo.default_branch, "--mode", &repo.clone_mode].map(str::to_owned).to_vec();
            let out = provider.exec(&builder.id, &argv, b"")?;
            anyhow::ensure!(out.code == 0, "cloning {} failed in the builder: {}", repo.key, super::hooks::first_line(&String::from_utf8_lossy(&out.stdout)));
            provider.image_commit(&builder.id, &name)
        })();
        let _ = provider.delete(&builder.id);
        let _ = provider.image_delete(&base);
        built
    })();
    let mut images = store::load_images(home)?;
    let record = match result {
        Ok(image) => ImageRecord { repo: repo.key.clone(), id: image.id, name, built_ms: now_ms(), state: "ready".into(), note: String::new() },
        Err(error) => ImageRecord { repo: repo.key.clone(), id: String::new(), name, built_ms: now_ms(), state: "failed".into(), note: super::hooks::first_line(&format!("{error:#}")) },
    };
    images.images.retain(|i| !(i.repo == record.repo && i.state == "failed"));
    images.images.push(record.clone());
    store::save_images(home, &mut images)?;
    log(format!("VM image of {}: {}{}", repo.key, record.state, if record.note.is_empty() { String::new() } else { format!(" ({})", record.note) }));
    Ok(record)
}

/// One pass of the manager: the day's cost, the pool (made and recycled)
/// and old images deleted. Image builds are started by the caller (they
/// take minutes).
pub fn tick(home: &Path, pack: &Pack, provider: &dyn Provider, hub: &str, since_ms: u64, now: u64, log: &dyn Fn(String)) -> Result<()> {
    let mut sandboxes = store::load_sandboxes(home)?;
    // The day's estimated cost: every running sandbox since the last pass.
    let mut budget = store::load_budget(home)?;
    let elapsed = now.saturating_sub(since_ms).min(10 * 60_000);
    for s in sandboxes.sandboxes.iter().filter(|s| !s.suspended && s.role != Role::Orphan) {
        let rate = pack.size(Some(&s.size)).map(|z| z.cents_per_hour as u64).unwrap_or(0);
        let cents = rate * elapsed / 3_600_000;
        if cents > 0 || budget.day != store::day_of(now) {
            budget.add(now, &s.id, cents);
        }
    }
    store::save_budget(home, &mut budget)?;
    let images = store::load_images(home)?;
    let plan = pool_plan(pack, &sandboxes, &images, now);
    for id in &plan.recycle {
        log(format!("VM pool: recycling {id}"));
        let _ = provider.delete(id);
        sandboxes.sandboxes.retain(|s| s.id != *id);
    }
    for (repo, size) in &plan.create {
        let Some(image) = images.newest(repo) else { continue };
        // Pool members are made only while the cores allow it (they run until suspended).
        let cores = pack.size(Some(size)).map(|z| z.cores()).unwrap_or(0.0);
        let used: f32 = sandboxes.sandboxes.iter().filter(|s| !s.suspended).filter_map(|s| pack.size(Some(&s.size))).map(|z| z.cores()).sum();
        if pack.budget.max_cores > 0.0 && used + cores > pack.budget.max_cores {
            break;
        }
        let member = provider.create(&spec(pack, size, Some(image.id.clone()), labels(hub, Role::Pool, repo))?)?;
        let suspended = provider.suspend(&member.id).is_ok();
        log(format!("VM pool: {} ready for {repo}", member.id));
        sandboxes.sandboxes.push(SandboxRecord { id: member.id, role: Role::Pool, repo: repo.clone(), size: size.clone(), image: image.id.clone(), created_ms: now, suspended, ..Default::default() });
    }
    if !plan.recycle.is_empty() || !plan.create.is_empty() {
        store::save_sandboxes(home, &mut sandboxes)?;
    }
    let in_use: Vec<String> = sandboxes.sandboxes.iter().map(|s| s.image.clone()).collect();
    let old = images_to_delete(&images, pack.images.keep, &in_use);
    if !old.is_empty() {
        for image in &old {
            let _ = provider.image_delete(&super::provider::ImageRef { id: image.id.clone(), name: image.name.clone(), created_ms: image.built_ms });
        }
        let mut images = store::load_images(home)?;
        images.images.retain(|i| !old.iter().any(|o| o.id == i.id && o.repo == i.repo));
        store::save_images(home, &mut images)?;
    }
    Ok(())
}

/// Repositories whose image is due now.
pub fn images_due(pack: &Pack, images: &ImagesFile, now: u64) -> Vec<String> {
    if pack.provider == ProviderKind::Hooks && !pack.hooks.contains_key("image") {
        return Vec::new();
    }
    pack.repos
        .iter()
        .filter(|r| pack.pool.min.contains_key(&r.key) || images.images.iter().any(|i| i.repo == r.key))
        .filter(|r| !images.images.iter().any(|i| i.repo == r.key && i.state == "building"))
        .filter(|r| image_due(&pack.images.schedule, images.last_build_ms.get(&r.key).copied(), now))
        .map(|r| r.key.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::pack::{Budget, Pool, Repo, Size};

    fn pack() -> Pack {
        Pack {
            provider: ProviderKind::Local,
            sizes: vec![Size { name: "S".into(), cpu_milli: 1000, memory_mi: 2048, disk_gi: 20, cents_per_hour: 6 }, Size { name: "L".into(), cpu_milli: 4000, memory_mi: 8192, disk_gi: 80, cents_per_hour: 24 }],
            budget: Budget { max_running: 2, max_cores: 5.0, daily_cents: 1000 },
            pool: Pool { max_age_ms: 12 * 3_600_000, min: [("app".to_owned(), 2)].into_iter().collect() },
            repos: vec![Repo { key: "app".into(), ..Default::default() }],
            images: super::super::pack::Images { schedule: "daily 02:00".into(), keep: 2 },
            ..Default::default()
        }
    }

    fn session(id: &str, size: &str, suspended: bool) -> SandboxRecord {
        SandboxRecord { id: id.into(), role: Role::Session, size: size.into(), suspended, ..Default::default() }
    }

    #[test]
    fn budgets_queue_starts_and_say_why() {
        let pack = pack();
        let mut file = SandboxesFile::default();
        assert_eq!(admit(&pack, &file, "S", 0), Ok(()));
        file.sandboxes.push(session("a", "L", false));
        assert!(admit(&pack, &file, "L", 0).unwrap_err().contains("cores"), "4 + 4 > 5 cores");
        assert_eq!(admit(&pack, &file, "S", 0), Ok(()));
        file.sandboxes.push(session("b", "S", false));
        assert!(admit(&pack, &file, "S", 0).unwrap_err().contains("2 of 2 VMs are running"));
        file.sandboxes[1].suspended = true;
        assert_eq!(admit(&pack, &file, "S", 0), Ok(()), "suspended VMs do not count");
        assert!(admit(&pack, &file, "S", 1000).unwrap_err().contains("daily budget"));
        let pool_member = SandboxRecord { id: "p".into(), role: Role::Pool, size: "L".into(), ..Default::default() };
        file.sandboxes.push(pool_member);
        assert_eq!(admit(&pack, &file, "S", 0), Ok(()), "pool members are not sessions");
    }

    #[test]
    fn image_schedules() {
        let day = 86_400_000u64 * 100;
        let at = |h: u64, m: u64| day + (h * 60 + m) * 60_000;
        assert_eq!(last_slot("daily 02:00", at(3, 0)), Some(at(2, 0)));
        assert_eq!(last_slot("daily 02:00", at(1, 0)), Some(at(2, 0) - 86_400_000));
        assert_eq!(last_slot("every 6h", at(13, 0)), Some(at(12, 0)));
        assert_eq!((last_slot("off", at(1, 0)), last_slot("daily 25:99", 0)), (None, None));
        assert!(image_due("daily 02:00", None, at(1, 0)), "never built");
        assert!(image_due("daily 02:00", Some(at(1, 0)), at(3, 0)));
        assert!(!image_due("daily 02:00", Some(at(2, 30)), at(3, 0)));
        assert!(!image_due("off", Some(1), at(3, 0)));
    }

    #[test]
    fn the_pool_is_kept_full_from_the_newest_image_and_old_members_recycled() {
        let pack = pack();
        let now = 1_000_000_000u64;
        let image = |id: &str, ms: u64| ImageRecord { repo: "app".into(), id: id.into(), built_ms: ms, state: "ready".into(), ..Default::default() };
        let mut images = ImagesFile { images: vec![image("i1", 1)], ..Default::default() };
        let member = |id: &str, image: &str, created: u64| SandboxRecord { id: id.into(), role: Role::Pool, repo: "app".into(), size: "S".into(), image: image.into(), created_ms: created, suspended: true, ..Default::default() };
        let mut file = SandboxesFile { sandboxes: vec![member("m1", "i1", now - 1000)], ..Default::default() };
        assert_eq!(pool_plan(&pack, &file, &images, now), PoolPlan { create: vec![("app".into(), "S".into())], recycle: vec![] });
        file.sandboxes.push(member("m2", "i1", now - pack.pool.max_age_ms));
        assert_eq!(pool_plan(&pack, &file, &images, now).recycle, ["m2"], "too old");
        images.images.push(image("i2", 2));
        let plan = pool_plan(&pack, &file, &images, now);
        assert_eq!((plan.recycle.len(), plan.create.len()), (2, 2), "a newer image replaces every member");
        let leased = SandboxRecord { role: Role::Session, ..member("s1", "i1", 0) };
        file.sandboxes.push(leased);
        assert!(!pool_plan(&pack, &file, &images, now).recycle.contains(&"s1".to_owned()), "a leased member is never recycled");
        let hooks = Pack { provider: ProviderKind::Hooks, ..pack.clone() };
        assert_eq!(pool_plan(&hooks, &file, &images, now), PoolPlan::default(), "a hooks pack has no pool");
        images.images.push(image("i0", 0));
        let old = images_to_delete(&images, 2, &[]);
        assert_eq!(old.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["i0"]);
        assert!(images_to_delete(&images, 2, &["i0".to_owned()]).is_empty(), "an image in use stays");
        assert_eq!(images_due(&pack, &ImagesFile::default(), now), ["app"], "a pooled repository without an image");
    }

    #[test]
    fn images_are_built_in_a_builder_then_committed_and_the_pool_filled_from_them() {
        let home = std::env::temp_dir().join(format!("atelier-images-{}", uuid::Uuid::new_v4()));
        let provider = super::super::fake::FakeProvider::new("fake");
        let mut pack = pack();
        pack.repos[0].url = "https://example.com/app.git".into();
        pack.repos[0].default_branch = "main".into();
        let log = |_: String| {};
        let image = build_image(&home, &pack, &provider, "hub1", "app", &log).unwrap();
        assert_eq!(image.state, "ready", "{image:?}");
        let calls = provider.calls();
        assert!(calls.iter().any(|c| c.contains("--clone") && c.contains("--repo app")), "{calls:?}");
        assert!(calls.iter().any(|c| c.starts_with("image_commit")) && provider.live().is_empty(), "the builder is deleted");
        let images = store::load_images(&home).unwrap();
        assert_eq!(images.newest("app").map(|i| i.id.as_str()), Some(image.id.as_str()));
        assert!(!image_due(&pack.images.schedule, images.last_build_ms.get("app").copied(), now_ms()), "built: not due again today");
        // The tick fills the pool from it (two members, suspended) and keeps them under the cores.
        tick(&home, &pack, &provider, "hub1", now_ms(), now_ms(), &log).unwrap();
        let file = store::load_sandboxes(&home).unwrap();
        assert_eq!(file.sandboxes.iter().filter(|s| s.role == Role::Pool && s.suspended && s.image == image.id).count(), 2);
        // A failed build is recorded with why, never as ready.
        provider.fail("image_commit", 1);
        assert_eq!(build_image(&home, &pack, &provider, "hub1", "app", &log).unwrap().state, "failed");
        assert!(build_image(&home, &pack, &provider, "hub1", "nope", &log).is_err());
        let _ = std::fs::remove_dir_all(home);
    }
}
