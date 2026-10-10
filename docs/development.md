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
address. `hive serve` needs no agent argument: it loads profiles from
`$XDG_CONFIG_HOME/hive/config.toml`, falling back to `~/.config/hive/config.toml`,
or accepts `--config` / `HIVE_CONFIG`. `HIVE_DEFAULT_AGENT` overrides the optional
`default_agent` setting. Profiles declare `command`, `args`, `env`, or an
`endpoint` with optional `headers`, and `versions = ["v2", "v1"]` where supported.
Each session uses `_meta.hive.agent` or the configured default; only declared
v1 profiles use the compatibility adapter. Never log secrets from profiles.
Agent stderr is independent of ACP stdio stdout. The per-user lock lives under
`$XDG_RUNTIME_DIR/hive/daemon.lock` (or `~/.local/state/hive/daemon.lock`),
and is released on process exit. In-memory state is lost on daemon restart.
