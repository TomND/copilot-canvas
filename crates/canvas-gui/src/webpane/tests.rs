use super::*;

const ROOT: &str = r"C:\Users\me\notebook";

#[test]
fn files_map_to_urls_and_back() {
    let cases = [
        (r"C:\Users\me\notebook\plan.html", "https://notes.canvas.invalid/plan.html"),
        (r"C:\Users\me\notebook\Groups\My group\ai docs\Q3 plan (v2).html", "https://notes.canvas.invalid/Groups/My%20group/ai%20docs/Q3%20plan%20%28v2%29.html"),
        (r"C:\Users\me\notebook\été\naïve#1.html", "https://notes.canvas.invalid/%C3%A9t%C3%A9/na%C3%AFve%231.html"),
        (r"C:\Users\me\notebook\a%20b&c=d;e.html", "https://notes.canvas.invalid/a%2520b%26c%3Dd%3Be.html"),
    ];
    for (path, url) in cases {
        assert_eq!(url_for(ROOT, path).as_deref(), Some(url), "{path}");
        assert_eq!(file_for(ROOT, url).as_deref(), Some(path), "{url}");
    }
    // Forward slashes, a trailing separator on the root, other case (Windows).
    assert_eq!(url_for(r"C:\Users\me\notebook\", "C:/Users/me/NOTEBOOK/sub/x.html").as_deref(), Some("https://notes.canvas.invalid/sub/x.html"));
    assert_eq!(file_for(r"C:\Users\me\notebook\", "https://notes.canvas.invalid/sub/x.html?v=2#top").as_deref(), Some(r"C:\Users\me\notebook\sub\x.html"));
    assert_eq!(file_for(r"C:\", "https://notes.canvas.invalid/x.html").as_deref(), Some(r"C:\x.html"));
    assert_eq!(file_for(r"\\server\share\notes", "https://notes.canvas.invalid/x.png").as_deref(), Some(r"\\server\share\notes\x.png"));
    // A service with Unix paths: `/` joins, case counts.
    assert_eq!(url_for("/home/me/notebook", "/home/me/notebook/a b.html").as_deref(), Some("https://notes.canvas.invalid/a%20b.html"));
    assert_eq!(url_for("/home/me/notebook", "/home/me/NOTEBOOK/a.html"), None);
    assert_eq!(file_for("/home/me/notebook", "https://notes.canvas.invalid/img/p.png").as_deref(), Some("/home/me/notebook/img/p.png"));
    assert_eq!(file_for("/", "https://notes.canvas.invalid/etc/x").as_deref(), Some("/etc/x"));
    // Scheme and host compare without case (browsers lower-case them anyway).
    assert_eq!(file_for(ROOT, "HTTPS://Notes.Canvas.Invalid/a.css").as_deref(), Some(r"C:\Users\me\notebook\a.css"));
}

#[test]
fn nothing_outside_the_folder_maps() {
    // Files outside the root, the root itself, or names a URL cannot carry.
    for path in [r"C:\Users\me\other\x.html", r"C:\Users\me\notebook", r"C:\Users\me\notebookx\a.html", r"D:\notebook\a.html", r"C:\Users\me\notebook\..\secret.html", r"C:\Users\me\notebook\a:stream.html", r"C:\Users\me\notebook\CON.html"] {
        assert_eq!(url_for(ROOT, path), None, "{path}");
    }
    // Roots that are relative or climb.
    for root in ["notebook", r"C:\Users\me\..\notebook", r".\notebook", "C:notebook", ""] {
        assert_eq!(url_for(root, r"C:\Users\me\notebook\a.html"), None, "{root:?}");
        assert_eq!(file_for(root, "https://notes.canvas.invalid/a.html"), None, "{root:?}");
    }
    for uri in [
        "https://notes.canvas.invalid/../secret.txt",
        "https://notes.canvas.invalid/%2e%2e/secret.txt",
        "https://notes.canvas.invalid/a/%2E%2E/%2E%2E/secret.txt",
        "https://notes.canvas.invalid/..%2F..%2Fsecret.txt",
        "https://notes.canvas.invalid/..%5C..%5Csecret.txt",
        "https://notes.canvas.invalid/a%5Cb.txt",
        "https://notes.canvas.invalid/.%20/x",
        "https://notes.canvas.invalid/..%20/x",
        "https://notes.canvas.invalid/.../x",
        "https://notes.canvas.invalid/./x",
        "https://notes.canvas.invalid/C:%5Cx",
        "https://notes.canvas.invalid/a.txt:hidden",
        "https://notes.canvas.invalid/a%3A%24DATA",
        "https://notes.canvas.invalid/a%00.png",
        "https://notes.canvas.invalid/a%0A.png",
        "https://notes.canvas.invalid/con",
        "https://notes.canvas.invalid/sub/NUL.txt",
        "https://notes.canvas.invalid/COM1.png",
        "https://notes.canvas.invalid/lpt%C2%B9.txt",
        "https://notes.canvas.invalid/conin$",
        "https://notes.canvas.invalid/a*.png",
        "https://notes.canvas.invalid/a%3F.png",
        "https://notes.canvas.invalid/",
        "https://notes.canvas.invalid",
        "https://notes.canvas.invalid/dir/",
        "https://notes.canvas.invalid/a//b.png",
        "https://notes.canvas.invalid/%",
        "https://notes.canvas.invalid/%zz.png",
        "https://notes.canvas.invalid/%FF.png",
        "https://notes.canvas.invalid./a.png",
        "https://notes.canvas.invalid:443/a.png",
        "https://user@notes.canvas.invalid/a.png",
        "https://notes.canvas.invalid.evil.com/a.png",
        "http://notes.canvas.invalid/a.png",
        "https://evil.example/a.png",
        "file:///C:/Users/me/notebook/a.png",
        "data:text/html,<p>x</p>",
        "about:blank",
    ] {
        assert_eq!(file_for(ROOT, uri), None, "{uri}");
    }
    assert!(name_ok("Notes.v2.html") && name_ok("~draft") && name_ok("console.html") && name_ok("COM10.txt") && name_ok("été"));
    assert!(!name_ok("aux") && !name_ok("Aux.tar.gz") && !name_ok("x.") && !name_ok("x ") && !name_ok("a|b") && !name_ok("a\"b") && !name_ok("<a>"));
}

#[test]
fn requests_are_served_from_the_folder_or_refused() {
    let serving = Serving { root: ROOT.into(), main: format!(r"{ROOT}\Plans\q3.html"), scripts: false };
    let file = |path: &str, main| Resource::File { path: path.into(), main };
    assert_eq!(classify("https://notes.canvas.invalid/Plans/q3.html", "GET", true, Some(&serving)), file(&serving.main, true));
    assert_eq!(classify("https://notes.canvas.invalid/plans/Q3.html", "get", true, Some(&serving)), file(&format!(r"{ROOT}\plans\Q3.html"), true), "the same file, other case");
    assert_eq!(classify("https://notes.canvas.invalid/Plans/img/a.png", "GET", false, Some(&serving)), file(&format!(r"{ROOT}\Plans\img\a.png"), false));
    assert_eq!(classify("https://notes.canvas.invalid/Plans/style.css?v=1", "GET", false, Some(&serving)), file(&format!(r"{ROOT}\Plans\style.css"), false));
    // Only the note's folder and below: not the rest of its root (a sibling's
    // files, the root's own, a mount's other folders), though links resolve there.
    for outside in ["https://notes.canvas.invalid/style.css", "https://notes.canvas.invalid/Other/a.png", "https://notes.canvas.invalid/Plansx/a.png", "https://notes.canvas.invalid/q3.html"] {
        assert_eq!(classify(outside, "GET", false, Some(&serving)), Resource::Refused, "{outside}");
    }
    assert_eq!(link_action("https://notes.canvas.invalid/Other/b.html", Some(ROOT)), LinkAction::App(Space::Notebook, NavTarget::Note(format!(r"{ROOT}\Other\b.html"))));
    assert!(in_folder_of(r"C:\n\a.html", r"c:\N\sub\x.png") && in_folder_of("/n/a.html", "/n/x.png") && !in_folder_of("/n/a.html", "/N/x.png") && !in_folder_of(r"C:\n\a.html", r"C:\n"));
    // Frames and other documents, other methods, other origins, traversal, nothing served.
    assert_eq!(classify("https://notes.canvas.invalid/Plans/other.html", "GET", true, Some(&serving)), Resource::Refused);
    assert_eq!(classify("https://notes.canvas.invalid/Plans/q3.html", "POST", true, Some(&serving)), Resource::Refused);
    assert_eq!(classify("https://example.com/a.png", "GET", false, Some(&serving)), Resource::Refused);
    assert_eq!(classify("https://notes.canvas.invalid/%2e%2e/x.png", "GET", false, Some(&serving)), Resource::Refused);
    assert_eq!(classify("https://notes.canvas.invalid/Plans/%2e%2e/x.png", "GET", false, Some(&serving)), Resource::Refused);
    assert_eq!(classify("data:image/png;base64,AAAA", "GET", false, Some(&serving)), Resource::Refused);
    assert_eq!(classify("about:blank", "GET", true, Some(&serving)), Resource::Refused);
    assert_eq!(classify("https://notes.canvas.invalid/Plans/q3.html", "GET", true, None), Resource::Refused, "a report (no note served) fetches nothing");
}

#[test]
fn replies_carry_the_policy_and_sanitize_the_note_without_scripts() {
    let note = br#"<html><head><style>h1{color:red}</style></head><body><h1>T</h1><script>alert(1)</script><img src="a.png"></body></html>"#.to_vec();
    let plain = reply(r"C:\n\note.html", true, false, Ok(note.clone()));
    assert_eq!((plain.status, plain.reason), (200, "OK"));
    let body = String::from_utf8(plain.body.clone()).unwrap_or_default();
    assert!(!body.contains("<script") && body.contains("<img src=\"a.png\">") && body.contains("<style>h1{color:red}</style>"), "{body}");
    assert!(plain.headers.starts_with("Content-Type: text/html; charset=utf-8\r\nContent-Security-Policy: default-src 'none';"), "{}", plain.headers);
    assert!(plain.headers.contains("\r\nX-Content-Type-Options: nosniff") && plain.headers.contains("\r\nCache-Control: no-store") && !plain.headers.contains("sandbox") && !plain.headers.contains("Allow-Origin"));
    assert!(plain.headers.contains("\r\nX-DNS-Prefetch-Control: off") && plain.headers.contains("\r\nReferrer-Policy: no-referrer"));
    assert!(plain.headers.lines().all(|l| l.contains(": ")), "one header per line: {}", plain.headers);
    // Scripts allowed: as it is, in the sandbox.
    let scripted = reply(r"C:\n\note.html", true, true, Ok(note.clone()));
    assert_eq!(scripted.body, note);
    assert!(scripted.headers.contains(&format!("Content-Security-Policy: {}", notes::SCRIPTS_CSP)) && !scripted.headers.contains("allow-same-origin"), "{}", scripted.headers);
    // Other files: as they are, typed by extension, with the policy.
    let image = reply(r"C:\n\a.png", false, false, Ok(vec![1, 2, 3]));
    assert_eq!(image.body, vec![1, 2, 3]);
    assert!(image.headers.starts_with("Content-Type: image/png\r\n") && image.headers.contains(notes::CSP));
    let sub_html = reply(r"C:\n\other.html", false, false, Ok(b"<script>x()</script>".to_vec()));
    assert_eq!(sub_html.body, b"<script>x()</script>".to_vec(), "only the note is rewritten; scripts stay off by setting and policy");
    let svg_main = reply(r"C:\n\pic.svg", true, false, Ok(b"<svg/>".to_vec()));
    assert!(svg_main.headers.starts_with("Content-Type: image/svg+xml") && svg_main.body == b"<svg/>".to_vec());
    // Missing files: 404; the note itself says why.
    let missing = reply(r"C:\n\gone.png", false, false, Err("not found".into()));
    assert_eq!((missing.status, missing.body.len()), (404, 0));
    let gone = reply(r"C:\n\<gone>.html", true, false, Err("not found".into()));
    let page = String::from_utf8(gone.body).unwrap_or_default();
    assert!(gone.status == 404 && page.contains("&lt;gone&gt;.html could not be read: not found") && page.contains("script-src 'none'"), "{page}");
    let big = reply(r"C:\n\big.png", false, false, Ok(vec![0; (notebook::MAX_BYTES + 1) as usize]));
    assert_eq!((big.status, big.body.len()), (404, 0), "larger than the notebook reads");
    assert_eq!(refused(), Reply { status: 403, reason: "Blocked by Copilot Canvas", headers: String::new(), body: Vec::new() });
}

#[test]
fn only_the_views_own_load_is_expected() {
    let main = format!(r"{ROOT}\a b.html");
    let file = Expect::File(main.clone());
    let url = url_for(ROOT, &main).unwrap_or_default();
    assert!(expected(&file, &url, Some(ROOT)));
    assert!(expected(&file, "https://notes.canvas.invalid/A%20B.html", Some(ROOT)), "Windows names compare without case");
    assert!(!expected(&file, "https://notes.canvas.invalid/other.html", Some(ROOT)));
    assert!(!expected(&file, &url, None));
    assert!(!expected(&file, "data:text/html,x", Some(ROOT)));
    assert!(!expected(&file, "https://example.com/a%20b.html", Some(ROOT)));
    assert!(expected(&Expect::Page, "data:text/html;charset=utf-8;base64,AAAA", None) && expected(&Expect::Page, "about:blank", None));
    assert!(!expected(&Expect::Page, &url, Some(ROOT)) && !expected(&Expect::Page, "https://example.com/", None));
    // Unloading the scripts view: only its own about:blank, never a page its script asks for.
    assert!(expected(&Expect::Blank, "about:blank", None));
    for other in ["data:text/html,<script>x()</script>", "about:blank#x", "about:srcdoc", url.as_str()] {
        assert!(!expected(&Expect::Blank, other, Some(ROOT)), "{other}");
    }
}

#[test]
fn links_become_app_places_browser_requests_or_nothing() {
    let note = |s: &str| LinkAction::App(Space::Notebook, NavTarget::Note(s.into()));
    assert_eq!(link_action("about:blank#note:Other%20note", None), note("Other note"));
    assert_eq!(link_action("data:text/html;base64,AAAA#todo:DEC-031", None), LinkAction::App(Space::Tasks, NavTarget::Todo("DEC-031".into())));
    assert_eq!(link_action("https://notes.canvas.invalid/a.html#note:Plan", Some(ROOT)), note("Plan"), "the fragment names the place");
    assert_eq!(link_action("https://notes.canvas.invalid/sub/page.md", Some(ROOT)), note(&format!(r"{ROOT}\sub\page.md")));
    assert_eq!(link_action("https://notes.canvas.invalid/other.html#section", Some(ROOT)), note(&format!(r"{ROOT}\other.html")));
    assert_eq!(link_action("https://notes.canvas.invalid/../x.html", Some(ROOT)), LinkAction::Ignore);
    assert_eq!(link_action("https://notes.canvas.invalid/other.html", None), LinkAction::Ignore, "no folder served: nothing to open, never the browser");
    assert_eq!(link_action("https://example.com/pr/1", Some(ROOT)), LinkAction::Browser("https://example.com/pr/1".into()));
    assert_eq!(link_action("mailto:me@example.com", None), LinkAction::Browser("mailto:me@example.com".into()));
    assert_eq!(link_action("https://example.com/#note:X", None), LinkAction::Browser("https://example.com/#note:X".into()), "another site's fragment is no app link");
    for ignored in ["about:blank#section", "about:blank#todo:not-an-id", "about:blank#note:", "javascript:alert(1)", "file:///C:/x.txt", "data:text/html,x", "ms-settings:", "https://exa mple.com/"] {
        assert_eq!(link_action(ignored, Some(ROOT)), LinkAction::Ignore, "{ignored}");
    }
    assert_eq!(fragment_target("about:blank#section"), None);
    assert_eq!(fragment_target("https://notes.canvas.invalid/a.html#note:X"), Some((Space::Notebook, NavTarget::Note("X".into()))));
}

#[test]
fn page_fragments_drive_the_app_only_from_clicks() {
    assert!(same_document("https://notes.canvas.invalid/a.html", "https://notes.canvas.invalid/a.html#part"));
    assert!(same_document("about:blank#x", "about:blank#note:Y") && same_document("about:blank", "about:blank#"));
    assert!(!same_document("https://notes.canvas.invalid/a.html", "https://notes.canvas.invalid/b.html#part"));
    assert!(!same_document("https://notes.canvas.invalid/a.html", "https://notes.canvas.invalid/a.html"), "a reload is no anchor");
    assert!(!same_document("", "#x") && !same_document("about:blank", "https://example.com/#x"));
    let second = Some(Duration::from_secs(1));
    assert!(fragment_allowed(false, true, false, None) && fragment_allowed(false, true, true, Some(Duration::ZERO)), "no scripts: only a click changes it");
    assert!(!fragment_allowed(false, false, true, None), "hidden");
    assert!(fragment_allowed(true, true, true, None) && fragment_allowed(true, true, true, second));
    assert!(!fragment_allowed(true, true, false, None), "scripts, without the keyboard: a script did it");
    assert!(!fragment_allowed(true, true, true, Some(Duration::from_millis(300))), "scripts: at most once a second");
    assert!(!fragment_allowed(true, false, true, None));
}

#[test]
fn global_keys_go_to_the_canvas_the_rest_stays_in_the_page() {
    let fwd = |code, mods| Some(WebKey::Forward(KeyEvent::new(code, mods)));
    let (c, s, a) = (Modifiers::CONTROL, Modifiers::SHIFT, Modifiers::ALT);
    assert_eq!(web_key(VK_ESCAPE, None, false, false, false), Some(WebKey::Escape));
    assert_eq!(web_key(VK_F6, None, false, false, false), fwd(KeyCode::F(6), Modifiers::NONE));
    assert_eq!(web_key(VK_F6, None, true, true, false), fwd(KeyCode::F(6), c | s));
    assert_eq!(web_key(0xDD, Some(']'), true, false, false), fwd(KeyCode::Char(']'), c));
    for (vk, ch) in [(0x31, '1'), (0x32, '2'), (0x33, '3')] {
        assert_eq!(web_key(vk, Some(ch), true, false, false), fwd(KeyCode::Char(ch), c));
    }
    assert_eq!(web_key(b'P' as u32, Some('p'), true, false, false), fwd(KeyCode::Char('p'), c));
    assert_eq!(web_key(VK_LEFT, None, false, false, true), fwd(KeyCode::Left, a));
    assert_eq!(web_key(VK_RIGHT, None, false, false, true), fwd(KeyCode::Right, a));
    assert_eq!(web_key(0xBC, Some(','), false, false, true), fwd(KeyCode::Char(','), a), "Alt+, (back where Windows Terminal keeps Alt+←)");
    assert_eq!(web_key(0xBE, Some('.'), false, false, true), fwd(KeyCode::Char('.'), a));
    assert_eq!(web_key(0xBC, Some(','), false, false, false), None, "typing a comma stays in the page");
    assert_eq!(web_key(0xBE, Some('.'), true, false, true), None, "AltGr");
    for letter in ['E', 'W', 'S', 'F', 'N'] {
        let lower = letter.to_ascii_lowercase();
        assert_eq!(web_key(letter as u32, Some(lower), true, false, false), fwd(KeyCode::Char(lower), c), "Ctrl+{letter}");
    }
    assert_eq!(web_key(b'N' as u32, Some('n'), true, true, false), fwd(KeyCode::Char('N'), c | s), "Ctrl+Shift+N");
    assert_eq!(web_key(b'N' as u32, Some('n'), false, false, true), fwd(KeyCode::Char('n'), a), "Alt+N");
    assert_eq!(web_key(VK_TAB, None, true, false, false), fwd(KeyCode::Tab, c));
    assert_eq!(web_key(VK_TAB, None, true, true, false), fwd(KeyCode::BackTab, c | s));
    // The page keeps the rest: copying, selecting, scrolling, typing, AltGr.
    for (vk, ch, ctrl, shift, alt) in [
        (b'C' as u32, Some('c'), true, false, false),
        (b'A' as u32, Some('a'), true, false, false),
        (b'P' as u32, Some('p'), true, true, false),
        (b'E' as u32, Some('e'), true, false, true),
        (0x34, Some('4'), true, false, false),
        (0x31, Some('1'), true, true, false),
        (0x39, Some('9'), true, false, true),
        (VK_TAB, None, false, false, false),
        (VK_LEFT, None, false, false, false),
        (VK_LEFT, None, true, false, false),
        (VK_F6, None, false, false, true),
        (0x21, None, false, false, false),
        (0x28, None, false, true, false),
        (b'N' as u32, Some('n'), false, false, false),
        (0xDB, Some('['), true, false, false),
        (0xDD, Some(']'), true, false, true),
    ] {
        assert_eq!(web_key(vk, ch, ctrl, shift, alt), None, "{vk:#x} ctrl={ctrl} shift={shift} alt={alt}");
    }
}

#[test]
fn a_dialog_over_the_pane_hides_it_its_own_does_not() {
    let pane = Rect::new(10, 5, 60, 20);
    assert!(pane_clear(pane, None));
    assert!(pane_clear(pane, Some(Rect::new(8, 2, 70, 30))), "the report dialog holding the pane");
    assert!(pane_clear(pane, Some(pane)));
    assert!(!pane_clear(pane, Some(Rect::new(20, 10, 20, 5))), "a menu over the pane");
    assert!(!pane_clear(pane, Some(Rect::new(0, 0, 12, 6))), "a corner");
    assert!(pane_clear(pane, Some(Rect::new(0, 0, 10, 5))), "next to it");
    assert!(pane_clear(pane, Some(Rect::new(70, 5, 10, 10))));
}

#[test]
fn pdfs_never_overwrite() {
    use std::io::{Error, ErrorKind};
    let dir = Path::new(r"C:\Users\me\Downloads");
    let first = pdf_path(dir, "Q3 plan: draft?", |_| Ok(())).unwrap_or_default();
    assert_eq!(first.parent(), Some(dir));
    let name = first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    assert!(name.starts_with("Q3 plan") && name.ends_with(".pdf") && !name.contains([':', '?']), "{name}");
    // A name is taken only by creating it: one made meanwhile is never written over.
    let taken = [dir.join("Plan.pdf"), dir.join("Plan-2.pdf")];
    let mut tried = Vec::new();
    let claim = |p: &Path| {
        tried.push(p.to_path_buf());
        if taken.iter().any(|t| t == p) { Err(Error::from(ErrorKind::AlreadyExists)) } else { Ok(()) }
    };
    assert_eq!(pdf_path(dir, "Plan", claim), Ok(dir.join("Plan-3.pdf")));
    assert_eq!(tried, [dir.join("Plan.pdf"), dir.join("Plan-2.pdf"), dir.join("Plan-3.pdf")]);
    assert_eq!(pdf_path(dir, "", |_| Ok(())), Ok(dir.join("note.pdf")));
    assert_eq!(pdf_path(dir, "..\\..\\evil", |_| Ok(())).ok().and_then(|p| p.parent().map(Path::to_path_buf)).as_deref(), Some(dir), "the title names a file, never a folder");
    // Another error, or every name taken: no PDF (and no endless loop).
    assert!(pdf_path(dir, "Plan", |_| Err(Error::from(ErrorKind::PermissionDenied))).is_err());
    assert!(pdf_path(dir, "Plan", |_| Err(Error::from(ErrorKind::AlreadyExists))).is_err());
    let temp = std::env::temp_dir().join(format!("canvas-pdf-claim-{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    assert!(claim_new(&temp).is_ok());
    assert_eq!(claim_new(&temp).map_err(|e| e.kind()), Err(ErrorKind::AlreadyExists), "a file that is there is never claimed");
    let _ = std::fs::remove_file(&temp);
}

#[test]
fn a_view_waits_on_a_bounded_number_of_requests() {
    assert!(admit(0) && admit(MAX_PENDING - 1) && !admit(MAX_PENDING) && !admit(usize::MAX));
    let busy = busy();
    assert_eq!((busy.status, busy.body.len()), (503, 0));
}

#[test]
fn the_browser_process_reaches_no_network() {
    let args: Vec<&str> = BROWSER_ARGS.split(' ').collect();
    for wanted in ["--proxy-server=127.0.0.1:9", "--proxy-bypass-list=<-loopback>", "--dns-prefetch-disable", "--force-webrtc-ip-handling-policy=disable_non_proxied_udp", "--disable-background-networking"] {
        assert!(args.contains(&wanted), "{wanted} in {BROWSER_ARGS}");
    }
    assert!(args.iter().all(|a| a.starts_with("--") && !a.contains(['"', '\''])), "one switch per word, nothing quoted: {BROWSER_ARGS}");
}

#[test]
fn a_script_gets_one_app_link_per_click_into_its_view() {
    let t0 = Instant::now();
    let later = |ms| t0 + Duration::from_millis(ms);
    let mut links = AppLinks::default();
    assert!(!links.follow(true, true, t0), "scripts, the keyboard elsewhere: a script did it");
    links.focus(true);
    assert!(links.focused() && links.follow(true, true, t0));
    assert!(!links.focused(), "following one gives up the keyboard");
    assert!(!links.follow(true, true, later(5000)), "a second one needs another click into the view");
    links.focus(true);
    assert!(links.follow(true, true, later(5000)));
    links.focus(true);
    assert!(!links.follow(true, true, later(5500)), "at most once a second");
    assert!(!links.follow(true, false, later(9000)), "hidden");
    // Without scripts only a click changes the URL: the keyboard does not matter.
    let mut plain = AppLinks::default();
    assert!(plain.follow(false, true, t0) && plain.follow(false, true, t0) && !plain.follow(false, false, t0));
}
