//! Conversation projection independent of the bounded incremental transport log.

use std::collections::HashMap;

use agent_client_protocol::{Error, schema::v2};
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error(
        "hive::transcript::unsupported: ACPv1 agent cannot replay history; use an agent advertising session/load, or attach without earlier history"
    )]
    Unsupported,
    #[error(
        "hive::transcript::operation_rejected: ACP {protocol} {method} was advertised but is not implemented; check the agent's protocol implementation"
    )]
    Rejected {
        protocol: &'static str,
        method: &'static str,
    },
    #[error(
        "hive::transcript::session_missing: saved session not found by ACP {protocol} {method}; verify the original agent's retained sessions and cwd, then restart Hive"
    )]
    Missing {
        protocol: &'static str,
        method: &'static str,
    },
    #[error(
        "hive::transcript::replay_failed: ACP {protocol} {method} could not restore retained history ({code}); check the agent's replay support, retained session and availability, then restart Hive"
    )]
    Failed {
        protocol: &'static str,
        method: &'static str,
        code: i32,
    },
}

impl RestoreError {
    pub fn downstream(protocol: &'static str, method: &'static str, error: &Error) -> Self {
        if i32::from(error.code) == -32601 {
            return Self::Rejected { protocol, method };
        }
        // Classify known missing-session text without exposing arbitrary agent error payloads.
        if error
            .data
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("unknown session") || text.contains("not found"))
        {
            Self::Missing { protocol, method }
        } else {
            Self::Failed {
                protocol,
                method,
                code: error.code.into(),
            }
        }
    }
}

#[derive(Default)]
pub struct Transcript {
    entries: Vec<Value>,
    indices: HashMap<String, usize>,
    limited: bool,
}

impl Transcript {
    pub fn limited(&self) -> bool {
        self.limited
    }
    pub fn has_messages(&self) -> bool {
        self.indices
            .keys()
            .any(|key| key.starts_with("user_message:") || key.starts_with("agent_message:"))
    }

