//! `canvas-mock`: a stand-in service for the iPhone client (see `canvas_mock`).
//!
//! ```text
//! canvas-mock [--listen 127.0.0.1:47600] [--code CODE|random] [--cwd DIR] [--state DIR] [--quiet]
//!             [--tunnel ID [--url wss://…]] [--real-agents] [--name NAME] [--qr FILE.png]
//! ```
//!
//! Locally (no `--tunnel`) it uses the fixed test code. Behind a tunnel
//! (`ios/cloud-mac.sh`) it opens a random code, issues a tunnel access token
//! and writes the pairing QR code for the phone.

use std::path::PathBuf;

const USAGE: &str = "usage: canvas-mock [--listen 127.0.0.1:47600] [--code CODE|random] [--cwd DIR] [--state DIR] [--quiet] [--tunnel ID [--url wss://…]] [--real-agents] [--name NAME] [--qr FILE.png]";

fn main() {
    let mut args = std::env::args().skip(1);
    let mut listen = "127.0.0.1:47600".to_owned();
    let mut code: Option<String> = None;
    let mut cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|_| "/".into());
    let mut state_dir = std::env::temp_dir().join("canvas-mock");
    let mut verbose = true;
    let mut tunnel = None;
    let mut web_url = None;
    let mut real_agents = false;
    let mut name = "mock".to_owned();
    let mut qr: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().unwrap_or_else(|| fail(&format!("{flag} needs a value")));
        match arg.as_str() {
            "--listen" => listen = value("--listen"),
            "--code" => code = Some(value("--code")),
            "--cwd" => cwd = value("--cwd"),
            "--state" => state_dir = PathBuf::from(value("--state")),
            "--quiet" => verbose = false,
            "--tunnel" => tunnel = Some(value("--tunnel")),
            "--url" => web_url = Some(value("--url")),
            "--real-agents" => real_agents = true,
            "--name" => name = value("--name"),
            "--qr" => qr = Some(PathBuf::from(value("--qr"))),
            _ => fail(USAGE),
        }
    }
    // Behind a tunnel the code is random (the fixed one is for local tests).
    let code = match code.as_deref() {
        Some("random") => canvas_common::pairing::new_code(),
        Some(code) => code.to_owned(),
        None if tunnel.is_some() => canvas_common::pairing::new_code(),
        None => "ABCDEFGHJKMN".to_owned(),
    };
    let code = canvas_common::pairing::normalize_code(&code).unwrap_or_else(|| fail("a pairing code has 12 characters (ABCD-EFGH-JKMN)"));
    let config = canvas_mock::Config { listen, code: code.clone(), state_dir, cwd, verbose, tunnel: tunnel.clone(), web_url, real_agents };
    let handle = canvas_mock::serve(config).unwrap_or_else(|error| fail(&format!("{error:#}")));
    println!("Copilot Canvas mock service on {} (TCP and WebSocket)", handle.addr);
    println!("Pairing code: {}", canvas_common::pairing::format_code(&code));
    let mut link = canvas_common::remote::PairLink { name, url: format!("ws://{}", handle.addr), code, ..Default::default() };
    if tunnel.is_some() {
        match handle.tunnel_access() {
            Ok(access) if !access.url.is_empty() => {
                link.url = access.url;
                link.token = access.token;
                link.token_expires_ms = access.expires_ms;
            }
            Ok(_) => eprintln!("canvas-mock: the tunnel's web address is unknown; pass --url wss://TUNNEL-PORT.REGION.devtunnels.ms"),
            Err(error) => eprintln!("canvas-mock: no tunnel access token: {error:#}"),
        }
    }
    let uri = link.to_uri();
    println!("Pairing link: {uri}");
    if let Some(path) = qr {
        match write_qr(&uri, &path) {
            Ok(()) => println!("Pairing QR code: {}", path.display()),
            Err(error) => eprintln!("canvas-mock: cannot write the QR code: {error:#}"),
        }
    }
    loop {
        std::thread::park();
    }
}

fn fail(message: &str) -> ! {
    eprintln!("canvas-mock: {message}");
    std::process::exit(2);
}

/// The pairing link as a QR code PNG (dark modules on white, a quiet zone).
fn write_qr(uri: &str, path: &std::path::Path) -> anyhow::Result<()> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M)?;
    let modules = code.width();
    let colors = code.to_colors();
    let (scale, quiet) = (12usize, 4usize);
    let size = (modules + 2 * quiet) * scale;
    let mut pixels = vec![255u8; size * size];
    for (i, color) in colors.iter().enumerate() {
        if *color == qrcode::Color::Dark {
            let (mx, my) = (i % modules + quiet, i / modules + quiet);
            for y in my * scale..(my + 1) * scale {
                pixels[y * size + mx * scale..y * size + (mx + 1) * scale].fill(0);
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut encoder = png::Encoder::new(std::fs::File::create(path)?, size as u32, size as u32);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    Ok(())
}
