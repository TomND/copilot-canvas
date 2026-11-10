//! VM sessions on the service's side (docs/VM-SESSIONS.md, the plan in
//! docs/VM-SESSIONS-PLAN.md): the VM pack, providers, the manager and the
//! launcher, the satellite link and the satellite itself.
//!
//! VM sessions are optional; local stays the default. Without a pack
//! (`settings.json` `vm.pack`, `ATELIER_VM_PACK`) nothing here runs and no
//! VM UI shows: the bus snapshot carries no `vm` view and the requests say
//! that no pack is configured.

use std::path::Path;

use canvas_model::rpc::{Request, Response};


/// What the service answers a VM request (`Request::is_service_state`).
pub fn service_request(home: &Path, request: Request) -> Response {
    let pack = canvas_common::settings::Settings::load(home).vm_pack();
    match (request, pack) {
        (Request::VmPackStatus, None) => Response::VmPack(Box::default()),
        (request, None) => Response::Error(format!("{}: no VM pack is configured (settings.json vm.pack or ATELIER_VM_PACK); VM sessions are off", request.name())),
        (request, Some(_)) => Response::Error(format!("{}: VM sessions are not available in this build yet", request.name())),
    }
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
