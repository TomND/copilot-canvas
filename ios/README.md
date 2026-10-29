# Copilot Canvas for iPhone

The canvas of your Cloud PC on your phone: sessions sorted by what needs
you, the canvas itself, and real terminals you can type in, over the same
private Dev Tunnel the home PC uses. Design: [docs/IOS-PLAN.md](../docs/IOS-PLAN.md).

```
 iPhone app (SwiftUI + UIKit terminal)        Cloud PC
  Rust core (crates/canvas-mobile, UniFFI) ══ wss://<tunnel>-47600.<region>.devtunnels.ms ══ copilot-canvas service
  protocol 8, pairing, canvas replica,          X-Tunnel-Authorization: tunnel <token>          (same port as `devtunnel connect`)
  terminal screens, session statuses
```

## Build and run

Needs Xcode 26 and Rust (`rustup`; the script adds the iOS targets).

```sh
ios/build-core.sh          # once: the Rust core as CanvasMobileCore.xcframework + Swift bindings (ios/Core)
open ios/CopilotCanvas.xcodeproj
```

Then pick your iPhone and press Run. Xcode rebuilds the core before every
build (the "Build the Rust core" phase; `COPILOT_CANVAS_SKIP_CORE=1` skips it).

- **Signing:** the project uses your team (`DEVELOPMENT_TEAM` in
  `project.yml`) with automatic signing. Add your Apple ID once in
  *Xcode › Settings › Accounts*; the first run on the phone asks you to trust
  the developer in *Settings › General › VPN & Device Management*.
- **Project file:** generated from `project.yml` with
  [XcodeGen](https://github.com/yonaskolb/XcodeGen) (`brew install xcodegen`,
  then `cd ios && xcodegen`). Change `project.yml`, not the project, for
  lasting changes.

## Pair with the Cloud PC

On the Cloud PC (with remote mode set up, docs/REMOTE.md):

```powershell
copilot-canvas remote pair --qr --name iphone
```

It prints a QR code holding the tunnel's web address, a one-time pairing
code and a Dev Tunnels *connect* token (24 h; the service sends fresh ones
while the phone is connected). In the app: **Scan the pairing QR code**. If
the web address cannot be found, pass it: `--url wss://copilot-canvas-47600.usw2.devtunnels.ms`
(the `Connect via browser` address `devtunnel host` prints, with `wss://`).

The phone then appears in `copilot-canvas remote clients`; revoke it with
`copilot-canvas remote revoke iphone`. The app locks with Face ID (a paired
phone can do what you can on the Cloud PC). The pairing lives in the
Keychain, on this device only.

If the token expired (the phone was not connected for a day), scan a new QR
code; the pairing itself stays.

## Without Windows: the mock service

`crates/canvas-mock` is a stand-in service for development on a Mac: the
real protocol, pairing and WebSocket, a demo canvas with sessions in every
state, and real shells in pseudo-terminals (agents are shells that print an
agent-like transcript; Enter makes one "work" for 4 s).

```sh
cargo run -p canvas-mock                       # 127.0.0.1:47600, code ABCD-EFGH-JKMN, prints the pairing link
cargo run -p canvas-mock -- --listen 0.0.0.0:47600   # reachable from a phone on the same Wi-Fi (ws://<mac>:47600)
```

In the simulator, paste the printed `ccanvas://pair?…` link, or launch with
`-pairLink <link>`. Other launch arguments for automated runs:
`-noNotificationPrompt`, `-noLock`, `-openSession <id>`, `-startMode canvas`,
`-machine <name>` (`-pairLink` may be repeated).

## This Mac as a stand-in Cloud PC (real tunnel)

To try the phone through a real private Dev Tunnel without the Windows
machine, `ios/cloud-mac.sh` runs the mock service behind its own tunnel and
shows a pairing QR code:

```sh
brew install --cask devtunnel && devtunnel user login -g   # once
ios/cloud-mac.sh start      # tunnel copilot-canvas-mac, port 47700; opens the QR code
ios/cloud-mac.sh pair       # a new one-time code (pairings stay)
ios/cloud-mac.sh status | logs | stop
```

A second stand-in (to try several machines) needs its own tunnel, port and
state: `NAME=personal TUNNEL=copilot-canvas-mac2 PORT=47701 STATE=~/.copilot-canvas-mock2 ios/cloud-mac.sh start`.

