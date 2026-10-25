//! One session handle over OpenCode's two API families.
//!
//! The flavor is chosen once per session at attach time and persisted. In OpenCode 1.19.3 the
//! v1 and `/api/*` message stores are separate, so silently switching flavor would show a
//! different (or empty) history: that would be a duplicate conversation in everything but
//! name.

use agent_client_protocol::schema::v2;
use reqwest::Method;
use serde_json::{Value, json};

use super::{
    error::OpencodeError,
    http::{Client, Sse},
    map::{self, Mapped, Mapper},
    version::Capabilities,
};

/// History fetched when attaching to an existing session. Bounded so a very long native
/// conversation cannot stall startup; omitted prefixes must be reported as partial history.
pub(crate) const BACKFILL_MESSAGES: usize = 50;

pub(crate) struct HistoryBatch {
    pub updates: Vec<Mapped>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    V1,
    V2,
}

impl Flavor {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "v1" => Some(Self::V1),
            "v2" => Some(Self::V2),
            _ => None,
        }
    }

    fn available(self, capabilities: &Capabilities) -> bool {
        match self {
            Self::V1 => capabilities.v1,
            Self::V2 => capabilities.v2,
        }
    }

    /// New sessions prefer the documented, TUI-compatible v1 routes.
    pub(crate) fn for_new(capabilities: &Capabilities) -> Self {
        if capabilities.v1 { Self::V1 } else { Self::V2 }
    }
}

/// A session as the OpenCode server describes it.
#[derive(Clone, Debug)]
pub(crate) struct NativeSession {
    pub id: String,
    pub title: Option<String>,
    pub directory: String,
    pub updated: Option<u64>,
}

impl NativeSession {
    fn from_value(flavor: Flavor, value: &Value) -> Option<Self> {
        let directory = match flavor {
            Flavor::V1 => &value["directory"],
            Flavor::V2 => &value["location"]["directory"],
        };
        Some(Self {
            id: value["id"].as_str()?.to_owned(),
            title: value["title"].as_str().map(str::to_owned),
            directory: directory.as_str().unwrap_or_default().to_owned(),
            updated: value["time"]["updated"].as_u64(),
        })
    }
}

/// Lists sessions (optionally scoped to a directory) with the ids that are currently busy.
pub(crate) async fn list(
    client: &Client,
    flavor: Flavor,
    directory: Option<&str>,
    limit: usize,
) -> Result<(Vec<NativeSession>, Vec<String>), OpencodeError> {
    let limit = limit.to_string();
    let mut query = vec![("limit", limit.as_str())];
    if let Some(directory) = directory {
        query.push(("directory", directory));
    }
    let (path, active) = match flavor {
        Flavor::V1 => ("/session", "/session/status"),
        Flavor::V2 => ("/api/session", "/api/session/active"),
    };
    let body: Value = client.json(Method::GET, path, &query, None).await?;
    let items = match flavor {
        Flavor::V1 => body.as_array(),
        Flavor::V2 => body["data"].as_array(),
    }
    .ok_or(OpencodeError::Protocol {
        what: "session list",
    })?;
    let sessions = items
        .iter()
        .filter_map(|item| NativeSession::from_value(flavor, item))
        .collect();
    let scope = directory.map(|directory| [("directory", directory)]);
    let status: Value = client
        .json(
            Method::GET,
            active,
            scope.as_ref().map_or(&[], |s| &s[..]),
            None,
        )
        .await?;
    let map = match flavor {
        Flavor::V1 => &status,
        Flavor::V2 => &status["data"],
    };
    let busy = map
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    Ok((sessions, busy))
}

/// Handle on one native session.
#[derive(Clone)]
pub(crate) struct Native {
    client: Client,
    pub flavor: Flavor,
    pub id: String,
    directory: String,
}

impl Native {
    pub(crate) async fn create(
        client: &Client,
        flavor: Flavor,
        directory: &str,
    ) -> Result<(Self, NativeSession), OpencodeError> {
        let value: Value = match flavor {
            Flavor::V1 => {
                client
                    .json(
                        Method::POST,
                        "/session",
                        &[("directory", directory)],
                        Some(&json!({})),
                    )
                    .await?
            }
            Flavor::V2 => {
                let created: Value = client
                    .json(
                        Method::POST,
                        "/api/session",
                        &[],
                        Some(&json!({"location": {"directory": directory}})),
                    )
                    .await?;
                created["data"].clone()
            }
        };
        let session = NativeSession::from_value(flavor, &value).ok_or(OpencodeError::Protocol {
            what: "created session",
        })?;
        Ok((Self::new(client, flavor, &session.id, directory), session))
    }

