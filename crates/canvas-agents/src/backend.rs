//! The backend API's handler ([`canvas_model::rpc`]). The same code answers
//! the in-process [`LocalBackend`] (clients on this machine's pipe) and, in
//! the service, remote clients' `ClientMsg::Rpc` requests.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use canvas_model::automation::AutoLoad;
use canvas_model::doc::Group;
use canvas_model::rpc::{Backend, BackendInfo, PassiveFiles, PathInfo, Request, Response};

use crate::agents::{AgentKind, Provide};
use crate::paths::Instance;
use crate::{agents, automations, bus, copilot_cfg, doc, factory, factory_cli, fork, git, migrate, taskschd, vscode};

/// Largest text file the viewer reads.
const MAX_TEXT: u64 = 4 << 20;
/// Files one `Tails` request reads at most (together at most `MAX_TEXT` bytes).
const MAX_TAILS: usize = 8;
/// Unfinished upload pieces older than this are removed.
const STALE_PART: Duration = Duration::from_secs(24 * 3600);

/// Where the backend runs: the canvas data folder and Copilot's history.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub home: PathBuf,
    pub sessions_dir: PathBuf,
}

impl Ctx {
    pub fn new(instance: &Instance) -> Self {
        Self { home: instance.home.clone(), sessions_dir: instance.sessions_dir.clone() }
    }
}

/// The backend in the caller's own process.
pub struct LocalBackend {
    ctx: Ctx,
}

impl LocalBackend {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx }
    }
}

impl Backend for LocalBackend {
    fn call(&self, request: Request) -> Result<Response> {
        Ok(handle(&self.ctx, request))
    }
}

/// Runs one request. Failures are answered with `Response::Error`.
pub fn handle(ctx: &Ctx, request: Request) -> Response {
    run(ctx, request).unwrap_or_else(|error| Response::Error(format!("{error:#}")))
}

fn text(path: &Path) -> Response {
    Response::Text(path.display().to_string())
}

