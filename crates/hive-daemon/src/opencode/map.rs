//! Translation of OpenCode events into ACPv2 session updates.
//!
//! The mapper is stateful because OpenCode delivers the same text twice: as incremental
//! deltas and again as the full part. It remembers how much of each part it has already
//! emitted, which makes it idempotent: replaying history after a reconnect, or overlapping
//! a backfill with the live stream, can never duplicate output.

use std::collections::HashMap;

use agent_client_protocol::schema::v2;
use serde_json::{Value, json};

use super::api::Flavor;

/// One normalized occurrence, independent of which OpenCode API produced it.
// `Update` carries a whole ACP update while the others are tiny. Values are created and consumed
// immediately, so boxing would only add an allocation per streamed token.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Mapped {
    /// Forward verbatim to Hive subscribers.
    Update(v2::SessionUpdate),
    /// The native session started working.
    Busy,
    /// The native session stopped working.
    Idle,
    /// A turn failed; a short, sanitized summary.
    Failed(String),
    Permission(PermissionAsk),
    Question(QuestionAsk),
    /// The native side answered or withdrew this permission or question.
    Resolved(String),
    /// A durable v2 sequence number, used to resume the stream after a reconnect.
    Cursor(u64),
    /// Hint that the status should be re-read now rather than at the next tick.
    CheckStatus,
}

pub(crate) struct PermissionAsk {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct QuestionAsk {
    pub id: String,
    pub session: String,
    pub tool: Option<(String, String)>,
    pub items: Vec<QuestionItem>,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct QuestionItem {
    pub header: String,
    pub question: String,
    pub options: Vec<String>,
    pub multiple: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Reasoning,
}

pub(crate) struct Mapper {
    flavor: Flavor,
    session: String,
    /// Part (or text) identifier to bytes already emitted.
    emitted: HashMap<String, usize>,
    kinds: HashMap<String, Kind>,
    /// Message identifier to `true` when it is a user message (v1 only).
    user_messages: HashMap<String, bool>,
    /// A user message's content is replaced wholesale by each update, so keep every part.
    user_content: HashMap<String, Vec<(String, String)>>,
    reported_errors: HashMap<String, ()>,
}

impl Mapper {
    pub(crate) fn new(flavor: Flavor, session: &str) -> Self {
        Self {
            flavor,
            session: session.to_owned(),
            emitted: HashMap::new(),
            kinds: HashMap::new(),
            user_messages: HashMap::new(),
            user_content: HashMap::new(),
            reported_errors: HashMap::new(),
        }
    }

    /// Maps one SSE payload.
    pub(crate) fn event(&mut self, event: &Value) -> Vec<Mapped> {
        match self.flavor {
            Flavor::V1 => self.v1_event(event),
            Flavor::V2 => self.v2_event(event),
        }
    }

    /// Maps one entry of `GET /session/{id}/message` (v1 history).
    pub(crate) fn v1_message(&mut self, message: &Value) -> Vec<Mapped> {
        let mut out = self.v1_info(&message["info"]);
        for part in message["parts"].as_array().into_iter().flatten() {
            out.extend(self.v1_part(part));
        }
        out
    }

    // ---- v1 ---------------------------------------------------------------------------

    fn ours(&self, id: &Value) -> bool {
        id.as_str() == Some(self.session.as_str())
    }

    fn v1_event(&mut self, event: &Value) -> Vec<Mapped> {
        let properties = &event["properties"];
        match event["type"].as_str().unwrap_or_default() {
            "message.updated" if self.ours(&properties["sessionID"]) => {
                self.v1_info(&properties["info"])
            }
            "message.part.updated" if self.ours(&properties["part"]["sessionID"]) => {
                self.v1_part(&properties["part"])
            }
            "message.part.delta"
                if self.ours(&properties["sessionID"]) && properties["field"] == "text" =>
            {
                self.v1_delta(properties)
            }
            "session.status" if self.ours(&properties["sessionID"]) => {
                match properties["status"]["type"].as_str() {
                    Some("idle") => vec![Mapped::Idle],
                    // A retry is still foreground work.
                    Some("busy" | "retry") => vec![Mapped::Busy],
                    _ => vec![],
                }
            }
            "session.idle" if self.ours(&properties["sessionID"]) => vec![Mapped::Idle],
            "session.error" if self.ours(&properties["sessionID"]) => {
                vec![Mapped::Failed(summarize_error(&properties["error"]))]
            }
            "permission.asked" if self.ours(&properties["sessionID"]) => {
                vec![Mapped::Permission(v1_permission(properties))]
            }
            "question.asked" if self.ours(&properties["sessionID"]) => v1_question(properties)
                .map(Mapped::Question)
                .into_iter()
                .collect(),
            "permission.replied" | "question.replied" | "question.rejected"
                if self.ours(&properties["sessionID"]) =>
            {
                properties["requestID"]
                    .as_str()
                    .map(|id| Mapped::Resolved(id.to_owned()))
                    .into_iter()
                    .collect()
            }
            _ => vec![],
        }
    }

