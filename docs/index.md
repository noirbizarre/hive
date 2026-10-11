# Hive

<p align="center">
  <img src="images/logo.svg" alt="Hive wordmark" width="220" style="background:#101d35; border-radius:16px">
</p>

Hive is a local ACPv2 gateway for ACP coding agents. Start with the
[architecture](architecture.md) and the [roadmap](roadmap.md); consult the
[compatibility](acp-compatibility.md), [transport](transports.md) and
[operational API](hive-api.md) guides for the implemented boundary.

The full-screen TUI (`hive` or `hive tui`) is the primary interactive
client. Start the daemon first (`hive daemon`); use `hive daemon status` to
check it or `hive web` to open the browser client. Install the CLI from a
checkout with `cargo install --path crates/hive-cli --bin hive`.
It lists current-directory sessions, creates sessions using configured agent
profiles, and attaches as pilot or observer. Press `Tab` for sessions, `n` for
a new profile, `Ctrl-T` to become pilot, `Ctrl-H` to relinquish pilot control
while observing, and `Esc` to leave the session running. See the repository
README for other shortcuts.
