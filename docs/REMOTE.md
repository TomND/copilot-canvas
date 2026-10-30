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

Only loopback IP literals are accepted (`127.0.0.1:PORT`, `[::1]:PORT`; names
such as `localhost` and other addresses are refused). Remote access always
goes through a Dev Tunnel that forwards to that port. The port is bound with
`SO_EXCLUSIVEADDRUSE`, so no other process (another user's included) can bind
it as well, and the service holds it for as long as it runs. The bound port is
written to `<home>/service-<instance>.port` (an owner-only file: a protected
DACL that names only this user, whatever the folder allows) and removed when
the service exits. When binding fails (the port is taken) the service logs
`NOT LISTENING for remote clients: …` and keeps the reason in
`<home>/service-<instance>.listen-error`; `copilot-canvas doctor` shows it as
a failure.

Clients:

| Command | Transport | Credential |
|---|---|---|
| `copilot-canvas` | named pipe | per-start service token |
| `copilot-canvas --remote loopback` | TCP to the local service; starts it with `COPILOT_CANVAS_LISTEN=127.0.0.1:0` when it is not running (a running service without listening is reported, not restarted) | service token (same machine, same Windows user only) |
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
   - `caps`: `CAP_FRAME8` (applies `Frame8`), `CAP_COMPRESS`, `CAP_IMAGES`,
     `CAP_NOTIFY`, `CAP_REPORTS` (the reports inbox, BACKEND.md);
   - `session`: 16 random bytes naming this client across reconnects;
   - `remote_id`: a paired client (`None`: the service token).

   The service checks every field before it looks anything up: a 32-byte
   nonce, a 16-byte session, `build` and `name` of at most 256 bytes, and a
   `remote_id` of the paired-client form (1–40 of `[A-Za-z0-9_.-]`).
2. Service → `Challenge { protocol: 8, build, nonce, proof }`; the proof is an
   HMAC-SHA256 over the client's nonce with the secret. The secret is the
   paired client's (`HostApp::remote_credential(id)`: known, not revoked) or
   the per-start token. Over TCP the token is only accepted from a process of
   the service's own Windows user on this machine: the service finds the
   process at the other end of the loopback connection
   (`GetExtendedTcpTable`, `TCP_TABLE_OWNER_PID_CONNECTIONS`) and compares
   its token's user SID with its own; anything else is refused. Over TCP
   `build` is empty here: the service tells it only to authenticated clients
   (`Welcome`, step 4).

   A refused client gets `Refusal { proof }` and the connection closes. For a
   revoked paired client the proof is an HMAC over the client's nonce with
   that client's revocation key (`pairing::revocation_key(secret)`; the
   service keeps only that key once it revokes a client), so only the client
   itself can check it; for anything else it is random. The client treats a
   refusal it could verify ("this client was revoked on the service; pair it
   again") as final. An unverified refusal (an unknown id, a refused token, or
   anything else that answers on the port) is retried and shown generically
   ("the service refused this client"): it carries no text, so nothing that
   squats on a port can put words in front of the user or stop a client for
   good.
3. Client checks the proof and sends `Auth { proof }` over the service's
   nonce. A wrong proof (`AuthError`: an impostor, or a secret the service no
   longer has) is retried like an unverified refusal.
4. Service (TCP) → `Welcome { build }`, then (both) `Ready8 { pid,
   input_ack }`, the terminal list and the backend snapshots, as for every
   client. Last seen is recorded only now, after the client's proof.

