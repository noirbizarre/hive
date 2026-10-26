//! Configuration is agent-owned; Hive caches a snapshot and fences edits like prompt admission.

use super::*;

const LEGACY_MODE: &str = "_hive_legacy_mode";

/// Native configuration takes precedence over the deprecated modes surface as a whole.
#[derive(Default)]
pub(super) struct Legacy {
    native: Vec<v2::SessionConfigOption>,
    modes: Option<v1::SessionModeState>,
    revision: u64,
}

pub(super) struct Discovery {
    pub options: Option<Vec<v2::SessionConfigOption>>,
    pub modes: Option<v1::SessionModeState>,
}

impl Discovery {
    pub fn v1(
        options: Option<Vec<v1::SessionConfigOption>>,
        modes: Option<v1::SessionModeState>,
    ) -> agent_client_protocol::Result<Self> {
        Ok(Self {
            options: options.map(from_v1).transpose()?,
            modes,
        })
    }
}

impl Legacy {
    pub fn discover(
        &mut self,
        discovery: Discovery,
    ) -> agent_client_protocol::Result<Vec<v2::SessionConfigOption>> {
        if let Some(options) = discovery.options {
            self.native = options;
            // A source change must also fence an in-flight legacy acknowledgement.
            self.revision += 1;
        }
        if let Some(modes) = discovery.modes {
            self.modes = Some(modes);
            self.revision += 1;
        }
        self.project()
    }

    fn active(&self) -> bool {
        // Pi's legacy modes duplicate thinking level, so even non-mode native options win.
        !self.native.iter().any(|option| {
            let value = serde_json::to_value(option).unwrap_or_default();
            value["type"] == "boolean"
                || value["type"] == "select"
                    && value["options"].as_array().is_some_and(|options| {
                        options.iter().any(|choice| {
                            choice["value"].is_string()
                                || choice["options"].as_array().is_some_and(|items| {
                                    items.iter().any(|item| item["value"].is_string())
                                })
                        })
                    })
        }) && !self
            .native
            .iter()
            .any(|option| option.config_id.to_string() == LEGACY_MODE)
    }

    pub fn route(&self, request: &v2::SetSessionConfigOptionRequest) -> Option<u64> {
        (self.active() && request.config_id.to_string() == LEGACY_MODE).then_some(self.revision)
    }

    pub fn observe(
        &mut self,
        mode: v1::SessionModeId,
    ) -> agent_client_protocol::Result<Vec<v2::SessionConfigOption>> {
        // Repeated observations still supersede a setter's empty acknowledgement.
        self.revision += 1;
        if let Some(modes) = &mut self.modes {
            modes.current_mode_id = mode;
        }
        self.project()
    }

    pub fn acknowledge(
        &mut self,
        revision: u64,
        mode: v1::SessionModeId,
    ) -> agent_client_protocol::Result<Vec<v2::SessionConfigOption>> {
        if self.active() && self.revision == revision {
            self.observe(mode)
        } else {
            self.project()
        }
    }

    fn project(&self) -> agent_client_protocol::Result<Vec<v2::SessionConfigOption>> {
        let mut options = self.native.clone();
        if self.active()
            && let Some(modes) = &self.modes
            && !modes.available_modes.is_empty()
        {
            let choices: Vec<_> = modes.available_modes.iter().map(|mode| serde_json::json!({
                "value": mode.id, "name": mode.name, "description": mode.description, "_meta": mode.meta,
            })).collect();
            options.push(serde_json::from_value(serde_json::json!({
                "configId": LEGACY_MODE, "name": "Session mode", "category": "mode", "type": "select",
                "currentValue": modes.current_mode_id, "options": choices, "_meta": modes.meta,
            })).map_err(Error::into_internal_error)?);
        }
        Ok(options)
    }
}

pub(super) fn publish(
    state: &mut Control,
    id: &v2::SessionId,
    options: Vec<v2::SessionConfigOption>,
) -> agent_client_protocol::Result<()> {
    if state.config_options != options {
        forward_locked(
            state,
            id,
            Some(v2::SessionUpdate::ConfigOptionUpdate(
                v2::ConfigOptionUpdate::new(options),
            )),
        )?;
    }
    Ok(())
}