    /// Attaches to an existing session, or fails: never creates one.
    pub(crate) async fn open(
        client: &Client,
        flavor: Flavor,
        id: &str,
        directory: &str,
    ) -> Result<(Self, NativeSession), OpencodeError> {
        let value: Value = match flavor {
            Flavor::V1 => {
                client
                    .json(
                        Method::GET,
                        &format!("/session/{id}"),
                        &[("directory", directory)],
                        None,
                    )
                    .await
            }
            Flavor::V2 => client
                .json::<Value>(Method::GET, &format!("/api/session/{id}"), &[], None)
                .await
                .map(|body| body["data"].clone()),
        }
        .map_err(|error| error.not_found_as(id))?;
        let session = NativeSession::from_value(flavor, &value)
            .filter(|session| session.id == id)
            .ok_or(OpencodeError::SessionNotFound { id: id.into() })?;
        Ok((Self::new(client, flavor, id, directory), session))
    }

    fn new(client: &Client, flavor: Flavor, id: &str, directory: &str) -> Self {
        Self {
            client: client.clone(),
            flavor,
            id: id.to_owned(),
            directory: directory.to_owned(),
        }
    }

    fn scope(&self) -> [(&str, &str); 1] {
        [("directory", self.directory.as_str())]
    }

    /// Whether the session is doing foreground work right now.
    pub(crate) async fn busy(&self) -> Result<bool, OpencodeError> {
        Ok(match self.flavor {
            Flavor::V1 => {
                let status: Value = self
                    .client
                    .json(Method::GET, "/session/status", &self.scope(), None)
                    .await?;
                matches!(status[&self.id]["type"].as_str(), Some("busy" | "retry"))
            }
            Flavor::V2 => {
                let active: Value = self
                    .client
                    .json(Method::GET, "/api/session/active", &[], None)
                    .await?;
                active["data"].get(&self.id).is_some()
            }
        })
    }

    /// Recent history, mapped through `mapper` so later live events never repeat it.
    pub(crate) async fn history(&self, mapper: &mut Mapper) -> Result<HistoryBatch, OpencodeError> {
        let mut out = Vec::new();
        let mut truncated = false;
        match self.flavor {
            Flavor::V1 => {
                let messages: Value = self
                    .client
                    .json(
                        Method::GET,
                        &format!("/session/{}/message", self.id),
                        &self.scope(),
                        None,
                    )
                    .await
                    .map_err(|error| error.not_found_as(&self.id))?;
                let messages = messages.as_array().ok_or(OpencodeError::Protocol {
                    what: "message list",
                })?;
                let skip = messages.len().saturating_sub(BACKFILL_MESSAGES);
                truncated |= skip > 0;
                for message in &messages[skip..] {
                    out.extend(mapper.v1_message(message));
                }
            }
            Flavor::V2 => {
                // Durable events are paged oldest-first; keep a bounded tail and the final cursor.
                let mut events = Vec::new();
                let mut after = 0_u64;
                for _ in 0..25 {
                    let after_text = after.to_string();
                    let page: Value = self
                        .client
                        .json(
                            Method::GET,
                            &format!("/api/session/{}/history", self.id),
                            &[("after", after_text.as_str()), ("limit", "200")],
                            None,
                        )
                        .await
                        .map_err(|error| error.not_found_as(&self.id))?;
                    let data = page["data"].as_array().cloned().unwrap_or_default();
                    if let Some(seq) = data.last().and_then(|e| e["durable"]["seq"].as_u64()) {
                        after = seq;
                    }
                    events.extend(data);
                    // Exhausting the page budget is a known omission, even if the tail is short.
                    truncated = page["hasMore"] == true;
                    if page["hasMore"] != true {
                        break;
                    }
                }
                let skip = events.len().saturating_sub(BACKFILL_MESSAGES * 4);
                truncated |= skip > 0;
                for event in &events[skip..] {
                    out.extend(mapper.event(event));
                }
                out.push(Mapped::Cursor(after));
            }
        }
        Ok(HistoryBatch {
            updates: out,
            truncated,
        })
    }

    pub(crate) async fn prompt(&self, blocks: &[v2::ContentBlock]) -> Result<(), OpencodeError> {
        let texts = blocks
            .iter()
            .map(|block| match block {
                v2::ContentBlock::Text(text) => Ok(text.text.clone()),
                _ => Err(OpencodeError::UnsupportedPrompt),
            })
            .collect::<Result<Vec<_>, _>>()?;
        match self.flavor {
            Flavor::V1 => {
                let parts: Vec<Value> = texts
                    .iter()
                    .map(|text| json!({"type": "text", "text": text}))
                    .collect();
                self.client
                    .ok(
                        Method::POST,
                        &format!("/session/{}/prompt_async", self.id),
                        &self.scope(),
                        Some(&json!({"parts": parts})),
                    )
                    .await
            }
            Flavor::V2 => {
                self.client
                    .ok(
                        Method::POST,
                        &format!("/api/session/{}/prompt", self.id),
                        &[],
                        Some(&json!({"prompt": {"text": texts.join("\n\n")}, "delivery": "queue"})),
                    )
                    .await
            }
        }
    }

