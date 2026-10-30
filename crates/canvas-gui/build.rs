//! Embeds the app icon (`assets/icon/copilot-canvas.ico`, drawn by
//! `assets/icon/make_icon.py`) as icon resource 1 of
//! `copilot-canvas-gui.exe`: the windows, the taskbar button and shortcuts
//! use it (`shell.rs` loads it with `Icon::from_resource(1, ..)`).

use std::path::Path;

fn main() {
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("assets").join("icon").join("copilot-canvas.ico");
    let icon = icon.canonicalize().unwrap_or(icon);
    println!("cargo:rerun-if-changed={}", icon.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let rc = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("app.rc");
    // rc.exe does not take the \\?\ prefix canonicalize adds.
    let path = icon.display().to_string();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path).replace('\\', "\\\\");
    std::fs::write(&rc, format!("1 ICON \"{path}\"\n")).expect("write app.rc");
    embed_resource::compile_for(&rc, ["copilot-canvas-gui"], embed_resource::NONE).manifest_optional().expect("compile the icon resource (rc.exe from the Windows SDK)");
}