pub(super) fn advertised(
    options: &[v2::SessionConfigOption],
    request: &v2::SetSessionConfigOptionRequest,
) -> bool {
    let Some(option) = options
        .iter()
        .find(|option| option.config_id == request.config_id)
    else {
        return false;
    };
    let option = serde_json::to_value(option).unwrap_or_default();
    let value = serde_json::to_value(&request.value).unwrap_or_default();
    match (option["type"].as_str(), value["type"].as_str()) {
        (Some("boolean"), Some("boolean")) => value["value"].is_boolean(),
        (Some("select"), Some("id")) => option["options"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice["value"] == value["value"]
                    || choice["options"].as_array().is_some_and(|items| {
                        items.iter().any(|item| item["value"] == value["value"])
                    })
            })
        }),
        _ => false,
    }
}

pub(super) fn from_v1(
    options: Vec<v1::SessionConfigOption>,
) -> agent_client_protocol::Result<Vec<v2::SessionConfigOption>> {
    options
        .into_iter()
        .map(|option| {
            let mut value = serde_json::to_value(option).map_err(Error::into_internal_error)?;
            // Rename versioned identities explicitly instead of silently losing them during deserialization.
            if let Some(id) = value.as_object_mut().and_then(|object| object.remove("id")) {
                value["configId"] = id;
            }
            // Provider group identities were renamed too; without this the SDK silently drops grouped choices.
            if value["type"] == "select"
                && let Some(choices) = value["options"].as_array_mut()
            {
                for choice in choices {
                    if let Some(group) = choice
                        .as_object_mut()
                        .and_then(|object| object.remove("group"))
                    {
                        choice["groupId"] = group;
                    }
                }
            }
            serde_json::from_value(value).map_err(Error::into_internal_error)
        })
        .collect()
}

pub(super) fn to_v1(
    request: &v2::SetSessionConfigOptionRequest,
    id: &v1::SessionId,
) -> agent_client_protocol::Result<v1::SetSessionConfigOptionRequest> {
    let value = match &request.value {
        v2::SessionConfigOptionValue::Id { value } => {
            v1::SessionConfigOptionValue::value_id(v1::SessionConfigValueId::new(value.to_string()))
        }
        v2::SessionConfigOptionValue::Boolean { value } => {
            v1::SessionConfigOptionValue::boolean(*value)
        }
        _ => {
            return Err(Error::invalid_params()
                .data("the v1 agent does not support this configuration value type"));
        }
    };
    Ok(v1::SetSessionConfigOptionRequest::new(
        id.clone(),
        v1::SessionConfigId::new(request.config_id.to_string()),
        value,
    )
    .meta(request.meta.clone()))
}

pub(super) struct Change {
    pub request: v2::SetSessionConfigOptionRequest,
    pub generation: u64,
    pub reply:
        Option<oneshot::Sender<agent_client_protocol::Result<v2::SetSessionConfigOptionResponse>>>,
    pub control: Arc<Mutex<Control>>,
}

impl Drop for Change {
    fn drop(&mut self) {
        // A finished guard must not release twice: the caller may already have reserved its next edit.
        if self.reply.is_some()
            && let Ok(mut state) = self.control.lock()
        {
            state.configuring = false;
        }
    }
}

impl Change {
    pub fn dispatch<T: Send + 'static>(
        self,
        request: PreparedRequest<T>,
        id: v2::SessionId,
        reduce: impl FnOnce(
            &mut Control,
            T,
        ) -> agent_client_protocol::Result<Vec<v2::SessionConfigOption>>
        + Send
        + 'static,
    ) -> agent_client_protocol::Result<oneshot::Receiver<()>> {
        let (done, received) = oneshot::channel();
        // Commit at the wire response boundary: later notifications must not overtake task wakeup and be rolled back.
        request.on_receiving_result(async move |result| {
            self.finish_with(&id, |state| {
                result
                    .map_err(|_| failed())
                    .and_then(|response| reduce(state, response))
            });
            let _ = done.send(());
            Ok(())
        })?;
        Ok(received)
    }

    pub fn finish(
        self,
        id: &v2::SessionId,
        result: agent_client_protocol::Result<Vec<v2::SessionConfigOption>>,
    ) {
        self.finish_with(id, |_| result);
    }

    pub fn finish_with(
        mut self,
        id: &v2::SessionId,
        reduce: impl FnOnce(&mut Control) -> agent_client_protocol::Result<Vec<v2::SessionConfigOption>>,
    ) {
        let result = (|| {
            let mut state = self.control.lock().map_err(|_| Error::internal_error())?;
            // Reduce and publish under the same lock so a notification cannot be rolled back between them.
            let options = reduce(&mut state)?;
            if state.config_options != options {
                forward_locked(
                    &mut state,
                    id,
                    Some(v2::SessionUpdate::ConfigOptionUpdate(
                        v2::ConfigOptionUpdate::new(options),
                    )),
                )?;
            }
            Ok(
                v2::SetSessionConfigOptionResponse::new(state.config_options.clone())
                    .meta(state.metadata("")),
            )
        })();
        // Release before waking the caller, so an immediate next prompt cannot race cleanup.
        if let Ok(mut state) = self.control.lock() {
            state.configuring = false;
        }
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(result);
        }
    }
}