    fn v1_info(&mut self, info: &Value) -> Vec<Mapped> {
        let Some(id) = info["id"].as_str() else {
            return vec![];
        };
        self.user_messages
            .insert(id.to_owned(), info["role"] == "user");
        // A provider failure is recorded on the assistant message, not always as a session error.
        if info["error"].is_object() && self.reported_errors.insert(id.to_owned(), ()).is_none() {
            return vec![Mapped::Failed(summarize_error(&info["error"]))];
        }
        vec![]
    }

    fn v1_part(&mut self, part: &Value) -> Vec<Mapped> {
        let (Some(id), Some(message)) = (part["id"].as_str(), part["messageID"].as_str()) else {
            return vec![];
        };
        match part["type"].as_str().unwrap_or_default() {
            "text" if part["synthetic"] != true && part["ignored"] != true => {
                let text = part["text"].as_str().unwrap_or_default();
                if self.user_messages.get(message) == Some(&true) {
                    return self.user_text(message, id, text);
                }
                self.kinds.insert(id.to_owned(), Kind::Text);
                self.catch_up(id, message, text, Kind::Text)
            }
            "reasoning" => {
                self.kinds.insert(id.to_owned(), Kind::Reasoning);
                let text = part["text"].as_str().unwrap_or_default();
                self.catch_up(id, message, text, Kind::Reasoning)
            }
            "tool" => v1_tool(part).map(Mapped::Update).into_iter().collect(),
            _ => vec![],
        }
    }

    fn v1_delta(&mut self, properties: &Value) -> Vec<Mapped> {
        let (Some(id), Some(message), Some(delta)) = (
            properties["partID"].as_str(),
            properties["messageID"].as_str(),
            properties["delta"].as_str(),
        ) else {
            return vec![];
        };
        // A delta may outrun the part announcement; treat unknown parts as text.
        let kind = *self.kinds.entry(id.to_owned()).or_insert(Kind::Text);
        *self.emitted.entry(id.to_owned()).or_default() += delta.len();
        vec![chunk(kind, message, delta)]
    }

    fn user_text(&mut self, message: &str, part: &str, text: &str) -> Vec<Mapped> {
        let emitted = self.emitted.entry(part.to_owned()).or_default();
        if text.is_empty() || *emitted == text.len() {
            return vec![];
        }
        *emitted = text.len();
        let parts = self.user_content.entry(message.to_owned()).or_default();
        match parts.iter_mut().find(|(id, _)| id == part) {
            Some((_, existing)) => *existing = text.to_owned(),
            None => parts.push((part.to_owned(), text.to_owned())),
        }
        let content = parts
            .iter()
            .map(|(_, text)| v2::ContentBlock::from(text.clone()))
            .collect::<Vec<_>>();
        vec![Mapped::Update(v2::SessionUpdate::UserMessage(
            v2::UserMessage::new(v2::MessageId::new(message.to_owned())).content(content),
        ))]
    }

    /// Emits whatever part of `full` has not been forwarded yet.
    fn catch_up(&mut self, id: &str, message: &str, full: &str, kind: Kind) -> Vec<Mapped> {
        let emitted = self.emitted.entry(id.to_owned()).or_default();
        // A non-boundary offset means the text was rewritten, not extended; wait for a later
        // update rather than slice a character in half.
        if full.len() <= *emitted || !full.is_char_boundary(*emitted) {
            return vec![];
        }
        let tail = full[*emitted..].to_owned();
        *emitted = full.len();
        vec![chunk(kind, message, &tail)]
    }