fn run(ctx: &Ctx, request: Request) -> Result<Response> {
    Ok(match request {
        Request::Info => Response::Info(BackendInfo {
            home: ctx.home.display().to_string(),
            user_home: dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default(),
            sessions_dir: ctx.sessions_dir.display().to_string(),
            automation_db: automations::db_path().display().to_string(),
            default_agent: agents::machine_default(&ctx.home),
            installed: agents::installed(),
        }),

        Request::ConfigSnapshot { agent } => {
            let mut snapshot = agents::config::snapshot(agent).unwrap_or_else(|e| copilot_cfg::Snapshot { errors: vec![format!("{e:#}")], ..Default::default() });
            plugin_folders(&mut snapshot);
            Response::Config(Box::new(snapshot))
        }
        Request::ConfigSetEnabled { agent, what, name, enabled } => Response::Text(agents::config::set_enabled(agent, what, &name, enabled)?),
        Request::ConfigRemove { agent, what, name } => Response::Text(agents::config::remove(agent, what, &name)?),
        Request::PluginUpdate { name } => Response::Text(copilot_cfg::update_plugin(&name)?),
        Request::SkillAdd { source } => Response::Text(copilot_cfg::add_skill(&source)?),
        Request::McpAdd { line } => Response::Text(copilot_cfg::add_mcp(&line)?),
        Request::ConfigSet { key, value } => Response::Text(copilot_cfg::config_set(&key, &value)?),
        Request::ConfigUnset { key } => Response::Text(copilot_cfg::config_remove(&key)?),
        Request::CliCommand { args } => {
            let exe = copilot_cfg::copilot_exe().context("copilot.exe was not found on PATH")?;
            Response::Texts(std::iter::once(exe.to_string_lossy().into_owned()).chain(args).collect())
        }
        Request::RestoreMouse => Response::Flag(copilot_cfg::restore_mouse()?),

        Request::Automations { dir, tasks } => {
            let folder = factory_cli::configured_dir(&ctx.home, dir);
            let manifests = tasks.then_some(folder.as_path());
            let factory_loaded = manifests.is_some_and(Path::is_dir);
            let (list, errors) = automations::load_all(&automations::db_path(), manifests);
            Response::Automations(Box::new(AutoLoad { list, errors, factory_loaded, dir: folder.display().to_string() }))
        }
        Request::AutomationsDir { dir } => text(&factory_cli::configured_dir(&ctx.home, dir)),
        Request::AutomationSetEnabled { key, enabled } => {
            automations::set_enabled(&automations::db_path(), &key, enabled)?;
            Response::Done
        }
        Request::TaskSetEnabled { name, manifest, enabled } => {
            factory_cli::set_manifest_enabled(Path::new(&manifest), enabled)?;
            taskschd::set_enabled(&name, enabled)?;
            Response::Done
        }
        Request::TaskRun { name } => {
            taskschd::run(&name)?;
            Response::Done
        }
        Request::NewestMatches { patterns } => Response::Texts(patterns.iter().filter_map(|p| factory::newest_match(p)).map(|p| p.display().to_string()).collect()),
        Request::Tails { files } => {
            anyhow::ensure!(files.len() <= MAX_TAILS, "at most {MAX_TAILS} files at once");
            let mut budget = MAX_TEXT;
            Response::Texts(
                files
                    .iter()
                    .map(|(path, max)| {
                        let take = (*max).min(budget);
                        budget -= take;
                        factory::tail(Path::new(path), take)
                    })
                    .collect(),
            )
        }

        Request::VsCodeScan { created, since_ms } => {
            let (workspaces, mut warnings) = vscode::scan(&vscode::default_roots());
            let (groups, group_warnings) = vscode::agent_groups(&vscode::default_user_dirs());
            warnings.extend(group_warnings);
            let grouped: std::collections::HashSet<String> = groups.iter().flat_map(|g| g.sessions.iter().cloned()).collect();
            let (containers, container_warnings) = vscode::read_containers(&vscode::default_container_roots(), since_ms, &grouped);
            warnings.extend(container_warnings);
            Response::VsSync { candidates: vscode::sync_candidates(&workspaces, &groups, &containers, &created), warnings }
        }
        Request::MigratePlan => {
            let snapshot = copilot_cfg::snapshot();
            Response::MigratePlan(migrate::plan(&migrate::vscode_user_dirs(), &copilot_cfg::copilot_home(), &snapshot.mcp, &snapshot.skills))
        }
        Request::MigrateApply { items } => {
            let refs: Vec<&migrate::Item> = items.iter().collect();
            let runner = |args: &[String]| -> Result<String> {
                let refs: Vec<&str> = args.iter().map(String::as_str).collect();
                copilot_cfg::run_cli(&refs, Duration::from_secs(120))
            };
            let (backup, outcomes) = migrate::apply(&refs, &copilot_cfg::copilot_home(), &ctx.home.join("backups"), &runner)?;
            let mut lines = vec![format!("Backup: {}", backup.display())];
            lines.extend(outcomes.into_iter().map(|outcome| match outcome.result {
                Ok(_) => format!("✓ {}", outcome.name),
                Err(error) => format!("✗ {} — {error}", outcome.name),
            }));
            Response::Texts(lines)
        }

        Request::ListDir { dir } => Response::Texts(list_dir(dir.as_deref())?),
        Request::PathInfo { path } => Response::Path(path_info(Path::new(&path))),
        Request::DirsExist { paths } => Response::Flags(paths.iter().map(|p| Path::new(p).is_dir()).collect()),
        Request::ReadText { path, max_bytes } => Response::Text(read_text(Path::new(&path), max_bytes.min(MAX_TEXT))?),
        Request::OpenFolder { path } => {
            copilot_cfg::open_in_explorer(Path::new(&path))?;
            Response::Done
        }
        Request::OpenTarget { target } => {
            open_target(&target)?;
            Response::Done
        }

        Request::GitProbe { dir } => Response::Git(git::probe(Path::new(&dir))?),
        Request::WorktreeAdd { root, path, new_branch, base } => {
            git::add_worktree(Path::new(&root), Path::new(&path), new_branch.as_deref(), &base)?;
            Response::Done
        }

        Request::HasHistory { agent, id } => Response::Flag(match agent {
            // The canvas's own history folder (`--sessions-dir`).
            AgentKind::Copilot => crate::index::is_session_id(&id) && ctx.sessions_dir.join(&id).join("events.jsonl").is_file(),
            other => other.provider().has_history(&id),
        }),
        Request::Fork { parent, child, name, cwd } => {
            fork::fork_session(&ctx.sessions_dir, &parent, &child, &name, cwd.as_deref())?;
            Response::Done
        }

        Request::PrepareGroup { group } => {
            doc::prepare_group_folders(&group)?;
            Response::Done
        }
        Request::EnsureCatalog { folder } => text(&bus::ensure_catalog(Path::new(&folder))?),
        Request::DoctorFolder => {
            let folder = ctx.home.join("factory");
            std::fs::create_dir_all(&folder).with_context(|| format!("cannot create {}", folder.display()))?;
            text(&folder)
        }
        Request::CreatePassive { files } => Response::Text(create_passive(&files)?),

        Request::DocBackup { path, label, doc } => text(&doc::write_backup(Path::new(&path), &doc, &label)?),
        Request::DocBackups { path } => Response::Texts(doc::list_backups(Path::new(&path))?.iter().map(|p| p.display().to_string()).collect()),

        Request::PairCode { .. } | Request::RemoteClients | Request::RemoteRevoke { .. } => {
            anyhow::bail!("remote clients are managed by the running service (copilot-canvas remote …)")
        }
        Request::SetClipboardImage { png } => {
            crate::clipboard::set_image_png(&png)?;
            Response::Done
        }
        Request::Upload { name, id, offset, data, last } => upload(&ctx.home, &name, &id, offset, &data, last)?,
    })
}

