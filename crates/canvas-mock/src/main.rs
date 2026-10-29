//! `canvas-mock [--listen ADDR] [--code CODE] [--cwd DIR] [--quiet]`: a stand-in
//! service for the iPhone client (see `canvas_mock`).

fn main() {
    let mut args = std::env::args().skip(1);
    let mut listen = "127.0.0.1:47600".to_owned();
    let mut code = "ABCDEFGHJKMN".to_owned();
    let mut cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|_| "/".into());
    let mut verbose = true;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().expect("--listen ADDR"),
            "--code" => code = args.next().expect("--code CODE"),
            "--cwd" => cwd = args.next().expect("--cwd DIR"),
            "--quiet" => verbose = false,
            _ => {
                eprintln!("usage: canvas-mock [--listen 127.0.0.1:47600] [--code ABCDEFGHJKMN] [--cwd DIR] [--quiet]");
                std::process::exit(2);
            }
        }
    }
    let code = canvas_common::pairing::normalize_code(&code).expect("a 12-character pairing code");
    let state_dir = std::env::temp_dir().join("canvas-mock");
    let handle = match canvas_mock::serve(canvas_mock::Config { listen, code: code.clone(), state_dir, cwd, verbose }) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("canvas-mock: {error:#}");
            std::process::exit(1);
        }
    };
    let link = canvas_common::remote::PairLink { name: "mock".into(), url: format!("ws://{}", handle.addr), code: code.clone(), ..Default::default() };
    println!("Copilot Canvas mock service on {} (TCP and WebSocket)", handle.addr);
    println!("Pairing code: {}", canvas_common::pairing::format_code(&code));
    println!("Pairing link: {}", link.to_uri());
    loop {
        std::thread::park();
    }
}
