//! Startup-only listener policy; registry reload cannot silently change trust boundaries.

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Default)]
pub struct ServeOptions {
    pub allow_remote: bool,
    pub auth_token_env: Option<String>,
    pub allowed_origins: Vec<String>,
}

#[derive(Clone, Debug, thiserror::Error, miette::Diagnostic)]
#[error("{0}")]
#[diagnostic(code(hive::http::configuration))]
pub struct PolicyError(String);

#[derive(Clone)]
pub struct Policy {
    token_hash: Option<[u8; 32]>,
    // Existing route helpers accept one string; commas encode exact validated origins, never patterns.
    origins: String,
    sessions: Arc<Mutex<HashMap<String, Instant>>>,
}

impl ServeOptions {
    pub fn validate(&self, address: SocketAddr) -> Result<(), PolicyError> {
        if !address.ip().is_loopback() && !self.allow_remote {
            return Err(PolicyError("remote binding requires explicit --allow-remote; use a loopback address or enable access for your network topology".into()));
        }
        for origin in &self.allowed_origins {
            let url = reqwest::Url::parse(origin).map_err(|_| {
                PolicyError("invalid allowed origin; use an HTTP(S) origin without a path".into())
            })?;
            if !matches!(url.scheme(), "http" | "https")
                || url.origin().ascii_serialization() != *origin
            {
                return Err(PolicyError(
                    "invalid allowed origin; use an HTTP(S) origin without a path".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn policy(&self, address: SocketAddr) -> Result<Policy, PolicyError> {
        self.validate(address)?;
        let token_hash = self.auth_token_env.as_deref().map(|name| {
            let token = std::env::var(name).map_err(|_| PolicyError("authentication variable is missing; set the configured token environment variable".into()))?;
            hive_protocol::transport::Credentials::bearer(&token).map_err(|error| PolicyError(error.to_string()))?;
            Ok(hash(&token))
        }).transpose()?;
        let mut origins = vec![format!("http://{address}")];
        origins.extend(self.allowed_origins.clone());
        Ok(Policy {
            token_hash,
            origins: origins.join(","),
            sessions: Arc::default(),
        })
    }
}

fn hash(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

impl Policy {
    pub(crate) fn unauthenticated(origin: Option<&str>) -> Self {
        Self {
            token_hash: None,
            origins: origin.unwrap_or_default().into(),
            sessions: Arc::default(),
        }
    }

    pub fn origins(&self) -> &str {
        &self.origins
    }

    fn token_matches(&self, token: &str) -> bool {
        let Some(expected) = self.token_hash else {
            return false;
        };
        // Compare fixed-size digests without leaking a matching token prefix.
        expected
            .iter()
            .zip(hash(token))
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
    }

    fn authenticated(&self, headers: &HeaderMap) -> bool {
        if self.token_hash.is_none() {
            return true;
        }
        let mut auth = headers.get_all(header::AUTHORIZATION).iter();
        if let Some(value) = auth.next() {
            return auth.next().is_none()
                && value
                    .to_str()
                    .ok()
                    .and_then(|value| value.split_once(' '))
                    // HTTP scheme names are case-insensitive; token bytes remain case-sensitive.
                    .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
                    .map(|(_, token)| token)
                    .is_some_and(|token| self.token_matches(token));
        }
        let mut sessions = self
            .sessions
            .lock()
            .expect("browser sessions lock poisoned");
        sessions.retain(|_, expires| *expires > Instant::now());
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|cookie| cookie.trim().strip_prefix("hive_session="))
            .any(|cookie| sessions.contains_key(cookie))
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], Json(serde_json::json!({"code":"hive::http::unauthorized", "message":"authentication required", "help":"configure HIVE_AUTH_TOKEN_ENV or sign in through /ui"}))).into_response()
}

async fn enforce(State(policy): State<Policy>, request: Request, next: Next) -> Response {
    if !crate::reload::allowed_origin(request.headers(), Some(&policy.origins)) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"code":"hive::http::origin", "message":"browser origin is not allowed", "help":"configure --allowed-origin for this browser origin"}))).into_response();
    }
    if !policy.authenticated(request.headers()) {
        return unauthorized();
    }
    next.run(request).await
}

async fn status(State(policy): State<Policy>, headers: HeaderMap) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(
            serde_json::json!({"required": policy.token_hash.is_some(), "authenticated":policy.authenticated(&headers)}),
        ),
    )
}

