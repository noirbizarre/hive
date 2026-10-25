# Getting started

This guide takes you from a source checkout to a project conversation that you
can continue in another client. Hive runs the agent in its daemon, so closing a
client does not end the session.

## Prerequisites

- Git, plus Rust and Cargo installed through [rustup](https://rustup.rs/).
  The checkout selects the `stable` Rust channel through rustup.
  The workspace declares Rust 1.88 as its minimum version.
- An installed agent with an ACP entrypoint. This guide uses OpenCode 1.x's
  `opencode acp` command with ACPv1, matching the repository's recorded evidence.
- A working model and provider credentials configured in that agent.

Install OpenCode using its [installation guide](https://opencode.ai/docs/).
OpenCode 1 and 2 share the `opencode` executable name, so check which major
version your installation method provides. The recorded Hive matrix does not
establish OpenCode 2 compatibility.

Check your installation before configuring Hive:

```sh
opencode --version
opencode acp --help
opencode auth login
```

Select and configure a model in OpenCode itself. Hive does not install agents,
log in to providers, or choose provider credentials for you. Compatibility is
version-dependent: see the [recorded evidence](acp-compatibility.md#installed-agent-capability-matrix-2026-10-11).

If you want to try Hive without an account or model, use the
[mock-agent option](#try-without-a-model) instead.

## 1. Install Hive

Clone the repository and install from its root:

```sh
git clone https://github.com/noirbizarre/hive.git
cd hive
cargo install --path crates/hive-cli --bin hive
hive --version
```

If you already have a checkout, run the Cargo command from that checkout instead.

Ensure Cargo's executable directory, normally `~/.cargo/bin`, is on `PATH`.
This installs `hive`, including its terminal client and embedded browser assets.
There is no separate browser build or Node.js requirement to run Hive.

Source installation is the verified installation path in this repository.
For development without installation, use `cargo run -p hive-cli -- <arguments>`.

## 2. Configure an agent

Create the configuration directory:

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/hive"
```

Save the following as `${XDG_CONFIG_HOME:-$HOME/.config}/hive/config.toml`:

```toml
default_agent = "opencode"

[agents.opencode]
name = "OpenCode"
command = "opencode"
args = ["acp"]
versions = ["v1"]
```

`opencode` is the profile ID. The command and arguments launch a separate ACP
process for each new Hive session. This does not adopt a conversation already
open in OpenCode's own interface.

The version declaration must match your installed agent. Hive does not retry
with another protocol after a failed initialization. See
[configuration](configuration.md) for network agents, environment variables,
pricing, and alternative file locations.

## 3. Check the setup

```sh
hive doctor opencode
```

Expect configuration, profile, and daemon checks without errors. Doctor starts
an initialize-only probe and sends no prompt. A passing probe confirms the ACP
handshake, not that model credentials, prompting, or restart recovery work.

If a check fails, follow the printed diagnostic code and help text, or consult
[troubleshooting](troubleshooting.md).

## 4. Start the daemon

In the first terminal:

```sh
hive daemon
```

Leave this foreground process running. It listens on `127.0.0.1:8765` by default.
Only one daemon can hold the current user's runtime lock, even on different ports.

In a second terminal:

```sh
hive daemon status
```

Expect a JSON status response identifying Hive and the configured default agent.
A responding daemon does not mean every configured agent is available.

## 5. Start a project conversation

Open the second terminal in the project directory you want the agent to use, then run:

```sh
hive tui --agent opencode
```

1. Press `Ctrl-N` to open the new-session profile picker.
2. Select OpenCode with the arrow keys and press Enter.
3. Type a prompt such as “Describe this project without changing files.”
4. Press Enter and wait for the agent to finish.

The session uses this terminal's current directory. Its stable ID starts with
`hive-`. `Tab` opens the session overview, which lists all directories by default.
See the [TUI guide](guides/tui.md) for controls and agent questions.

## 6. Continue in the browser

```sh
hive web
```

This opens <http://127.0.0.1:8765/ui>. If opening a browser fails, visit
that URL manually.

1. Select the same session in the session rail. It opens read-only.
2. Type in the prompt box to request control.
3. After control is confirmed and the session is idle, send your next prompt.

The TUI becomes an observer when the browser takes control. The downstream
conversation stays the same. Press `Ctrl-T` in the TUI to take control back.

> **Create project sessions in the TUI.** The browser's new-session action
> currently uses `/` as the working directory. Open a TUI-created session to
> retain your intended project directory.

## 7. Leave or stop

Press Esc in the TUI or close the browser to detach. The daemon and session
remain alive. Detaching does not cancel active work.

To stop Hive, interrupt the foreground daemon with `Ctrl-C`. On the next start,
Hive attempts to reattach saved downstream sessions where supported and restore
their conversation from agent-retained history. Incremental replay cursors do not
survive daemon restart, and interrupted work may need explicit
acknowledgement. Read [session recovery](operations.md#restart-and-recovery)
before relying on restart continuity.

## Try without a model

From the checkout, build the deterministic development agent:

```sh
cargo build -p hive-mock-agent
```

Replace the configuration above with the following, substituting the absolute
path to your checkout:

```toml
default_agent = "mock"

[agents.mock]
name = "Mock agent"
command = "/absolute/path/to/hive/target/debug/hive-mock-agent"
versions = ["v2"]
```

Run `hive doctor mock`, start `hive daemon`, then use `hive tui --agent mock`
and follow the same session steps. Restart an already running daemon after
changing configuration.

The mock requires no provider credentials and uses no model tokens. It validates
Hive's session plumbing, not a real agent's capabilities. Installing `hive` does
not install this separate development executable.

## Next steps

- [Configure additional agents](configuration.md).
- [Understand pilot control and handoff](guides/sessions.md).
- [Continue native OpenCode sessions](agents/opencode.md).
