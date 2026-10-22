# Development

`git tpl --json questions https://github.com/noirbizarre/rust.tpl` verifies
template answers. This existing repository was adopted with `git tpl init
https://github.com/noirbizarre/rust.tpl --answer crate=hive --answer
kind=workspace --answer publish=false --answer docs=true --defaults
--strict-answers --json` (plus a project description). To maintain it use
`git tpl status --json`, `git tpl update --dry-run --json`, `git tpl update
--defaults --json`, `git tpl diff --stat`, then `git tpl merge --json` after
review. Template-owned files carry a `# --- project-specific ---` marker:
extend below it; workspace `members` is the template's documented exception.

Run `mise install`, `mise run build`, `mise run format`, `mise run lint`,
`mise run test`, `mise run cover`, `mise run ci`. `mise run ci` includes
Markdown, spellcheck, clippy, tests and documentation builds. The web project
has zero third-party dependencies: `npm run build --prefix clients/hive-web`
checks JavaScript syntax and `npm run test --prefix clients/hive-web` exercises
its browser adapter. Plugin WASM: `rustup target add wasm32-wasip1` then
`cargo build -p hive-zellij --target wasm32-wasip1`. Native plugin tests do
not replace a live Zellij host integration run.

The daemon refuses non-loopback listener addresses. The default is
`127.0.0.1:8765`; use `--listen` / `HIVE_LISTEN` for a different loopback
address. `hive daemon` needs no agent argument: it loads profiles from
`$XDG_CONFIG_HOME/hive/config.toml`, falling back to `~/.config/hive/config.toml`,
or accepts `--config` / `HIVE_CONFIG`. `HIVE_DEFAULT_AGENT` overrides the optional
`default_agent` setting. Profiles declare `command`, `args`, `env`, or an
`endpoint` with optional `headers`, and `versions = ["v2", "v1"]` where supported.
Each session uses `_meta.hive.agent` or the configured default; only declared
v1 profiles use the compatibility adapter. Never log secrets from profiles.
Agent stderr is independent of ACP stdio stdout. The per-user lock lives under
`$XDG_RUNTIME_DIR/hive/daemon.lock` (or `~/.local/state/hive/daemon.lock`),
and is released on process exit. Session metadata is persisted separately under
`$XDG_STATE_HOME/hive/sessions.sqlite3` (or `~/.local/state/hive/sessions.sqlite3`);
replay events and activity snapshots remain in-memory and are lost on restart.

## Real-agent compatibility probes

The mock gateway test (`cargo test -p hive-daemon --test gateway`) is the
deterministic CI baseline. CLI lifecycle and status checks live in
`cargo test -p hive-cli --test daemon`. `crates/hive-daemon/tests/real_agents.rs` contains
four **ignored** tests, independently selectable for Pi or OpenCode. They run
only with an explicit `HIVE_REAL_PI=1` or `HIVE_REAL_OPENCODE=1` opt-in; missing
binary/version, Pi ACP entrypoint, or native session ID prints `SKIP` with the missing
prerequisite. Once a configured binary is available, a failing protocol/model
probe is an actionable test failure. Runs can consume paid model tokens. Use an
existing empty directory as `HIVE_REAL_<AGENT>_CWD` to keep conversations
separate from the repository (the default is the current working directory).
Make sure a working provider/model is configured in the agent's own environment.
Credentials stay in that environment; the tests do not print them, command
arguments, agent replies, native IDs or Hive ownership tokens.

Inspect entrypoints and versions before opting in:

```sh
pi --version
pi --help
opencode --version
opencode acp --help
```

The inspected Pi 1.1.0 CLI does not advertise ACP; obtain and verify an
ACP-capable extension/entrypoint before setting Pi's argument array. The
inspected OpenCode 1.18.35 CLI advertises `opencode acp`; its documented stdio
wire is v1. These observations do not prove the agent can initialize or prompt.
For a configured OpenCode model, run:

```sh
HIVE_REAL_OPENCODE=1 HIVE_REAL_OPENCODE_WIRE=v1 HIVE_REAL_OPENCODE_CWD=/absolute/empty/workspace \
  cargo test -p hive-daemon --test real_agents opencode_creates_prompts_and_reattaches_to_a_live_hive_session -- --ignored --exact --nocapture
```

For a verified Pi ACP entrypoint, substitute the actual arguments and wire
version, for example (replace the placeholder before running):

```sh
HIVE_REAL_PI=1 HIVE_REAL_PI_COMMAND=pi HIVE_REAL_PI_ARGS_JSON='["<verified-acp-argument>"]' \
  HIVE_REAL_PI_WIRE=v1 HIVE_REAL_PI_CWD=/absolute/empty/workspace \
  cargo test -p hive-daemon --test real_agents pi_creates_prompts_and_reattaches_to_a_live_hive_session -- --ignored --exact --nocapture
```

`HIVE_REAL_<AGENT>_COMMAND` overrides the executable (including a path with
spaces); `HIVE_REAL_<AGENT>_ARGS_JSON` is a JSON array of **separate** argument
strings; `HIVE_REAL_<AGENT>_WIRE` is `v1` or `v2`. OpenCode defaults to `acp`
and v1, Pi requires explicit ACP arguments and wire selection. Each test prints
the agent/version, declared wire, downstream stdio and upstream WS/HTTP+SSE;
`FIRST` and `RESUMED` summarize observed text chunks/snapshots, running, tool
updates and idle. Hive has a 15-second agent startup limit; a slow model turn
has a 120-second timeout. Status checks verify that disconnect retains a live
Hive session and explicit close removes it. This is **not** a daemon-restart or
native-session adoption test. If initialization fails, check the entrypoint,
wire, model and agent logs; no automatic v2-to-v1 fallback is attempted.

To probe a session made in the agent's native interface, use the **same** cwd
and model, create a harmless text conversation there, then obtain its real ID.
For OpenCode, run `opencode run 'Say hello without using tools'`, list IDs with
`opencode session list --format json`, and check continuation using
`opencode run --session <id> 'Say hello again without using tools'`. For Pi,
run `pi --session-id <chosen-id> -p 'Say hello without using tools'` and then
`pi --session-id <chosen-id> -p 'Say hello again without using tools'`. Pi
creates a missing ID, so verify the first command succeeded. Native commands may use
credentials and incur model costs; avoid pasting their output into public logs.
Then run the independently ignored **direct agent ACP** attachment probe:

```sh
HIVE_REAL_OPENCODE=1 HIVE_REAL_OPENCODE_WIRE=v1 HIVE_REAL_OPENCODE_CWD=/same/absolute/workspace \
  HIVE_REAL_OPENCODE_NATIVE_ID='<id-from-session-list>' \
  cargo test -p hive-daemon --test real_agents opencode_reports_whether_a_native_session_can_be_attached_over_acp -- --ignored --exact --nocapture
```

Replace `OPENCODE`/`opencode` with `PI`/`pi` and provide Pi's verified ACP
entrypoint/args to test Pi. The v1 direct probe calls `session/load`; v2 calls
`session/resume`. A failed load does not establish that native sessions cannot
be reopened via their own CLI or server. Hive itself cannot adopt a native ID:
record native-CLI reopen, direct ACP attachment and Hive's live-worker resume
as **separate** results in the [capability matrix](acp-compatibility.md#installed-agent-capability-matrix-2026-10-11).
For optional tool calls/results, permission requests, user-input requests,
cancel, and disconnect while generation is active, inspect a deliberately
configured manual agent session and record unexercised capabilities as
unverified rather than inferring support from `initialize`.
