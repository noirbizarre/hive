//! A session-owned projection of downstream execution activity.

use std::collections::{BTreeMap, VecDeque};

use agent_client_protocol::schema::{v1, v2};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Activity {
    pub version: u8,
    pub seq: u64,
    pub foreground: String,
    pub session: String,
    pub tools: BTreeMap<String, Value>,
    pub plans: BTreeMap<String, Value>,
    pub tasks: BTreeMap<String, Value>,
    pub support: BTreeMap<String, &'static str>,
    #[serde(skip)]
    finished_tools: VecDeque<String>,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            version: 1,
            foreground: "unknown".into(),
            session: "active".into(),
            support: BTreeMap::from([
                ("tools".into(), "unknown"),
                ("plans".into(), "unknown"),
                ("tasks".into(), "unknown"),
            ]),
            ..Self::default()
        }
    }

    pub fn v1(&mut self, update: &v1::SessionUpdate) -> Option<Value> {
        match update {
            v1::SessionUpdate::ToolCall(call) => self.tool(serde_json::to_value(call).ok()?, true),
            v1::SessionUpdate::ToolCallUpdate(call) => {
                self.tool(serde_json::to_value(call).ok()?, false)
            }
            v1::SessionUpdate::Plan(plan) => {
                self.support.insert("plans".into(), "observed");
                let mut value = serde_json::to_value(plan).ok()?;
                value.as_object_mut()?.remove("_meta");
                self.plans.insert("v1-plan".into(), value.clone());
                Some(
                    json!({"kind": "plan", "planId": "v1-plan", "plan": value, "source": "acp-v1"}),
                )
            }
            _ => None,
        }
    }

    pub fn v2(&mut self, update: &v2::SessionUpdate) -> Option<Value> {
        match update {
            v2::SessionUpdate::ToolCallUpdate(call) => {
                self.tool(serde_json::to_value(call).ok()?, false)
            }
            v2::SessionUpdate::ToolCallContentChunk(chunk) => {
                let value = serde_json::to_value(chunk).ok()?;
                let id = value.get("toolCallId")?.as_str()?.to_owned();
                self.support.insert("tools".into(), "observed");
                let tool = self
                    .tools
                    .entry(id.clone())
                    .or_insert_with(|| json!({"toolCallId": id, "status": "pending"}));
                // Content chunks are append-only; a later full content update replaces them.
                let chunks = tool
                    .as_object_mut()?
                    .entry("chunks")
                    .or_insert_with(|| json!([]));
                let chunks = chunks.as_array_mut()?;
                chunks.push(value);
                if chunks.len() > 64 {
                    chunks.remove(0);
                    tool["chunksTruncated"] = json!(true);
                }
                Some(json!({"kind": "tool", "toolCallId": id, "tool": tool, "source": "acp-v2"}))
            }
            v2::SessionUpdate::PlanUpdate(plan) => {
                let v2::PlanUpdateContent::Items(items) = &plan.plan else {
                    return None;
                };
                self.support.insert("plans".into(), "observed");
                let id = items.plan_id.to_string();
                let mut value = serde_json::to_value(items).ok()?;
                value.as_object_mut()?.remove("_meta");
                self.plans.insert(id.clone(), value.clone());
                Some(json!({"kind": "plan", "planId": id, "plan": value, "source": "acp-v2"}))
            }
            v2::SessionUpdate::StateUpdate(state) => {
                let value = serde_json::to_value(state).ok()?;
                let status = value.get("state")?.as_str()?;
                if !matches!(status, "running" | "idle" | "requires_action") {
                    return None;
                }
                self.foreground = status.into();
                Some(json!({"kind": "foreground", "state": value, "source": "acp-v2"}))
            }
            // This extension is accepted only with a stable task ID. Unknown provider
            // extensions remain in the raw event stream rather than inventing tasks.
            v2::SessionUpdate::Other(other) if other.session_update == "_hive_task" => {
                let id = other.fields.get("taskId")?.as_str()?.to_owned();
                self.support.insert("tasks".into(), "observed");
                let value = serde_json::to_value(&other.fields).ok()?;
                let task = self
                    .tasks
                    .entry(id.clone())
                    .or_insert_with(|| json!({"taskId": id}));
                patch(task, &value);
                Some(json!({"kind": "task", "taskId": id, "task": task, "source": "extension"}))
            }
            _ => None,
        }
    }

    fn tool(&mut self, value: Value, initial: bool) -> Option<Value> {
        let id = value.get("toolCallId")?.as_str()?.to_owned();
        self.support.insert("tools".into(), "observed");
        let tool = self
            .tools
            .entry(id.clone())
            .or_insert_with(|| json!({"toolCallId": id, "status": "pending"}));
        if initial {
            *tool = json!({"toolCallId": id, "status": "pending"});
        }
        patch(tool, &value);
        let result = tool.clone();
        if matches!(
            tool["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) {
            self.finished_tools.retain(|previous| previous != &id);
            self.finished_tools.push_back(id.clone());
            while self.finished_tools.len() > 128 {
                if let Some(oldest) = self.finished_tools.pop_front() {
                    self.tools.remove(&oldest);
                }
            }
        } else {
            self.finished_tools.retain(|previous| previous != &id);
        }
        // A missing status in an ACPv2 patch must not reset a running call.
        Some(json!({"kind": "tool", "toolCallId": id, "tool": result,
            "source": if initial { "acp-v1" } else { "acp" }}))
    }
}

