use std::collections::HashMap;

use agent_client_protocol::schema::v2;

#[derive(Default)]
pub struct Conversation {
    pub lines: Vec<String>,
    messages: HashMap<String, usize>,
    tools: HashMap<String, usize>,
    pub seq: u64,
    pub running: bool,
    pub generation: u64,
    pub pilot_present: bool,
}

impl Conversation {
    fn upsert(&mut self, key: String, label: &str, text: &str, replace: bool, tool: bool) {
        let indexes = if tool {
            &mut self.tools
        } else {
            &mut self.messages
        };
        let index = *indexes.entry(key).or_insert_with(|| {
            self.lines.push(format!("{label}: "));
            self.lines.len() - 1
        });
        if replace {
            self.lines[index] = format!("{label}: {text}");
        } else {
            self.lines[index].push_str(text);
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
        // Replay and live events share sequence numbers; only advance after reducing the event.
        match notification.update {
            v2::SessionUpdate::AgentMessageChunk(chunk) => self.upsert(
                format!("agent:{}", chunk.message_id),
                "Agent",
                &content_text(&chunk.content),
                false,
                false,
            ),
            v2::SessionUpdate::UserMessageChunk(chunk) => self.upsert(
                format!("user:{}", chunk.message_id),
                "You",
                &content_text(&chunk.content),
                false,
                false,
            ),
            v2::SessionUpdate::AgentMessage(message) => {
                if let Some(text) = message_content(&message.content) {
                    self.upsert(
                        format!("agent:{}", message.message_id),
                        "Agent",
                        &text,
                        true,
                        false,
                    );
                }
            }
            v2::SessionUpdate::UserMessage(message) => {
                if let Some(text) = message_content(&message.content) {
                    self.upsert(
                        format!("user:{}", message.message_id),
                        "You",
                        &text,
                        true,
                        false,
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
                    "Tool",
                    &format!("{title} — {status} {}", value["content"]),
                    true,
                    true,
                );
            }
            v2::SessionUpdate::ToolCallContentChunk(chunk) => {
                let value = serde_json::to_value(&chunk).unwrap_or_default();
                self.upsert(
                    value["toolCallId"].to_string(),
                    "Tool",
                    &value.to_string(),
                    false,
                    true,
                );
            }
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Running(_)) => self.running = true,
            v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(idle)) => {
                self.running = false;
                self.lines
                    .push(format!("Turn ended: {:?}", idle.stop_reason));
            }
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_control" => {
                self.generation = other.fields["generation"]
                    .as_u64()
                    .unwrap_or(self.generation);
                self.pilot_present = other.fields["pilot"].as_bool().unwrap_or(false);
            }
            v2::SessionUpdate::Other(other)
                if other.session_update.starts_with("_hive_v1_tool") =>
            {
                let value = &other.fields["tool"];
                let id = value["toolCallId"].as_str().unwrap_or("tool");
                self.upsert(id.to_string(), "Tool", &value.to_string(), true, true);
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
        assert_eq!(conversation.lines, ["Agent: hello"]);
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
}