async fn login(
    State(policy): State<Policy>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // JSON extraction and Origin checks prevent form-based cross-site login.
    if !crate::reload::allowed_origin(&headers, Some(&policy.origins)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !body
        .get("token")
        .and_then(|v| v.as_str())
        .is_some_and(|token| policy.token_matches(token))
    {
        return unauthorized();
    }
    let cookie = uuid::Uuid::new_v4().to_string();
    let mut sessions = policy
        .sessions
        .lock()
        .expect("browser sessions lock poisoned");
    sessions.retain(|_, expires| *expires > Instant::now());
    if sessions.len() >= 256 {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"code":"hive::http::browser_sessions", "message":"browser login capacity reached", "help":"sign out an existing browser session or wait for its 12-hour expiry"}))).into_response();
    }
    sessions.insert(
        cookie.clone(),
        Instant::now() + Duration::from_secs(12 * 60 * 60),
    );
    // Origin has already been allowlisted; forwarding headers never decide cookie security.
    let secure = if headers
        .get(header::ORIGIN)
        .is_some_and(|origin| origin.as_bytes().starts_with(b"https://"))
    {
        "; Secure"
    } else {
        ""
    };
    ([(header::SET_COOKIE, format!("hive_session={cookie}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200{secure}")), (header::CACHE_CONTROL, "no-store".into())], Json(serde_json::json!({"authenticated":true}))).into_response()
}

