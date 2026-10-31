# iPhone app: status, next steps, leftover work

Status on 2026-10-30, `main` at b29b566. This file describes where the
app is today. The design and the phase history are in
[IOS-PLAN.md](IOS-PLAN.md); how to build, run and use the app is in
[../ios/README.md](../ios/README.md).

## 1. Where it is

The iPhone app is merged into `main`. It started on the `ios-client` branch
(PR #1, merged as bded559); the reports inbox came after the merge (6deb60c).
It is a remote client of the Copilot Canvas service. It connects to the
service over the same private Dev Tunnel as the desktop's remote mode, using
WebSocket web forwarding, a 24 h tunnel access token and a one-time pairing
code. Most of the logic is in a shared Rust core (`crates/canvas-mobile`,
built with UniFFI). The interface is SwiftUI (`ios/`).

| Area | State |
|---|---|
| Pairing | QR code, `ccanvas://` link, or the code typed by hand. Pairing the same address again replaces it and keeps the machine's name and colour. Credentials are kept in the Keychain. Face ID / passcode lock after a minute away |
| Several machines | Every paired machine stays connected while the app is open. A switcher shows each machine's state, what needs you and its new reports. Each machine has a colour, and an attention strip shows sessions that need you on the other machines |
| Sessions | Grouped by what needs you, then by canvas group. Subagents, subsessions, loops, passive sessions and queued mail are shown. Actions: stop, remove, rename, mark reviewed |
| Canvas | Groups, boxes, status borders and edges like the desktop. Pan, pinch to zoom, fit; tap a box to open it |
| Terminal | Renders the service's terminal cells with Core Text. Key bar, hardware keys, prompt composer, quick keys. Selection and copy, links, wheel scrolling, pinch to resize text. *Fit to the phone* (the terminal follows the device that typed last, like tmux) or *Mirror* the desktop's size; jump back to live output |
| Starting sessions | Choose the agent, browse folders on the service's machine, start in a group, give a first prompt. Subsessions and forks; passive session actions (run, pause, discard mail) |
| Files and pictures | Read-only viewer for file paths tapped in a terminal. Pictures go to the service machine's clipboard and are pasted into the agent (Ctrl+V) |
| Reports | Inbox behind the tray button, one row per thread, read state shared with every window. HTML, Markdown and messages each open as one sanitized page in a locked-down `WKWebView`. Links ask before Safari opens them. A toast when a new report arrives while the app is open |
| Notifications | Local notifications while the app is open or just backgrounded. APNs pushes from the service while the app is away: a session needs you or finished, a new report arrived, and silent renewal of the tunnel token. An expired token shows *Pair again* |
| Service side | WebSocket link, terminal sizes per client (tmux "latest"), tunnel token rotation, `remote pair --qr`, `remote push-key`, `remote push-test`. Paired clients cannot shut the service down. A push for each new report (at most 3 at once) |
| Mac stand-in | `canvas-mock` with `ios/cloud-mac.sh` stands in for the Cloud PC behind a real private tunnel, with real agent CLIs and a demo reports inbox. `ios/cloud-mac.sh report FILE` publishes a report |

## 2. How it is verified

| Check | Covers |
|---|---|
| Rust unit tests (`canvas-mobile`, `canvas-common`, `canvas-model`, `canvas-push`) | State machine, statuses, launches, forks, passive sessions, the reports inbox and its pages, push payloads, JWT signing, sealed tokens |
| `crates/canvas-mobile/tests/end_to_end.rs` | The core against the mock over a real WebSocket: pairing, a shell typed into, reconnecting, the reports inbox across two phones |
| XCTest (11 tests) | Core bridge, terminal helpers, and the report view's lock-down against a local network listener |
| XCUITest (7 tests, against two mocks) | Session list, canvas, typing into a shell, new session form and keyboard, two machines, subsessions, passive pause/resume, opening an HTML report |
| CI (`.github/workflows/ci.yml`) | All of the above on macOS. Windows: clippy, the service's tests, the GUI self-tests and the parity suite |
| By hand, on Thomas's iPhone 14 Pro | Pairing and daily use through real `*.devtunnels.ms` tunnels against the two Mac stand-ins |

## 3. Not verified yet (needs Thomas)

1. **The real Windows Cloud PC with the phone.** Update the Cloud PC from
   `main` (the protocol indices changed, so the phone and the service must
   both be current). Then:
   - run `copilot-canvas remote pair --qr` and scan it;
   - check terminal sizes while the desk and the phone take turns typing;
   - check Copilot, OpenCode and Claude Code at phone width;
   - start a new session and a fork from the phone;
   - check passive session actions;
   - check reports from real agents.
2. **APNs delivery.** Create the `.p8` key (developer.apple.com › Keys ›
   Apple Push Notifications service). Then run `copilot-canvas remote push-key
   AuthKey_ID.p8 --team 8Z37ZVMJ4V` and `remote push-test`. Check four
   notifications with the app closed: a session needs you, a session
   finished, a new report, and a silent token renewal. For the Mac
   stand-ins: `APNS_KEY=… APNS_TEAM=8Z37ZVMJ4V ios/cloud-mac.sh pair`.
3. **A day away.** Check that the token is renewed silently while the app is
   closed (this needs the key). Also check the expired-token banner and
   *Pair again* on the device.
4. **The reports inbox on the device.** The build is installed but has not
   been opened yet.

## 4. Next steps (in order)

1. **Validation on the Cloud PC** (§3.1). Fix what it finds.
2. **APNs key and push validation** (§3.2–3.3).
3. **TestFlight.** Today the app is installed from Xcode with a
   development profile, so pushes use the sandbox. To move to TestFlight:
   - create an App Store Connect record and archive a build;
   - switch the entitlement to `aps-environment` `production` for
     distribution builds. The app already reads the environment from its
     profile and tells the service;
   - add an app icon. The project names `AppIcon` but has no asset catalog
     yet; the desktop icon in `assets/icon/` can be reused.
4. **Tunnel access without re-pairing.** The 24 h tunnel token is renewed
   while the phone is connected or by a silent push. If the phone is away
   long enough, you have to scan a new QR code. Signing in with GitHub on the
   phone, using the tunnel owner's identity, would remove this. Its design
   still needs to be written.
5. **iPad.** A split view, with the session list or canvas beside a terminal.
6. **Inline images in terminals** (Kitty graphics). The core already keeps
   the service's `Image` and `Placements` messages; the Core Text renderer
   does not draw them yet.

## 5. Leftover work and known gaps

| Item | Notes |
|---|---|
| Notifications for automation failures | Listed in phase 5 of the plan, but the service only pushes for attention, reports and tokens |
| Canvas editing | Moving or resizing boxes and groups and editing groups stays on the desktop (D5, light client). The phone edits only open/review, rename, new session and remove |
| Automations, MCP, settings, VS Code views | Desktop only |
| PR coordination and review follow-ups (WI-014/016) | Their results reach the phone as reports and session statuses; their panels are desktop only |
| Metal renderer | Only if Core Text becomes too slow for large screens or fast output |
| QR code in the GUI's remote sheet | Today the QR code comes from `remote pair --qr` in a terminal on the Cloud PC |
| Repeated report alerts | A report that arrived while the app was away gets a push, then a toast when the app reconnects. This is minor; the core could drop toasts for reports older than the last disconnect |
| Older services | A service without reports never sends the inbox, and the inbox screen says so. The protocol indices changed during the merge, so services built before 7ad6e71 cannot talk to this app |
| The mock is not the service | The mock ignores bus actions other than reports. Pausing works through the canvas field. UI tests run against the mock, so the real service's bus behaviour is covered only by the Windows tests |
| Accessibility, localisation | English only. VoiceOver labels exist for controls and the terminal's text value; there is no full VoiceOver pass |
| Test gaps | No automated test of toasts, local notifications or APNs delivery, and no on-device automation |

## 6. Useful commands

```sh
ios/build-core.sh && (cd ios && xcodegen generate)      # Rust core + Xcode project
cargo test -p canvas-mobile -p canvas-push -p canvas-mock # core, push, mock
./target/debug/canvas-mock --listen 127.0.0.1:47600 --cwd "$PWD" &   # UI tests need both mocks
./target/debug/canvas-mock --listen 127.0.0.1:47601 --state /tmp/canvas-mock-personal --cwd "$PWD" --name personal &
(cd ios && xcodebuild -project CopilotCanvas.xcodeproj -scheme CopilotCanvas \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test)
ios/cloud-mac.sh start | pair | status | stop | logs | report FILE   # this Mac as a stand-in Cloud PC
```
