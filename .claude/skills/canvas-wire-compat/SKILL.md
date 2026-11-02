---
name: canvas-wire-compat
description: Rules and proving tests for changing anything Copilot Canvas sends over the wire or keeps on disk, so older services, desktop clients and the iPhone app keep working - ClientMsg and ServiceMsg (postcard), backend Request and Response (JSON), the canvas document (Doc, Node, Group, NodeKind, shared fields and sync ops), bus Routing, PassiveSpec, Snapshot, actions and events, AgentLaunch, and the service's JSON stores. Use when you add or change a protocol message, a backend request, a document or store field, an enum variant, a bus action or event, or a launch parameter, and when you review such a change.
---

# Wire and storage compatibility

Older peers stay in use: a home PC client against a Cloud PC service of
another build, the iPhone app, a service that keeps running sessions until
someone restarts it. Background: `docs/BACKEND.md` (*Protocol* and the
"*… on the wire*" tables), `docs/DEVELOPMENT.md` sections 2 and 5.

## Which encoding, which rule

| What | Where | Encoding | Rule |
|---|---|---|---|
| `ClientMsg`, `ServiceMsg` and every struct inside them (`AgentLaunch`, `ClientInfo`, …) | `canvas-common/src/protocol.rs` | postcard: variant index, then fields by position | append variants at the **end** only; never add, remove or reorder a field of an existing variant or struct |
| `Request`, `Response` | `canvas-model/src/rpc.rs` | JSON in `ClientMsg::Rpc` / `ServiceMsg::RpcReply` | append at the end; new fields `#[serde(default)]`; answer existing requests with their existing `Response` variants |
| `Doc`, `Node`, `Group` and their parts | `canvas-model/src/doc.rs` | JSON (the file, `Doc`, `DocPatch`) | `#[serde(default, skip_serializing_if = …)]` on every new field |
| Edits | `canvas-model/src/sync.rs` (`Op`) | JSON | no new `Op` variants without a plan for older services |
| `Routing`, `PassiveSpec`, `bus::Snapshot`, `Event`, bus actions | `canvas-model/src/bus.rs`, service `passive.rs` | JSON (`ClientMsg::Routing`, `ServiceMsg::Bus`, `ClientMsg::Bus`, inbox files) | new fields default and skip when empty; never rename or remove one |
| Stores (todo book, PR registry, review book, reports) | `<home>/canvas-agents/<canvas id>/…`, `<home>/reports` | JSON files the service alone writes | a format version; never overwrite a newer or unreadable store |

## postcard messages

- A **new `ClientMsg` variant** makes an older service drop the connection:
  its reader fails to decode it (`canvas-host/src/server.rs`). Send it only to
  a service known to read it, or prefer a JSON channel an older service
  ignores: a bus action (`ClientMsg::Bus`) or a request (`ClientMsg::Rpc`,
  answered "does not support this request (update it)" by older services).
- A **new `ServiceMsg` variant** breaks older clients of the same protocol:
  give it a capability bit (`CAP_*` in `ClientInfo::caps`, set by the clients
  that read it) and filter it in `ServiceMsg::shaped_for`; add it to the
  protocol-7 list in `shaped`.
- **Extra launch data** never becomes an `AgentLaunch` field: carry it in
  `AgentLaunch::env` as JSON under a `COPILOT_CANVAS_…` name and take it out
  in the service before the agent starts (`canvas-model/src/launch.rs`:
  `GROUP_SKILLS_ENV`, `attach_group_skills`, `take_group_skills`). An older
  service passes the variable to the agent, where it does nothing.
- Test: extend `protocol.rs` `protocol8_variants_round_trip_at_appended_indices`
  with the new variant at its index; `images_and_notifications_only_reach_clients_that_asked_for_them`
  shows the capability pattern.

## The document

- **Never add a `NodeKind`.** It has no fallback variant (`#[serde(other)]`):
  an older peer fails to parse the whole canvas. Use an existing kind plus a
  field (canvas-wide agents: `Session` with `source: "canvas-agent"`).
- Node and group fields sync by JSON name (`sync.rs:changed_fields`): only the
  serde attributes are needed. An older service drops an unknown field when it
  writes the canvas; say what an older peer does in the docs.
- A top-level `Doc` field that clients edit goes into `shared_fields!(…)` in
  `sync.rs`. An older service refuses a `SetField` name it does not know: the
  whole edit is refused and the window gets the document again
  (`service/mod.rs:doc_edit`). The same happens for an unknown `Op` variant.
- Fields the service assigns (`canvas_id`) are not shared fields. View state
  of one window is `ClientPrefs`, never the document.
- The service never originates document patches.
- Tests: `doc.rs` `agent_fields_default_for_old_canvases_and_are_omitted_when_unset`,
  `notebook_settings_are_shared_and_absent_from_older_canvases`; `visual.rs`
  `old_canvases_fold_subagents_and_the_choices_round_trip`; add one that loads
  old JSON without the field and checks an unset field is not written.

## Backend requests

- Unknown requests are answered `rpc::unsupported`; the caller shows the
  message ("update the service") instead of failing silently.
- Tests: a round trip in `rpc.rs` and a renamed-variant answer
  (`skill_source_requests_round_trip_and_old_services_say_so`), and a contract
  test in `tests/backend_contract.rs` (local and remote answers equal).

## Routing, the bus and passive runs

- `ClientMsg::Routing` carries `bus::Routing` with its `PassiveSpec`s: new
  fields `#[serde(default)]` (older clients do not send them) and skipped when
  empty (`passive_specs_carry_skill_folders_only_when_set`). The service keeps
  one routing table, the last canvas a window sent; demo windows send none.
- `bus::Snapshot` additions are `Option` fields (`prs`, `reviews`, `todos`):
  older services send none, older clients ignore them; fill them in both
  `passive.rs:bus_loop` and `passive.rs:snapshot`.
- Bus actions (`{"action": …}`): older services ignore unknown ones, so the
  UI must not assume success; the todo hub answers refusals with an error.
- Inbox files are written by any local process: never accept a user-only
  action from them (window-only actions are ignored there:
  `passive.rs:bus_loop`, `reviews::window_only`).

## Stores

- A format version in the stored JSON (`canvas_model::todos::VERSION`;
  reports `SUBMISSION_VERSION`), bumped when an older build would drop data
  on its next write.
- A store written by a newer version is shown but never written; one that
  cannot be read is never replaced and changes are refused until it reads
  (`service/todos.rs:Hub`, test `a_book_that_cannot_be_read_is_never_overwritten`).
- Atomic writes (a temporary file renamed over the old one), one writer: the
  service. The CLI reads the file and sends changes as inbox requests.

## Before you commit

Run `cargo test --workspace` and the `--remote loopback` self-tests (skill
`canvas-verify`), and add the row to the topic doc's compatibility table:
Where, Addition, An older peer.