/// The folder of each upload in progress (by id), fixed when it starts so a
/// day change between pieces does not split it.
static UPLOADS: std::sync::Mutex<Option<std::collections::HashMap<String, PathBuf>>> = std::sync::Mutex::new(None);
/// Uploads in progress remembered at most.
const MAX_UPLOADS: usize = 256;

/// Writes one piece of an upload (`Request::Upload`) under `<home>/uploads/<date>/`
/// (the date it started).
fn upload(home: &Path, name: &str, id: &str, offset: u64, data: &str, last: bool) -> Result<Response> {
    use base64::Engine;
    use std::io::{Seek, SeekFrom, Write};
    anyhow::ensure!(!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "invalid upload id");
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| anyhow!("invalid upload data: {e}"))?;
    anyhow::ensure!(offset + bytes.len() as u64 <= canvas_model::rpc::MAX_UPLOAD, "files over {} MB are not uploaded", canvas_model::rpc::MAX_UPLOAD >> 20);
    let uploads = home.join("uploads");
    // Uploads in progress are told apart by their data folder and id.
    let key = format!("{}|{id}", uploads.display());
    let dir = {
        let mut started = UPLOADS.lock().unwrap_or_else(|e| e.into_inner());
        let started = started.get_or_insert_with(Default::default);
        if offset == 0 {
            sweep_parts(&uploads);
            if started.len() >= MAX_UPLOADS {
                started.clear();
            }
            let date = crate::paths::rfc3339_now().chars().take(10).collect::<String>();
            started.insert(key.clone(), uploads.join(date));
        }
        started.get(&key).cloned().context("upload piece out of order")?
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let part = dir.join(format!(".{id}.part"));
    let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(offset == 0).open(&part).with_context(|| format!("cannot write {}", part.display()))?;
    anyhow::ensure!(file.metadata()?.len() == offset, "upload piece out of order");
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&bytes)?;
    if !last {
        return Ok(Response::Done);
    }
    drop(file);
    if let Some(started) = UPLOADS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        started.remove(&key);
    }
    let target = finish_upload(&part, &dir, &safe_file_name(name))?;
    Ok(text(&target))
}

/// Moves a finished upload to a free name in `dir`: the name is claimed
/// with `create_new` (never replacing a file), the next free one on collision.
fn finish_upload(part: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    for n in 1..10_000 {
        let candidate = if n == 1 { dir.join(name) } else { dir.join(format!("{stem} ({n}){ext}")) };
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            // The empty file claimed above is ours to replace.
            Ok(claimed) => {
                drop(claimed);
                std::fs::rename(part, &candidate).with_context(|| format!("cannot finish {}", candidate.display()))?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("cannot create {}", candidate.display())),
        }
    }
    anyhow::bail!("no free name for {name} in {}", dir.display())
}

/// Removes unfinished upload pieces older than a day.
fn sweep_parts(uploads: &Path) {
    let Ok(days) = std::fs::read_dir(uploads) else { return };
    for day in days.flatten() {
        let Ok(entries) = std::fs::read_dir(day.path()) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > STALE_PART);
            if name.starts_with('.') && name.ends_with(".part") && old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// A file name without folders, characters Windows refuses, trailing dots or
/// spaces (Windows drops them) or a reserved device name (`CON`, `NUL`,
/// `COM1`, … get a `_` prefix).
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['\\', '/']).next().unwrap_or_default();
    let clean: String = base.chars().map(|c| if c.is_control() || r#"<>:\"|?*"#.contains(c) { '_' } else { c }).collect();
    let clean = clean.trim_start().trim_end_matches(['.', ' ']).to_owned();
    if clean.is_empty() {
        return "upload".to_owned();
    }
    let device = clean.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (device.len() == 4 && (device.starts_with("COM") || device.starts_with("LPT")) && device.as_bytes()[3].is_ascii_digit() && device.as_bytes()[3] != b'0');
    if reserved { format!("_{clean}") } else { clean }
}

