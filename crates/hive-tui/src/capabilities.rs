//! Local terminal evidence and policy, independent of protocol encoders and rendering.

use std::{ffi::OsString, io::IsTerminal};

use clap::{Args, ValueEnum};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum CapabilityOverride {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Args, Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalOptions {
    /// Override Kitty graphics support (does not enable the Kitty keyboard protocol).
    #[arg(
        long = "terminal-kitty-graphics",
        env = "HIVE_TERMINAL_KITTY_GRAPHICS",
        value_enum,
        default_value = "auto"
    )]
    pub kitty_graphics: CapabilityOverride,
    /// Override terminal text clipboard write support.
    #[arg(
        long = "terminal-osc52-write",
        env = "HIVE_TERMINAL_OSC52_WRITE",
        value_enum,
        default_value = "auto"
    )]
    pub osc52_write: CapabilityOverride,
    /// Explicitly opt into terminal text clipboard read attempts.
    #[arg(
        long = "terminal-osc52-read",
        env = "HIVE_TERMINAL_OSC52_READ",
        value_enum,
        default_value = "auto"
    )]
    pub osc52_read: CapabilityOverride,
    /// Override terminal rich clipboard write support.
    #[arg(
        long = "terminal-osc5522-write",
        env = "HIVE_TERMINAL_OSC5522_WRITE",
        value_enum,
        default_value = "auto"
    )]
    pub osc5522_write: CapabilityOverride,
    /// Explicitly opt into terminal rich clipboard read attempts.
    #[arg(
        long = "terminal-osc5522-read",
        env = "HIVE_TERMINAL_OSC5522_READ",
        value_enum,
        default_value = "auto"
    )]
    pub osc5522_read: CapabilityOverride,
    /// Override terminal program status protocol support.
    #[arg(
        long = "terminal-osc7501",
        env = "HIVE_TERMINAL_OSC7501",
        value_enum,
        default_value = "auto"
    )]
    pub osc7501: CapabilityOverride,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Support {
    #[default]
    Unknown,
    Unsupported,
    Supported,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CapabilitySource {
    #[default]
    Automatic,
    Override,
    UnavailableTransport,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capability {
    pub detected: Support,
    /// Effective permission to attempt an operation, not a guarantee of terminal acceptance.
    pub effective: Support,
    pub source: CapabilitySource,
}

impl Capability {
    pub fn is_usable(self) -> bool {
        // Unknown support must never accidentally authorize a clipboard read or optional escape.
        self.effective == Support::Supported
    }

    fn resolve(detected: Support, policy: CapabilityOverride, available: bool) -> Self {
        if !available {
            // An explicit override cannot turn a pipe or a dumb terminal into a protocol transport.
            return Self {
                detected: Support::Unsupported,
                effective: Support::Unsupported,
                source: CapabilitySource::UnavailableTransport,
            };
        }
        let (effective, source) = match policy {
            CapabilityOverride::Auto => (detected, CapabilitySource::Automatic),
            CapabilityOverride::On => (Support::Supported, CapabilitySource::Override),
            CapabilityOverride::Off => (Support::Unsupported, CapabilitySource::Override),
        };
        Self {
            detected,
            effective,
            source,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub kitty_graphics: Capability,
    pub osc52_write: Capability,
    pub osc52_read: Capability,
    pub osc5522_write: Capability,
    pub osc5522_read: Capability,
    pub osc7501: Capability,
}

#[derive(Default)]
struct TerminalEnvironment {
    term: Option<OsString>,
    multiplexer: bool,
    stdin_tty: bool,
    stdout_tty: bool,
}

impl TerminalEnvironment {
    fn capture() -> Self {
        Self::from_lookup(
            |name| std::env::var_os(name),
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        )
    }

    fn from_lookup(
        lookup: impl Fn(&str) -> Option<OsString>,
        stdin_tty: bool,
        stdout_tty: bool,
    ) -> Self {
        Self {
            term: lookup("TERM"),
            // Even empty markers indicate an intermediary; outer-terminal hints can be stale.
            multiplexer: ["TMUX", "STY", "ZELLIJ"]
                .iter()
                .any(|name| lookup(name).is_some()),
            stdin_tty,
            stdout_tty,
        }
    }
}

impl TerminalCapabilities {
    /// Capture local evidence once at startup; consumers share the resulting snapshot.
    pub fn detect(options: &TerminalOptions) -> Self {
        Self::resolve(&TerminalEnvironment::capture(), options)
    }

    fn resolve(environment: &TerminalEnvironment, options: &TerminalOptions) -> Self {
        let term = environment.term.as_deref().and_then(|term| term.to_str());
        let available = environment.stdin_tty && environment.stdout_tty && term != Some("dumb");
        let multiplexer = environment.multiplexer
            || term.is_some_and(|term| term.starts_with("screen") || term.starts_with("tmux"));
        let kitty = if !multiplexer && term == Some("xterm-kitty") {
            Support::Supported
        } else {
            Support::Unknown
        };
        let resolve = |support, policy| Capability::resolve(support, policy, available);
        Self {
            kitty_graphics: resolve(kitty, options.kitty_graphics),
            osc52_write: resolve(kitty, options.osc52_write),
            // Identity and write support establish neither read consent nor read permission.
            osc52_read: resolve(Support::Unknown, options.osc52_read),
            // These newer protocols need negotiation or an explicit assertion, not a brand hint.
            osc5522_write: resolve(Support::Unknown, options.osc5522_write),
            osc5522_read: resolve(Support::Unknown, options.osc5522_read),
            osc7501: resolve(Support::Unknown, options.osc7501),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(term: Option<&str>) -> TerminalEnvironment {
        TerminalEnvironment {
            term: term.map(OsString::from),
            stdin_tty: true,
            stdout_tty: true,
            ..TerminalEnvironment::default()
        }
    }

    fn operations(capabilities: TerminalCapabilities) -> [Capability; 6] {
        [
            capabilities.kitty_graphics,
            capabilities.osc52_write,
            capabilities.osc52_read,
            capabilities.osc5522_write,
            capabilities.osc5522_read,
            capabilities.osc7501,
        ]
    }

    #[test]
    fn unknown_terminal_identity_never_authorizes_optional_protocols() {
        for term in [None, Some(""), Some("xterm-256color"), Some("unrecognized")] {
            let capabilities =
                TerminalCapabilities::resolve(&environment(term), &TerminalOptions::default());
            assert!(
                operations(capabilities)
                    .iter()
                    .all(|cap| cap.effective == Support::Unknown && !cap.is_usable())
            );
        }
        assert!(
            operations(TerminalCapabilities::default())
                .iter()
                .all(|cap| !cap.is_usable())
        );
    }

    #[test]
    fn direct_kitty_only_establishes_graphics_and_text_clipboard_writes() {
        let caps = TerminalCapabilities::resolve(
            &environment(Some("xterm-kitty")),
            &TerminalOptions::default(),
        );
        assert!(caps.kitty_graphics.is_usable());
        assert!(caps.osc52_write.is_usable());
        assert!(
            operations(caps)[2..]
                .iter()
                .all(|cap| cap.effective == Support::Unknown && !cap.is_usable())
        );
    }

    #[test]
    fn even_empty_multiplexer_markers_suppress_captured_kitty_hints() {
        for marker in ["TMUX", "STY", "ZELLIJ"] {
            for value in ["", "active"] {
                let env = TerminalEnvironment::from_lookup(
                    |name| match name {
                        "TERM" => Some("xterm-kitty".into()),
                        name if name == marker => Some(value.into()),
                        _ => None,
                    },
                    true,
                    true,
                );
                let caps = TerminalCapabilities::resolve(&env, &TerminalOptions::default());
                assert!(operations(caps).iter().all(|cap| !cap.is_usable()));
            }
        }
    }

    #[test]
    fn multiplexers_suppress_outer_terminal_hints_but_allow_explicit_assertions() {
        for term in [
            "xterm-kitty",
            "screen",
            "screen-256color",
            "tmux",
            "tmux-256color",
        ] {
            let mut env = environment(Some(term));
            env.multiplexer = term == "xterm-kitty";
            let caps = TerminalCapabilities::resolve(&env, &TerminalOptions::default());
            assert!(operations(caps).iter().all(|cap| !cap.is_usable()));
            let caps = TerminalCapabilities::resolve(
                &env,
                &TerminalOptions {
                    kitty_graphics: CapabilityOverride::On,
                    ..TerminalOptions::default()
                },
            );
            assert!(caps.kitty_graphics.is_usable());
            assert_eq!(caps.kitty_graphics.detected, Support::Unknown);
        }
    }

    #[test]
    fn overrides_preserve_evidence_and_unknown_support_falls_back() {
        for detected in [Support::Unknown, Support::Unsupported, Support::Supported] {
            for policy in [
                CapabilityOverride::Auto,
                CapabilityOverride::On,
                CapabilityOverride::Off,
            ] {
                let cap = Capability::resolve(detected, policy, true);
                assert_eq!(cap.detected, detected);
                assert_eq!(
                    cap.is_usable(),
                    match policy {
                        CapabilityOverride::Auto => detected == Support::Supported,
                        CapabilityOverride::On => true,
                        CapabilityOverride::Off => false,
                    }
                );
            }
        }
    }

    #[test]
    fn read_opt_in_is_independent_of_writes_and_other_protocols() {
        let caps = TerminalCapabilities::resolve(
            &environment(Some("xterm-kitty")),
            &TerminalOptions {
                kitty_graphics: CapabilityOverride::Off,
                osc52_write: CapabilityOverride::Off,
                osc52_read: CapabilityOverride::On,
                osc5522_read: CapabilityOverride::On,
                ..TerminalOptions::default()
            },
        );
        assert!(!caps.kitty_graphics.is_usable());
        assert_eq!(caps.kitty_graphics.detected, Support::Supported);
        assert!(!caps.osc52_write.is_usable());
        assert!(caps.osc52_read.is_usable());
        assert!(caps.osc5522_read.is_usable());
        assert!(!caps.osc5522_write.is_usable());
        assert!(!caps.osc7501.is_usable());
    }

    #[test]
    fn every_operation_obeys_its_own_override() {
        for index in 0..6 {
            for policy in [CapabilityOverride::On, CapabilityOverride::Off] {
                let mut options = TerminalOptions::default();
                let fields = [
                    &mut options.kitty_graphics,
                    &mut options.osc52_write,
                    &mut options.osc52_read,
                    &mut options.osc5522_write,
                    &mut options.osc5522_read,
                    &mut options.osc7501,
                ];
                *fields.into_iter().nth(index).unwrap() = policy;
                let caps = TerminalCapabilities::resolve(&environment(None), &options);
                for (other, cap) in operations(caps).iter().enumerate() {
                    assert_eq!(
                        cap.is_usable(),
                        other == index && policy == CapabilityOverride::On
                    );
                    assert_eq!(cap.source == CapabilitySource::Override, other == index);
                }
            }
        }
    }

    #[test]
    fn unavailable_terminal_transport_defeats_every_override() {
        let options = TerminalOptions {
            kitty_graphics: CapabilityOverride::On,
            osc52_write: CapabilityOverride::On,
            osc52_read: CapabilityOverride::On,
            osc5522_write: CapabilityOverride::On,
            osc5522_read: CapabilityOverride::On,
            osc7501: CapabilityOverride::On,
        };
        for (stdin_tty, stdout_tty, term) in [
            (false, false, "xterm-kitty"),
            (true, false, "xterm-kitty"),
            (false, true, "xterm-kitty"),
            (true, true, "dumb"),
        ] {
            let mut env = environment(Some(term));
            env.stdin_tty = stdin_tty;
            env.stdout_tty = stdout_tty;
            let caps = TerminalCapabilities::resolve(&env, &options);
            assert!(operations(caps).iter().all(
                |cap| !cap.is_usable() && cap.source == CapabilitySource::UnavailableTransport
            ));
        }
    }
}