pub(super) fn failed() -> Error {
    Error::invalid_request().data("agent configuration failed; check the advertised choices and downstream provider configuration, then retry")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes() -> v1::SessionModeState {
        serde_json::from_value(serde_json::json!({"currentModeId":"build", "availableModes":[
            {"id":"build", "name":"Build", "description":"Execute work", "_meta":{"custom":true}},
            {"id":"plan", "name":"Plan"}
        ]})).unwrap()
    }

    fn native(id: &str, category: &str) -> v2::SessionConfigOption {
        serde_json::from_value(serde_json::json!({"configId":id,"category":category,"name":id,
            "type":"select","currentValue":"default","options":[{"value":"default","name":"Default"},{"value":"plan","name":"Plan"}]})).unwrap()
    }

    #[test]
    fn native_agent_settings_win_without_duplicating_legacy_modes_or_losing_unknown_categories() {
        for options in [
            vec![native("mode", "mode")],
            vec![
                native("mode", "mode"),
                native("collaboration_mode", "collaboration_mode"),
            ],
            vec![
                native("model", "model"),
                native("thought_level", "thought_level"),
            ],
        ] {
            let mut state = Legacy::default();
            assert_eq!(
                state
                    .discover(Discovery {
                        options: Some(options.clone()),
                        modes: Some(modes())
                    })
                    .unwrap(),
                options
            );
            assert_eq!(
                state.observe(v1::SessionModeId::new("plan")).unwrap(),
                options
            );
            // Explicit replacement removes old choices; restoring omitted fields preserves observations.
            let legacy = state
                .discover(Discovery {
                    options: Some(vec![]),
                    modes: None,
                })
                .unwrap();
            assert_eq!(legacy.len(), 1);
            assert_eq!(
                serde_json::to_value(&legacy).unwrap()[0]["currentValue"],
                "plan"
            );
            assert_eq!(
                state
                    .discover(Discovery {
                        options: None,
                        modes: None
                    })
                    .unwrap(),
                legacy
            );
        }
    }

    #[test]
    fn legacy_acknowledgements_cannot_override_newer_observations_or_native_configuration() {
        let mut state = Legacy::default();
        let options = state
            .discover(Discovery {
                options: None,
                modes: Some(modes()),
            })
            .unwrap();
        let value = serde_json::to_value(options).unwrap();
        assert_eq!(value[0]["options"][0]["description"], "Execute work");
        assert_eq!(value[0]["options"][0]["_meta"]["custom"], true);
        let revision = state.revision;
        // Even a repeated current value is newer evidence than dispatch.
        state.observe(v1::SessionModeId::new("build")).unwrap();
        let options = state
            .acknowledge(revision, v1::SessionModeId::new("plan"))
            .unwrap();
        assert_eq!(
            serde_json::to_value(options).unwrap()[0]["currentValue"],
            "build"
        );
        let options = state
            .acknowledge(state.revision, v1::SessionModeId::new("plan"))
            .unwrap();
        assert_eq!(
            serde_json::to_value(options).unwrap()[0]["currentValue"],
            "plan"
        );
        let revision = state.revision;
        let native = vec![native("mode", "mode")];
        state
            .discover(Discovery {
                options: Some(native.clone()),
                modes: None,
            })
            .unwrap();
        assert_eq!(
            state
                .acknowledge(revision, v1::SessionModeId::new("build"))
                .unwrap(),
            native
        );
    }

    #[test]
    fn absent_capabilities_and_empty_catalogs_do_not_invent_a_current_mode() {
        let mut state = Legacy::default();
        assert!(
            state
                .observe(v1::SessionModeId::new("plan"))
                .unwrap()
                .is_empty()
        );
        let mut empty = modes();
        empty.available_modes.clear();
        assert!(
            state
                .discover(Discovery {
                    options: None,
                    modes: Some(empty)
                })
                .unwrap()
                .is_empty()
        );
    }
}
