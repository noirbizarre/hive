//! Connection policy shared by native tools and future Hive peer consumers.

use std::time::Duration;

use agent_client_protocol::{Agent, Client, ConnectTo, Error};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

/// A peer driver exposes controlled failures rather than remote response bodies or URLs.
pub struct PeerTransport(crate::HttpClient);

impl std::fmt::Debug for PeerTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PeerTransport { .. }")
    }
}

impl ConnectTo<Client> for PeerTransport {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
        ConnectTo::<Client>::connect_to(self.0, client).await.map_err(|_| {
            // SDK errors can include response bodies and URL credentials from untrusted servers.
            Error::internal_error().data(serde_json::json!({
                "code": "hive::transport::connection",
                "message": "ACP peer connection failed or disconnected",
                "help": "check the endpoint, authentication and network; explicitly resume the known session on a new connection"
            }))
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(
        "authentication variable `{name}` is missing; set it to the daemon token or unset HIVE_AUTH_TOKEN_ENV"
    )]
    Credential { name: String },
    #[error(
        "invalid authentication token; use a token containing printable ASCII without whitespace"
    )]
    Token,
    #[error("invalid ACP endpoint; provide an HTTP(S) URL without userinfo or a fragment")]
    Endpoint,
    #[error("cannot construct HTTP transport; check the endpoint and TLS configuration")]
    Client,
}

impl TransportError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Credential { .. } => "hive::transport::credential",
            Self::Token => "hive::transport::token",
            Self::Endpoint => "hive::transport::endpoint",
            Self::Client => "hive::transport::client",
        }
    }
}

/// Secrets deliberately have no Debug implementation.
#[derive(Clone, Default)]
pub struct Credentials(Option<HeaderValue>);

impl Credentials {
    pub fn from_env(name: Option<&str>) -> Result<Self, TransportError> {
        name.map(|name| {
            let token = std::env::var(name)
                .map_err(|_| TransportError::Credential { name: name.into() })?;
            Self::bearer(&token)
        })
        .unwrap_or(Ok(Self::default()))
    }

    /// A common environment selector also covers commands without explicit credential flags.
    pub fn native() -> Result<Self, TransportError> {
        let name = std::env::var_os("HIVE_AUTH_TOKEN_ENV")
            .map(|name| {
                name.into_string().map_err(|_| TransportError::Credential {
                    name: "HIVE_AUTH_TOKEN_ENV".into(),
                })
            })
            .transpose()?;
        Self::from_env(name.as_deref())
    }

    pub fn bearer(token: &str) -> Result<Self, TransportError> {
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(TransportError::Token);
        }
        let mut value =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| TransportError::Token)?;
        value.set_sensitive(true);
        Ok(Self(Some(value)))
    }

    pub fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(value) = &self.0 {
            headers.insert(AUTHORIZATION, value.clone());
        }
        headers
    }

    pub fn http(&self, timeout: Duration) -> Result<reqwest::Client, TransportError> {
        reqwest::Client::builder()
            .default_headers(self.headers())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()
            .map_err(|_| TransportError::Client)
    }

    /// Base URLs retain the SDK's existing `/acp` expansion and WebSocket support.
    pub fn acp(&self, base: &str) -> Result<crate::HttpClient, TransportError> {
        crate::HttpClient::builder(base)
            .configure_http(|http| {
                http.default_headers(self.headers())
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
            })
            .build()
            .map_err(|_| TransportError::Client)
    }

    /// This constructs only a connection, never a session or a retry policy.
    pub fn peer(&self, endpoint: &str) -> Result<PeerTransport, TransportError> {
        let url = reqwest::Url::parse(endpoint).map_err(|_| TransportError::Endpoint)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(TransportError::Endpoint);
        }
        crate::HttpClient::builder_with_endpoint(endpoint)
            // Whole-request timeouts would terminate healthy SSE bodies.
            .configure_http(|http| {
                http.default_headers(self.headers())
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
            })
            .build()
            .map(PeerTransport)
            .map_err(|_| TransportError::Client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_peer_endpoints_and_tokens_have_controlled_actionable_errors() {
        let credentials = Credentials::bearer("test-private-token").unwrap();
        assert!(credentials.headers()[AUTHORIZATION].is_sensitive());
        for endpoint in [
            "not a URL",
            "ws://host/acp",
            "http://user:secret@host/acp",
            "https://host/acp#secret",
            "file:///tmp/acp",
        ] {
            let error = credentials.peer(endpoint).unwrap_err();
            assert_eq!(error.code(), "hive::transport::endpoint");
            assert!(!error.to_string().contains("secret"));
        }
        for token in ["", "with space", "with\nnewline", "non-ascii-λ"] {
            assert!(matches!(
                Credentials::bearer(token),
                Err(TransportError::Token)
            ));
        }
        let missing = "HIVE_TEST_UNSET_TRANSPORT_CREDENTIAL_78";
        assert!(matches!(
            Credentials::from_env(Some(missing)),
            Err(TransportError::Credential { .. })
        ));
    }
}
