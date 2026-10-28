# Remote mode

Remote mode runs the service (terminals, history, bus, automations) on the
Cloud PC and the canvas on the home PC. See PLAN.md §2, §4 (D2, D5), §5
Phase 1 and §6. This page describes the transport, pairing, the
`CopilotCanvas-Tunnel` automation, the home client's `devtunnel connect`
management and what happens where. The canvas document (owned by the service,
synced with patches) and the backend requests remote clients make are
described in [BACKEND.md](BACKEND.md).

```
 HOME PC                                             CLOUD PC
 copilot-canvas --remote cloudpc                     copilot-canvas service
   └ devtunnel connect copilot-canvas  ══ Dev Tunnel ══ devtunnel host copilot-canvas
       127.0.0.1:47600 (forwarded)       (TLS, owner     (CopilotCanvas-Tunnel automation)
                                          sign-in)          └ 127.0.0.1:47600 (remote.listen)
   <home>\remote\servers.json                          <home>\remote\clients.json
   (client id + DPAPI secret)                          (client ids + DPAPI secrets)
```

## Remote mode (loopback / direct TCP)

### Turning it on

The service listens on TCP only when asked:

- `remote.listen` in `<home>/settings.json`, e.g. `"127.0.0.1:47600"`, or
- `COPILOT_CANVAS_LISTEN` in the service's environment (overrides the setting;
  `127.0.0.1:0` picks a free port), or
- `copilot-canvas remote pair` (opening a pairing code starts listening in the
  running service; without a setting it saves `remote.listen =
  127.0.0.1:47600` first), or `remote tunnel-setup` (saves the setting).

Only loopback addresses are accepted; anything else is logged and skipped.
Remote access always goes through a Dev Tunnel that forwards to that port. The
bound port is written to `<home>/service-<instance>.port` and removed when the
service exits.

Clients:

| Command | Transport | Credential |
|---|---|---|
| `copilot-canvas` | named pipe | per-start service token |
| `copilot-canvas --remote loopback` | TCP to the local service; starts it with `COPILOT_CANVAS_LISTEN=127.0.0.1:0` when it is not running (a running service without listening is reported, not restarted) | service token (same machine) |
| `copilot-canvas --remote NAME` | a server paired with `remote add`: through its Dev Tunnel (`devtunnel connect`, managed by the client) or its direct address | the paired client id and secret (DPAPI) |
| `copilot-canvas --remote HOST:PORT` | TCP, e.g. a forwarded tunnel port | `COPILOT_CANVAS_REMOTE_SECRET` (hex) and `COPILOT_CANVAS_REMOTE_ID` |

The GUI takes the same `--remote` values (`copilot-canvas --gui --remote NAME`).

`remote.compress` (default on; `COPILOT_CANVAS_COMPRESS=0` turns it off) decides
whether a client asks for compressed frames and whether the service sends them.

### Protocol 8 handshake

Both transports carry the same frames: a little-endian u32 length and a
postcard body. Enums are append-only.

1. Client → `Hello8 { protocol: 8, build, nonce, client: ClientInfo }` where
   `ClientInfo { name, caps, session, remote_id }`:
   - `caps`: `CAP_FRAME8` (applies `Frame8`), `CAP_COMPRESS`, `CAP_IMAGES` (reserved);
   - `session`: 16 random bytes naming this client across reconnects;
   - `remote_id`: a paired client (`None`: the service token).
2. Service → `Challenge { protocol: 8, build, nonce, proof }`; the proof is an
   HMAC-SHA256 over the client's nonce with the secret. The secret is the
   paired client's (`HostApp::remote_secret(id)`: known, not revoked; it also
   records when the client was last seen) or the per-start token. A refused
   client (unknown or revoked id) gets `Error { message }` with the reason
   instead, and the connection closes; the client reports it as `Refused` and
   stops reconnecting.
3. Client checks the proof (a wrong one is an impostor, or a secret the
   service no longer has: the client stops) and sends `Auth { proof }` over
   the service's nonce.
4. Service → `Ready8 { pid, input_ack }`, then the terminal list and the
   backend snapshots, as for every client.

