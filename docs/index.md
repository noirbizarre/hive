# Hive

Hive is a local ACPv2 gateway for ACP coding agents. Start with the
[architecture](architecture.md) and the [roadmap](roadmap.md); consult the
[compatibility](acp-compatibility.md), [transport](transports.md) and
[operational API](hive-api.md) guides for the implemented boundary.

The full-screen TUI (`cargo run -p hive-tui`) is the primary interactive
client. Start the daemon first (`cargo run -p hive-daemon --bin hive -- serve`).
It lists current-directory sessions, creates sessions using configured agent
profiles, and attaches as pilot or observer. Press `Tab` for sessions, `n` for
a new profile, `Ctrl-T` to become pilot, `Ctrl-H` to relinquish pilot control
while observing, and `Esc` to leave the session running. See the repository
README for other shortcuts.