It is the same path the Windows service uses: `wss://copilot-canvas-mac-47700.REGION.devtunnels.ms`,
a 24 h connect token issued with this Mac's `devtunnel` (renewed while the
phone is connected), pairing with a random one-time code. Sessions started
from the phone run the agent CLIs installed on the Mac (OpenCode, Claude
Code, Copilot; they ask before running tools); the demo sessions are shells.
The canvas and pairings are kept in `~/.copilot-canvas-mock`. `TUNNEL`,
`PORT`, `STATE` and `WORKDIR` choose another tunnel, port, state folder and
start folder. The Mac stays awake while it hosts (on power).

## Tests

```sh
cargo test -p canvas-mobile                    # the core, and the phone ↔ mock end to end over a WebSocket
cargo run -p canvas-mock &                     # then the app's unit and UI tests on a simulator:
xcodebuild -project ios/CopilotCanvas.xcodeproj -scheme CopilotCanvas \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test
```

The UI tests pair with the mock, list its sessions, open the canvas, start a
session, type into its shell with the keyboard and check the echo, check that
the keyboard gets out of the way of the new-session form, and switch between
two machines (a second mock: `cargo run -p canvas-mock -- --listen
127.0.0.1:47601 --state /tmp/canvas-mock-personal`).

## What it does

| | |
|---|---|
| **Sessions** | Grouped like the canvas, what wants you first: *Needs you* (gold), *Ready for review* (blue), then groups by state and activity. Subagents under their session, subsessions after their parent, loops (`↻ every 15m`), queued mail, branch and agent. Swipe to stop, remove or mark reviewed; long-press to rename. Pull to reconnect |
| **Canvas** | The desktop canvas: groups, boxes with the same status borders (gold heavy for needs you, blue double for done · review, dashed passive), subagent and subsession edges, spinners. Drag to pan, pinch to zoom, tap a box to open it |
| **Terminal** | The service's terminal engine (libghostty-vt on the Cloud PC) draws the screen; the phone renders its cells with Core Text: colours, bold, italic, dim, inverse, every underline style, strike-through, wide and emoji cells, box drawing as geometry, cursor shapes, your-prompt tint in Copilot, OSC 8 links. Keys are encoded on the service for the program's keyboard mode |
| **Machines** | Pair several (work, personal): every one stays connected while the app is open. The menu at the top left switches between them and shows each one's state and what needs you there; the machine's colour runs under the bar; a strip lists the sessions of the *other* machines that need you or finished (tap: switch and open). Alerts name the machine; a notification opens its machine. Each machine gets a name and a colour when paired (Settings › Machines: rename, recolour, reorder, forget); pairing a machine again at the same address replaces it |
| **Keyboard** | Closes with a tap outside any field, the button above it, or a swipe down on forms and lists; in terminals the key bar's hide button stays at its right end and a swipe down on the bar closes it. Picking a folder never brings it back |
| **Typing** | iOS keyboard plus a key bar (Esc, Ctrl, Alt, Tab, ⇧Tab, arrows, ^C, ^D, Home/End, PgUp/PgDn, paste); hardware keyboards (arrows, Esc, Ctrl/Option combinations, ⌘V); a **prompt composer** (autocorrect, dictation) that sends one paste and Enter; one-hand quick keys (Esc, ↑ ↓ ⏎, 1 2 3, y n) for permission prompts |
| **Gestures** | Swipe up/down: wheel (the agent scrolls its own view, else the scrollback); long-press and drag: select, release copies; tap: links open in Safari, clicks reach agents that read the mouse; pinch: text size |
| **Sizes** | *Fit to this phone*: the terminal follows the phone while you type in it (tmux "latest"; typing at the desk gives it back). *Mirror the desktop*: the desktop's grid scaled down, nothing resized |
| **New session** | Agent, folder (browse the Cloud PC's folders, recent folders), group, first prompt |
| **Connection** | Offline banner, reconnect with backoff (0.5 s, 1 s, 1.5 s), typing never queued offline, lost keystrokes reported; the connection lingers ~25 s in the background, then closes; reconnects when you come back |
| **Notifications** | Local notifications when a session needs you or finishes while the app is open or just backgrounded; tapping one opens the session |

Not yet (docs/IOS-PLAN.md phase 5): push notifications while the app is
closed, GitHub sign-in for tunnel tokens, inline images, automations and
configuration views.