TCP clients that have not authenticated within 5 s are dropped; protocol-7
`Hello` is refused on TCP. A TCP connection may open with `Pair` instead of
`Hello8` (see [Pairing](#pairing)).

Compatibility:

- A protocol-7 client sends `Hello`; the service answers `Challenge { protocol: 7 }`
  and serves only protocol-7 shapes (`Frame`, `Ready`, no heartbeats): every
  message passes `ServiceMsg::shaped` on its way to a client.
- A protocol-8 client of an older service: the old service closes the pipe on
  `Hello8`, the client retries with `Hello` and talks protocol 7 (no `Input8`,
  `Subscribe8` or heartbeats). Older protocols still go through the lenient
  reader (`read_frame_lenient`, `decode_v3_terminals`) for the upgrade prompt.

Variants added by protocol 8 (appended):

- `ClientMsg`: `Hello8`, `Heartbeat`, `Input8 { id, input, seq }`,
  `Subscribe8 { ids, focused }`, … (see BACKEND.md), `Pair { code_proof, name,
  nonce }` (index 24).
- `ServiceMsg`: `Frame8(Frame8)`, `Heartbeat { input_ack }`, `Ready8 { pid,
  input_ack }`, … , `Paired { client_id, nonce, secret }` (index 20).

### Frames

Protocol-8 clients get `Frame8`: rows of `Run8` (grapheme clusters, wide
cells, underline style and colour, overline, hyperlink index), cursor shape and
blink, the mode bits, and only the hyperlink targets the included rows
reference. Rows are diffed by a hash over the cells and link targets; a new
subscription (every reconnect) starts with a full frame. `TermView::apply8`
keeps the protocol-8 rows and derives the protocol-7 `lines` the TUI draws with
`runs_v7`, so the TUI output is unchanged.

### Compression

Only on TCP, only for protocol 8, and only when the client set `CAP_COMPRESS`
and the service's `remote.compress` is on. The high bit of the length marks a
compressed body `[u32 uncompressed length][zstd block]`. Each direction of a
connection is one zstd stream; every compressed frame is a flushed block of
it, so rows that repeat across frames cost almost nothing. Bodies of 512 bytes
or less are sent as they are. Pipes never compress.

### Coalescing

TCP clients get at most one frame per tile every 16 ms for the focused tile
(`Subscribe8::focused`) and every 100 ms for the others; the pump wakes when a
deferred tile is due. Pipe clients are unchanged (4 ms pump).

### Liveness

Protocol-8 clients and the service send `Heartbeat` every 2 s, on the pipe and
on TCP. Either side closes a connection that stayed silent for 6 s (the
service's `liveness` thread, the client's watchdog thread). TCP keepalive is on
as a backstop and Nagle is off.

### Reconnect and input

- When the connection drops, the canvas shows "Offline — reconnecting…"
  (status bar; `App::banner` for other front ends) and reconnects with backoff
  0.5 s, 1 s, 2 s, then every 5 s. A local service that is gone is started
  again. Remote clients never restart or upgrade the service: a protocol
  mismatch is shown as an error.
- The new connection replaces the old one (like a service restart): the client
  sends its subscriptions, sizes, tracked sessions and routing again; the
  service sends the terminal list and full frames.
- While offline, terminal input (keys, paste, clicks, wheel, scroll) is dropped
  with "Offline: input not sent." It is never queued.
- Keyboard input travels as `Input8` with a per-session sequence number. The
  service applies each number at most once and only from the session's newest
  connection (a half-open old connection is closed and stops applying input),
  and reports the highest applied number in `Heartbeat` and `Ready8`. After a
  reconnect the client compares `Ready8.input_ack` with what it sent and shows
  "Reconnected. N keystroke(s) were not delivered." It never resends them.
- A refused client (revoked, unknown) and a service that fails authentication
  end reconnecting at once: the banner says "Offline" and the notice gives the
  service's reason, e.g. "Could not reconnect to the terminal service: this
  client (home-pc) was revoked on CLOUDPC; pair it again (`copilot-canvas
  remote pair` there)".
- Through a Dev Tunnel every attempt first asks for the forwarded address: a
  `devtunnel connect` child that died is started again, and one whose
  connections failed twice in a row (the tunnel is up locally but the relay is
  broken) is restarted.

## Pairing

Paired clients authenticate with their own long-lived secret (PLAN.md D5
"App-level authentication"). The flow:

1. On the Cloud PC, `copilot-canvas remote pair [--name NAME]` asks the
   running service (over its pipe, `Request::PairCode`; the service is
   started when needed) for a one-time code and prints it with the commands to
   run at home. Codes are 8 characters from `ABCDEFGHJKMNPQRSTUVWXYZ23456789`
   (no 0/O/1/I/L; shown as `ABCD-EFGH`, typed in any case, dashes optional),
   valid for 10 minutes, used up by one pairing, at most 3 open at once.
   Opening a code also turns remote listening on in the running service.
2. At home, `copilot-canvas remote add NAME --tunnel TUNNEL_ID [--port PORT]
   --code CODE` (or `--addr HOST:PORT` for a direct connection) connects (for
   a tunnel: through a `devtunnel connect` child) and sends
   `ClientMsg::Pair { code_proof, name, nonce }` instead of `Hello8`.
   `code_proof` is HMAC-SHA256(code, "pair" ‖ nonce): the code itself never
   travels. `name` is the home PC's name (`--client-name`; a name given to
   `remote pair --name` wins), made unique among active clients.
3. The service compares the proof with every open code (constant time each).
   A miss is answered with "wrong or expired pairing code" after 300 ms; the
   fifth miss cancels every open code ("too many wrong pairing codes"). A match
   uses the code up, creates a client id (`pc-` + 16 hex digits) and a 32-byte
   secret, stores them in `<home>\remote\clients.json` with the secret
   DPAPI-protected (user scope, extra entropy), and answers `ServiceMsg::Paired
   { client_id, nonce, secret }`: the secret is sealed under keys derived from
   HMAC-SHA256(code, context ‖ client nonce ‖ server nonce) (HMAC-SHA256 in
   counter mode as the keystream, then an HMAC tag over the client id and the
   ciphertext). The tunnel is TLS already; the seal keeps the secret out of
   anything that only sees frames. The service then closes the connection.
4. The client opens the seal, stores `{name, tunnel or addr, port, client_id,
   secret (DPAPI), vscode_tunnel}` in `<home>\remote\servers.json` and makes a
   first normal connection (`Hello8 { remote_id }`, HMAC challenge with the
   secret) to check it.

On the Cloud PC, `remote clients` lists the clients (paired, last seen,
connected now, revoked) and `remote revoke ID|NAME` revokes one: its secret is
erased, its live connections are closed at once, and every later attempt is
refused with "this client (NAME) was revoked on MACHINE; pair it again". At
home, `remote servers` lists the paired services and `remote forget NAME`
removes one (revoke it on the service too). The pairing requests are only
answered on the service's own pipe, never to TCP clients.

| File | Side | Contents |
|---|---|---|
| `<home>\remote\clients.json` | service | `id`, `name`, `created_ms`, `last_seen_ms`, `revoked_ms`, `secret` (DPAPI, hex; empty once revoked) |
| `<home>\remote\servers.json` | client | `name`, `tunnel`, `port`, `addr`, `vscode_tunnel`, `client_id`, `secret` (DPAPI, hex), `added_ms` |

DPAPI ties both files to the Windows user (and machine, unless the profile
roams): copying them elsewhere does not copy the secrets.

## Tunnel setup (Cloud PC)

`copilot-canvas remote tunnel-setup [--tunnel ID] [--port PORT]` (defaults:
`copilot-canvas`, the `remote.listen` port or 47600):

- writes the automation manifest `<automations>\copilot-canvas\CopilotCanvas-Tunnel.yaml`:
  `devtunnel.exe host ID --port-numbers PORT` (the `devtunnel host` syntax of
  CLI 1.0.2115; the CLI's full path), schedule `logon` and `every 1h` (with
  `overlap: ignore` an hourly start does nothing while the host runs, and
  starts it again when it is gone), `restart_on_failure: 10 every 1m`, no time
  limit;
- sets `remote.listen = 127.0.0.1:PORT` and `remote.tunnel = ID` in
  `settings.json`;
- prints the one-time commands: `devtunnel user login -g` (GitHub; avoids the
  401s seen with Microsoft accounts), `devtunnel create ID` (private: only the
  owner can connect; never `--allow-anonymous`), `devtunnel port create ID -p
  PORT`, then `copilot-canvas automations apply CopilotCanvas-Tunnel --yes`
  and `copilot-canvas automations run CopilotCanvas-Tunnel`.

The tunnel host is an ordinary manifest automation: it shows on the canvas,
the automation doctor watches its launcher log, and a failure event for it
carries a hint (check `devtunnel user show`, sign in again, `devtunnel show
ID`). `copilot-canvas doctor` reports remote listening, paired clients and
servers, whether the Dev Tunnels CLI is installed and signed in (`devtunnel
user show --json`; it exits 0 even when signed out, so its status is read),
and the tunnel manifest and its Task Scheduler state.

## Connecting from home

`copilot-canvas --remote NAME` (or the GUI with the same option) resolves
`NAME` in `servers.json` and connects as the paired client:

- Tunnel records run `devtunnel connect TUNNEL` as a managed child: found on
  `PATH`, in WinGet's links or in WinGet's package folder
  (`%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.devtunnel_*`);
  `COPILOT_CANVAS_DEVTUNNEL` overrides it with a program and optional prefix
  arguments separated by `|` (`C:\…\copilot-canvas.exe|devtunnel-standin`).
  It runs without a console window, inside a kill-on-close job, so it ends
  with the client (also when the client crashes).
- The client reads the child's output for `Forwarding from 127.0.0.1:LOCAL to
  host port PORT` (the local port may differ when PORT is taken here) and
  connects there; without such a line it probes `127.0.0.1:PORT` after 8 s.
  A child that exits first is reported with its last lines, e.g. "`devtunnel
  connect copilot-canvas` exited (exit code: 3): Login required. Sign in once
  with `devtunnel user login -g`".
- Reconnecting (above) restarts the child when it died or kept failing.
- `--addr` records connect directly (an SSH forward; the loopback tests).
- The old `--remote HOST:PORT` with `COPILOT_CANVAS_REMOTE_ID` /
  `COPILOT_CANVAS_REMOTE_SECRET` still works.

For tests without a signed-in CLI, `copilot-canvas devtunnel-standin connect
TUNNEL` (hidden) emulates `devtunnel connect`: it forwards a free local port
(`COPILOT_CANVAS_STANDIN_PORT` to choose one) to
`COPILOT_CANVAS_STANDIN_TARGET` (`HOST:PORT`), prints the same lines, and with
`COPILOT_CANVAS_STANDIN_CONTROL=DIR` writes its pid to `DIR\standin.pid` and
drops every connection when `DIR\drop` appears.

## Local vs remote behaviours

What happens where (PLAN.md §6). "Remote" means a service on another machine
(`--remote NAME` or `HOST:PORT`); `--remote loopback` is the same machine and
behaves like local mode here.

| What | Local | Remote |
|---|---|---|
| Text copy (selection, Ctrl+C, Ctrl+Shift+C), paste | this machine's clipboard | the home clipboard (client side) |
| OSC 52 copies from a terminal | the client that last sent input to that terminal (else the last client that typed) | the same: with the home PC and the Cloud PC's own window open, the copy goes to the machine you typed on |
| Image paste (Ctrl+V / Alt+V with an image, pane menu Paste) | the key goes to the agent, which reads the local clipboard | the client reads the home clipboard image (`PNG` format, else `CF_DIBV5`/`CF_DIB` converted to PNG; at most 20 MB), sends `Request::SetClipboardImage`, the service puts it on the Cloud PC clipboard in its user session as `CF_DIB` (32-bit) and `PNG`, then the client sends the same key. Offline: nothing is sent |
| URLs in terminals (Ctrl+click), report links | this machine's browser | the home browser |
| Paths in terminals, passive-session files (rules, memory, log), the events catalog | VS Code at the line / Explorer | checked on the service (`PathInfo`, so a missing file says "… does not exist"); with `--vscode-tunnel NAME` on the server record and VS Code installed at home: `vscode://vscode-remote/tunnel+NAME/C:/path:LINE`; otherwise the text viewer |
| Folders (config views' "Open folder", group folders, passive folder) | Explorer | VS Code through the tunnel, else the viewer (subfolders, "Copy path") |
| Automation outputs, manifests, logs, status reports | their default program | always the text viewer, with "Open in VS Code" when a tunnel is configured |
| Bell, OSC 777, taskbar progress, window title | this machine | the home PC (they are computed from terminal state the service sends) |
| Crash log | `<home>\crash.log` of the client | each side keeps its own: the client's in its data folder at home; the service logs to `service-<instance>.log` on the Cloud PC |

The text viewer is a canvas modal (TUI and GUI): read-only, monospace (the
cell grid), the first 2 MB of a file (`ReadText`), tabs expanded and control
characters shown as `·` (nothing is interpreted), line numbers; ↑↓ PgUp/PgDn
Ctrl+Home/End scroll, ←→ scroll sideways, Tab moves between the buttons, Enter
presses one, Ctrl+C copies the path, Esc or q closes; the wheel scrolls.
Buttons: "Open in VS Code" (with a tunnel), "Copy path" (to the home
clipboard), "Close". Nothing is ever executed.

## Manual checklist (real tunnel)

The exit criterion "the TUI at home through a real tunnel passes a manual
checklist over a day" could not be run where this was built: the Dev Tunnels
CLI there (1.0.2115) is not signed in, and signing in needs an interactive
browser. Everything up to the tunnel is covered by `selftest --remote paired`
with the stand-in. To run it for real:

On the Cloud PC:

```powershell
copilot-canvas remote tunnel-setup                  # manifest + remote.listen; prints the next lines
devtunnel user login -g                             # once (GitHub)
devtunnel create copilot-canvas                     # once; private
devtunnel port create copilot-canvas -p 47600       # once
copilot-canvas automations apply CopilotCanvas-Tunnel --yes
copilot-canvas automations run CopilotCanvas-Tunnel
copilot-canvas service start
copilot-canvas remote pair --name home-pc           # note the code
copilot-canvas doctor                               # listening, CLI signed in, task running
```

At home:

```powershell
winget install Microsoft.devtunnel
devtunnel user login -g                             # the same GitHub account
copilot-canvas remote add cloudpc --tunnel copilot-canvas --port 47600 --code ABCD-EFGH [--vscode-tunnel NAME]
copilot-canvas --remote cloudpc                     # or: copilot-canvas --gui --remote cloudpc
```

Checklist (over a day):

1. The canvas opens; `copilot-canvas remote clients` on the Cloud PC shows
   home-pc connected.
2. Open a session, type, run a Copilot turn; typing latency feels like RTT.
3. Copy text from a pane at home, paste it into another; an agent's OSC 52
   copy lands on the home clipboard.
4. Screenshot at home (Win+Shift+S), Ctrl+V / Alt+V in Copilot: the image is
   attached ("Image pasted …").
5. Ctrl+click a URL (home browser), a file path (VS Code tunnel or the
   viewer), a folder; open an automation's log and a passive session's memory.
6. Bell / OSC 777 / progress show on the home taskbar.
7. Disconnect the home network for 30 s during a turn: banner, typing
   blocked, recovery under 5 s after the network returns, the turn unaffected.
8. Kill `devtunnel.exe` at home (Task Manager): the canvas reconnects with a
   new child.
9. Restart the tunnel host on the Cloud PC (`automations run
   CopilotCanvas-Tunnel` after stopping it; or sign out of Windows and back
   in): the canvas reconnects once it is back.
10. Leave it overnight; check the next morning (sleep, token expiry).
11. `copilot-canvas remote revoke home-pc` on the Cloud PC: the canvas drops
    with "this client (home-pc) was revoked"; pair again.
12. Quit the canvas: no `devtunnel.exe` stays behind.

## Testing

- Unit tests: DPAPI round trip and tampering (`canvas-common::dpapi`); codes,
  proofs and sealing (`canvas-common::pairing`); `servers.json` persistence
  (`canvas-common::remote`); DIB↔PNG (`canvas-common::clipimage`); pairing over
  TCP, refusals with reasons, pairing only over TCP
  (`canvas-common::handshake`); code expiry, single use, attempt limits,
  `clients.json`, unique names, revocation and the tunnel manifest
  (`canvas-agents::remote`); OSC 52 routing (`canvas-host::server`); named
  servers and a refused client ending reconnects, forward-line parsing
  (`canvas-core::conn`, `canvas-core::tunnel`); remote image paste, remote
  links, folders and automation files, the viewer (`canvas-core::actions`,
  `canvas-core::viewer`, `canvas-core::links`).
- Transport unit tests: framing with and without compression, oversized and truncated
  frames (`canvas-common::wire`); the handshake for protocol 7 and 8, wrong
  secrets, unknown paired clients and `Hello` over TCP
  (`canvas-common::handshake`); every new variant and the protocol-7 filter
  (`canvas-common::protocol`, `canvas-host::server`); `Frame8` diffs and link
  tables; input dedupe and takeover; heartbeats keeping a connection and
  silence ending it with short timeouts (`canvas-core::conn`); offline input,
  reconnect resync and the lost-keystroke report (`canvas-core::app`).
- `copilot-canvas selftest --remote loopback`: the 26-step UI self-test with
  the client on TCP.
- `copilot-canvas selftest --remote faults` (or, after `cargo build --release`,
  `cargo test --release fault_injection -- --ignored --nocapture`): a TCP proxy
  between a protocol-8 client and the service drops the connection while
  keystrokes stream to an `echo` terminal, cuts a frame in half, and adds 300 ms
  of latency each way. It checks reconnect time, full frames after resync, that
  the echo terminal shows exactly the acknowledged keystrokes once and in order,
  and that unacknowledged ones are reported, not resent. A last step runs the
  canvas itself through the proxy: banner, offline input blocked, reconnect
  after the network returns, the pane resynced.
- `copilot-canvas selftest --remote paired`: a real service in a temporary
  data folder plays the Cloud PC, a second folder the home PC, the stand-in
  plays `devtunnel connect`. It checks that listening is off until a code is
  opened; a wrong code and a used code are refused; `remote add` over a
  direct address and through the stand-in (whose child ends with `remote
  add`); secrets are not stored in the clear; a wrong secret (AuthError), an
  unknown id (refused with a reason) and a client without credentials are
  refused; then the canvas with `--remote NAME` through the stand-in: a shell,
  a dropped connection (reconnected 0.5 s later), a killed tunnel child
  (started again, reconnected 0.6 s later), a path Ctrl+clicked in the
  terminal opening in the viewer through `ReadText`, `remote revoke` dropping
  the canvas with "was revoked" and no further retries, the tunnel child ending
  with the canvas; `remote clients` and `remote forget`. With
  `CANVAS_PROBE_CLIPBOARD=1` it also puts an image on the service's clipboard
  over the paired connection and reads it back (this replaces your clipboard,
  and needs an unlocked interactive session).
- `copilot-canvas bench --remote loopback`: keystroke latency and client
  traffic with the echo tile focused and four busy tiles, compressed and not.

Measured on the development Cloud PC (12 logical CPUs, 100 streaming terminals,
`bench` and `bench --remote loopback`):

| | keystroke → frame p50 / p99 | client traffic |
|---|---|---|
| pipe (local) | 1.2 / 10.3 ms | 309 KB/s (4 ms pump) |
| TCP loopback, coalesced | 4.2 / 10.3 ms | 148 KB/s |
| TCP loopback, coalesced + zstd | 4.2 / 10.8 ms | 19 KB/s |

The extra ~3 ms over loopback is the focused tile's 16 ms coalescing (the
benchmark types every ~25 ms); a loopback round trip itself takes ~30 µs.
Reconnect after a drop or a cut frame: 0.5 s (the first backoff step); the
canvas reconnected 0.4 s after the proxy accepted connections again.
