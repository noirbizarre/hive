use std::collections::HashMap;

use agent_client_protocol::schema::v2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    User,
    Agent,
    Tool,
    Notice,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ConversationLine {
    pub kind: LineKind,
    pub text: String,
    pub tool: Option<serde_json::Value>,
    pub revision: u64,
}

#[derive(Default)]
pub struct Conversation {
    pub lines: Vec<ConversationLine>,
    messages: HashMap<String, usize>,
    tools: HashMap<String, usize>,
    pub seq: u64,
    snapshot_through: Option<u64>,
    pub running: bool,
    pub generation: u64,
    pub pilot_present: bool,
    pub activity: serde_json::Value,
    pub activity_seq: u64,
    pub plan_snapshot: serde_json::Value,
    pub plan_seq: u64,
    pub plans_authoritative: bool,
    pub usage: serde_json::Value,
    pub usage_seq: u64,
    pub gap: bool,
    pub config_options: Vec<v2::SessionConfigOption>,
    pub config_seq: u64,
    pub transcript: hive_protocol::history::TranscriptStatus,
    pub transcript_seq: u64,
}

impl Conversation {
    pub fn transcript_snapshot(&mut self, value: &serde_json::Value, seq: u64) {
        // Resume metadata is newer than control events replayed from an earlier cursor.
        if seq >= self.transcript_seq
            && value.is_object()
            && let Ok(status) = serde_json::from_value(value.clone())
        {
            self.transcript = status;
            self.transcript_seq = seq;
        }
    }
    pub fn begin_transcript(&mut self, through: u64) {
        // The request must succeed before queued snapshot notifications can replace this view.
        *self = Self {
            seq: through,
            snapshot_through: Some(through),
            ..Self::default()
        };
    }

    pub fn snapshot(&mut self, value: &serde_json::Value) {
        let seq = value["seq"].as_u64().unwrap_or(0);
        if seq >= self.activity_seq && value["version"] == 1 {
            self.activity = value.clone();
            self.activity_seq = seq;
        }
    }

    pub fn usage_snapshot(&mut self, value: &serde_json::Value, seq: u64) {
        if seq >= self.usage_seq && value.is_object() {
            self.usage = value.clone();
            self.usage_seq = seq;
        }
    }

    pub fn plans_snapshot(&mut self, value: &serde_json::Value) {
        // A stale attachment cannot replace plans already reduced from later live events.
        if value["version"] == 1
            && let Some(seq) = value["seq"].as_u64()
            && seq >= self.plan_seq
        {
            self.plan_snapshot = value.clone();
            self.plan_seq = seq;
            self.plans_authoritative = true;
        }
    }

    #[cfg(test)]
    pub fn push_user(&mut self, text: String) {
        self.lines.push(ConversationLine {
            kind: LineKind::User,
            text: format!("You: {text}"),
            tool: None,
            revision: 0,
        });
    }

    pub fn push_notice(&mut self, text: String) {
        self.lines.push(ConversationLine {
            kind: LineKind::Notice,
            text,
            tool: None,
            revision: 0,
        });
    }

    pub fn config_snapshot(&mut self, options: Vec<v2::SessionConfigOption>, seq: u64) {
        // Replay predating the attachment snapshot must not undo the active model selection.
        if seq >= self.config_seq {
            self.config_options = options;
            self.config_seq = seq;
        }
    }

    fn upsert(&mut self, key: String, kind: LineKind, text: &str, replace: bool) {
        let indexes = if kind == LineKind::Tool {
            &mut self.tools
        } else {
            &mut self.messages
        };
        let label = match kind {
            LineKind::User => "You",
            LineKind::Agent => "Agent",
            LineKind::Tool => "Tool",
            LineKind::Notice => "Notice",
        };
        let index = *indexes.entry(key).or_insert_with(|| {
            self.lines.push(ConversationLine {
                kind,
                text: format!("{label}: "),
                tool: None,
                revision: 0,
            });
            self.lines.len() - 1
        });
        if replace {
            self.lines[index].text = format!("{label}: {text}");
        } else {
            self.lines[index].text.push_str(text);
        }
        self.lines[index].revision += 1;
    }