    // ---- v2 ---------------------------------------------------------------------------

    fn v2_event(&mut self, event: &Value) -> Vec<Mapped> {
        let Some(data) = event.get("data").or_else(|| event.get("properties")) else {
            return vec![];
        };
        if !self.ours(&data["sessionID"]) {
            return vec![];
        }
        let mut out = Vec::new();
        if let Some(seq) = event["durable"]["seq"].as_u64() {
            out.push(Mapped::Cursor(seq));
        }
        let assistant = data["assistantMessageID"].as_str().unwrap_or_default();
        let text_key = format!(
            "{assistant}/{}",
            data["textID"].as_str().unwrap_or_default()
        );
        match event["type"].as_str().unwrap_or_default() {
            "session.next.prompted" => {
                if let (Some(id), Some(text)) =
                    (data["messageID"].as_str(), data["prompt"]["text"].as_str())
                {
                    out.push(Mapped::Update(v2::SessionUpdate::UserMessage(
                        v2::UserMessage::new(v2::MessageId::new(id.to_owned()))
                            .content(vec![v2::ContentBlock::from(text.to_owned())]),
                    )));
                }
            }
            "session.next.step.started" | "session.next.retried" => out.push(Mapped::Busy),
            "session.next.step.ended" => out.push(Mapped::CheckStatus),
            "session.next.step.failed" => {
                out.push(Mapped::Failed(summarize_error(&data["error"])));
                out.push(Mapped::CheckStatus);
            }
            "session.next.text.delta" => {
                if let Some(delta) = data["delta"].as_str() {
                    *self.emitted.entry(text_key).or_default() += delta.len();
                    out.push(chunk(Kind::Text, assistant, delta));
                }
            }
            "session.next.text.ended" => {
                let full = data["text"].as_str().unwrap_or_default();
                out.extend(self.catch_up(&text_key, assistant, full, Kind::Text));
            }
            "session.next.reasoning.ended" => {
                let full = data["text"].as_str().unwrap_or_default();
                out.extend(self.catch_up(&text_key, assistant, full, Kind::Reasoning));
            }
            "session.next.tool.called" | "session.next.tool.progress" => {
                if let Some(call) = data["callID"].as_str() {
                    let mut update = v2::ToolCallUpdate::new(call.to_owned())
                        .status(v2::ToolCallStatus::InProgress);
                    if let Some(tool) = data["tool"].as_str() {
                        update = update.name(tool.to_owned()).kind(tool_kind(tool));
                    }
                    if data["input"].is_object() {
                        update = update.raw_input(data["input"].clone());
                    }
                    out.push(Mapped::Update(v2::SessionUpdate::ToolCallUpdate(update)));
                }
            }
            "session.next.tool.success" | "session.next.tool.failed" => {
                if let Some(call) = data["callID"].as_str() {
                    let ok = event["type"] == "session.next.tool.success";
                    let output = if ok {
                        json!({ "structured": data["structured"], "content": data["content"] })
                    } else {
                        json!({ "error": summarize_error(&data["error"]) })
                    };
                    out.push(Mapped::Update(v2::SessionUpdate::ToolCallUpdate(
                        v2::ToolCallUpdate::new(call.to_owned())
                            .status(if ok {
                                v2::ToolCallStatus::Completed
                            } else {
                                v2::ToolCallStatus::Failed
                            })
                            .raw_output(output),
                    )));
                }
            }
            _ => {}
        }
        out
    }
}

fn chunk(kind: Kind, message: &str, text: &str) -> Mapped {
    let chunk = v2::ContentChunk::new(
        text.to_owned().into(),
        v2::MessageId::new(message.to_owned()),
    );
    Mapped::Update(match kind {
        Kind::Text => v2::SessionUpdate::AgentMessageChunk(chunk),
        Kind::Reasoning => v2::SessionUpdate::AgentThoughtChunk(chunk),
    })
}

fn tool_kind(tool: &str) -> v2::ToolKind {
    match tool {
        "read" => v2::ToolKind::Read,
        "edit" | "write" | "patch" | "multiedit" | "apply_patch" => v2::ToolKind::Edit,
        "bash" | "shell" => v2::ToolKind::Execute,
        "grep" | "glob" | "list" | "ls" => v2::ToolKind::Search,
        "webfetch" | "websearch" => v2::ToolKind::Fetch,
        _ => v2::ToolKind::Other,
    }
}

fn v1_tool(part: &Value) -> Option<v2::SessionUpdate> {
    let call = part["callID"].as_str()?;
    let state = &part["state"];
    let status = match state["status"].as_str()? {
        "pending" => v2::ToolCallStatus::Pending,
        "running" => v2::ToolCallStatus::InProgress,
        "completed" => v2::ToolCallStatus::Completed,
        "error" => v2::ToolCallStatus::Failed,
        _ => return None,
    };
    let tool = part["tool"].as_str().unwrap_or("tool");
    let mut update = v2::ToolCallUpdate::new(call.to_owned())
        .name(tool.to_owned())
        .kind(tool_kind(tool))
        .status(status);
    if let Some(title) = state["title"].as_str() {
        update = update.title(title.to_owned());
    }
    if state["input"].is_object() {
        update = update.raw_input(state["input"].clone());
    }
    match state["status"].as_str() {
        Some("completed") => update = update.raw_output(state["output"].clone()),
        Some("error") => update = update.raw_output(json!({ "error": state["error"] })),
        _ => {}
    }
    Some(v2::SessionUpdate::ToolCallUpdate(update))
}

fn v1_permission(properties: &Value) -> PermissionAsk {
    let patterns = properties["patterns"]
        .as_array()
        .map(|patterns| {
            patterns
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|joined| !joined.is_empty());
    PermissionAsk {
        id: properties["id"].as_str().unwrap_or_default().to_owned(),
        title: format!(
            "OpenCode requests permission: {}",
            properties["permission"].as_str().unwrap_or("action")
        ),
        description: patterns.map(|patterns| truncate(&patterns, 300)),
    }
}

/// Builds the permission ask from a `PermissionV2Request`.
pub(crate) fn v2_permission(request: &Value) -> PermissionAsk {
    let resources = request["resources"]
        .as_array()
        .map(|resources| {
            resources
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|joined| !joined.is_empty());
    PermissionAsk {
        id: request["id"].as_str().unwrap_or_default().to_owned(),
        title: format!(
            "OpenCode requests permission: {}",
            request["action"].as_str().unwrap_or("action")
        ),
        description: resources.map(|resources| truncate(&resources, 300)),
    }
}

/// Builds the question ask from a `QuestionRequest`.
pub(crate) fn v1_question(request: &Value) -> Option<QuestionAsk> {
    let items = request["questions"]
        .as_array()?
        .iter()
        .map(|item| QuestionItem {
            header: item["header"].as_str().unwrap_or_default().to_owned(),
            question: item["question"].as_str().unwrap_or_default().to_owned(),
            options: item["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|option| option["label"].as_str().map(str::to_owned))
                .collect(),
            multiple: item["multiple"] == true,
        })
        .collect::<Vec<_>>();
    Some(QuestionAsk {
        id: request["id"].as_str()?.to_owned(),
        session: request["sessionID"].as_str().unwrap_or_default().to_owned(),
        // Preserve malformed linkage as unresolved, rather than bypassing classification.
        tool: request
            .get("tool")
            .filter(|tool| !tool.is_null())
            .map(|tool| {
                (
                    tool["messageID"].as_str().unwrap_or_default().to_owned(),
                    tool["callID"].as_str().unwrap_or_default().to_owned(),
                )
            }),
        items,
    })
}

/// Classify by provider-owned tool identity, never by translated question wording.
pub(crate) fn plan_exit(ask: &QuestionAsk, message: &Value) -> Result<bool, &'static str> {
    let Some((message_id, call_id)) = &ask.tool else {
        return Ok(false);
    };
    let info = &message["info"];
    if message_id.is_empty()
        || call_id.is_empty()
        || info["id"] != *message_id
        || info["sessionID"] != ask.session
        || info["role"] != "assistant"
    {
        return Err("question message identity");
    }
    let part = message["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|part| {
            part["type"] == "tool"
                && part["callID"] == *call_id
                && part["messageID"] == *message_id
                && part["sessionID"] == ask.session
        })
        .ok_or("question tool identity")?;
    if part["tool"] != "plan_exit" {
        return Ok(false);
    }
    if !matches!(
        part["state"]["status"].as_str(),
        Some("pending" | "running")
    ) {
        return Err("inactive plan_exit question");
    }
    if ask.items.len() != 1 || ask.items[0].multiple || ask.items[0].options != ["Yes", "No"] {
        return Err("plan_exit question contract");
    }
    Ok(true)
}

pub(crate) fn leave_plan_permission(
    hive_id: v2::SessionId,
    ask: &QuestionAsk,
    message: &Value,
) -> v2::RequestPermissionRequest {
    let excerpt = message["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| {
            part["type"] == "text" && part["synthetic"] != true && part["ignored"] != true
        })
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let context = if excerpt.is_empty() {
        "Plan content was not supplied; review the provider's plan reference above.".to_owned()
    } else {
        format!(
            "Available plan context (assistant excerpt, not the plan file):\n{}",
            truncate(&excerpt, 2500)
        )
    };
    // The browser focuses the first action; accidental Enter must not select execution.
    v2::RequestPermissionRequest::new(hive_id, "OpenCode: leave Plan mode?", vec![
        v2::PermissionOption::new("stay", "Stay in Plan / refine the plan", v2::PermissionOptionKind::RejectOnce),
        v2::PermissionOption::new("approve", "Leave Plan and start implementing", v2::PermissionOptionKind::AllowOnce),
    ]).description(bounded_context(&format!("Approving lets OpenCode switch to Build and start implementing.\n\nProvider request:\n{}\n\n{}",
        truncate(&ask.items[0].question, 1000), context)))
}

fn bounded_context(text: &str) -> String {
    if text.len() <= 4096 {
        return text.to_owned();
    }
    let mut end = 4096 - "\n[truncated]".len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

pub(crate) fn v1_permission_ask(request: &Value) -> PermissionAsk {
    v1_permission(request)
}

/// Permission choices OpenCode understands, as ACP options. Option IDs are OpenCode's own
/// reply values so the response needs no translation table.
pub(crate) fn permission_options() -> Vec<v2::PermissionOption> {
    vec![
        v2::PermissionOption::new("once", "Allow once", v2::PermissionOptionKind::AllowOnce),
        v2::PermissionOption::new(
            "always",
            "Allow always",
            v2::PermissionOptionKind::AllowAlways,
        ),
        v2::PermissionOption::new("reject", "Reject", v2::PermissionOptionKind::RejectOnce),
    ]
}

/// A form elicitation with one field per OpenCode question (`q0`, `q1`, ...).
pub(crate) fn elicitation(
    hive_id: &v2::SessionId,
    ask: &QuestionAsk,
) -> Option<v2::CreateElicitationRequest> {
    let mut properties = serde_json::Map::new();
    for (index, item) in ask.items.iter().enumerate() {
        let title = if item.header.is_empty() {
            format!("Question {}", index + 1)
        } else {
            item.header.clone()
        };
        let property = match (item.options.is_empty(), item.multiple) {
            // No choices: free text.
            (true, _) => json!({"type": "string", "title": title, "description": item.question}),
            (false, false) => json!({
                "type": "string", "title": title, "description": item.question,
                "enum": item.options,
            }),
            (false, true) => json!({
                "type": "array", "title": title, "description": item.question,
                "items": {"type": "string", "enum": item.options},
            }),
        };
        properties.insert(format!("q{index}"), property);
    }
    let required = (0..ask.items.len())
        .map(|i| format!("q{i}"))
        .collect::<Vec<_>>();
    serde_json::from_value(json!({
        "mode": "form",
        "sessionId": hive_id.to_string(),
        "message": "OpenCode needs input to continue",
        "requestedSchema": {"type": "object", "properties": properties, "required": required},
    }))
    .ok()
}

/// Answers in OpenCode's `string[][]` shape, from an accepted elicitation.
pub(crate) fn answers(
    ask: &QuestionAsk,
    content: &std::collections::BTreeMap<String, v2::ElicitationContentValue>,
) -> Vec<Vec<String>> {
    (0..ask.items.len())
        .map(|index| match content.get(&format!("q{index}")) {
            Some(v2::ElicitationContentValue::String(answer)) => vec![answer.clone()],
            Some(v2::ElicitationContentValue::StringArray(answers)) => answers.clone(),
            _ => vec![],
        })
        .collect()
}

/// Classify provider failures without retaining arbitrary messages that can echo secrets.
pub(crate) fn summarize_error(error: &Value) -> String {
    let failure = hive_protocol::AgentFailure::classify(&error.to_string());
    format!("{failure} — {}", failure.help())
}

fn truncate(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "ses_1";

    fn mapper() -> Mapper {
        Mapper::new(Flavor::V1, SESSION)
    }

    fn text_of(mapped: &[Mapped]) -> String {
        mapped
            .iter()
            .filter_map(|item| match item {
                Mapped::Update(v2::SessionUpdate::AgentMessageChunk(chunk)) => {
                    serde_json::to_value(&chunk.content).ok()?["text"]
                        .as_str()
                        .map(str::to_owned)
                }
                _ => None,
            })
            .collect()
    }

    fn assistant(mapper: &mut Mapper) {
        mapper.event(&json!({"type": "message.updated", "properties": {
            "sessionID": SESSION, "info": {"id": "msg_a", "role": "assistant"}}}));
    }

    fn delta(text: &str) -> Value {
        json!({"type": "message.part.delta", "properties": {
            "sessionID": SESSION, "messageID": "msg_a", "partID": "prt_1",
            "field": "text", "delta": text}})
    }

    fn part(text: &str) -> Value {
        json!({"type": "message.part.updated", "properties": {"part": {
            "id": "prt_1", "sessionID": SESSION, "messageID": "msg_a",
            "type": "text", "text": text}}})
    }

    #[test]
    fn deltas_followed_by_the_full_part_emit_the_text_exactly_once() {
        let mut mapper = mapper();
        assistant(&mut mapper);
        let mut all = mapper.event(&part(""));
        all.extend(mapper.event(&delta("Wor")));
        all.extend(mapper.event(&delta("ld")));
        all.extend(mapper.event(&part("World")));
        assert_eq!(text_of(&all), "World");
    }

    #[test]
    fn replaying_a_part_after_a_reconnect_emits_only_the_missing_tail() {
        let mut mapper = mapper();
        assistant(&mut mapper);
        assert_eq!(text_of(&mapper.event(&part("Wor"))), "Wor");
        // The stream dropped; reconciliation re-reads the full message.
        assert_eq!(text_of(&mapper.event(&part("World wide"))), "ld wide");
        assert_eq!(text_of(&mapper.event(&part("World wide"))), "");
    }

    #[test]
    fn a_tail_is_never_cut_through_a_multibyte_character() {
        let mut mapper = mapper();
        assistant(&mut mapper);
        mapper.event(&part("é"));
        // Emitted length 2 is a boundary of "éa"; a rewrite to a shorter string is ignored.
        assert_eq!(text_of(&mapper.event(&part("éa"))), "a");
        assert_eq!(text_of(&mapper.event(&part("e"))), "");
    }

    #[test]
    fn events_for_other_sessions_in_the_same_directory_are_ignored() {
        let mut mapper = mapper();
        let foreign = json!({"type": "session.status", "properties": {
            "sessionID": "ses_other", "status": {"type": "busy"}}});
        assert!(mapper.event(&foreign).is_empty());
    }

    #[test]
    fn user_messages_are_forwarded_once_with_all_their_parts() {
        let mut mapper = mapper();
        mapper.event(&json!({"type": "message.updated", "properties": {
            "sessionID": SESSION, "info": {"id": "msg_u", "role": "user"}}}));
        let first = json!({"type": "message.part.updated", "properties": {"part": {
            "id": "prt_u1", "sessionID": SESSION, "messageID": "msg_u", "type": "text",
            "text": "one"}}});
        let second = json!({"type": "message.part.updated", "properties": {"part": {
            "id": "prt_u2", "sessionID": SESSION, "messageID": "msg_u", "type": "text",
            "text": "two"}}});
        assert_eq!(mapper.event(&first).len(), 1);
        let Some(Mapped::Update(v2::SessionUpdate::UserMessage(message))) =
            mapper.event(&second).pop()
        else {
            panic!("expected a user message");
        };
        let content = serde_json::to_value(&message.content).unwrap();
        assert_eq!(content.as_array().map(Vec::len), Some(2));
        // The same part again is a no-op.
        assert!(mapper.event(&second).is_empty());
    }

    #[test]
    fn status_events_map_to_busy_and_idle() {
        let mut mapper = mapper();
        let status = |kind: &str| {
            json!({"type": "session.status", "properties": {
                "sessionID": SESSION, "status": {"type": kind}}})
        };
        assert!(matches!(mapper.event(&status("busy"))[..], [Mapped::Busy]));
        assert!(matches!(mapper.event(&status("retry"))[..], [Mapped::Busy]));
        assert!(matches!(mapper.event(&status("idle"))[..], [Mapped::Idle]));
    }

    #[test]
    fn tool_parts_map_to_tool_call_updates_with_status() {
        let mut mapper = mapper();
        let running = json!({"type": "message.part.updated", "properties": {"part": {
            "id": "prt_t", "sessionID": SESSION, "messageID": "msg_a", "type": "tool",
            "callID": "call_1", "tool": "bash",
            "state": {"status": "running", "input": {"command": "ls"}, "title": "ls"}}}});
        let Some(Mapped::Update(v2::SessionUpdate::ToolCallUpdate(update))) =
            mapper.event(&running).pop()
        else {
            panic!("expected a tool call update");
        };
        let value = serde_json::to_value(&update).unwrap();
        assert_eq!(value["toolCallId"], "call_1");
        assert_eq!(value["status"], "in_progress");
        assert_eq!(value["kind"], "execute");
    }

    #[test]
    fn a_native_answer_resolves_a_pending_permission() {
        let mut mapper = mapper();
        let replied = json!({"type": "permission.replied", "properties": {
            "sessionID": SESSION, "requestID": "per_1", "reply": "once"}});
        assert!(matches!(&mapper.event(&replied)[..], [Mapped::Resolved(id)] if id == "per_1"));
    }

    #[test]
    fn a_provider_failure_on_the_assistant_message_is_reported_once_without_its_payload() {
        let mut mapper = mapper();
        let updated = json!({"type": "message.updated", "properties": {
            "sessionID": SESSION, "info": {"id": "msg_a", "role": "assistant",
                "error": {"name": "APIError", "data": {"message": "bad key", "responseBody": "sk-secret"}}}}});
        let first = mapper.event(&updated);
        let [Mapped::Failed(summary)] = &first[..] else {
            panic!("expected a failure");
        };
        assert!(summary.contains("provider credentials"));
        assert!(!summary.contains("bad key"));
        assert!(!summary.contains("sk-secret"));
        assert!(mapper.event(&updated).is_empty(), "reported only once");
    }

    #[test]
    fn a_question_becomes_a_form_with_one_field_per_question() {
        let ask = v1_question(&json!({"id": "que_1", "questions": [
            {"header": "Mode", "question": "Pick one", "options": [{"label": "a"}, {"label": "b"}]},
            {"header": "Tags", "question": "Pick many", "multiple": true,
             "options": [{"label": "x"}]},
            {"header": "Name", "question": "Free text", "options": []}]}))
        .unwrap();
        let hive = v2::SessionId::new("hive-1");
        let request = elicitation(&hive, &ask).expect("a valid ACP form");
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["requestedSchema"]["properties"]["q0"]["enum"][1], "b");
        assert_eq!(
            value["requestedSchema"]["properties"]["q1"]["type"],
            "array"
        );
        assert_eq!(
            value["requestedSchema"]["properties"]["q2"]["type"],
            "string"
        );
    }

    #[test]
    fn form_answers_are_converted_to_opencode_string_arrays() {
        let ask = v1_question(&json!({"id": "que_1", "questions": [
            {"header": "A", "question": "?", "options": [{"label": "a"}]},
            {"header": "B", "question": "?", "multiple": true, "options": [{"label": "x"}]}]}))
        .unwrap();
        let content = std::collections::BTreeMap::from([
            (
                "q0".to_owned(),
                v2::ElicitationContentValue::String("a".into()),
            ),
            (
                "q1".to_owned(),
                v2::ElicitationContentValue::StringArray(vec!["x".into(), "y".into()]),
            ),
        ]);
        assert_eq!(
            answers(&ask, &content),
            vec![vec!["a".to_owned()], vec!["x".to_owned(), "y".to_owned()]]
        );
    }

    #[test]
    fn v2_events_carry_a_cursor_and_map_text_tools_and_failures() {
        let mut mapper = Mapper::new(Flavor::V2, SESSION);
        let delta = json!({"type": "session.next.text.delta", "durable": {"seq": 7},
            "data": {"sessionID": SESSION, "assistantMessageID": "msg_a", "textID": "t1", "delta": "Hi"}});
        let out = mapper.event(&delta);
        assert!(matches!(out[0], Mapped::Cursor(7)));
        assert_eq!(text_of(&out), "Hi");
        let ended = json!({"type": "session.next.text.ended",
            "data": {"sessionID": SESSION, "assistantMessageID": "msg_a", "textID": "t1", "text": "Hi there"}});
        assert_eq!(text_of(&mapper.event(&ended)), " there");
        let failed = json!({"type": "session.next.step.failed",
            "data": {"sessionID": SESSION, "error": {"type": "unknown", "message": "boom"}}});
        let out = mapper.event(&failed);
        assert!(
            matches!(&out[0], Mapped::Failed(summary) if summary.contains("Check the agent") && !summary.contains("boom"))
        );
        assert!(matches!(out[1], Mapped::CheckStatus));
    }

    #[test]
    fn known_expired_keys_get_actionable_guidance_without_echoing_arbitrary_message_text() {
        let summary = summarize_error(
            &json!({"name": "E", "data": {"message": "This API key has expired. sk-secret"}}),
        );
        assert!(summary.contains("Create a new API key in OpenCode Console"));
        assert!(!summary.contains("sk-secret"));
    }

    fn plan_fixture() -> (QuestionAsk, Value) {
        let ask = v1_question(&json!({"id":"q", "sessionID":SESSION,
            "tool":{"messageID":"m", "callID":"c"},
            "questions":[{"question":"Review plan.md", "options":[{"label":"Yes"},{"label":"No"}]}]})).unwrap();
        let message = json!({"info":{"id":"m", "sessionID":SESSION, "role":"assistant"},
            "parts":[{"type":"tool", "messageID":"m", "sessionID":SESSION, "callID":"c",
                "tool":"plan_exit", "state":{"status":"running"}}]});
        (ask, message)
    }

    #[test]
    fn plan_exit_requires_exact_message_session_call_and_question_contract() {
        let (ask, message) = plan_fixture();
        assert_eq!(plan_exit(&ask, &message), Ok(true));
        for pointer in [
            "/info/id",
            "/info/sessionID",
            "/info/role",
            "/parts/0/messageID",
            "/parts/0/sessionID",
            "/parts/0/callID",
            "/parts/0/state/status",
        ] {
            let mut invalid = message.clone();
            *invalid.pointer_mut(pointer).unwrap() = json!("other");
            assert!(plan_exit(&ask, &invalid).is_err(), "{pointer}");
        }
        let mut other_tool = message.clone();
        other_tool["parts"][0]["tool"] = json!("question");
        assert_eq!(
            plan_exit(&ask, &other_tool),
            Ok(false),
            "identical wording is not tool identity"
        );
        let mut malformed = ask.clone();
        malformed.items[0].options.clear();
        assert!(plan_exit(&malformed, &message).is_err());
        malformed = ask.clone();
        malformed.items[0].multiple = true;
        assert!(plan_exit(&malformed, &message).is_err());
        let mut missing_link = ask;
        missing_link.tool = None;
        assert_eq!(plan_exit(&missing_link, &message), Ok(false));
    }

    #[test]
    fn plan_context_is_bounded_utf8_and_never_includes_private_or_synthetic_text() {
        let (ask, mut message) = plan_fixture();
        let no_context = leave_plan_permission(v2::SessionId::new("h"), &ask, &message);
        assert!(no_context.description.unwrap().contains("not supplied"));
        message["parts"].as_array_mut().unwrap().extend([
            json!({"type":"reasoning", "text":"SECRET"}),
            json!({"type":"text", "synthetic":true, "text":"SECRET"}),
            json!({"type":"text", "ignored":true, "text":"SECRET"}),
            json!({"type":"text", "text":"界".repeat(5000)}),
        ]);
        let description = leave_plan_permission(v2::SessionId::new("h"), &ask, &message)
            .description
            .unwrap();
        assert!(description.len() <= 4096);
        assert!(description.ends_with("[truncated]"));
        assert!(description.contains("assistant excerpt"));
        assert!(!description.contains("SECRET"));
    }
}
