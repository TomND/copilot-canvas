//! Short names for the platforms this crate implements (MAC-LINUX-PLAN D1).

fn main() {
    cfg_aliases::cfg_aliases! {
        macos: { target_os = "macos" },
        linux: { target_os = "linux" },
        desktop_unix: { any(target_os = "macos", target_os = "linux") },
    }
}
