# iPhone client: plan

Status: implemented 2026-10-29 (owner: Thomas); real-tunnel check pending (§7). Goal: an on-the-go iPhone client
for the canvas that connects to the Cloud PC's service over the existing Dev
Tunnel and lets you see and work with the remote sessions: a session list
sorted by attention, the canvas, and real terminals you can type in. Not full
parity with the desktop clients; a good mobile subset.

Approved decisions: service changes are fine; tmux-style terminal sizing
("the client that typed last sets the size"); paid Apple developer account
(push notifications possible); the app lives in this repository (`ios/`).

## 1. Constraints found in the code

| Finding | Consequence |
|---|---|
| The home client reaches the tunnel by running `devtunnel connect` (`canvas-core/src/tunnel.rs`); iOS cannot run it | The phone uses Dev Tunnels **web forwarding** (`https://<tunnel>-<port>.<region>.devtunnels.ms`, WebSockets allowed) with `X-Tunnel-Authorization: tunnel <connect token>`; the service needs a WebSocket front door |
| Terminals are parsed on the service (libghostty-vt) and sent as `Frame8` cell rows | The phone needs no terminal engine; it renders cells. Ghostty's VT core and key encoder already run on the Cloud PC |
| `Resize` resizes the one ConPTY for every client, last writer wins (`server.rs`, `term.rs`) | A per-client size policy on the service (tmux "latest") |
| Status labels are derived on the client (`canvas-core/src/status.rs`) | Move the pure status rules into a shared crate |
| Nothing is `cfg(windows)`-gated; `windows-sys` is unconditional | Gate Windows modules so the protocol, model and term view crates build for iOS |
| `canvas-core::App` and `canvas-gui` are desktop/cell-layout bound | New mobile core crate; Swift UI |
| Desktop clients write derived content into the document (subagent boxes, automation boxes, routing) | The phone is a *light* client: it only writes what the user does on it |

## 2. Architecture

```
 iPhone (SwiftUI)                                         Cloud PC
 ┌──────────────────────────────────────┐               ┌───────────────────────────────────┐
 │ Sessions · Canvas · Terminal         │               │ devtunnel host (CopilotCanvas-    │
 │ Terminal renderer (Core Text)        │   wss://      │   Tunnel automation)              │
 │ Keyboard bar · prompt composer       │ devtunnels.ms │   └ 127.0.0.1:47600               │
 │ URLSessionWebSocketTask ─────────────┼──────────────▶│ service                            │
 │ CanvasMobile (Rust, UniFFI):         │ X-Tunnel-     │  + WebSocket link (sniffed on the │
 │  protocol, wire, handshake, pairing, │ Authorization │    remote port; paired clients    │
 │  replica, term views, status, launch │               │    only)                          │
 │ Keychain (secret, tunnel token)      │               │  + per-client size (tmux latest)  │
 │ Face ID lock                         │               │  + tunnel access token rotation   │
 └──────────────────────────────────────┘               │  + QR pairing, no remote Shutdown │
                                                        └───────────────────────────────────┘
```

### D1. Rust core through UniFFI (`crates/canvas-mobile`)

Reuses `canvas-common` (protocol, wire framing and zstd, handshake, pairing
crypto), `canvas-model` (document, `Replica` sync, bus snapshot types, RPC
types), `canvas-termview` (`TermView::apply8`) and the status rules. Swift owns
the UI, the WebSocket and the Keychain. The core is transport-agnostic: Swift
feeds received bytes in and sends the bytes the core asks it to send.

### D2. WebSocket transport

WebSocket binary messages carry the same byte stream as the TCP link (length
prefix + postcard, zstd per direction), so framing, limits and the handshake
are unchanged. The service recognises an HTTP request on its remote port by
its first bytes (`GET ` as a little-endian length is ~542 MB, far above the
16 KiB pre-authentication limit, so no valid frame starts that way), so the
existing tunnel port 47600 serves both `devtunnel connect` and web forwarding.
WebSocket clients must be paired clients (the service token is refused).
Limits: request head ≤ 8 KiB, the same 5 s handshake deadline and handshake
slots as TCP.

### D3. Tunnel access

