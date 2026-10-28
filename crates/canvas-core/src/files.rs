//! Files dropped on the window. On a terminal they are pasted as quoted
//! paths, as Windows Terminal does; in remote mode they are uploaded to the
//! service's machine first (`Request::Upload`, into `<home>/uploads/<date>/`)
//! and the remote paths are pasted. A folder dropped on the canvas opens the
//! new-session dialog in it (local mode).

use std::path::Path;

use crate::app::{App, AppEvent};
use crate::dialogs::Modal;
use crate::protocol::{ClientMsg, Input};
use crate::rpc::{Backend, MAX_UPLOAD};

/// Bytes per upload request (base64 grows it by a third).
const PIECE: usize = 4 << 20;

impl App {
    /// Files dropped at `at` (fractional cells).
    pub fn drop_files(&mut self, paths: Vec<String>, at: (f32, f32)) {
        if paths.is_empty() {
            return;
        }
        let (x, y) = (at.0.max(0.0) as u16, at.1.max(0.0) as u16);
        let inside = |r: &ratatui::layout::Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        let tile = self.layout.tiles.iter().find(|(_, outer, _)| inside(outer)).map(|(id, _, _)| id.clone());
        if let Some(id) = tile {
            if !self.owned_alive(&id) {
                self.notify("That terminal is not running.", true);
                return;
            }
            self.focused_term = Some(id.clone());
            if self.target.as_ref().is_some_and(|t| t.is_remote()) {
                self.upload_and_paste(id, paths);
            } else {
                self.paste_paths(&id, &paths);
            }
            return;
        }
        let canvas = self.layout.canvas;
        let remote = self.target.as_ref().is_some_and(|t| t.is_remote());
        if inside(&canvas) && !remote && self.modals.is_empty() && paths.len() == 1 && Path::new(&paths[0]).is_dir() {
            self.open_new_session(None, None);
            if let Some(Modal::NewSession(dialog)) = self.modals.last_mut() {
                dialog.folder.set(&paths[0]);
                dialog.folder_changed_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
            }
            return;
        }
        self.notify("Drop files on a terminal to paste their paths.", false);
    }

    /// Pastes paths into a terminal, quoted when they contain spaces.
    pub fn paste_paths(&mut self, id: &str, paths: &[String]) {
        let text = paths.iter().map(|p| quote_path(p)).collect::<Vec<_>>().join(" ");
        self.send(ClientMsg::Input { id: id.to_owned(), input: Input::Paste(text) });
    }

    fn upload_and_paste(&mut self, term: String, paths: Vec<String>) {
        let count = paths.len();
        self.notify(format!("Uploading {count} file(s) to the remote machine…"), false);
        self.in_background(
            move |backend| {
                let mut uploaded = Vec::new();
                let mut errors = Vec::new();
                for path in &paths {
                    match upload_file(backend, Path::new(path)) {
                        Ok(remote) => uploaded.push(remote),
                        Err(error) => errors.push(format!("{}: {error:#}", Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())),
                    }
                }
                (uploaded, errors)
            },
            move |(paths, errors)| AppEvent::Uploaded { term, paths, errors },
        );
    }

    /// Uploads finished: paste what arrived, report what did not.
    pub fn on_uploaded(&mut self, term: &str, paths: Vec<String>, errors: Vec<String>) {
        if !paths.is_empty() && self.owned_alive(term) {
            self.paste_paths(term, &paths);
        }
        match errors.is_empty() {
            true => self.notify(format!("Uploaded {} file(s); their remote paths were pasted.", paths.len()), false),
            false => self.notify(format!("Not uploaded: {}", errors.join("; ")), true),
        }
    }
}

/// Uploads one local file in pieces; returns its path on the service's machine.
pub fn upload_file(backend: &dyn Backend, path: &Path) -> anyhow::Result<String> {
    use base64::Engine;
    use std::io::Read;
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(meta.is_file(), "only files are uploaded");
    anyhow::ensure!(meta.len() <= MAX_UPLOAD, "larger than {} MB", MAX_UPLOAD >> 20);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "upload".into());
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut file = std::fs::File::open(path)?;
    let mut offset = 0u64;
    let mut buffer = vec![0u8; PIECE];
    loop {
        let mut filled = 0;
        while filled < PIECE {
            let n = file.read(&mut buffer[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        let last = offset + filled as u64 >= meta.len() || filled < PIECE;
        let data = base64::engine::general_purpose::STANDARD.encode(&buffer[..filled]);
        if let Some(remote) = backend.upload_piece(&name, &id, offset, data, last)? {
            return Ok(remote);
        }
        anyhow::ensure!(!last, "the service did not answer the file's path");
        offset += filled as u64;
    }
}

/// A path as a shell argument: quoted when it has spaces or shell characters.
pub fn quote_path(path: &str) -> String {
    if path.chars().any(|c| c.is_whitespace() || "&()[]{}^=;!'+,`~$#@".contains(c)) { format!("\"{path}\"") } else { path.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_quoted_like_windows_terminal() {
        assert_eq!(quote_path(r"C:\work\a.txt"), r"C:\work\a.txt");
        assert_eq!(quote_path(r"C:\My Files\a b.png"), "\"C:\\My Files\\a b.png\"");
        assert_eq!(quote_path(r"C:\x\(1).txt"), "\"C:\\x\\(1).txt\"");
    }

    #[test]
    fn uploads_go_in_pieces_through_the_backend() {
        use std::sync::Mutex;
        struct Recorder(Mutex<Vec<(u64, usize, bool)>>);
        impl Backend for Recorder {
            fn call(&self, request: crate::rpc::Request) -> anyhow::Result<crate::rpc::Response> {
                let crate::rpc::Request::Upload { offset, data, last, .. } = request else { unreachable!() };
                self.0.lock().unwrap().push((offset, data.len(), last));
                Ok(if last { crate::rpc::Response::Text(r"C:\remote\big.bin".into()) } else { crate::rpc::Response::Done })
            }
        }
        let path = std::env::temp_dir().join(format!("cc-upload-{}.bin", uuid::Uuid::new_v4()));
        std::fs::write(&path, vec![7u8; PIECE + 10]).unwrap();
        let recorder = Recorder(Mutex::new(Vec::new()));
        assert_eq!(upload_file(&recorder, &path).unwrap(), r"C:\remote\big.bin");
        let calls = recorder.0.lock().unwrap().clone();
        assert_eq!(calls.iter().map(|(o, _, l)| (*o, *l)).collect::<Vec<_>>(), [(0, false), (PIECE as u64, true)]);
        std::fs::remove_file(&path).unwrap();
        assert!(upload_file(&recorder, &std::env::temp_dir()).is_err(), "folders are not uploaded");
    }
}
