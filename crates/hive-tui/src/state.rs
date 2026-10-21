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
}

#[derive(Default)]
pub struct Conversation {
    pub lines: Vec<ConversationLine>,
    messages: HashMap<String, usize>,
    tools: HashMap<String, usize>,
    pub seq: u64,
    pub running: bool,
    pub generation: u64,
    pub pilot_present: bool,
    pub activity: serde_json::Value,
    pub activity_seq: u64,
    pub gap: bool,
}

impl Conversation {
    pub fn snapshot(&mut self, value: &serde_json::Value) {
        let seq = value["seq"].as_u64().unwrap_or(0);
        if seq >= self.activity_seq && value["version"] == 1 {
            self.activity = value.clone();
            self.activity_seq = seq;
        }
    }

    pub fn push_user(&mut self, text: String) {
        self.lines.push(ConversationLine {
            kind: LineKind::User,
            text: format!("You: {text}"),
        });
    }

    pub fn push_notice(&mut self, text: String) {
        self.lines.push(ConversationLine {
            kind: LineKind::Notice,
            text,
        });
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
            });
            self.lines.len() - 1
        });
        if replace {
            self.lines[index].text = format!("{label}: {text}");
        } else {
            self.lines[index].text.push_str(text);
        }
    }

    pub fn apply(&mut self, notification: v2::UpdateSessionNotification) -> bool {
        let seq = notification
            .meta
            .as_ref()
            .and_then(|m| m.get("hive"))
            .and_then(|h| h.get("seq"))
            .and_then(serde_json::Value::as_u64);
        let Some(seq) = seq else {
            return false;
        };
        if seq <= self.seq {
            return false;
        }
        if self.seq != 0 && seq != self.seq + 1 {
            self.gap = true;
            return false;
        }
        // Replay and live events share sequence numbers; only advance after reducing the event.
        match notification.update {
            v2::SessionUpdate::AgentMessageChunk(chunk) => self.upsert(
                format!("agent:{}", chunk.message_id),
                LineKind::Agent,
                &content_text(&chunk.content),
                false,
            ),
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
                let value = serde_json::to_value(&tool).unwrap_or_default();
                let id = value["toolCallId"].as_str().unwrap_or("tool");
                let title = value["title"].as_str().unwrap_or("Tool");
                let status = value["status"].as_str().unwrap_or("working");
                self.upsert(
                    id.to_string(),
                    LineKind::Tool,
                    &format!("{title} — {status} {}", value["content"]),
                    true,
                );
            }
            v2::SessionUpdate::ToolCallContentChunk(chunk) => {
                let value = serde_json::to_value(&chunk).unwrap_or_default();
                self.upsert(
                    value["toolCallId"].to_string(),
                    LineKind::Tool,
                    &value.to_string(),
                    false,
                );
            }
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => self.running = true,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                self.running = false;
                self.push_notice(format!("Turn ended: {:?}", idle.stop_reason));
            }
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_control" => {
                self.generation = other.fields["generation"]
                    .as_u64()
                    .unwrap_or(self.generation);
                self.pilot_present = other.fields["pilot"].as_bool().unwrap_or(false);
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
                let value = &other.fields["tool"];
                let id = value["toolCallId"].as_str().unwrap_or("tool");
                self.upsert(id.to_string(), LineKind::Tool, &value.to_string(), true);
            }
            _ => {}
        }
        self.seq = seq;
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