    fn tool(&mut self, value: serde_json::Value, chunk: bool) {
        let Some(id) = value["toolCallId"].as_str() else {
            return;
        };
        let index = *self.tools.entry(id.to_owned()).or_insert_with(|| {
            self.lines.push(ConversationLine {
                kind: LineKind::Tool,
                text: String::new(),
                tool: Some(serde_json::json!({"toolCallId": id})),
                revision: 0,
            });
            self.lines.len() - 1
        });
        let line = &mut self.lines[index];
        let stored = line.tool.as_mut().unwrap();
        if chunk {
            // Chunk content appends; a concrete content array in a later patch replaces it.
            if !stored["content"].is_array() {
                stored["content"] = serde_json::json!([]);
            }
            stored["content"]
                .as_array_mut()
                .unwrap()
                .push(value["content"].clone());
        } else if let Some(fields) = value.as_object() {
            // Keep omitted fields across both native v2 and translated v1 patches.
            for (key, value) in fields {
                if key == "_meta" && value.is_object() {
                    if !stored[key].is_object() {
                        stored[key] = serde_json::json!({});
                    }
                    for (name, value) in value.as_object().unwrap() {
                        if value.is_null() {
                            stored[key].as_object_mut().unwrap().remove(name);
                        } else {
                            stored[key][name] = value.clone();
                        }
                    }
                } else {
                    stored[key] = value.clone();
                }
            }
        }
        line.revision += 1;
    }

