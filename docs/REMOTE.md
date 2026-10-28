# Remote mode

Remote mode runs the service (terminals, history, bus, automations) on the
Cloud PC and the canvas on the home PC. See PLAN.md §2, §4 (D2, D5) and §5
Phase 1. Pairing, the `CopilotCanvas-Tunnel` automation and `devtunnel connect`
management come later; this page describes the transport they build on. The
canvas document (owned by the service, synced with patches) and the backend
requests remote clients make are described in [BACKEND.md](BACKEND.md).

## Remote mode (loopback / direct TCP)

### Turning it on

The service listens on TCP only when asked:

- `remote.listen` in `<home>/settings.json`, e.g. `"127.0.0.1:47600"`, or
- `COPILOT_CANVAS_LISTEN` in the service's environment (overrides the setting;
  `127.0.0.1:0` picks a free port).

Only loopback addresses are accepted; anything else is logged and skipped.
Remote access always goes through a Dev Tunnel that forwards to that port. The
bound port is written to `<home>/service-<instance>.port` and removed when the
service exits.

Clients:

| Command | Transport | Credential |
|---|---|---|
| `copilot-canvas` | named pipe | per-start service token |
| `copilot-canvas --remote loopback` | TCP to the local service; starts it with `COPILOT_CANVAS_LISTEN=127.0.0.1:0` when it is not running (a running service without listening is reported, not restarted) | service token (same machine) |
| `copilot-canvas --remote HOST:PORT` | TCP, e.g. a forwarded tunnel port | `COPILOT_CANVAS_REMOTE_SECRET` (hex) and, once pairing exists, `COPILOT_CANVAS_REMOTE_ID` |

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
   paired client's (`HostApp::remote_secret(id)`; pairing is not built yet, so
   every id is refused) or the per-start token.
3. Client checks the proof (a wrong one is an impostor: the client stops) and
   sends `Auth { proof }` over the service's nonce.
4. Service → `Ready8 { pid, input_ack }`, then the terminal list and the
   backend snapshots, as for every client.

TCP clients that have not authenticated within 5 s are dropped; protocol-7
`Hello` is refused on TCP.

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
  `Subscribe8 { ids, focused }`.
- `ServiceMsg`: `Frame8(Frame8)`, `Heartbeat { input_ack }`, `Ready8 { pid, input_ack }`.

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

### Testing

- Unit tests: framing with and without compression, oversized and truncated
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
