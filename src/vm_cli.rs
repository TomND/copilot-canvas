//! `atelier vm list|pool|images|budget|image build REPO|suspend|resume|delete|cancel|retry BOX`:
//! VM sessions as the running service sees them (docs/VM-SESSIONS.md),
//! asked over its pipe. Without a VM pack the commands say VM sessions are
//! off (main.rs refuses them before they get here).

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use canvas_model::rpc::{Backend, Request};
use canvas_model::vm::{VmOp, VmView};

use crate::client::conn::{self, ClientSession, Conn, ConnOptions, ConnTarget};
use crate::client::remote::RemoteBackend;
use crate::paths::Instance;

fn service(instance: &Instance) -> Result<(Conn, Arc<RemoteBackend>)> {
    let backend = RemoteBackend::new();
    let options = ConnOptions { start: true, name: "cli".into(), rpc: Some(backend.clone()), ..Default::default() };
    let conn = conn::open(&ConnTarget::Local(instance.clone()), &options, &ClientSession::new()).context("cannot reach the local service")?;
    anyhow::ensure!(conn.protocol >= 8, "the running service is too old for VM sessions; restart it (`atelier service stop`)");
    Ok((conn, backend))
}

fn age(now: u64, ms: u64) -> String {
    if ms == 0 { "—".into() } else { crate::paths::format_age(ms, now) }
}

fn money(cents: u64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// The text of `vm list` (every VM box, then the sandboxes).
pub fn list_text(view: &VmView, now: u64) -> String {
    let mut out = vec![format!("VM sessions ({} pack, provider {}): {} box(es), {} sandbox(es)", if view.pack.name.is_empty() { "the" } else { &view.pack.name }, view.pack.provider, view.boxes.len(), view.sandboxes.len())];
    for (id, vm) in &view.boxes {
        out.push(format!("  {id}  {:<20} {} on {} · {} · {}{}", vm.state.word(), vm.repo, vm.branch, if vm.sandbox.is_empty() { "no VM yet" } else { &vm.sandbox }, vm.size, if vm.note.is_empty() { String::new() } else { format!(" · {}", vm.note) }));
        if vm.offline_ms > 0 {
            out.push(format!("      offline for {}", age(now, vm.offline_ms)));
        }
        if vm.refusals > 0 {
            out.push(format!("      refused {} time(s) (MCP or egress; newest below)", vm.refusals));
        }
        if let Some(git) = &vm.git {
            let unpushed = if git.unpushed.is_empty() { String::new() } else { format!(" · not pushed: {}", git.unpushed.join(", ")) };
            out.push(format!("      git {}: ahead {}, behind {}, {} changed, {} untracked, {} stashed{unpushed}", if git.branch.is_empty() { "(detached)" } else { &git.branch }, git.ahead, git.behind, git.dirty, git.untracked, git.stashes));
        }
    }
    for s in &view.sandboxes {
        out.push(format!("  sandbox {} ({}, {}, {}) up {} · boxes: {}", s.id, s.role, s.repo, s.size, age(now, s.created_ms), if s.boxes.is_empty() { "none".into() } else { s.boxes.join(", ") }));
    }
    for r in view.refusals.iter().take(5) {
        out.push(format!("  refused ({}) {} ago for {}: {} · {}", r.kind, age(now, r.ms), r.r#box, r.what, r.why));
    }
    out.join("\n")
}

pub fn run(instance: &Instance, words: &[&str], yes: bool, discard: bool) -> Result<()> {
    let (_conn, backend) = service(instance)?;
    let backend: &dyn Backend = &*backend;
    let now = crate::paths::now_ms();
    match words {
        ["list"] => println!("{}", list_text(&backend.vm_view(Request::VmList)?, now)),
        ["pool"] => {
            let view = backend.vm_view(Request::VmPool)?;
            let pool: Vec<_> = view.sandboxes.iter().filter(|s| s.role == "pool").collect();
            println!("VM pool: {} member(s) ready", pool.len());
            for s in pool {
                println!("  {} ({}, {}) made {} ago", s.id, s.repo, s.size, age(now, s.created_ms));
            }
        }
        ["images"] => {
            let view = backend.vm_view(Request::VmImages)?;
            println!("VM images: {}", view.images.len());
            for i in &view.images {
                println!("  {:<20} {:<10} {} built {} ago{}", i.repo, i.state, if i.id.is_empty() { "—" } else { &i.id }, age(now, i.built_ms), if i.note.is_empty() { String::new() } else { format!(" · {}", i.note) });
            }
        }
        ["image", "build", repo] => println!("{}", backend.vm_image_build(repo)?),
        ["budget"] => {
            let b = backend.vm_view(Request::VmBudget)?.budget;
            println!("VMs running {} of {} · cores {} of {} · today {} of {} (estimated)", b.running, b.max_running, b.cores, b.max_cores, money(b.today_cents), if b.daily_cents == 0 { "no daily budget".into() } else { money(b.daily_cents) });
        }
        [verb @ ("suspend" | "resume" | "delete" | "cancel" | "retry"), id] => {
            let op = match *verb {
                "suspend" => VmOp::Suspend,
                "resume" => VmOp::Resume,
                "delete" if !yes => bail!("deleting a VM loses what is not pushed: add --yes (`atelier vm list` shows its branch)"),
                // --discard: lose whatever the barrier finds not pushed.
                "delete" => VmOp::Delete { confirm: if discard { vec![canvas_model::vm::DISCARD_ALL.into()] } else { Vec::new() } },
                "cancel" => VmOp::CancelStart,
                _ => VmOp::RetryStart,
            };
            println!("{}", backend.vm_action(id, op)?);
        }
        _ => bail!("usage: atelier vm list|pool|images|budget · vm image build REPO · vm suspend|resume|cancel|retry BOX · vm delete BOX --yes [--discard] · vm pack check"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_names_boxes_and_sandboxes() {
        let mut view = VmView::default();
        view.pack.provider = "hooks".into();
        view.boxes.insert("b1".into(), canvas_model::vm::VmInfo { state: canvas_model::vm::VmState::Queued, repo: "app".into(), branch: "atelier/x".into(), note: "1 of 1 VMs are running".into(), ..Default::default() });
        let text = list_text(&view, 1);
        assert!(text.contains("b1  queued") && text.contains("no VM yet") && text.contains("1 of 1 VMs"), "{text}");
        assert_eq!(money(1234), "12.34");
    }
}
