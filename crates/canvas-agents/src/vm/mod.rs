//! VM sessions on the service's side (docs/VM-SESSIONS.md, the plan in
//! docs/VM-SESSIONS-PLAN.md): the VM pack, providers, the manager and the
//! launcher, the satellite link and the satellite itself.
//!
//! VM sessions are optional; local stays the default. Without a pack
//! (`settings.json` `vm.pack`, `ATELIER_VM_PACK`) nothing here runs and no
//! VM UI shows: the bus snapshot carries no `vm` view and the requests say
//! that no pack is configured.

pub mod hooks;
pub mod launch;
pub mod lite;
pub mod local;
pub mod pack;
pub mod provider;
pub mod spike;
pub mod thin;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use canvas_model::rpc::{Request, Response};
use canvas_model::vm::{PackStatus, VmInfo, VmView};


/// What the service answers a VM request (`Request::is_service_state`).
pub fn service_request(home: &Path, request: Request) -> Response {
    let pack = pack::configured(home);
    match (request, pack) {
        (Request::VmPackStatus, None) => Response::VmPack(Box::default()),
        (Request::VmPackStatus, Some(folder)) => Response::VmPack(Box::new(pack::load(&folder).1)),
        (Request::VmList | Request::VmPool | Request::VmImages | Request::VmBudget, Some(_)) => Response::Vm(Box::new(view(home).unwrap_or_default())),
        (request, None) => Response::Error(format!("{}: no VM pack is configured (settings.json vm.pack or ATELIER_VM_PACK); VM sessions are off", request.name())),
        (request, Some(_)) => Response::Error(format!("{}: VM sessions are not available in this build yet", request.name())),
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
/// pack, so no VM UI shows anywhere.
pub fn view(home: &Path) -> Option<VmView> {
    let (pack, status) = loaded(home)?;
    let mut view = VmView { pack: pack.summary(status.errors()), ..Default::default() };
    for intent in launch::all(home) {
        let info = VmInfo {
            state: intent.state,
            sandbox: intent.sandbox.clone(),
            repo: intent.placement.repo.clone(),
            branch: intent.placement.branch.clone().unwrap_or_default(),
            size: intent.placement.size.clone().or_else(|| pack.sizes.first().map(|s| s.name.clone())).unwrap_or_default(),
            note: intent.error.clone(),
            ..Default::default()
        };
        view.boxes.insert(intent.r#box.clone(), info);
    }
    Some(view)
}

#[cfg(test)]
mod tests {
    use super::*;

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