fn patch(target: &mut Value, update: &Value) {
    let (Some(existing), Some(fields)) = (target.as_object_mut(), update.as_object()) else {
        return;
    };
    for (key, value) in fields {
        // Never persist credentials or private provider metadata in a shared snapshot.
        if key == "_meta" {
            continue;
        }
        if value.is_null() {
            existing.remove(key);
        } else {
            existing.insert(key.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_patch_retains_previous_fields_and_preserves_failures() {
        let mut activity = Activity::new();
        activity.tool(
            json!({"toolCallId":"call", "title":"Compile", "status":"in_progress"}),
            false,
        );
        activity.tool(json!({"toolCallId":"call", "status":"failed", "rawOutput":{"error":"compiler failed"}}), false);
        assert_eq!(activity.tools["call"]["title"], "Compile");
        assert_eq!(activity.tools["call"]["status"], "failed");
        assert_eq!(
            activity.tools["call"]["rawOutput"]["error"],
            "compiler failed"
        );
        assert_eq!(activity.support["plans"], "unknown");
    }

    #[test]
    fn a_late_task_does_not_reopen_the_foreground_turn() {
        let mut activity = Activity::new();
        activity.v2(&v2::SessionUpdate::StateUpdate(v2::StateUpdate::Idle(
            v2::IdleStateUpdate::new(),
        )));
        activity.v2(&v2::SessionUpdate::Other(v2::OtherSessionUpdate::new(
            "_hive_task",
            BTreeMap::from([
                ("taskId".into(), json!("background")),
                ("status".into(), json!("completed")),
            ]),
        )));
        assert_eq!(activity.foreground, "idle");
        assert_eq!(activity.tasks["background"]["status"], "completed");
        assert_eq!(activity.support["tasks"], "observed");
    }

    #[test]
    fn a_v1_plan_and_tool_failure_keep_their_native_statuses() {
        let mut activity = Activity::new();
        let plan = v1::SessionUpdate::Plan(v1::Plan::new(vec![v1::PlanEntry::new(
            "Check files",
            v1::PlanEntryPriority::High,
            v1::PlanEntryStatus::InProgress,
        )]));
        assert_eq!(activity.v1(&plan).unwrap()["planId"], "v1-plan");
        let call = v1::SessionUpdate::ToolCall(v1::ToolCall::new("call", "Check files"));
        activity.v1(&call);
        activity.v1(&v1::SessionUpdate::ToolCallUpdate(v1::ToolCallUpdate::new(
            "call",
            v1::ToolCallUpdateFields::new().status(v1::ToolCallStatus::Failed),
        )));
        assert_eq!(activity.tools["call"]["status"], "failed");
        assert_eq!(activity.tools["call"]["title"], "Check files");
        assert_eq!(
            activity.plans["v1-plan"]["entries"][0]["status"],
            "in_progress"
        );
    }
}
