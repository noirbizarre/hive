//! Version detection and API capability probing.
//!
//! Hive never guesses what a server can do: it asks, and refuses (with a diagnostic) rather
//! than degrade into creating a second conversation.

use reqwest::Method;
use serde_json::Value;

use super::{error::OpencodeError, http::Client};

/// Oldest OpenCode this adapter is tested against (see the capability matrix in
/// docs/compatibility.md). Older servers lack routes the adapter relies on.
pub(crate) const MINIMUM: &str = "1.19.0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parses `1.19.3`, `v1.19.3` and `1.19.3-beta.1`; anything else is `None`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let core = text
            .trim()
            .trim_start_matches('v')
            .split(['-', '+'])
            .next()?;
        let mut parts = core.split('.');
        let mut next = || parts.next()?.parse().ok();
        let version = Self {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        // A fourth component is not semver; refuse rather than guess.
        parts.next().is_none().then_some(version)
    }
}

/// Which API families the server answered for.
///
/// `v1` is the documented REST/SSE surface the TUI uses. `v2` is the `/api/*` preview with
/// durable, sequence-numbered events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub version: String,
    pub v1: bool,
    pub v2: bool,
}

/// Asks the server what it is. Auth and reachability problems are returned as such; an
/// unknown route only means "not advertised".
pub(crate) async fn probe(client: &Client) -> Result<Capabilities, OpencodeError> {
    // Validate the body, not just the status: older servers proxy unknown paths to a web UI
    // and answer 200 with HTML, which would look like a supported API.
    let v1 = healthy(client, "/global/health").await?;
    let v2 = healthy(client, "/api/health").await?.is_some();
    let version = v1
        .as_ref()
        .and_then(|body| body.get("version"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let capabilities = Capabilities {
        // A v2-only server reports no version; say so rather than invent one.
        version: version.clone().unwrap_or_else(|| "unknown".into()),
        v1: v1.is_some(),
        v2,
    };
    if !capabilities.v1 && !capabilities.v2 {
        return Err(OpencodeError::ApiUnavailable {
            what: "a supported session API (/global/health or /api/health)",
        });
    }
    if let Some(version) = version {
        let minimum = Version::parse(MINIMUM).expect("MINIMUM is valid");
        match Version::parse(&version) {
            Some(found) if found >= minimum => {}
            _ => {
                return Err(OpencodeError::UnsupportedVersion {
                    found: version,
                    minimum: MINIMUM,
                });
            }
        }
    }
    Ok(capabilities)
}

async fn healthy(client: &Client, path: &str) -> Result<Option<Value>, OpencodeError> {
    match client.json::<Value>(Method::GET, path, &[], None).await {
        Ok(body) if body.get("healthy").and_then(Value::as_bool) == Some(true) => Ok(Some(body)),
        Ok(_) => Ok(None),
        Err(error @ (OpencodeError::Unauthorized | OpencodeError::Unreachable { .. })) => {
            Err(error)
        }
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_prerelease_and_prefixed_versions_parse() {
        let expected = Some(Version {
            major: 1,
            minor: 19,
            patch: 3,
        });
        for text in [
            "1.19.3",
            "v1.19.3",
            "1.19.3-beta.1",
            "1.19.3+build5",
            " 1.19.3 ",
        ] {
            assert_eq!(Version::parse(text), expected, "{text}");
        }
    }

    #[test]
    fn malformed_versions_are_refused_rather_than_guessed() {
        for text in ["", "1", "1.19", "one.two.three", "1.19.3.4", "1.19.x"] {
            assert_eq!(Version::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn versions_order_numerically_not_lexically() {
        assert!(Version::parse("1.19.3") > Version::parse("1.9.30"));
        assert!(Version::parse("2.0.0") > Version::parse("1.99.99"));
    }
}