    pub fn ingest(&mut self, update: &v2::SessionUpdate) -> agent_client_protocol::Result<()> {
        let mut value = serde_json::to_value(update).map_err(Error::into_internal_error)?;
        let kind = value["sessionUpdate"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let message = matches!(
            kind.as_str(),
            "user_message"
                | "agent_message"
                | "user_message_chunk"
                | "agent_message_chunk"
                | "agent_thought"
                | "agent_thought_chunk"
        );
        let tool = matches!(
            kind.as_str(),
            "tool_call_update"
                | "tool_call_content_chunk"
                | "_hive_v1_tool_call"
                | "_hive_v1_tool_update"
        );
        if !message && !tool {
            // These entities carry history that this projection cannot yet reconstruct.
            if matches!(
                kind.as_str(),
                "terminal_update"
                    | "terminal_output_chunk"
                    | "compaction"
                    | "compaction_update"
                    | "compaction_content_chunk"
            ) {
                self.limited = true;
            }
            if matches!(kind.as_str(), "_hive_error" | "_hive_opencode_error") {
                self.entries.push(value);
            }
            return Ok(());
        }
        let chunk = kind.ends_with("_chunk");
        // Normalize chunks to full-message upserts; speaker namespaces prevent user/agent ID collisions.
        let (key, target) = if message {
            let speaker = if kind.starts_with("agent_thought") {
                "agent_thought"
            } else if kind.starts_with("user") {
                "user_message"
            } else {
                "agent_message"
            };
            (format!("{speaker}:{}", value["messageId"]), speaker)
        } else {
            if kind.starts_with("_hive_v1") {
                // V1 tool updates are wrapped extensions; merge their payload, not the wrapper.
                value = value["tool"].clone();
            }
            (
                format!("tool:{}", value["toolCallId"]),
                if kind.starts_with("_hive_v1") {
                    "_hive_v1_tool_call"
                } else {
                    "tool_call_update"
                },
            )
        };
        // Keep the first-appearance slot when later patches replace content or complete a tool.
        let index = *self.indices.entry(key).or_insert_with(|| {
            self.entries.push(if message {
                json!({"sessionUpdate": target, "messageId": value["messageId"], "content": []})
            } else if target.starts_with("_hive_v1") {
                json!({"sessionUpdate": target, "tool": {"toolCallId": value["toolCallId"]}})
            } else {
                json!({"sessionUpdate": target, "toolCallId": value["toolCallId"]})
            });
            self.entries.len() - 1
        });
        let stored = if target.starts_with("_hive_v1") {
            &mut self.entries[index]["tool"]
        } else {
            &mut self.entries[index]
        };
        if chunk {
            if !stored["content"].is_array() {
                stored["content"] = json!([]);
            }
            let content = stored["content"].as_array_mut().expect("initialized array");
            let block = value["content"].clone();
            // Coalesce token-sized text chunks so memory scales with content, not token count.
            if let Some(last) = content.last_mut()
                && compatible_text(last, &block)
                && let Some(next) = block["text"].as_str()
                && let Value::String(previous) = &mut last["text"]
            {
                previous.push_str(next);
            } else {
                content.push(block);
            }
        } else if let Some(fields) = value.as_object() {
            // Omitted fields are patches, not a request to erase earlier content.
            for (key, field) in fields {
                if key == "_meta" && field.is_object() {
                    if !stored[key].is_object() {
                        stored[key] = json!({});
                    }
                    for (name, value) in field.as_object().expect("checked object") {
                        if value.is_null() {
                            stored[key]
                                .as_object_mut()
                                .expect("initialized object")
                                .remove(name);
                        } else {
                            stored[key][name] = value.clone();
                        }
                    }
                } else if key != "sessionUpdate" {
                    stored[key] = field.clone();
                }
            }
        }
        Ok(())
    }

    pub fn updates(&self) -> agent_client_protocol::Result<Vec<v2::SessionUpdate>> {
        self.entries
            .iter()
            .cloned()
            .map(|entry| serde_json::from_value(entry).map_err(Error::into_internal_error))
            .collect()
    }
}

fn compatible_text(previous: &Value, next: &Value) -> bool {
    if previous["type"] != "text" || next["type"] != "text" {
        return false;
    }
    // Coalescing must preserve annotations and extension metadata on content blocks.
    let previous = previous.as_object().expect("text block");
    let next = next.as_object().expect("text block");
    previous.len() == next.len()
        && previous
            .iter()
            .all(|(key, value)| key == "text" || next.get(key) == Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingest(transcript: &mut Transcript, value: Value) {
        transcript
            .ingest(&serde_json::from_value(value).unwrap())
            .unwrap();
    }

    #[test]
    fn more_than_a_ring_of_chunks_and_messages_retains_every_conversation_prefix() {
        let mut transcript = Transcript::default();
        for i in 0..300 {
            ingest(
                &mut transcript,
                json!({"sessionUpdate":"agent_message_chunk", "messageId":"one", "content":{"type":"text", "text":"é"}}),
            );
            ingest(
                &mut transcript,
                json!({"sessionUpdate":"user_message", "messageId":format!("user-{i}"), "content":[{"type":"text", "text":i.to_string()}]}),
            );
        }
        let updates = serde_json::to_value(transcript.updates().unwrap()).unwrap();
        assert_eq!(updates.as_array().unwrap().len(), 301);
        assert_eq!(
            updates[0]["content"],
            json!([{"type":"text", "text":"é".repeat(300)}])
        );
        assert_eq!(updates[1]["content"][0]["text"], "0");
        assert_eq!(updates[300]["content"][0]["text"], "299");
    }

    #[test]
    fn thought_upserts_replace_chunks_without_colliding_with_assistant_messages() {
        let mut transcript = Transcript::default();
        for value in [
            json!({"sessionUpdate":"agent_thought_chunk", "messageId":"same", "content":{"type":"text", "text":"draft"}}),
            json!({"sessionUpdate":"agent_message", "messageId":"same", "content":[{"type":"text", "text":"answer"}]}),
            json!({"sessionUpdate":"agent_thought", "messageId":"same", "content":[{"type":"text", "text":"revised reasoning"}]}),
        ] {
            ingest(&mut transcript, value);
        }
        let updates = serde_json::to_value(transcript.updates().unwrap()).unwrap();
        assert_eq!(updates.as_array().unwrap().len(), 2);
        assert_eq!(updates[0]["sessionUpdate"], "agent_thought");
        assert_eq!(updates[0]["content"][0]["text"], "revised reasoning");
        assert_eq!(updates[1]["content"][0]["text"], "answer");
    }

    #[test]
    fn omitted_terminal_history_marks_the_projection_as_limited() {
        let mut transcript = Transcript::default();
        ingest(
            &mut transcript,
            json!({"sessionUpdate":"terminal_update", "terminalId":"term"}),
        );
        assert!(transcript.limited());
    }

    #[test]
    fn full_message_patches_replace_content_without_reordering_or_erasing_omitted_fields() {
        let mut transcript = Transcript::default();
        ingest(
            &mut transcript,
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"one", "content":{"type":"text", "text":"draft"}}),
        );
        ingest(
            &mut transcript,
            json!({"sessionUpdate":"user_message", "messageId":"two", "content":[{"type":"text", "text":"next"}]}),
        );
        ingest(
            &mut transcript,
            json!({"sessionUpdate":"agent_message", "messageId":"one", "content":[{"type":"image", "data":"AA==", "mimeType":"image/png"}, {"type":"text", "text":"final"}]}),
        );
        ingest(
            &mut transcript,
            json!({"sessionUpdate":"agent_message", "messageId":"one", "_meta":{"reviewed":true}}),
        );
        ingest(
            &mut transcript,
            json!({"sessionUpdate":"agent_message_chunk", "messageId":"one", "content":{"type":"text", "text":"!"}}),
        );
        let updates = serde_json::to_value(transcript.updates().unwrap()).unwrap();
        assert_eq!(updates[0]["messageId"], "one");
        assert_eq!(updates[0]["content"][0]["type"], "image");
        assert_eq!(updates[0]["content"][1]["text"], "final!");
        assert_eq!(updates[0]["_meta"]["reviewed"], true);
        ingest(
            &mut transcript,
            json!({"sessionUpdate":"agent_message", "messageId":"one", "content":null}),
        );
        assert!(
            serde_json::to_value(transcript.updates().unwrap()).unwrap()[0]["content"].is_null()
        );
    }

    #[test]
    fn historical_tool_patches_retain_fields_and_append_content_without_an_execution_path() {
        let mut transcript = Transcript::default();
        for update in [
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new("call")
                    .title("Read file")
                    .status(v2::ToolCallStatus::InProgress),
            ),
            v2::SessionUpdate::ToolCallContentChunk(v2::ToolCallContentChunk::new(
                "call", "output",
            )),
            v2::SessionUpdate::ToolCallUpdate(
                v2::ToolCallUpdate::new("call").status(v2::ToolCallStatus::Completed),
            ),
        ] {
            transcript.ingest(&update).unwrap();
        }
        let updates = serde_json::to_value(transcript.updates().unwrap()).unwrap();
        assert_eq!(updates.as_array().unwrap().len(), 1);
        assert_eq!(updates[0]["title"], "Read file");
        assert_eq!(updates[0]["status"], "completed");
        assert_eq!(updates[0]["content"][0]["content"]["text"], "output");
    }
}
