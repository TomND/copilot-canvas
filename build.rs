//! Embeds the app icon (`assets/icon/copilot-canvas.ico`, drawn by
//! `assets/icon/make_icon.py`) as icon resource 1 of `copilot-canvas.exe`,
//! so Explorer, Task Manager and the service's copies show it.

use std::path::Path;

fn main() {
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("icon").join("copilot-canvas.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let rc = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("app.rc");
    std::fs::write(&rc, format!("1 ICON \"{}\"\n", icon.display().to_string().replace('\\', "\\\\"))).expect("write app.rc");
    embed_resource::compile_for(&rc, ["copilot-canvas"], embed_resource::NONE).manifest_optional().expect("compile the icon resource (rc.exe from the Windows SDK)");
}
