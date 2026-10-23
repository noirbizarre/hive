//! Adapter for the OpenCode server (`opencode serve` / the TUI's embedded server).
//!
//! Unlike the ACP downstreams this speaks OpenCode's REST + SSE API, because that is the only
//! surface through which a conversation started in the OpenCode UI can be attached to without
//! creating a second one. Everything Hive-facing is still ACPv2, via the shared session worker
//! seam in `lib.rs`.

pub(crate) mod api;
pub(crate) mod error;
mod http;
mod map;
mod version;
mod worker;

use crate::config::Profile;
use api::NativeSession;
use error::OpencodeError;

pub(crate) use worker::{Attach, run_opencode_agent};

/// A native session found on the server.
pub(crate) struct Discovered {
    pub session: NativeSession,
    pub busy: bool,
}

pub(crate) struct Discovery {
    pub version: String,
    pub api: &'static str,
    pub sessions: Vec<Discovered>,
}

/// Lists the sessions an OpenCode server holds, without attaching to any of them.
pub(crate) async fn discover(
    profile: &Profile,
    directory: Option<&str>,
    limit: usize,
) -> Result<Discovery, OpencodeError> {
    let client = http::Client::new(profile)?;
    let capabilities = version::probe(&client).await?;
    let flavor = api::Flavor::for_new(&capabilities);
    let (sessions, busy) = api::list(&client, flavor, directory, limit).await?;
    Ok(Discovery {
        version: capabilities.version,
        api: flavor.as_str(),
        sessions: sessions
            .into_iter()
            .map(|session| Discovered {
                busy: busy.contains(&session.id),
                session,
            })
            .collect(),
    })
}

/// What an OpenCode server answered to the health probes. Never holds the endpoint.
pub(crate) struct Health {
    pub version: String,
    pub v1: bool,
    pub v2: bool,
}

/// Checks credentials, reachability and version without listing or touching any session.
pub(crate) async fn check(profile: &Profile) -> Result<Health, OpencodeError> {
    let client = http::Client::new(profile)?;
    let capabilities = version::probe(&client).await?;
    Ok(Health {
        version: capabilities.version,
        v1: capabilities.v1,
        v2: capabilities.v2,
    })
}