/// Fills in each plugin's folder under Copilot's `installed-plugins`.
fn plugin_folders(snapshot: &mut copilot_cfg::Snapshot) {
    let plugins = copilot_cfg::copilot_home().join("installed-plugins");
    for plugin in &mut snapshot.plugins {
        let folder = [plugins.join(&plugin.name), plugins.join(&plugin.marketplace)].into_iter().find(|d| d.is_dir()).unwrap_or_else(|| plugins.clone());
        plugin.local_folder = if folder.is_dir() { folder.display().to_string() } else { String::new() };
    }
}

/// Folder names in `dir` (".." first, hidden system folders left out), or the drives.
fn list_dir(dir: Option<&str>) -> Result<Vec<String>> {
    let Some(dir) = dir else {
        // SAFETY: no arguments; returns a bit mask.
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        return Ok((0..26u8).filter(|i| mask & (1 << i) != 0).map(|i| format!("{}:\\", (b'A' + i) as char)).collect());
    };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| anyhow!(e.to_string()))?
        .filter_map(|e| e.ok())
        .filter(|e| std::fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('$') && n != "System Volume Information")
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.insert(0, "..".into());
    Ok(names)
}

fn path_info(path: &Path) -> PathInfo {
    let exists = path.exists();
    PathInfo {
        exists,
        dir: path.is_dir(),
        file: path.is_file(),
        absolute: crate::paths::absolute(path).display().to_string(),
        occupied: exists && path.read_dir().map(|mut d| d.next().is_some()).unwrap_or(true),
    }
}

fn read_text(path: &Path, max: u64) -> Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Opens a file or URL with its default program.
fn open_target(target: &str) -> Result<()> {
    let target = target.trim();
    anyhow::ensure!(!target.is_empty(), "nothing to open");
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    anyhow::ensure!(is_url || Path::new(target).exists(), "{target} does not exist");
    std::process::Command::new("explorer.exe").arg(target).spawn().map(|_| ()).map_err(|e| anyhow!("cannot open {target}: {e}"))
}