    pub fn apply(&mut self, notification: v2::UpdateSessionNotification) -> bool {
        let snapshot = notification
            .meta
            .as_ref()
            .and_then(|m| m.get("hive"))
            .is_some_and(|h| h["snapshot"] == true);
        let seq = notification
            .meta
            .as_ref()
            .and_then(|m| m.get("hive"))
            .and_then(|h| h.get("seq"))
            .and_then(serde_json::Value::as_u64);
        let Some(seq) = seq else {
            return false;
        };
        if snapshot && self.snapshot_through != Some(seq) {
            return false;
        }
        if !snapshot && seq <= self.seq {
            return false;
        }
        if !snapshot && self.seq != 0 && seq != self.seq + 1 {
            self.gap = true;
            return false;
        }
        // Replay and live events share sequence numbers; only advance after reducing the event.
        match notification.update {
            v2::SessionUpdate::PlanUpdate(update) if seq > self.plan_seq => {
                // Native observation alone does not make an older daemon's activity fallback complete.
                let value = serde_json::to_value(update).unwrap_or_default();
                if let Some(id) = value["plan"]["planId"].as_str().map(str::to_owned) {
                    if !self.plan_snapshot.is_object() {
                        self.plan_snapshot = serde_json::json!({"version":1,"plans":{}});
                    }
                    // A replacement also preserves unknown content types and native metadata.
                    self.plan_snapshot["plans"][id] = value;
                    self.plan_snapshot["support"] = "supported".into();
                    self.plan_snapshot["observation"] = "observed".into();
                    self.plan_snapshot["seq"] = seq.into();
                    self.plan_seq = seq;
                }
            }
            v2::SessionUpdate::ConfigOptionUpdate(update) => {
                self.config_snapshot(update.config_options, seq)
            }
            v2::SessionUpdate::AgentMessageChunk(chunk) => self.upsert(
                format!("agent:{}", chunk.message_id),
                LineKind::Agent,
                &content_text(&chunk.content),
                false,
            ),
            v2::SessionUpdate::AgentThoughtChunk(chunk) => self.upsert(
                format!("thought:{}", chunk.message_id),
                LineKind::Notice,
                &content_text(&chunk.content),
                false,
            ),
            v2::SessionUpdate::AgentThought(message) => {
                if let Some(text) = message_content(&message.content) {
                    self.upsert(
                        format!("thought:{}", message.message_id),
                        LineKind::Notice,
                        &text,
                        true,
                    );
                }
            }
            v2::SessionUpdate::UserMessageChunk(chunk) => self.upsert(
                format!("user:{}", chunk.message_id),
                LineKind::User,
                &content_text(&chunk.content),
                false,
            ),
            v2::SessionUpdate::AgentMessage(message) => {
                if let Some(text) = message_content(&message.content) {
                    self.upsert(
                        format!("agent:{}", message.message_id),
                        LineKind::Agent,
                        &text,
                        true,
                    );
                }
            }
            v2::SessionUpdate::UserMessage(message) => {
                if let Some(text) = message_content(&message.content) {
                    self.upsert(
                        format!("user:{}", message.message_id),
                        LineKind::User,
                        &text,
                        true,
                    );
                }
            }
            v2::SessionUpdate::ToolCallUpdate(tool) => {
                self.tool(serde_json::to_value(tool).unwrap_or_default(), false);
            }
            v2::SessionUpdate::ToolCallContentChunk(chunk) => {
                self.tool(serde_json::to_value(chunk).unwrap_or_default(), true);
            }
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => self.running = true,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                self.running = false;
                if matches!(idle.stop_reason, Some(v2::StopReason::Cancelled)) {
                    self.push_notice("Turn cancelled".into());
                }
            }
            v2::SessionUpdate::Other(other)
                if other.session_update == "_hive_error"
                    || other.session_update == "_hive_opencode_error" =>
            {
                self.push_notice(format!(
                    "{} — {}",
                    other.fields["message"].as_str().unwrap_or("Agent failed"),
                    other.fields["help"]
                        .as_str()
                        .unwrap_or("Check the agent logs")
                ));
            }
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_control" => {
                self.generation = other.fields["generation"]
                    .as_u64()
                    .unwrap_or(self.generation);
                self.pilot_present = other.fields["pilot"].as_bool().unwrap_or(false);
                if let Some(status) = other.fields.get("transcript") {
                    self.transcript_snapshot(status, seq);
                }
            }
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_usage" => {
                if seq > self.usage_seq
                    && let Some(session) = other.fields.get("session")
                {
                    self.usage = session.clone();
                    self.usage_seq = seq;
                }
            }
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_activity" => {
                let value = serde_json::to_value(other.fields).unwrap_or_default();
                if seq > self.activity_seq {
                    let kind = value["kind"].as_str().unwrap_or_default();
                    if kind == "foreground" {
                        self.activity["foreground"] = value["state"]["state"].clone();
                    }
                    if kind == "session" {
                        self.activity["session"] = value["status"].clone();
                    }
                    let (key, id) = match kind {
                        "tool" => ("tools", value["toolCallId"].as_str()),
                        "plan" => ("plans", value["planId"].as_str()),
                        "task" => ("tasks", value["taskId"].as_str()),
                        _ => ("", None),
                    };
                    if let Some(id) = id {
                        if !self.activity[key].is_object() {
                            self.activity[key] = serde_json::json!({});
                        }
                        self.activity[key][id] = value[kind].clone();
                        let support = if key == "plans" {
                            "plans"
                        } else if key == "tasks" {
                            "tasks"
                        } else {
                            "tools"
                        };
                        self.activity["support"][support] = serde_json::json!("observed");
                    }
                    self.activity_seq = seq;
                }
            }
            v2::SessionUpdate::Other(other)
                if other.session_update.starts_with("_hive_v1_tool") =>
            {
                self.tool(other.fields["tool"].clone(), false);
            }
            _ => {}
        }
        if !snapshot {
            self.seq = seq;
            self.snapshot_through = None;
        }
        true
    }
}

fn content_text(block: &v2::ContentBlock) -> String {
    match block {
        v2::ContentBlock::Text(text) => text.text.clone(),
        other => format!(
            "[{}]",
            serde_json::to_value(other).unwrap_or_default()["type"]
        ),
    }
}

