//! Registrations are inert; only verified owner identities can become routes.

use super::*;
use hive_core::identity::NodeId;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerRegistration {
    pub owner: NodeId,
    pub endpoint: String,
    pub name: Option<String>,
    pub auth_token_env: Option<String>,
}

pub(super) fn problems(peers: &BTreeMap<String, PeerRegistration>) -> Vec<ConfigError> {
    let mut owners = std::collections::BTreeSet::new();
    let mut problems = Vec::new();
    for (alias, peer) in peers {
        if alias.is_empty()
            || !alias
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            problems.push(ConfigError::Peer(
                "peer aliases must contain letters, digits, '-' or '_'".into(),
            ));
        }
        if !owners.insert(peer.owner.clone()) {
            problems.push(ConfigError::Peer(
                "a peer owner NodeId is registered more than once".into(),
            ));
        }
        // Reuse transport policy without reading secrets or contacting the peer.
        if hive_protocol::transport::Credentials::default()
            .peer(&peer.endpoint)
            .is_err()
        {
            problems.push(ConfigError::Peer(
                "peer endpoint must be HTTP(S) without userinfo or fragment".into(),
            ));
        }
        if peer.auth_token_env.as_ref().is_some_and(|name| {
            name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            problems.push(ConfigError::Peer(
                "peer auth_token_env must name an environment variable".into(),
            ));
        }
    }
    problems
}
