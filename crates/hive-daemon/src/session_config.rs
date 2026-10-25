//! Configuration is agent-owned; Hive caches a snapshot and fences edits like prompt admission.

use super::*;

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
    pub fn finish(
        mut self,
        id: &v2::SessionId,
        result: agent_client_protocol::Result<Vec<v2::SessionConfigOption>>,
    ) {
        let result = result.and_then(|options| {
            let mut state = self.control.lock().map_err(|_| Error::internal_error())?;
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
        });
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