fn message_content<T: serde::Serialize>(content: &T) -> Option<String> {
    let value = serde_json::to_value(content).ok()?;
    let blocks = value.as_array()?;
    Some(
        blocks
            .iter()
            .map(|block| block["text"].as_str().unwrap_or("[content]"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn event(seq: u64, update: v2::SessionUpdate) -> v2::UpdateSessionNotification {
        let mut meta = v2::Meta::new();
        meta.insert("hive".into(), serde_json::json!({"seq": seq}));
        v2::UpdateSessionNotification::new(v2::SessionId::new("hive-test"), update).meta(meta)
    }

    #[test]
    fn tool_patches_and_chunks_keep_one_structured_call_without_losing_earlier_fields() {
        let mut conversation = Conversation::default();
        conversation.apply(event(
            1,
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new("read-one")
                    .name("read")
                    .title("Read the configuration")
                    .raw_input(serde_json::json!({"path":"config.toml"}))
                    .status(v2::ToolCallStatus::InProgress),
            ),
        ));
        conversation.apply(event(
            2,
            v2::SessionUpdate::ToolCallContentChunk(v2::ToolCallContentChunk::new(
                "read-one",
                "first output",
            )),
        ));
        conversation.apply(event(
            3,
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new("read-one").status(v2::ToolCallStatus::Completed),
            ),
        ));
        assert_eq!(conversation.lines.len(), 1);
        let tool = conversation.lines[0].tool.as_ref().unwrap();
        assert_eq!(tool["name"], "read");
        assert_eq!(tool["rawInput"]["path"], "config.toml");
        assert_eq!(tool["content"][0]["content"]["text"], "first output");
        assert_eq!(tool["status"], "completed");
        conversation.apply(event(
            4,
            v2::SessionUpdate::ToolCallUpdate(v2::ToolCallUpdate::new("read-one").content(vec![])),
        ));
        assert_eq!(
            conversation.lines[0].tool.as_ref().unwrap()["content"],
            serde_json::json!([])
        );
        assert_eq!(conversation.lines[0].revision, 4);
    }

    #[test]
    fn translated_v1_tool_patches_preserve_titles_inputs_and_unescaped_output() {
        let mut conversation = Conversation::default();
        for (index, tool) in [
            serde_json::json!({"toolCallId":"call", "title":"Read README.md", "kind":"read", "rawInput":{"path":"README.md"}}),
            serde_json::json!({"toolCallId":"call", "status":"completed", "content":[{"type":"content", "content":{"type":"text", "text":"# Heading\n\n**answer**"}}]}),
        ].into_iter().enumerate() {
            conversation.apply(event(index as u64 + 1, v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                "_hive_v1_tool_update", BTreeMap::from([("tool".into(), tool)]),
            ))));
        }
        assert_eq!(conversation.lines.len(), 1);
        let tool = conversation.lines[0].tool.as_ref().unwrap();
        assert_eq!(tool["title"], "Read README.md");
        assert_eq!(
            tool["content"][0]["content"]["text"],
            "# Heading\n\n**answer**"
        );
    }

    #[test]
    fn a_usage_snapshot_ignores_old_replay_and_accepts_a_new_authoritative_cost() {
        let mut conversation = Conversation::default();
        conversation.usage_snapshot(
            &serde_json::json!({"turns": 1, "tokens": {"totalTokens": 30}}),
            3,
        );
        conversation.apply(event(
            3,
            v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                "_hive_usage",
                BTreeMap::from([("session".into(), serde_json::json!({"turns": 0}))]),
            )),
        ));
        assert_eq!(conversation.usage["turns"], 1);
        conversation.apply(event(
            4,
            v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                "_hive_usage",
                BTreeMap::from([(
                    "session".into(),
                    serde_json::json!({
                        "turns": 1, "actualSessionCost": {"amount": 0.04, "currency": "USD"}
                    }),
                )]),
            )),
        ));
        assert_eq!(conversation.usage["actualSessionCost"]["amount"], 0.04);
    }

    #[test]
    fn native_plans_replace_ordered_entries_and_cannot_regress_the_attachment_snapshot() {
        let mut conversation = Conversation::default();
        let plan = |entries| serde_json::json!({"plan":{"type":"items","planId":"steps","entries":entries,"_meta":{"plan":true}},"_meta":{"update":true}});
        let first = plan(
            serde_json::json!([{"content":"A","priority":"high","status":"pending"},{"content":"B","priority":"future","status":"in_progress","_meta":{"entry":true}}]),
        );
        let second = plan(
            serde_json::json!([first["plan"]["entries"][1],{"content":"A","priority":"low","status":"cancelled"}]),
        );
        for (seq, value) in [(1, first), (2, second.clone())] {
            conversation.apply(event(
                seq,
                v2::SessionUpdate::PlanUpdate(serde_json::from_value(value).unwrap()),
            ));
        }
        assert_eq!(conversation.plan_snapshot["plans"]["steps"], second);
        assert!(!conversation.plans_authoritative);
        let current = serde_json::json!({"version":1,"seq":5,"support":"supported","observation":"observed","plans":{"steps":plan(serde_json::json!([]))}});
        conversation.plans_snapshot(&current);
        assert!(conversation.plans_authoritative);
        conversation.apply(event(
            3,
            v2::SessionUpdate::PlanUpdate(serde_json::from_value(second).unwrap()),
        ));
        assert_eq!(conversation.plan_snapshot, current);
        conversation.apply(event(
            4,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate::new())),
        ));
        conversation.apply(event(
            5,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate::new())),
        ));
        let unknown =
            serde_json::json!({"plan":{"type":"_future","planId":"other","nested":[2,1]}});
        conversation.apply(event(
            6,
            v2::SessionUpdate::PlanUpdate(serde_json::from_value(unknown.clone()).unwrap()),
        ));
        conversation.plans_snapshot(&current);
        assert_eq!(conversation.plan_snapshot["plans"]["other"], unknown);
        assert_eq!(
            conversation.plan_snapshot["plans"]["steps"]["plan"]["entries"],
            serde_json::json!([])
        );
        assert_eq!(conversation.plan_seq, 6);
        assert!(!conversation.running);
        assert!(!conversation.gap);
    }

    #[test]
    fn full_transcript_entries_share_a_boundary_without_consuming_live_sequences() {
        let mut conversation = Conversation::default();
        conversation.begin_transcript(2048);
        for (id, text) in [("first", "one"), ("second", "two")] {
            let mut notification = event(
                2048,
                v2::SessionUpdate::AgentMessage(
                    v2::AgentMessage::new(v2::MessageId::new(id)).content(vec![text.into()]),
                ),
            );
            notification.meta.as_mut().unwrap().get_mut("hive").unwrap()["snapshot"] = true.into();
            assert!(conversation.apply(notification));
        }
        assert_eq!(conversation.seq, 2048);
        assert!(conversation.apply(event(
            2049,
            v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                "!".into(),
                v2::MessageId::new("second")
            ))
        )));
        assert_eq!(conversation.lines.len(), 2);
        assert_eq!(conversation.lines[1].text, "Agent: two!");
        assert!(!conversation.gap);
    }

    #[test]
    fn a_snapshot_from_an_unacknowledged_attachment_is_discarded() {
        let mut conversation = Conversation::default();
        let mut notification = event(
            2048,
            v2::SessionUpdate::AgentMessage(
                v2::AgentMessage::new(v2::MessageId::new("stale")).content(vec!["discard".into()]),
            ),
        );
        notification.meta.as_mut().unwrap().get_mut("hive").unwrap()["snapshot"] = true.into();
        assert!(!conversation.apply(notification));
        assert!(conversation.lines.is_empty());
        assert_eq!(conversation.seq, 0);
    }

    #[test]
    fn replayed_chunks_are_not_rendered_twice() {
        let mut conversation = Conversation::default();
        let update = event(
            1,
            v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                "hello".into(),
                v2::MessageId::new("message"),
            )),
        );
        assert!(conversation.apply(update.clone()));
        assert!(!conversation.apply(update));
        assert_eq!(conversation.lines[0].text, "Agent: hello");
        assert_eq!(conversation.lines[0].kind, LineKind::Agent);
        assert_eq!(conversation.seq, 1);
    }

    #[test]
    fn configuration_snapshots_ignore_older_replay_and_errors_remain_readable_after_idle() {
        let mut conversation = Conversation::default();
        let options = |value| {
            vec![v2::SessionConfigOption::select(
                v2::SessionConfigId::new("model"),
                "Model",
                v2::SessionConfigValueId::new(value),
                vec![v2::SessionConfigSelectOption::new(
                    v2::SessionConfigValueId::new(value),
                    "Model",
                )],
            )]
        };
        conversation.config_snapshot(options("new"), 5);
        conversation.apply(event(
            1,
            v2::SessionUpdate::ConfigOptionUpdate(v2::ConfigOptionUpdate::new(options("old"))),
        ));
        assert_eq!(conversation.config_seq, 5);
        assert_eq!(
            serde_json::to_value(&conversation.config_options).unwrap()[0]["currentValue"],
            "new"
        );
        let failure =
            hive_protocol::AgentFailure::classify("This API key has expired. sk-do-not-echo");
        conversation.apply(event(2, failure.update("acp-v1")));
        conversation.apply(event(
            3,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate::new())),
        ));
        assert_eq!(conversation.lines.len(), 1);
        assert!(conversation.lines[0].text.contains("Create a new API key"));
        assert!(!conversation.lines[0].text.contains("sk-do-not-echo"));
        assert!(!conversation.running);
    }

    #[test]
    fn resumed_active_work_ends_only_after_idle() {
        let mut conversation = Conversation {
            running: true,
            ..Conversation::default()
        };
        conversation.apply(event(
            4,
            v2::SessionUpdate::AgentMessageChunk(v2::ContentChunk::new(
                "working".into(),
                v2::MessageId::new("message"),
            )),
        ));
        assert!(conversation.running);
        conversation.apply(event(
            5,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(v2::IdleStateUpdate::new())),
        ));
        assert!(!conversation.running);
    }

    #[test]
    fn a_handoff_updates_generation_without_exposing_a_token() {
        let mut conversation = Conversation::default();
        conversation.apply(event(
            1,
            v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                "_hive_control",
                BTreeMap::from([
                    ("generation".into(), serde_json::json!(2)),
                    ("pilot".into(), serde_json::json!(false)),
                ]),
            )),
        ));
        assert_eq!(conversation.generation, 2);
        assert!(!conversation.pilot_present);
    }

    #[test]
    fn a_snapshot_and_late_task_reconcile_without_changing_idle_or_duplicating_calls() {
        let mut conversation = Conversation::default();
        conversation.snapshot(
            &serde_json::json!({"version":1,"seq":10,"foreground":"idle",
            "tools":{"call":{"title":"Compile","status":"completed"}},"plans":{},"tasks":{},
            "support":{"tools":"observed","plans":"unknown","tasks":"unknown"}}),
        );
        conversation.seq = 10;
        assert!(conversation.apply(event(
            11,
            v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
                "_hive_activity",
                BTreeMap::from([
                    ("kind".into(), serde_json::json!("task")),
                    ("taskId".into(), serde_json::json!("background")),
                    ("task".into(), serde_json::json!({"status":"completed"})),
                ]),
            ))
        )));
        assert_eq!(conversation.activity["foreground"], "idle");
        assert_eq!(
            conversation.activity["tasks"]["background"]["status"],
            "completed"
        );
        assert_eq!(
            conversation.activity["tools"]["call"]["status"],
            "completed"
        );
        assert!(!conversation.apply(event(
            13,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(v2::RunningStateUpdate::new()))
        )));
        assert!(conversation.gap);
        assert!(!conversation.running);
    }
}
