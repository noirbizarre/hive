# Development

## Setup and daily checks

Install Rust through rustup and development tools with `mise install`.
See [CONTRIBUTING.md](https://github.com/noirbizarre/hive/blob/main/CONTRIBUTING.md)
for hooks, commit conventions, and releases.

| Task | Command |
| --- | --- |
| Build workspace and embedded assets | `mise run hive:build` |
| Format / check formatting | `mise run format` / `mise run format:check` |
| Clippy | `mise run lint` |
| Deterministic tests | `mise run test` |
| Coverage | `mise run cover` |
| Markdown / spelling | `mise run lint:md` / `mise run spell` |
| Build / preview docs | `mise run docs:build` / `mise run docs` |
| Aggregate local checks | `mise run ci` |

`mise run ci` includes formatting checks, Clippy, workflow/Markdown lint,
spelling, tests, and the documentation build. It does not include every separate
CI job, including browser checks and MSRV testing.

## Client checks

The browser uses Vue 3.5, OpenVue 1.0.0 and the Aura theme. Vite compiles its
JavaScript and CSS into `clients/hive-web/dist/`, which Rust embeds. Generated
assets are not committed. Install Node 22.22.2+ and run `mise run hive:web` before
Cargo on a clean checkout and after frontend changes. This installs the locked
dependencies, builds production assets, and runs Node protocol tests and Vue
component tests with Vitest/jsdom. Use `mise run hive:ci` to prepare/test the
frontend before aggregate CI without racing duplicate builds.

For focused iterations after installation, run `npm test --prefix clients/hive-web`
or `npm run build --prefix clients/hive-web`. Rebuild Rust to update the embedded
UI. No frontend server is needed at runtime. The adapter in
`clients/hive-web/src/client.js` owns the browser's derived protocol view;
Vue components consume its snapshots and commands, never a separate registry.
See [ADR-008](adr/008-embedded-vue-interface.md) for the dependency evaluation and
the [browser smoke checklist](guides/web.md#browser-smoke-checklist) for visual checks.

## Local runtime

For the TUI, `cargo test -p hive-tui` covers editing, selection, clipboard result
routing, and protocol interactions. On Unix it also runs controlling-PTY tests
through Python 3, exercising the real event loop, queued agent questions during
external editing, reconnects, and terminal restoration. The subprocess reuses the
instrumented test executable so these paths contribute to CI coverage. Run
`python3 crates/hive-tui/tests/terminal.py` from the repository root for the
opt-in controlling-PTY smoke test. It checks cursor visibility, bracketed paste,
exclusive editor handoff, and restoration after editor failure without opening
a desktop clipboard or running an interactive editor.

The ignored native clipboard round-trip test writes the clipboard. Run it on an
isolated display, for example with Xvfb on Linux:

```sh
xvfb-run -a env -u WAYLAND_DISPLAY cargo test -p hive-tui \
  clipboard::tests::a_native_clipboard_worker_retains_ownership_between_copy_and_paste \
  -- --ignored --exact
```

Follow [getting started](getting-started.md) for a real-agent or mock session.
Use [configuration](configuration.md) and [operations](operations.md) for path,
listener, persistence, and recovery behavior instead of duplicating those rules
in development setup. Never log profile secrets or mix agent stderr with ACP stdout.

## Editing documentation

Pages live under `docs/`, with explicit navigation in `zensical.toml`. Add every
maintained page to the sidebar, including individual ADRs. Keep existing page
URLs and linked headings stable when extracting sections.

Use short paragraphs with one subject each. Write procedures as numbered steps,
controls and defaults as tables, and protocol guarantees in the reference pages.
Prefer periods and visible paragraph breaks to semicolon-linked explanations.

Run `mise run lint:md`, `mise run spell`, and `mise run docs:build` after edits.
Preview with `mise run docs` and check navigation, internal anchors, diagrams,
code copying, and narrow-screen layout in both themes. The site build alone
does not validate every link or browser-rendered diagram. The deployment workflow
builds on main-branch pushes, not as a dedicated pull-request documentation gate.

Architecture diagrams use native Mermaid fences through SuperFences. Keep the
prose explanation useful without the diagram, and verify rendering after
instant navigation as well as a full page load.

## Real-agent compatibility probes

### Session settings without inference

The ignored settings probe creates a session, changes one advertised setting,
restores its original value, and verifies it on reattachment without sending a
prompt. Use an installed, authenticated adapter and an existing workspace:

```sh
HIVE_REAL_OPENCODE=1 HIVE_REAL_OPENCODE_WIRE=v1 \
  cargo test -p hive-daemon --test real_agents \
  real_agent_settings_can_change_and_restore_without_inference -- --ignored --exact --nocapture
```

The same probe accepts `HIVE_REAL_CLAUDE=1`, `HIVE_REAL_CODEX=1`, or
`HIVE_REAL_PI=1`, with installed `claude-agent-acp`, `codex-acp`, or `pi-acp`.
Set `HIVE_REAL_<AGENT>_CONFIG_ID` to select a specific advertised option, such as
Codex's `collaboration_mode`. Otherwise it chooses the first mode, thinking, or
collaboration selector. The probe prints `PROBE` version evidence and a `CONFIG`
summary. Keep source-inspected support separate from completed runtime probes.

### Prompt and lifecycle probes

The mock gateway test (`cargo test -p hive-daemon --test gateway`) is the
deterministic CI baseline. CLI lifecycle and status checks live in
`cargo test -p hive-cli --test daemon`. `crates/hive-daemon/tests/real_agents.rs` contains
five **ignored** tests, independently selectable for Pi or OpenCode. They run
only with an explicit `HIVE_REAL_PI=1` or `HIVE_REAL_OPENCODE=1` opt-in. Missing
binary/version or native session ID prints `SKIP` with the missing
prerequisite. Once a configured binary is available, a failing protocol/model
probe is an actionable test failure.

Runs with model turns can consume paid model tokens. Use an
existing empty directory as `HIVE_REAL_<AGENT>_CWD` to keep conversations
separate from the repository (the default is the current working directory).
Make sure a working provider/model is configured in the agent's own environment.
Credentials stay in that environment. The tests do not print them, command
arguments, agent replies, native IDs or Hive ownership tokens.

Inspect entrypoints and versions before opting in:

```sh
pi --version
pi --help
opencode --version
opencode acp --help
```

Pi has no ACP mode of its own. Install the external `pi-acp` adapter (see
[Pi](compatibility.md#pi)). The inspected OpenCode 1.18.35 CLI advertises
`opencode acp` with documented stdio wire v1.

These observations do not prove the agent can initialize or prompt.
For a configured OpenCode model, run:

```sh
HIVE_REAL_OPENCODE=1 HIVE_REAL_OPENCODE_WIRE=v1 HIVE_REAL_OPENCODE_CWD=/absolute/empty/workspace \
  cargo test -p hive-daemon --test real_agents opencode_creates_prompts_and_reattaches_to_a_live_hive_session -- --ignored --exact --nocapture
```

Pi defaults to an installed `pi-acp` (otherwise `npx -y pi-acp`) on wire v1.
`hive doctor pi` runs the same probe from the CLI. The initialize-only probe
sends no prompt and uses no model tokens. It prints a
`CAPABILITIES` line and fails with a `hive::compat::*` diagnostic naming the fix
for a missing adapter or a bare `pi` command:

```sh
HIVE_REAL_PI=1 cargo test -p hive-daemon --test real_agents \
  pi_initialize_probe_reports_a_capability_report -- --ignored --exact --nocapture
```

The full Pi turn (this may use model tokens) is:

```sh
HIVE_REAL_PI=1 HIVE_REAL_PI_CWD=/absolute/empty/workspace \
  cargo test -p hive-daemon --test real_agents pi_creates_prompts_and_reattaches_to_a_live_hive_session -- --ignored --exact --nocapture
```

| Override | Meaning |
| --- | --- |
| `HIVE_REAL_<AGENT>_COMMAND` | Executable, including paths with spaces |
| `HIVE_REAL_<AGENT>_ARGS_JSON` | JSON array of separate argument strings |
| `HIVE_REAL_<AGENT>_WIRE` | `v1` or `v2` |

OpenCode defaults to `acp` and v1. Pi defaults to `pi-acp` and v1, and its version
is read from `pi --version` because the adapter has no `--version`.

Each test prints the agent/version, declared wire, downstream stdio and upstream WS/HTTP+SSE.
`FIRST` and `RESUMED` summarize observed text chunks/snapshots, running, tool
updates and idle. Session startup has a 15-second silence watchdog, extended
while human decisions are pending. The probe gives a model turn 120 seconds.

Status checks verify that disconnect retains a live
Hive session and explicit close removes it. This is **not** a daemon-restart or
native-session adoption test. If initialization fails, check the entrypoint,
wire, model, and agent logs. No automatic v2-to-v1 fallback is attempted.

To probe a session made in the agent's native interface, use the **same** cwd
and model, create a harmless text conversation there, then obtain its real ID.
For OpenCode, run `opencode run 'Say hello without using tools'`, list IDs with
`opencode session list --format json`, and check continuation using
`opencode run --session <id> 'Say hello again without using tools'`. For Pi,
run `pi --session-id <chosen-id> -p 'Say hello without using tools'` and then
`pi --session-id <chosen-id> -p 'Say hello again without using tools'`. Pi
creates a missing ID, so verify the first command succeeded. Native commands may use
credentials and incur model costs. Avoid pasting their output into public logs.
Then run the independently ignored **direct agent ACP** attachment probe:

```sh
HIVE_REAL_OPENCODE=1 HIVE_REAL_OPENCODE_WIRE=v1 HIVE_REAL_OPENCODE_CWD=/same/absolute/workspace \
  HIVE_REAL_OPENCODE_NATIVE_ID='<id-from-session-list>' \
  cargo test -p hive-daemon --test real_agents opencode_reports_whether_a_native_session_can_be_attached_over_acp -- --ignored --exact --nocapture
```

Replace `OPENCODE`/`opencode` with `PI`/`pi` to test Pi through `pi-acp`.
The v1 direct probe calls `session/load`. V2 calls
`session/resume` with `replayFrom: start`. Both require a nonempty retained
conversation, count replayed message updates without printing their contents,
and fail if attachment returns no conversation. A failed load does not establish that native sessions cannot
be reopened via their own CLI or server. Hive adopts native IDs only through an
`opencode-server` profile (below), never an ACP one: record native-CLI reopen,
direct ACP attachment and Hive's live-worker resume as **separate** results in the
[Known agents matrix](compatibility.md#known-agents).

For optional tool calls/results, permission requests, user-input requests,
cancel, and disconnect while generation is active, inspect a deliberately
configured manual agent session and record unexercised capabilities as
unverified rather than inferring support from `initialize`.

### OpenCode server adoption probe

`crates/hive-daemon/tests/real_opencode_server.rs` holds one **ignored** test for
the `opencode-server` adapter. By default it starts its own throwaway
`opencode serve` with temporary `HOME`/`XDG_*` directories and the free
`opencode/big-pickle` model, so it touches neither your sessions nor your
credentials. It needs only the `opencode` binary.

It acts as the OpenCode TUI
would (creates a session and runs a turn natively), then discovers and adopts it
through Hive, continues it, and checks that OpenCode still holds exactly one
session containing both turns.

```sh
HIVE_REAL_OPENCODE_SERVER=1   cargo test -p hive-daemon --test real_opencode_server -- --ignored --nocapture
```

| Variable | Meaning |
| --- | --- |
| `HIVE_REAL_OPENCODE_SERVER` | `1` to opt in; otherwise the test prints `SKIP` and passes |
| `HIVE_REAL_OPENCODE_SERVER_COMMAND` | `opencode` executable to start (default `opencode`) |
| `HIVE_REAL_OPENCODE_SERVER_URL` | use an existing server instead of starting one (it **will** create and prompt a session in your current directory) |
| `HIVE_REAL_OPENCODE_SERVER_PASSWORD_ENV` | name of the variable holding that server's password |

It prints `PROBE`, `NATIVE`, `ADOPTED`, `FIRST` and `RESUMED` markers. Retain them
with the run date before changing a cell of the
[OpenCode server matrix](compatibility.md#capability-and-version-matrix).
Failures name a fixed stage and never echo server replies. The deterministic
equivalent, covering cancel, permissions, questions, reconnect, restart, version
and credential handling, is `cargo test -p hive-daemon --test opencode`. It runs
against `tests/support/fake_opencode.rs`, which records every call so tests can
assert that Hive did **not** create a session.

## Template maintenance

The repository is generated from `rust.tpl`. Use the template workflow in
[CONTRIBUTING.md](https://github.com/noirbizarre/hive/blob/main/CONTRIBUTING.md#this-repository-is-generated-from-a-template).
Review diffs before merging template updates. Extend template-owned files below
their project-specific markers. The workspace member list is the documented exception.