Private tunnels need the owner's identity. The service (whose `devtunnel` CLI
is signed in as the owner) issues `connect` tokens (`devtunnel token ID
--scopes connect`, valid 24 h), puts the first one in the pairing QR code and
sends fresh ones to phone clients over the live connection
(`ServiceMsg::TunnelAccess`, capability `CAP_TUNNEL_ACCESS`). The phone keeps
the newest in the Keychain. Recovery when the token expired (phone not opened
for a day): re-scan a QR from the desktop GUI or `copilot-canvas remote pair
--qr`, or (later) sign in with GitHub on the phone.

### D4. Terminal size: tmux "latest"

The service remembers each connection's wanted size per terminal. The ConPTY
takes the size of the connection that most recently typed in the terminal (or
claimed it); when that connection leaves, the newest remaining wish wins.
Single-client behaviour is unchanged. The phone has two modes per terminal:
*Fit phone* (sends its size and claims the terminal when you type) and
*Mirror* (never resizes; shows the desktop-sized grid scaled to fit).

### D5. Light client

The phone sends `Track` for the sessions it shows (statuses stay live with no
desktop connected), opens the shared document, and only edits it for user
actions (open → `reviewed_ms`, rename, new session, remove). It never writes
subagent or automation boxes, never sends routing and never sends `Shutdown`
(the service also refuses `Shutdown` from remote clients now).

### D6. Rendering

Core Text, one cached line per row with damage tracking (a phone shows ~55×30
cells in portrait and ~110 columns in landscape). Grapheme clusters, wide
cells, palette/truecolour, bold/italic/dim/inverse/strike, every underline
style, the four cursor shapes, OSC 8 links, Copilot's user-message tint.
Metal is a later option.

## 3. Phases

| Phase | Scope | Exit |
|---|---|---|
| 0 | Toolchain (rustup with iOS targets), UniFFI, Xcode project | builds |
| 1 | `cfg(windows)` gating of `canvas-common`, `canvas-model`, `canvas-termview`; shared status rules; `canvas-mobile` core | `cargo test` of those crates on macOS; `cargo build --target aarch64-apple-ios` |
| 2 | Service: WebSocket link, size policy, tunnel token rotation, `remote pair --qr`, remote `Shutdown` refused; a macOS **mock service** (real shell in a PTY, fake sessions and canvas) for development without Windows | unit tests; Windows CI; the mock serves the app |
| 3 | iOS MVP: pairing (QR or manual), Keychain, Face ID lock, connection lifecycle (foreground reconnect, offline banner, lost keystrokes), session list by attention, terminal (renderer, keyboard and accessory bar, prompt composer, scroll, copy, Fit/Mirror), open/resume, stop, mark reviewed | simulator run against the mock; device run over the real tunnel |
| 4 | Canvas (pan, zoom, groups, boxes, edges, status styles; tap opens, long-press menu), new session (agent, folder browser over RPC, first prompt), rename, remove, links to Safari | |
| 5 | Push notifications (needs you, done · review, automation failures) from the service, GitHub sign-in for tunnel tokens, background token refresh, inline images, iPad split view, Metal renderer | |

## 4. Pairing payload

`copilot-canvas remote pair --qr` (and the GUI's remote sheet) shows a QR code
with a URL:

```
ccanvas://pair?v=1&name=cloudpc&url=wss%3A%2F%2Fcopilot-canvas-47600.usw2.devtunnels.ms&code=ABCDEFGHJKMN&token=<connect token>&exp=<ms>
```

`url` is the tunnel port's web-forwarding address (from `devtunnel show`, or
`--url`). The code is the usual one-time pairing code (10 minutes, single
use); the token is a 24 h tunnel connect token. A plain `ws://HOST:PORT` URL
(no token) is used for the mock service and LAN tests.

## 5. Security

- A paired phone can do what you can on the Cloud PC: the app locks with Face
  ID / passcode, the client secret and tunnel token are Keychain items
  available only on this device after first unlock, and `copilot-canvas
  remote revoke iphone` cuts it off at once.
- WebSocket connections are paired-only and get the TCP pre-authentication
  limits; `PairCode`/`RemoteClients`/`RemoteRevoke` stay pipe-only; remote
  clients cannot shut the service down.
- The QR code holds a one-time code and a 24 h connect token: show it only on
  your own screen.

## 6. Risks