async fn logout(State(policy): State<Policy>, headers: HeaderMap) -> Response {
    if !crate::reload::allowed_origin(&headers, Some(&policy.origins)) {
        return StatusCode::FORBIDDEN.into_response();
    }
    for value in headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
    {
        for cookie in value
            .split(';')
            .filter_map(|v| v.trim().strip_prefix("hive_session="))
        {
            policy
                .sessions
                .lock()
                .expect("browser sessions lock poisoned")
                .remove(cookie);
        }
    }
    (
        [(
            header::SET_COOKIE,
            "hive_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )],
        StatusCode::NO_CONTENT,
    )
        .into_response()
}

pub(crate) fn protect(router: Router, policy: Policy) -> Router {
    router.layer(middleware::from_fn_with_state(policy, enforce))
}

pub(crate) fn authentication(policy: Policy) -> Router {
    Router::new()
        .route("/v1/auth", get(status))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/logout", post(logout))
        .with_state(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::{
        Client,
        schema::{ProtocolVersion, v2},
    };

    async fn server() -> (SocketAddr, Policy, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut policy = ServeOptions {
            allowed_origins: vec![
                "http://hive.private:8765".into(),
                "https://hive.private".into(),
            ],
            ..Default::default()
        }
        .policy(address)
        .unwrap();
        policy.token_hash = Some(hash("test-secret"));
        let router = crate::router_with_policy(
            crate::Broker::new(crate::config::Config::default()),
            policy.clone(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, policy, server)
    }

    #[test]
    fn remote_opt_in_and_authentication_are_independent() {
        let remote = "0.0.0.0:8765".parse().unwrap();
        assert!(ServeOptions::default().policy(remote).is_err());
        let options = ServeOptions {
            allow_remote: true,
            ..Default::default()
        };
        assert!(options.policy(remote).unwrap().token_hash.is_none());
        let missing = format!("HIVE_TEST_ABSENT_{}", uuid::Uuid::new_v4().simple());
        assert!(
            ServeOptions {
                auth_token_env: Some(missing.clone()),
                ..options.clone()
            }
            .policy(remote)
            .is_err()
        );
        assert!(
            ServeOptions {
                auth_token_env: Some(missing),
                ..Default::default()
            }
            .policy("127.0.0.1:0".parse().unwrap())
            .is_err()
        );
        assert!(
            ServeOptions {
                allowed_origins: vec!["http://host/path".into()],
                ..options
            }
            .policy(remote)
            .is_err()
        );
    }

    #[tokio::test]
    async fn authentication_covers_all_operational_routes_and_acp_methods() {
        let (address, _, server) = server().await;
        let http = reqwest::Client::new();
        for (method, path) in [
            ("GET", "/health"),
            ("GET", "/v1/status"),
            ("GET", "/v1/health"),
            ("GET", "/v1/ready"),
            ("GET", "/v1/providers"),
            ("GET", "/v1/sessions"),
            ("GET", "/v1/sessions/events"),
            ("GET", "/v1/projects"),
            ("GET", "/v1/workspaces"),
            ("GET", "/v1/repositories"),
            ("POST", "/v1/config/reload"),
            ("POST", "/acp"),
            ("GET", "/acp"),
            ("DELETE", "/acp"),
        ] {
            let url = format!("http://{address}{path}");
            for token in [None, Some("Bearer wrong"), Some("Basic test-secret")] {
                let mut request = http.request(method.parse().unwrap(), &url);
                if let Some(token) = token {
                    request = request.header(header::AUTHORIZATION, token);
                }
                let response = request.send().await.unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::UNAUTHORIZED,
                    "{method} {path}"
                );
                assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
                assert!(!response.text().await.unwrap().contains("test-secret"));
            }
        }
        assert_eq!(
            http.get(format!("http://{address}/ui"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let url = format!("http://{address}/v1/status");
        for scheme in ["bearer", "BEARER", "bEaReR"] {
            assert_eq!(
                http.get(&url)
                    .header(header::AUTHORIZATION, format!("{scheme} test-secret"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
            assert_eq!(
                http.get(&url)
                    .header(header::AUTHORIZATION, format!("{scheme} TEST-SECRET"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            http.get(&url)
                .bearer_auth("test-secret")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let mut headers = HeaderMap::new();
        headers.append(header::AUTHORIZATION, "Bearer test-secret".parse().unwrap());
        headers.append(header::AUTHORIZATION, "Bearer test-secret".parse().unwrap());
        assert_eq!(
            http.get(&url)
                .headers(headers)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            http.get(&url)
                .bearer_auth("test-secret")
                .header(header::ORIGIN, "http://evil.test")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            http.get(&url)
                .bearer_auth("test-secret")
                .header(header::ORIGIN, "http://hive.private:8765")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        // Correct credentials must reach the SDK even on malformed stream/DELETE requests.
        for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
            let status = http
                .request(method, format!("http://{address}/acp"))
                .bearer_auth("test-secret")
                .header(header::ACCEPT, "text/event-stream")
                .send()
                .await
                .unwrap()
                .status();
            assert_ne!(status, StatusCode::UNAUTHORIZED);
        }
        for scheme in ["http", "ws"] {
            let credentials = hive_protocol::transport::Credentials::bearer("test-secret").unwrap();
            Client
                .v2()
                .connect_with(
                    credentials.acp(&format!("{scheme}://{address}")).unwrap(),
                    async |cx| {
                        cx.send_request(v2::InitializeRequest::new(
                            ProtocolVersion::V2,
                            v2::Implementation::new("auth-test", "1"),
                        ))
                        .block_task()
                        .await?;
                        Ok(())
                    },
                )
                .await
                .unwrap();
        }
        server.abort();
    }

    #[tokio::test]
    async fn browser_login_cookies_authenticate_streams_and_expire_without_storing_the_token() {
        let (address, policy, server) = server().await;
        let base = format!("http://{address}");
        let http = reqwest::Client::new();
        let response = http
            .post(format!("{base}/v1/auth/login"))
            .header(header::ORIGIN, "http://evil.test")
            .json(&serde_json::json!({"token":"test-secret"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            http.post(format!("{base}/v1/auth/login"))
                .body("token=test-secret")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        let response = http
            .post(format!("{base}/v1/auth/login"))
            .header(header::ORIGIN, &base)
            .json(&serde_json::json!({"token":"test-secret"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.contains("HttpOnly; SameSite=Strict; Path=/"));
        assert!(!cookie.contains("test-secret"));
        let cookie = cookie.split(';').next().unwrap();
        assert_eq!(
            http.get(format!("{base}/v1/status"))
                .header(header::COOKIE, cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, cookie.parse().unwrap());
        headers.insert(header::ORIGIN, base.parse().unwrap());
        for scheme in ["http", "ws"] {
            let headers = headers.clone();
            let transport = hive_protocol::HttpClient::builder(format!("{scheme}://{address}"))
                .configure_http(|http| http.default_headers(headers))
                .build()
                .unwrap();
            Client
                .v2()
                .connect_with(transport, async |cx| {
                    cx.send_request(v2::InitializeRequest::new(
                        ProtocolVersion::V2,
                        v2::Implementation::new("cookie-test", "1"),
                    ))
                    .block_task()
                    .await?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        http.post(format!("{base}/v1/auth/logout"))
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, &base)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(
            http.get(format!("{base}/v1/status"))
                .header(header::COOKIE, cookie)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        policy
            .sessions
            .lock()
            .unwrap()
            .insert("expired".into(), Instant::now() - Duration::from_secs(1));
        assert_eq!(
            http.get(format!("{base}/v1/status"))
                .header(header::COOKIE, "hive_session=expired")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        server.abort();
    }
}