/// Creates a passive session's files in the first free slug; returns the slug.
fn create_passive(files: &PassiveFiles) -> Result<String> {
    let folder = PathBuf::from(&files.folder);
    let mut slug = files.base_slug.clone();
    let mut n = 2;
    while files.taken.contains(&slug) || bus::passive_paths(&folder, &slug).dir.exists() {
        slug = format!("{}-{n}", files.base_slug);
        n += 1;
    }
    doc::prepare_group_folders(&Group { name: files.group_name.clone(), folder: Some(files.folder.clone()), ..Default::default() })?;
    let paths = bus::passive_paths(&folder, &slug);
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&paths.dir)?;
        std::fs::write(&paths.rules, &files.rules)?;
        std::fs::write(&paths.state, "# Memory\n\n(Nothing yet.)\n")?;
        std::fs::write(&paths.log, format!("# Run log of {}\n\n", files.name))?;
        if !paths.goals.exists() {
            std::fs::create_dir_all(paths.goals.parent().unwrap_or(&folder))?;
            std::fs::write(
                &paths.goals,
                format!("# Goals of {}\n\nWrite the project's goals and non-goals here. Passive sessions such as the drift guard compare changes against this file; if it is still this template, the drift guard drafts it for you to confirm.\n", files.group_name),
            )?;
        }
        Ok(())
    };
    write().map_err(|e| anyhow!("cannot create the passive session's files: {e}"))?;
    bus::ensure_catalog(&folder)?;
    Ok(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-backend-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn passive_files_get_a_free_slug_and_keep_existing_goals() {
        let dir = temp("passive");
        let files = PassiveFiles {
            folder: dir.display().to_string(),
            group_name: "Team".into(),
            base_slug: "guard".into(),
            taken: vec!["guard".into()],
            name: "Guard".into(),
            rules: "# Rules\n".into(),
        };
        assert_eq!(create_passive(&files).unwrap(), "guard-2");
        assert_eq!(create_passive(&files).unwrap(), "guard-3", "an existing folder is never reused");
        let paths = bus::passive_paths(&dir, "guard-2");
        assert_eq!(std::fs::read_to_string(&paths.rules).unwrap(), "# Rules\n");
        assert!(std::fs::read_to_string(&paths.goals).unwrap().starts_with("# Goals of Team"));
        assert!(bus::catalog_path(&dir).is_file() && dir.join("ai-docs").is_dir());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn path_info_and_listing() {
        let dir = temp("paths");
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::create_dir_all(dir.join("A")).unwrap();
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        assert_eq!(list_dir(Some(&dir.display().to_string())).unwrap(), ["..", "A", "b"]);
        assert!(list_dir(None).unwrap().iter().any(|d| d.ends_with(":\\")));
        let info = path_info(&dir.join("b"));
        assert!(info.exists && info.dir && !info.occupied);
        assert!(path_info(&dir).occupied && path_info(&dir.join("f.txt")).occupied);
        assert!(!path_info(&dir.join("missing")).exists);
        assert_eq!(read_text(&dir.join("f.txt"), 10).unwrap(), "x");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn uploads_arrive_in_pieces_under_a_free_safe_name() {
        use base64::Engine;
        let home = temp("upload");
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let backend: &dyn Backend = &LocalBackend::new(Ctx { home: home.clone(), sessions_dir: home.clone() });
        assert_eq!(backend.upload_piece(r"C:\x\..\re:port?.txt", "u1", 0, b64(b"hello "), false).unwrap(), None);
        let path = backend.upload_piece(r"C:\x\..\re:port?.txt", "u1", 6, b64(b"world"), true).unwrap().unwrap();
        assert!(path.ends_with("re_port_.txt") && path.contains(r"\uploads\"), "{path}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
        let second = backend.upload_piece("re_port_.txt", "u2", 0, b64(b"x"), true).unwrap().unwrap();
        assert!(second.ends_with("re_port_ (2).txt"), "{second}");
        assert!(backend.upload_piece("a", "u3", 5, b64(b"late"), true).is_err(), "out of order");
        assert!(backend.upload_piece("a", "../evil", 0, b64(b"x"), true).is_err(), "ids are plain");
        assert!(backend.upload_piece("a", "u4", canvas_model::rpc::MAX_UPLOAD, b64(b"x"), true).is_err(), "size cap");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn finished_uploads_never_replace_a_file_and_avoid_device_names() {
        let dir = temp("collide");
        let part = |n: u8| {
            let path = dir.join(format!(".p{n}.part"));
            std::fs::write(&path, [n]).unwrap();
            path
        };
        std::fs::write(dir.join("a.txt"), "mine").unwrap();
        std::fs::write(dir.join("a (2).txt"), "also mine").unwrap();
        let first = finish_upload(&part(1), &dir, "a.txt").unwrap();
        assert_eq!(first, dir.join("a (3).txt"), "the next free name");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "mine", "never replaced");
        assert_eq!(std::fs::read(&first).unwrap(), [1]);
        assert_eq!(finish_upload(&part(2), &dir, "a.txt").unwrap(), dir.join("a (4).txt"));
        assert_eq!(finish_upload(&part(3), &dir, "noext").unwrap(), dir.join("noext"));
        assert_eq!(finish_upload(&part(4), &dir, "noext").unwrap(), dir.join("noext (2)"));
        for (name, safe) in [
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("NUL.tar.gz", "_NUL.tar.gz"),
            ("com1.log", "_com1.log"),
            ("LPT9", "_LPT9"),
            ("aux .txt", "_aux .txt"),
            ("COM0.txt", "COM0.txt"),
            ("console.txt", "console.txt"),
            ("prn2", "prn2"),
            (r"..\..\x\CON", "_CON"),
            ("a<b>.txt", "a_b_.txt"),
            ("trail. . ", "trail"),
            ("", "upload"),
        ] {
            assert_eq!(safe_file_name(name), safe, "{name:?}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_upload_pieces_are_swept() {
        let uploads = temp("sweep");
        let day = uploads.join("2026-01-01");
        std::fs::create_dir_all(&day).unwrap();
        let old = day.join(".old.part");
        let fresh = day.join(".fresh.part");
        let kept = day.join("old.txt");
        for path in [&old, &fresh, &kept] {
            std::fs::write(path, "x").unwrap();
        }
        let long_ago = std::time::SystemTime::now() - STALE_PART - Duration::from_secs(60);
        for path in [&old, &kept] {
            std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(long_ago).unwrap();
        }
        sweep_parts(&uploads);
        assert!(!old.exists(), "an unfinished piece older than a day goes");
        assert!(fresh.exists() && kept.exists(), "recent pieces and finished files stay");
        std::fs::remove_dir_all(uploads).unwrap();
    }
}