| Risk | Mitigation |
|---|---|
| Web forwarding of a port created with protocol `auto` | Fall back to a second port with `--protocol http` (`remote.ws_listen`) |
| Agent TUIs at ~50 columns | Landscape (~110 columns), Mirror mode, the prompt composer |
| iOS drops sockets in the background | Stateless clients resync in ~0.5 s on foreground; push for alerts (phase 5) |
| 24 h connect tokens | Rotation over the live connection; QR re-scan; GitHub sign-in (phase 5) |

## 7. Status (2026-10-29)

| Phase | State | Verified by |
|---|---|---|
| 0 Toolchain | Done: rustup with `aarch64-apple-ios(-sim)`, UniFFI 0.32, XcodeGen project (`ios/project.yml`), `ios/build-core.sh` (xcframework + Swift bindings) | device (arm64) and simulator builds |
| 1 Portability, core | Done: `windows-sys` is a Windows-only dependency of `canvas-common`, `canvas-model`, `canvas-host`; the pipe, DPAPI, token files and Win32 helpers are `cfg(windows)`; `tcp.rs` is portable; `canvas-termview`'s ratatui adapter is a feature; status rules (`canvas_model::status`) and the launch builder (`canvas_model::launch`) are shared with the desktop clients; `crates/canvas-mobile` (state, screens, connection, pairing, UniFFI API) | `cargo test` of `canvas-common`, `canvas-model`, `canvas-termview`, `canvas-host --no-default-features`, `canvas-mobile` on macOS; `cargo xwin clippy --workspace -D warnings` for Windows |
| 2 Service | Done: WebSocket link sniffed on the remote port (`canvas_common::ws`, paired clients only), tmux-"latest" sizes (`canvas_common::sizing`, `ClientMsg::Claim`), `ServiceMsg::TunnelAccess` + `CAP_TUNNEL_ACCESS` (tokens issued with `devtunnel token`, renewed half a day before expiry, `canvas_agents::tunnel_access`), `remote pair --qr [--url]`, `remote.web_url`, `Shutdown` refused from paired remote clients; `crates/canvas-mock` | unit tests (`ws`, `sizing`, pairing links, token and URL parsing); `websocket_clients_on_the_remote_port_must_be_paired` (Windows); `canvas-mobile/tests/end_to_end.rs` against the mock over a WebSocket |
| 3 iOS MVP | Done: pairing (QR, link, by hand), Keychain, Face ID lock, connection lifecycle, session list, terminal (renderer, key bar, hardware keys, composer, quick keys, selection/copy, links, wheel, pinch, fit/mirror), open/resume, stop, rename, remove, mark reviewed, local notifications | XCTest unit tests; XCUITest against the mock (pair, list, canvas, start a session, type into its shell and see the echo) |
| 4 Canvas, new session | Done: canvas (groups, boxes, status borders, edges, pan/zoom/fit, tap to open), new session (agent, folder browser over RPC, recent folders, group, first prompt) | simulator screenshots; UI test |
| 4b Several machines | Done: one live connection per paired machine, a switcher (state, what needs you), colours, an attention strip for the other machines, notifications open their machine, re-pairing replaces by address; keyboard closes by tap outside, a button and swipe-down | XCUITest with two mock services |
| 5 | Done: notifications while the app is away (APNs from the service: `canvas-push`, `remote push-key`, `push-test`, `ClientMsg::PushRegister`), tunnel tokens renewed by sealed silent notifications and an expired-token prompt, pictures to agents, a file viewer for terminal paths, passive session actions (`pause` bus action), subsessions and forks, jump to live, fit or mirror per session. Not done: GitHub sign-in for tokens, inline images, iPad split view, Metal renderer, QR code in the GUI's remote sheet | Rust unit tests (JWT verified, sealing, attention, registrations, forks, passive); XCTest; XCUITest (subsession, passive, keyboard, two machines); APNs delivery needs the team's key (manual) |

Still to check on the real setup (needs the Cloud PC and the phone):

1. `copilot-canvas remote pair --qr` finds the web address and issues a
   token with the signed-in `devtunnel` (else pass `--url`).
2. The phone pairs and connects through `wss://…devtunnels.ms` with the
   token. If the relay does not forward WebSockets to a port created with
   protocol `auto`, set the port to HTTP: `devtunnel port update
   copilot-canvas -p 47600 --protocol http` and check that the home PC's
   `copilot-canvas --remote` still connects (its `devtunnel connect`
   forwards the port over SSH); if not, give the phone its own port.
3. Sizes: typing on the phone fits a terminal to it; typing at the desk
   gives it back.