    pub(crate) async fn abort(&self) -> Result<(), OpencodeError> {
        match self.flavor {
            Flavor::V1 => {
                self.client
                    .ok(
                        Method::POST,
                        &format!("/session/{}/abort", self.id),
                        &self.scope(),
                        None,
                    )
                    .await
            }
            Flavor::V2 => {
                self.client
                    .ok(
                        Method::POST,
                        &format!("/api/session/{}/interrupt", self.id),
                        &[],
                        None,
                    )
                    .await
            }
        }
    }

    pub(crate) async fn events(&self, cursor: Option<u64>) -> Result<Sse, OpencodeError> {
        match self.flavor {
            Flavor::V1 => self.client.stream("/event", &self.scope()).await,
            Flavor::V2 => {
                let after = cursor.map(|cursor| cursor.to_string());
                let query: Vec<(&str, &str)> = after
                    .as_deref()
                    .map(|after| ("after", after))
                    .into_iter()
                    .collect();
                self.client
                    .stream(&format!("/api/session/{}/event", self.id), &query)
                    .await
            }
        }
    }

    /// Permissions and questions waiting for an answer, e.g. one raised before Hive attached.
    pub(crate) async fn pending(&self) -> Result<Vec<Mapped>, OpencodeError> {
        let mut out = Vec::new();
        match self.flavor {
            Flavor::V1 => {
                let permissions: Value = self
                    .client
                    .json(Method::GET, "/permission", &self.scope(), None)
                    .await?;
                for request in permissions.as_array().into_iter().flatten() {
                    if request["sessionID"] == self.id.as_str() {
                        out.push(Mapped::Permission(map::v1_permission_ask(request)));
                    }
                }
                let questions: Value = self
                    .client
                    .json(Method::GET, "/question", &self.scope(), None)
                    .await?;
                for request in questions.as_array().into_iter().flatten() {
                    if request["sessionID"] == self.id.as_str()
                        && let Some(ask) = map::v1_question(request)
                    {
                        out.push(Mapped::Question(ask));
                    }
                }
            }
            Flavor::V2 => {
                let permissions: Value = self
                    .client
                    .json(
                        Method::GET,
                        &format!("/api/session/{}/permission", self.id),
                        &[],
                        None,
                    )
                    .await?;
                for request in permissions["data"].as_array().into_iter().flatten() {
                    out.push(Mapped::Permission(map::v2_permission(request)));
                }
            }
        }
        Ok(out)
    }

    pub(crate) async fn reply_permission(
        &self,
        request: &str,
        reply: &str,
    ) -> Result<(), OpencodeError> {
        match self.flavor {
            Flavor::V1 => {
                self.client
                    .ok(
                        Method::POST,
                        &format!("/permission/{request}/reply"),
                        &self.scope(),
                        Some(&json!({"reply": reply})),
                    )
                    .await
            }
            Flavor::V2 => {
                self.client
                    .ok(
                        Method::POST,
                        &format!("/api/session/{}/permission/{request}/reply", self.id),
                        &[],
                        Some(&json!({"reply": reply})),
                    )
                    .await
            }
        }
    }

    pub(crate) async fn reply_question(
        &self,
        request: &str,
        answers: Vec<Vec<String>>,
    ) -> Result<(), OpencodeError> {
        self.question_call(request, "reply", Some(json!({"answers": answers})))
            .await
    }

    pub(crate) async fn reject_question(&self, request: &str) -> Result<(), OpencodeError> {
        self.question_call(request, "reject", None).await
    }

    async fn question_call(
        &self,
        request: &str,
        action: &str,
        body: Option<Value>,
    ) -> Result<(), OpencodeError> {
        if self.flavor == Flavor::V2 {
            // The /api/* question flow is not verified against a real server; refuse loudly.
            return Err(OpencodeError::ApiUnavailable {
                what: "question replies over the v2 preview API",
            });
        }
        self.client
            .ok(
                Method::POST,
                &format!("/question/{request}/{action}"),
                &self.scope(),
                body.as_ref(),
            )
            .await
    }
}

/// Picks the flavor for an existing session without ever creating one.
///
/// A pinned flavor (recovery) is honoured or refused; it is never swapped for the other one.
pub(crate) async fn resolve(
    client: &Client,
    capabilities: &Capabilities,
    id: &str,
    directory: &str,
    pinned: Option<Flavor>,
) -> Result<(Native, NativeSession), OpencodeError> {
    if let Some(flavor) = pinned {
        if !flavor.available(capabilities) {
            return Err(OpencodeError::ApiUnavailable {
                what: match flavor {
                    Flavor::V1 => "the v1 session API this session was attached with",
                    Flavor::V2 => "the v2 session API this session was attached with",
                },
            });
        }
        return Native::open(client, flavor, id, directory).await;
    }
    for flavor in [Flavor::V1, Flavor::V2] {
        if !flavor.available(capabilities) {
            continue;
        }
        match Native::open(client, flavor, id, directory).await {
            Err(OpencodeError::SessionNotFound { .. }) => continue,
            other => return other,
        }
    }
    Err(OpencodeError::SessionNotFound { id: id.into() })
}