Before authentication the service reads frames of at most 16 KiB
(`PRE_AUTH_FRAME`; 32 MiB afterwards), and bodies are read as they arrive, so
a claimed length allocates nothing. At most 16 TCP connections may be in their
handshake at once; more are closed at once (logged at most every 10 s). The
handshake must finish within 5 s: a read deadline on the socket, no thread per
connection. Thread creation failures in the accept loops are logged and the
connection is dropped. Protocol-7 `Hello` is refused on TCP. A TCP connection
may open with `PairStart` instead of `Hello8` (see [Pairing](#pairing)).
Compressed bodies are decompressed with a bounded output (at most the size the
frame announced, a 16 MiB zstd window).

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
  nonce }` (index 24; the first pairing scheme, now refused), `PairStart {
  name, nonce }` (25), `PairProof { proof }` (26).
- `ServiceMsg`: `Frame8(Frame8)`, `Heartbeat { input_ack }`, `Ready8 { pid,
  input_ack }`, … , `Paired { client_id, nonce, secret }` (index 20),
  `PairChallenge { nonce, proofs }` (24), `Refusal { proof }` (25), `Welcome {
  build }` (26).

Both ends of a TCP connection need this build or a later one: an older client
cannot read `Welcome` and cannot pair with this service.

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
  0.5 s, 1 s, then every 1.5 s (Phase 6: it was 2 s, then every 5 s, which
  missed PLAN §9's "≤ 2 s after the network returns" after outages longer
  than a few seconds; an attempt is one TCP connect). A local service that is
  gone is started again. Remote clients never restart or upgrade the service:
  a protocol mismatch is shown as an error.
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
- Only a refusal the service proved (a revoked client, above) ends
  reconnecting: the banner says "Offline" and the notice "Could not reconnect
  to the terminal service: this client was revoked on the service; pair it
  again (…)". An unverified refusal or a failed service proof keeps the client
  retrying every 5 s with the banner "Offline — reconnecting… (the service
  refused this client)" or "… (the service did not prove it knows this
  client's secret)".
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
   run at home. Codes are 12 characters from `ABCDEFGHJKMNPQRSTUVWXYZ23456789`
   (31^12, about 59 bits; no 0/O/1/I/L), shown as `ABCD-EFGH-JKMN` and typed
   in any case, with or without dashes or spaces; valid for 10 minutes, used
   up by one pairing, at most 3 open at once. Opening a code also turns remote
   listening on in the running service. The service log records it
   prominently: `PAIRING CODE OPENED by a local process (copilot-canvas.exe
   pid N)` (or "a command in canvas terminal …" when the request came from a
   process inside one of its own terminals).
2. At home, `copilot-canvas remote add NAME --tunnel TUNNEL_ID [--port PORT]
   --code CODE` (or `--addr HOST:PORT` for a direct connection) connects (for
   a tunnel: through a `devtunnel connect` child) and sends `ClientMsg::PairStart
   { name, nonce }` instead of `Hello8`. `name` is the home PC's name
   (`--client-name`; a name given to `remote pair --name` wins), made unique
   among active clients.
3. Both sides derive keys from a code with PBKDF2-HMAC-SHA256, 200,000 rounds,
   salted with a context string and both nonces (one 32-byte master key, then
   HMAC-expanded into a proof key, an encryption key and a seal key). The
   service answers `PairChallenge { nonce, proofs }`: for every open code an
   HMAC with its proof key over the transcript (SHA-256 of the context, both
   nonces and the client's name, each length-prefixed), padded with random
   values to 3 and sorted. **The service proves it knows the code first**: the
   client derives its keys and only continues when one of the proofs matches;
   otherwise it stops with "wrong or expired pairing code (or too many recent
   attempts: wait a minute) …" and never sends anything that depends on the
   code. A service that has no open code answers exactly like one whose codes
   do not match.
4. Client → `PairProof { proof }` over the same transcript (its own role
   label). The service finds the code it matches, uses it up, creates a client
   id (`pc-` + 16 hex digits) and a 32-byte secret, stores them in
   `<home>\remote\clients.json` with the secret DPAPI-protected (user scope,
   extra entropy), and answers `ServiceMsg::Paired { client_id, nonce, secret
   }`: the secret is encrypted with the encryption key (HMAC-SHA256 in counter
   mode as the keystream) and tagged with the seal key over the transcript,
   the client id and the ciphertext. The tunnel is TLS already; the seal keeps
   the secret out of anything that only sees frames. The service logs
   `PAIRED a new remote client "pc-…" named "…" (it calls itself "…") with the
   code opened by … at …` and closes the connection.
5. The client opens the seal, stores `{name, tunnel or addr, port, client_id,
   secret (DPAPI), vscode_tunnel}` in `<home>\remote\servers.json` and makes a
   first normal connection (`Hello8 { remote_id }`, HMAC challenge with the
   secret) to check it.

Guessing: an attempt whose proof matches no code (or that ends early) counts
as a failure; the next attempt waits 250 ms, doubling up to 30 s, and a
success resets it. An attempt that would wait more than 2 s is answered with
random proofs at once (no derivation; the codes stay open). Derivations run
one at a time. Because the service proves first, anyone who can reach the port
can collect proofs and test guesses offline, but each guess costs 200,000
PBKDF2 rounds against a 59-bit code that lives 10 minutes; a captured
`PairStart` alone is worth nothing.

On the Cloud PC, `remote clients` lists the clients (paired, last seen,
connected now, revoked, and who opened the code each paired with, and when)
and `remote revoke ID|NAME` revokes one: its secret is erased (only its
revocation key is kept), its live connections are closed at once, and every
later attempt gets a refusal only that client can verify ("this client was
revoked on the service; pair it again"). A client admitted while it was being
revoked is checked again once it is listed and dropped. When the last active
client is revoked and no code is open, remote listening is turned off (setting
and listener) unless `COPILOT_CANVAS_LISTEN` asks for it; `remote pair` turns
it on again. At home, `remote servers` lists the paired services and `remote
forget NAME` removes one (revoke it on the service too). The pairing requests
are only answered on the service's own pipe, never to TCP clients.
`remote pair` and `remote revoke` change `settings.json` only when it parses:
a malformed file is reported and left alone.

| File | Side | Contents |
|---|---|---|
| `<home>\remote\clients.json` | service | `id`, `name`, `created_ms`, `last_seen_ms`, `revoked_ms`, `secret` (DPAPI, hex; empty once revoked), `revoked_key` (DPAPI, hex), `paired_by`, `code_opened_ms` |
| `<home>\remote\servers.json` | client | `name`, `tunnel`, `port`, `addr`, `vscode_tunnel`, `client_id`, `secret` (DPAPI, hex), `added_ms` |

DPAPI ties both files to the Windows user (and machine, unless the profile
roams): copying them elsewhere does not copy the secrets. Tunnel ids in
`remote add` and in `servers.json` must be lower-case letters, digits, `-`
and `.` starting with a letter or digit (they become `devtunnel` arguments, so
one can never read as an option); a record with another id makes the file an
error.

## Security notes

What is protected:

- Only processes of the same Windows user reach the service's pipe, the
  token file and the port file (owner-only DACLs). Over loopback TCP the token
  is refused unless the connecting process belongs to the service's user, so
  other users of a shared Cloud PC cannot use it even if they could read it.
- Everything else on the TCP port must prove a paired client's secret (or pair
  with a code). Before that, a peer can make the service read at most 16 KiB
  per frame, hold at most 16 handshakes for at most 5 s, and learn nothing
  about the service (no build, machine or client names; refusals carry no
  text).
- The Dev Tunnel provides TLS and owner-only access: `devtunnel create` makes a
  private tunnel that only the signed-in owner (GitHub or Microsoft account)
  can connect to; never add `--allow-anonymous` (`-a`) or access entries.
- A client never acts on what an unauthenticated peer says: refusals count only
  when proven, and the managed `devtunnel connect` child's port is used only
  from its own output or when the child itself owns the listener.

What is not:

- **A paired client is as trusted as you are on the Cloud PC**: it can open
  terminals, run anything, read files through the backend and change the
  canvas. Pair only your own machines, and revoke one you no longer use
  (`remote clients` shows when each was paired and by which code). Every
  pairing is logged prominently in `service-<instance>.log`.
- Frames after the handshake are neither authenticated nor encrypted by
  Copilot Canvas (only the handshake is): between the machines they rely on
  the Dev Tunnel's TLS, and on each machine on loopback TCP between
  `devtunnel` and Copilot Canvas.
- Local processes of the same user are trusted (they can read the token and
  the DPAPI secrets anyway).
- Anyone who can reach the port can slow pairing down (failed attempts back
  off for everyone) and can collect proofs of open codes for offline guessing
  (see above: 200,000 PBKDF2 rounds per guess, 59-bit codes, 10 minutes).

## Tunnel setup (Cloud PC)

The commands below are those of the installed Dev Tunnels CLI (`devtunnel
--version`: `Tunnel CLI version: 1.0.2115+e16833f284`), checked against its
`--help` and the strings in its binary:

| Command | Syntax (from `--help`) |
|---|---|
| `devtunnel user login` | `-g, --github` (GitHub), `-e, --entra` (Entra ID / Microsoft account, the default), `-d, --use-device-code-auth` |
| `devtunnel user show` | prints `Logged in as NAME using PROVIDER.` or `Not logged in.` (exit code 0 either way); `-j` prints `{"status": "Not logged in"}` |
| `devtunnel create [<tunnel-id>]` | private unless `-a, --allow-anonymous` |
| `devtunnel port create [<tunnel-id>] -p, --port-number <port-number>` | the port number is required |
| `devtunnel host [<tunnel-id>]` | `-p, --port-number, --port-numbers <port-numbers>`; with an existing tunnel and no `-p` it hosts the ports made with `port create` (the setup's way) |
| `devtunnel connect [<tunnel-id>]` | options: only `-t, --access-token` and `-v, --verbose`; no local port option |
| `devtunnel show <tunnel-id>`, `devtunnel list` | the tunnel, its ports and access |

`copilot-canvas remote tunnel-setup [--tunnel ID] [--port PORT]` (defaults:
`copilot-canvas`, the `remote.listen` port or 47600):

- writes the automation manifest `<automations>\copilot-canvas\CopilotCanvas-Tunnel.yaml`
  that runs `<full path of devtunnel.exe> host ID` with schedule `logon` and
  `every 1h` (with `overlap: ignore` an hourly start does nothing while the
  host runs, and starts it again when it is gone), `restart_on_failure: 10
  every 1m`, no time limit;
- sets `remote.listen = 127.0.0.1:PORT` and `remote.tunnel = ID` in
  `settings.json` (refusing to touch a malformed one);
- prints the one-time commands:

```powershell
devtunnel user login -g                     # GitHub (`devtunnel user login` alone: Microsoft/Entra account)
devtunnel create copilot-canvas             # private: only your account can connect (never add -a)
devtunnel port create copilot-canvas -p 47600
devtunnel show copilot-canvas               # check: port 47600, no anonymous access
copilot-canvas automations apply CopilotCanvas-Tunnel --yes
copilot-canvas automations run CopilotCanvas-Tunnel
```

When `devtunnel.exe` is not on `PATH` (WinGet installs it under
`%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.devtunnel_*`), the
printed lines start with `$dt = "<full path>"` and use `& $dt` instead.

`devtunnel host copilot-canvas` prints, once it is up (strings from the
binary): `Hosting port: 47600`, `Connect via browser: …`, `Inspect network
activity: …` and `Ready to accept connections for tunnel: copilot-canvas.REGION.`
(`Warning: Tunnel does not have any ports!` when `port create` was skipped).

The tunnel host is an ordinary manifest automation: it shows on the canvas,
the automation doctor watches its launcher log, and a failure event for it
carries a hint (check `devtunnel user show`, sign in again, `devtunnel show
ID`). `copilot-canvas doctor` reports remote listening (including a port it
could not bind), paired clients and servers, whether the Dev Tunnels CLI is
installed and signed in (the first line of `devtunnel user show`: signed in
when it starts with `Logged in`; `Not logged in.`, `Login token expired` and
`Previously logged in as …` are not), and the tunnel manifest and its Task
Scheduler state.

## Connecting from home

```powershell
winget install Microsoft.devtunnel
devtunnel user login -g                     # the same account that created the tunnel
copilot-canvas remote add cloudpc --tunnel copilot-canvas --port 47600 --code ABCD-EFGH-JKMN [--vscode-tunnel NAME]
copilot-canvas --remote cloudpc             # or: copilot-canvas --gui --remote cloudpc
```

`copilot-canvas --remote NAME` (or the GUI with the same option) resolves
`NAME` in `servers.json` and connects as the paired client:

- Tunnel records run `devtunnel connect TUNNEL` as a managed child: found on
  `PATH`, in WinGet's links or in WinGet's package folder
  (`%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.devtunnel_*`);
  `COPILOT_CANVAS_DEVTUNNEL` overrides it with a program and optional prefix
  arguments separated by `|` (`C:\…\copilot-canvas.exe|devtunnel-standin`).
  It runs without a console window, inside a kill-on-close job, so it ends
  with the client (also when the client crashes).
- `devtunnel connect` has no option for the local port: it listens on each of
  the tunnel's port numbers here, and when one is taken it logs `Listening on
  port 47600 failed: …` and `Incrementing port and trying again` and takes the
  next free one. It then prints lines built from the strings `Connected to
  tunnel: `, `Forwarding from ` and ` to host port ` in its binary (a line
  may carry a prefix such as `SSH: `), e.g.:

  ```text
  Connected to tunnel: copilot-canvas.REGION
  Forwarding from 127.0.0.1:47600 to host port 47600.
  Forwarding from [::1]:47600 to host port 47600.
  ```

  The client connects to the address of the `Forwarding from … to host port
  PORT.` line for the record's port (IPv4 first; a single forwarded port is
  used even when the numbers differ). If no such line appears within 8 s (a
  CLI that words it differently), it looks up which process listens where
  (`GetExtendedTcpTable`, `TCP_TABLE_OWNER_PID_LISTENER`) and uses a loopback
  port only when the `devtunnel connect` child itself (or a process it
  started) listens on it: whatever else answers on `127.0.0.1:47600` is never
  used. After 30 s without either it gives up with the child's last lines.
- A child that exits first is reported with its last lines and a hint, e.g.
  "`devtunnel connect copilot-canvas` exited (exit code: 3): Login required.
  Sign in once with `devtunnel user login -g` (GitHub) or `devtunnel user
  login` (Microsoft), as the account that created the tunnel." (`Login
  required.` and exit code 3 are what the CLI does when signed out.)
- Reconnecting (above) restarts the child when it died or kept failing.
- `--addr` records connect directly (an SSH forward; the loopback tests).
- The old `--remote HOST:PORT` with `COPILOT_CANVAS_REMOTE_ID` /
  `COPILOT_CANVAS_REMOTE_SECRET` still works.

For tests without a signed-in CLI, `copilot-canvas devtunnel-standin connect
TUNNEL` (hidden) emulates `devtunnel connect`: it forwards a free local port
(`COPILOT_CANVAS_STANDIN_PORT` to choose one) to
`COPILOT_CANVAS_STANDIN_TARGET` (`HOST:PORT`), prints the real CLI's
`Connected to tunnel: …` and `Forwarding from … to host port ….` lines, and
with `COPILOT_CANVAS_STANDIN_CONTROL=DIR` writes its pid to `DIR\standin.pid`
and drops every connection when `DIR\drop` appears.

## The iPhone (WebSockets through web forwarding)

The iPhone app (`ios/`, docs/IOS-PLAN.md) cannot run `devtunnel connect`.
It reaches the same port through the tunnel's **web forwarding**
(`wss://TUNNEL-PORT.REGION.devtunnels.ms`), which carries HTTP and
WebSockets, with a Dev Tunnels *connect* access token in
`X-Tunnel-Authorization: tunnel TOKEN` (private tunnels need the owner's
identity; the token stands in for it).

- **One port, two transports.** A connection to `remote.listen` that opens
  with `GET ` is an HTTP request: the service answers the WebSocket upgrade
  (`canvas_common::ws`, at most 8 KiB of request head, within the 5 s
  handshake deadline and the 16 handshake slots) and then reads the same
  byte stream from binary messages: length-prefixed postcard frames, zstd,
  the handshake. No valid frame starts with `GET ` (as a length it is about
  542 MB, far over the 16 KiB pre-authentication limit). Anything else is
  the plain TCP protocol, as before.
- **Paired clients only.** The service token is refused over a WebSocket,
  even from this user (web forwarding is not loopback from a local process).
  Pairing (`PairStart`) works as over TCP.
- **Tunnel access tokens.** `copilot-canvas remote pair --qr` issues one
  with this machine's signed-in CLI (`devtunnel token TUNNEL --scopes
  connect`, 24 h) and shows a QR code with the web address, the pairing code
  and the token (`ccanvas://pair?v=1&name=…&url=…&code=…&token=…&exp=…`).
  Clients that set `CAP_TUNNEL_ACCESS` (the phone) get
  `ServiceMsg::TunnelAccess { url, token, expires_ms }` when they connect
  and whenever the service renews the token (half a day before it expires,
  while such a client is connected; at most every 10 minutes after a
  failure, logged). The web address is `remote.web_url` in `settings.json`,
  else what `devtunnel show TUNNEL` lists for the port, else built from the
  token's cluster (`wss://TUNNEL-PORT.CLUSTER.devtunnels.ms`).
- **Shutdown.** `ClientMsg::Shutdown` from a paired remote client (phone or
  home PC) is refused: quitting a remote canvas disconnects it, never ends
  the service and its sessions.

### Notifications to the iPhone (APNs)

While the app is away (closed, or more than ~25 s in the background), the
service tells the phone when a session starts waiting for you or finishes a
turn, through Apple's push service (`crates/canvas-push`,
`canvas-agents/src/service/push.rs`).

- **The key.** Once per machine: `copilot-canvas remote push-key
  AuthKey_KEYID.p8 --team TEAMID` (developer.apple.com › Certificates, IDs &
  Profiles › Keys › "+" › Apple Push Notifications service; the team id is in
  Membership details). It is kept owner-only in `<home>\remote\apns.json`;
  the running service picks it up at once. `copilot-canvas remote push-test`
  sends a test notification to every registered phone.
- **Registration.** The app sends `ClientMsg::PushRegister { token,
  environment, bundle, label }` (appended at index 28) on every connection:
  its APNs device token, `sandbox` or `production` (from its provisioning
  profile), its bundle id (the APNs topic) and what the phone calls this
  machine (the notification's subtitle). Only paired clients can register;
  one phone per paired client, in `<home>\remote\push.json`. Revoking the
  client forgets it; a token APNs reports as unregistered is forgotten too.
- **What is notified.** Every 2 s the service derives each canvas session's
  status with the clients' own rules (`canvas_model::status`) from the
  history index, its terminals and `canvas.json`, and notifies phones that
  are not connected when a session becomes *needs you*, or *done · review*
  after working (`canvas_push::Attention`; collapsed per session). Connected
  phones show it themselves.
- **Tunnel tokens while away.** With phones registered and a key, the
  service keeps a Dev Tunnels connect token fresh (renewed 12 h before it
  expires) and sends each away phone a silent notification with the token
  sealed with that phone's own pairing secret
  (`pairing::seal_for_client`: HMAC-SHA256 counter-mode stream and tag,
  keys from the secret, a label and a fresh nonce): Apple only carries
  ciphertext. The phone opens it (`open_tunnel_access`) and connects with it
  next time. iOS may delay silent notifications; if a token still expires,
  the app says so and offers to pair again (the pairing is replaced by
  address, keeping the name and colour).

### Terminal sizes with several clients

A hosted terminal has one size (its ConPTY). The service remembers each
connection's wish (its last `Resize`) per terminal and gives the terminal
the wish of its **owner**: the connection that most recently typed in it
(`Input`, `Input8`, `Click`) or claimed it (`ClientMsg::Claim { id }`,
appended at index 27). Without an owner, or when the owner asked for no
size, the newest wish applies, which is the old "last `Resize` wins" for a
single client. A connection that disconnects or stops showing the terminal
(`Subscribe8` without it) is forgotten; the newest remaining wish applies.
A terminal it launched starts with its size and owner.
(`canvas_common::sizing`, tmux's "latest" policy.)

So with the home PC and the phone on one session: typing on the phone (in
*Fit to this phone* mode) reflows the agent to the phone's width; typing at
the desk gives the desktop's size back. The phone's *Mirror the desktop*
mode sends no size: it draws the desktop's grid scaled down. The desktop
clients send their size again when they show a terminal again, and forget
what they sent for terminals they stopped showing.

## Local vs remote behaviours

What happens where (PLAN.md §6). "Remote" means a service on another machine
(`--remote NAME` or `HOST:PORT`); `--remote loopback` is the same machine and
behaves like local mode here, except for folders and group files, which open
in the canvas in any remote mode (that is how the tests check them).

| What | Local | Remote |
|---|---|---|
| Text copy (selection, Ctrl+C, Ctrl+Shift+C), paste | this machine's clipboard | the home clipboard (client side) |
| OSC 52 copies from a terminal | the client that last sent input to that terminal (else the last client that typed) | the same: with the home PC and the Cloud PC's own window open, the copy goes to the machine you typed on |
| Image paste (Ctrl+V / Alt+V with an image, pane menu Paste) | the key goes to the agent, which reads the local clipboard | the client reads the home clipboard image (`PNG` format, else `CF_DIBV5`/`CF_DIB` converted to PNG; at most 20 MB and 32 M pixels), sends `Request::SetClipboardImage`, the service puts it on the Cloud PC clipboard in its user session as `CF_DIB` (32-bit) and `PNG`, then the client sends the same key. Offline: nothing is sent |
| URLs in terminals (Ctrl+click), report links | this machine's browser | the home browser |
| Paths in terminals, passive-session files (rules, memory, log), the events catalog | VS Code at the line / Explorer | checked on the service (`PathInfo`, so a missing file says "… does not exist"); with `--vscode-tunnel NAME` on the server record and VS Code installed at home: `vscode://vscode-remote/tunnel+NAME/C:/path:LINE`; otherwise the text viewer |
| Folders (config views' "Open folder", group folders and their chips, the sidebar's folder rows, passive folder) | Explorer | VS Code through the tunnel, else the canvas's folder browser (subfolders and files, `ListEntries`; a file opens in the viewer through `ReadText`; "Copy path"). Folders follow this in any remote mode, `--remote loopback` included, so the service never opens Explorer for a client |
| Group files: passive rules, memory, log, group context, `GOALS.md`, the events catalog | VS Code at the file | the same in any remote mode (`loopback` included): VS Code through the tunnel, else the viewer |
| Automation outputs, manifests, logs, status reports | their default program | always the text viewer, with "Open in VS Code" when a tunnel is configured |
| Bell, OSC 777, taskbar progress, window title | this machine | the home PC (they are computed from terminal state the service sends) |
| Reports (`copilot-canvas report publish` on the service's machine) | the inbox from the local service | the same inbox and read state, pushed by the service (`ServiceMsg::Reports`) and opened through `Request::ReportContent`; HTML is shown by the home GUI's own WebView2 view; links inside a report open in the home browser after you confirm them. The iPhone app gets the same inbox (`CAP_REPORTS`) and shows each report as a sanitized page in a locked-down web view; links open in the phone's Safari after you confirm them; a phone that is away gets a notification per new report (at most 3 at once) |
| Crash log | `<home>\crash.log` of the client | each side keeps its own: the client's in its data folder at home; the service logs to `service-<instance>.log` on the Cloud PC |

The folder browser is the same modal for a folder: its subfolders (▸) and
files, ↑↓ pick one, Enter or a click opens it (a subfolder in place, a file
in the viewer on top), Backspace goes up, Tab reaches the buttons. An older
service that cannot list files shows the subfolders only.

The text viewer is a canvas modal (TUI and GUI): read-only, monospace (the
cell grid), the first 2 MB of a file (`ReadText`), tabs expanded and control
characters shown as `·` (nothing is interpreted), line numbers; ↑↓ PgUp/PgDn
Ctrl+Home/End scroll, ←→ scroll sideways, Tab moves between the buttons, Enter
presses one, Ctrl+C copies the path, Esc or q closes; the wheel scrolls.
Buttons: "Open in VS Code" (with a tunnel), "Copy path" (to the home
clipboard), "Close". Nothing is ever executed.

## Manual checklist (real tunnel)

Validated on the owner's real setup (home PC and Cloud PC over a private Dev
Tunnel) on 2026-10-28. The build machine's Dev Tunnels CLI (1.0.2115) is not
signed in, so automated tests cover everything up to the tunnel with the
stand-in (`selftest --remote paired`). To set it up or check it again:

On the Cloud PC:

```powershell
copilot-canvas remote tunnel-setup                  # manifest + remote.listen; prints the next lines
devtunnel user login -g                             # once (GitHub)
devtunnel create copilot-canvas                     # once; private
devtunnel port create copilot-canvas -p 47600       # once
devtunnel show copilot-canvas                       # port 47600, no anonymous access
copilot-canvas automations apply CopilotCanvas-Tunnel --yes
copilot-canvas automations run CopilotCanvas-Tunnel
copilot-canvas service start
copilot-canvas remote pair --name home-pc           # note the code (ABCD-EFGH-JKMN)
copilot-canvas doctor                               # listening, CLI signed in, task running
```

At home:

```powershell
winget install Microsoft.devtunnel
devtunnel user login -g                             # the same GitHub account
copilot-canvas remote add cloudpc --tunnel copilot-canvas --port 47600 --code ABCD-EFGH-JKMN [--vscode-tunnel NAME]
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
    with "this client was revoked on the service"; pair again.
12. Quit the canvas: no `devtunnel.exe` stays behind.

## Testing

- Unit tests: DPAPI round trip and tampering (`canvas-common::dpapi`); code
  format, PBKDF2 derivation, proofs bound to the transcript, sealing and the
  failure backoff (`canvas-common::pairing`); `servers.json` persistence and
  tunnel id validation (`canvas-common::remote`); DIB↔PNG and oversized images
  (`canvas-common::clipimage`); pairing over TCP with the service proving the
  code first, "no code open" and "wrong code" answered alike, the refusal of
  the old `Pair`, pairing only over TCP, opening fields checked before any
  lookup, verified and unverified refusals, the build told after
  authentication (`canvas-common::handshake`); owner-only token files
  (`canvas-common::auth`); loopback IP literals only, `SO_EXCLUSIVEADDRUSE`
  against a squatter, read deadlines, the token peer decision (same user, on
  loopback) and the peer process and listener owner lookups
  (`canvas-common::tcp`); the pre-authentication frame limit, claimed lengths
  not allocated, and a zstd bomb refused (`canvas-common::wire`); code expiry,
  single use, backoff and reset, `clients.json`, unique names, revocation keys
  and the tunnel manifest (`canvas-agents::remote`); answers too large for a
  frame (`canvas-agents::service`); a flood of unauthenticated connections
  with huge length prefixes and silent peers that neither stops the service
  nor locks out a legitimate token client over TCP, the image budget, OSC 52
  routing (`canvas-host::server`); named servers, an unproven refusal retried
  and a proven one ending reconnects (`canvas-core::conn`); the real CLI's
  `Forwarding from` lines and others, a squatter on the tunnel's port never
  used, a listener of the child's own process tree used, login hints
  (`canvas-core::tunnel`); `devtunnel user show` states and the setup commands
  (`remote_cli`); remote image paste, remote links, folders and automation
  files, the viewer (`canvas-core::actions`, `canvas-core::viewer`,
  `canvas-core::links`).
- Transport unit tests: framing with and without compression, oversized and truncated
  frames (`canvas-common::wire`); the handshake for protocol 7 and 8, wrong
  secrets, unknown paired clients and `Hello` over TCP
  (`canvas-common::handshake`); every new variant and the protocol-7 filter
  (`canvas-common::protocol`, `canvas-host::server`); `Frame8` diffs and link
  tables; input dedupe and takeover; heartbeats keeping a connection and
  silence ending it with short timeouts (`canvas-core::conn`); offline input,
  reconnect resync and the lost-keystroke report (`canvas-core::app`).
- `copilot-canvas selftest --remote loopback`: the 39-step UI self-test with
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
  opened; codes read `XXXX-XXXX-XXXX` and pair when typed in lower case
  without dashes; a wrong code and a used code are refused; `remote add` over
  a direct address and through the stand-in (whose child ends with `remote
  add`); secrets are not stored in the clear; a wrong secret (AuthError,
  retried), an unknown id (an unproven, generic refusal, retried) and a client
  without credentials are refused; pairings are logged prominently; then the
  canvas with `--remote NAME` through the stand-in: a shell,
  a dropped connection (reconnected 0.5 s later), a killed tunnel child
  (started again, reconnected 0.6 s later), a path Ctrl+clicked in the
  terminal opening in the viewer through `ReadText`, `remote revoke` dropping
  the canvas with "was revoked" (a refusal the client verified) and no further
  retries, the tunnel child ending
  with the canvas; `remote clients` and `remote forget`. With
  `CANVAS_PROBE_CLIPBOARD=1` it also puts an image on the service's clipboard
  over the paired connection and reads it back (this replaces your clipboard,
  and needs an unlocked interactive session).
- `copilot-canvas bench --remote loopback`: keystroke latency and client
  traffic with the echo tile focused and four busy tiles, compressed and not.
- `copilot-canvas selftest --remote soak [--minutes N] [--drop-every S]
  [--client tui|gui] [--json FILE]`: the long-run soak below.

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

## Long-run soak (Phase 6)

`copilot-canvas selftest --remote soak --minutes N --drop-every S --client
tui|gui` (`src/soak.rs`) runs a service listening on loopback TCP behind the
fault-injecting proxy of `--remote faults`, with three terminals streaming 20
coloured lines a second (`noise`) and two echo terminals. Two clients connect
through the proxy:

- a protocol client (the canvas's own `conn` code, reconnecting forever with
  the same backoff) typing into `echo` every 50 ms; every echo line it sees is
  recorded, and the keys the service confirmed must appear exactly once and in
  order (unconfirmed keys at a drop are reported and never resent; keys typed
  while offline are blocked);
- the canvas itself (the TUI in a hidden pseudoconsole, or the GUI over its
  test hooks) with four busy tiles (`ui-echo` and the three streams), typing a
  key into `ui-echo` every 500 ms and checking each one: applied once, or not
  applied near a fault (blocked or unconfirmed), never doubled, never lost
  while online. Before every key it checks that the echo tile has the
  keyboard (the GUI's `tree`: focus, focused terminal, no dialog; the TUI's
  hint line and the heavy outline of the focused tile). When it does not,
  nothing is typed (the batch is counted as skipped, with the reason) and a
  click on the tile's header gives it the keyboard back, so keys never reach
  the canvas, where letters are shortcuts (`x` stops a terminal, `o` opens the
  other client). Half-way the harness takes the keyboard away once on purpose
  (Ctrl+]) and the next batch must notice. An echo terminal that ended is
  started again and reported. The GUI runs with the other-client dry run, so
  an `o` that slipped through would show up as a failure instead of a window.

Every S seconds the proxy injects the next fault of a cycle: drop every
connection; take the network down for 5 s; cut the next frame in half; delay
everything by 300 ms each way for 10 s; take the network down for 15 s. For
each fault it measures the protocol client's reconnect after the network
returned and its full resync (reconnected → the echo terminal's full frame),
and the canvas's reconnect (polled every 50 ms: the GUI's `connected`, the
TUI's banner). Every 20 s it samples private memory and handle counts of the
canvas and the service; every minute it prints a summary with the bandwidth
from the proxy (all clients, and the canvas: all minus the protocol client).
The JSON report lists every fault. The run fails on a lost, doubled or
reordered keystroke, a reconnect over 2 s or a resync over 500 ms, a drop or
outage without a reconnect, steady memory growth (over 20 MB/h and 15 MB
after the first 20 %), growing handles, the canvas exiting, or more than
200 KB/s for the four busy tiles.

**Runs on 2026-10-28** (release build, 12 logical CPUs, workstation locked,
other tests running at the same time): 20 minutes per client with a fault
every 30 s (`--minutes 20 --drop-every 30`). Full reports:
`docs/soak/soak-tui-20min.json`, `docs/soak/soak-gui-20min.json`.

| | TUI | GUI |
|---|---|---|
| Result | **PASS** | **PASS** |
| Faults (8 drops, 8 × 5 s outages, 8 cut frames, 8 × 10 s delays, 7 × 15 s outages) | 39 | 39 |
| Connections through the proxy | 56 | 56 |
| Protocol client: keystrokes handed to the transport / applied exactly once in order | 16,846 / 16,839 | 16,839 / 16,833 |
| … out of order, changed or doubled · echo lines not observed | 0 · 0 | 0 · 0 |
| … unconfirmed at a drop (reported, not resent) · blocked while offline | 7 · 2,969 | 6 · 3,129 |
| … reconnect after the network returned, p50 / p95 / max (n) | 518 / 1,005 / 1,042 ms (27) | 527 / 1,032 / 1,044 ms (30) |
| … full resync p50 / p95 / max | 3 / 9 / 25 ms | 4 / 27 / 33 ms |
| Canvas: keys typed / applied once / blocked or unconfirmed near a fault | 1,967 / 1,889 / 77 | 1,971 / 1,894 / 76 |
| … lost while online · doubled · wrong | 0 · 0 · 0 | 0 · 0 · 0 |
| … reconnect after the network returned, p50 / p95 / max (n) | 535 / 1,052 / 1,065 ms (27) | 529 / 1,058 / 1,516 ms (24) |
| Canvas bandwidth, 4 busy tiles (zstd, coalesced) · all clients | 2.4 KB/s · 3.4 KB/s | 2.4 KB/s · 3.4 KB/s |
| Canvas private memory first → last (max) · slope after warm-up | 9.8 → 7.7 MB (10.3) · 2.4 MB/h | 162.1 → 168.3 MB (168.9) · 8.7 MB/h |
| Canvas handles first → last | 158 → 158 | 525 → 519 |
| Service private memory first → last (max) · slope | 25.1 → 24.0 MB (25.6) · 7.0 MB/h | 25.1 → 23.8 MB (25.8) · 3.1 MB/h |
| Service handles first → last | 256 → 252 | 256 → 253 |

By fault kind (max): drops reconnect in 0.53–0.58 s (the first backoff step),
5 s outages in 1.04–1.07 s after the network returned, 15 s outages in
0.03–0.09 s for the protocol client and up to 1.52 s for the GUI (the
1.5 s backoff step), cut frames in 0.55–0.58 s; the 300 ms delays never
caused a reconnect. Before this phase the backoff grew to 5 s, so an outage
longer than about 4 s could take up to 5 s to recover from; it is now 0.5 s,
1 s, then every 1.5 s.

The memory slopes are below the leak threshold and within the noise of a
20-minute run (the GUI's 6 MB over 16 minutes is consistent with its glyph
atlas and shape cache filling); a slow leak would need the long run to show.

**The 48 h soak (PLAN §5 Phase 6) is still to be done.** A 48 h GUI run
(2026-10-28) was stopped after its typed keys started reaching the canvas
itself; its first ~8.7 h were clean apart from one 6.32 s reconnect under
heavy build load (PLAN §0). Run it on an otherwise idle machine with:

```powershell
copilot-canvas selftest --remote soak --minutes 2880 --drop-every 300 --client gui --json soak-gui-48h.json
copilot-canvas selftest --remote soak --minutes 2880 --drop-every 300 --client tui --json soak-tui-48h.json
```

(on an unlocked workstation as well as a locked one; copy the two
executables elsewhere first if you want to rebuild meanwhile: a running soak
holds them open).
