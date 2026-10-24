//! ACP decision validation shared by the broker and native presentation adapters.

use crate::v2;
use serde_json::Value;

/// Never include submitted values in diagnostics: forms may contain credentials.
#[derive(Debug, thiserror::Error)]
#[error("{field}: {reason}")]
pub struct InvalidInput {
    pub field: String,
    pub reason: String,
}

fn invalid(field: &str, reason: &str) -> InvalidInput {
    InvalidInput {
        field: field.into(),
        reason: reason.into(),
    }
}

/// Restrict patterns to the common Rust/ECMAScript subset instead of changing their meaning.
pub fn pattern_supported(pattern: &str) -> bool {
    !pattern.contains("(?")
        && !pattern.contains(['\\', '.'])
        && !["&&", "--", "~~", "[[", "[:"]
            .iter()
            .any(|syntax| pattern.contains(syntax))
        && pattern.is_ascii()
        && regex::Regex::new(pattern).is_ok()
}

pub fn schema_supported(schema: &Value) -> bool {
    let Some(properties) = schema["properties"].as_object() else {
        return false;
    };
    (schema["type"].is_null() || schema["type"] == "object")
        && (schema["required"].is_null() || schema["required"].is_array())
        && schema["required"].as_array().is_none_or(|required| {
            required
                .iter()
                .all(|key| key.as_str().is_some_and(|key| properties.contains_key(key)))
        })
        && properties.values().all(|field| {
            matches!(
                field["type"].as_str(),
                Some("string" | "integer" | "number" | "boolean" | "array")
            ) && (field["pattern"].is_null()
                || field["pattern"].as_str().is_some_and(pattern_supported))
                && choices_supported(field)
                && ["minLength", "maxLength", "minItems", "maxItems"]
                    .iter()
                    .all(|key| field[key].is_null() || field[key].as_u64().is_some())
                && ["minimum", "maximum"].iter().all(|key| {
                    field[key].is_null() || field[key].as_f64().is_some_and(f64::is_finite)
                })
                && (field["type"] != "array"
                    || ((field["items"]["type"] == "string"
                        || (field["items"]["type"].is_null()
                            && field["items"]["anyOf"].is_array()))
                        && choices_supported(&field["items"])))
        })
}

fn choices_supported(field: &Value) -> bool {
    (field["enum"].is_null()
        || field["enum"]
            .as_array()
            .is_some_and(|choices| choices.iter().all(Value::is_string)))
        && ["oneOf", "anyOf"].iter().all(|kind| {
            field[kind].is_null()
                || field[kind]
                    .as_array()
                    .is_some_and(|choices| choices.iter().all(|choice| choice["const"].is_string()))
        })
}

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|number| {
                number.is_finite()
                    && number.fract() == 0.0
                    && *number >= i64::MIN as f64
                    && *number < 9_223_372_036_854_775_808.0
            })
            .map(|number| number as i64)
    })
}

pub fn validate_content(schema: &Value, content: &Value) -> Result<(), InvalidInput> {
    if !schema_supported(schema) {
        return Err(invalid("form", "unsupported schema; decline this request"));
    }
    let values = content
        .as_object()
        .ok_or_else(|| invalid("form", "enter the requested fields"))?;
    for key in schema["required"].as_array().into_iter().flatten() {
        let key = key
            .as_str()
            .ok_or_else(|| invalid("form", "invalid required field"))?;
        if !values.contains_key(key) {
            return Err(invalid(key, "required field"));
        }
    }
    for (key, value) in values {
        let field = &schema["properties"][key];
        let valid_type = match field["type"].as_str() {
            Some("string") => value.is_string(),
            Some("boolean") => value.is_boolean(),
            Some("integer") => integer(value).is_some(),
            Some("number") => value.as_f64().is_some_and(f64::is_finite),
            Some("array") => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            _ => false,
        };
        if !valid_type {
            return Err(invalid(key, "value has the wrong type"));
        }
        if field["type"] == "integer" {
            // JSON numbers such as 1.0 are integral too, without rounding out-of-range values.
            let number = integer(value).expect("integer type checked");
            if field["minimum"].as_i64().is_some_and(|min| number < min)
                || field["maximum"].as_i64().is_some_and(|max| number > max)
            {
                return Err(invalid(key, "value is outside the requested bounds"));
            }
        } else if let Some(number) = value.as_f64()
            && (field["minimum"].as_f64().is_some_and(|min| number < min)
                || field["maximum"].as_f64().is_some_and(|max| number > max))
        {
            return Err(invalid(key, "value is outside the requested bounds"));
        }
        if let Some(text) = value.as_str() {
            let length = text.chars().count() as u64;
            if field["minLength"].as_u64().is_some_and(|min| length < min)
                || field["maxLength"].as_u64().is_some_and(|max| length > max)
                || field["pattern"].as_str().is_some_and(|pattern| {
                    text.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
                        || !regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(text))
                })
            {
                return Err(invalid(
                    key,
                    "text does not satisfy the requested constraints",
                ));
            }
        }
        validate_choices(key, field, value)?;
        if let Some(items) = value.as_array() {
            if field["minItems"]
                .as_u64()
                .is_some_and(|min| (items.len() as u64) < min)
                || field["maxItems"]
                    .as_u64()
                    .is_some_and(|max| (items.len() as u64) > max)
            {
                return Err(invalid(key, "select the requested number of items"));
            }
            for item in items {
                validate_choices(key, &field["items"], item)?;
            }
        }
    }
    Ok(())
}

fn validate_choices(key: &str, field: &Value, value: &Value) -> Result<(), InvalidInput> {
    if field["enum"]
        .as_array()
        .is_some_and(|choices| !choices.contains(value))
    {
        return Err(invalid(key, "select a listed choice"));
    }
    for kind in ["oneOf", "anyOf"] {
        if field[kind]
            .as_array()
            .is_some_and(|choices| !choices.iter().any(|choice| choice["const"] == *value))
        {
            return Err(invalid(key, "select a listed choice"));
        }
    }
    Ok(())
}

pub fn validate_permission(
    request: &v2::RequestPermissionRequest,
    response: &v2::RequestPermissionResponse,
) -> bool {
    match &response.outcome {
        v2::RequestPermissionOutcome::Selected(selected) => request
            .options
            .iter()
            .any(|option| option.option_id == selected.option_id),
        // A proxy preserves future outcomes; consumers must not interpret them as authorization.
        _ => true,
    }
}

pub fn validate_elicitation(
    request: &v2::CreateElicitationRequest,
    response: &v2::CreateElicitationResponse,
) -> Result<(), InvalidInput> {
    if let v2::ElicitationAction::Accept(accept) = &response.action {
        match &request.mode {
            v2::ElicitationMode::Form(form) => validate_content(
                &serde_json::to_value(&form.requested_schema).expect("ACP schema serializes"),
                &accept
                    .content
                    .as_ref()
                    .map(|content| serde_json::to_value(content).expect("ACP content serializes"))
                    .unwrap_or_else(|| serde_json::json!({})),
            )?,
            v2::ElicitationMode::Url(url) if safe_url(&url.url) => {}
            _ => {
                return Err(invalid(
                    "elicitation",
                    "unsupported mode or URL; decline this request",
                ));
            }
        }
    }
    Ok(())
}

pub fn safe_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

/// Text conversion is separate from validation so both native clients retain invalid drafts.
pub fn parse_field(field: &Value, text: &str) -> Result<v2::ElicitationContentValue, InvalidInput> {
    let value = match field["type"].as_str() {
        Some("string") => Value::String(text.into()),
        Some("array") if text.starts_with('[') => serde_json::from_str(text)
            .map_err(|_| invalid("field", "enter a JSON array of strings"))?,
        Some("array") => serde_json::to_value(if text.is_empty() {
            Vec::new()
        } else {
            text.split('|').map(str::to_owned).collect::<Vec<_>>()
        })
        .expect("strings serialize"),
        Some("integer" | "number" | "boolean") => serde_json::from_str(text)
            .map_err(|_| invalid("field", "enter a value of the requested type"))?,
        _ => return Err(invalid("field", "unsupported field type")),
    };
    serde_json::from_value(value)
        .map_err(|_| invalid("field", "enter a value of the requested type"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invented_permission_options_are_rejected_and_future_outcomes_remain_opaque() {
        let request = v2::RequestPermissionRequest::new(
            v2::SessionId::new("session"),
            "Run?",
            vec![v2::PermissionOption::new(
                "listed",
                "Allow once",
                v2::PermissionOptionKind::AllowOnce,
            )],
        );
        let invented = v2::RequestPermissionResponse::new(v2::RequestPermissionOutcome::Selected(
            v2::SelectedPermissionOutcome::new("invented"),
        ));
        assert!(!validate_permission(&request, &invented));
        let raw = json!({"outcome":{"outcome":"future","nested":{"preserved":true}}});
        let response: v2::RequestPermissionResponse = serde_json::from_value(raw.clone()).unwrap();
        assert!(validate_permission(&request, &response));
        assert!(!matches!(
            response.outcome,
            v2::RequestPermissionOutcome::Selected(_)
        ));
        assert_eq!(serde_json::to_value(response).unwrap(), raw);
    }

    #[test]
    fn multiselect_values_can_contain_the_text_entry_delimiter() {
        let field = json!({"type":"array","items":{"type":"string","enum":["a|b"]}});
        assert_eq!(
            parse_field(&field, "[\"a|b\"]").unwrap(),
            v2::ElicitationContentValue::StringArray(vec!["a|b".into()])
        );
        assert_eq!(
            parse_field(&field, "").unwrap(),
            v2::ElicitationContentValue::StringArray(vec![])
        );
    }

    #[test]
    fn native_and_browser_validation_obey_the_same_conformance_cases() {
        let cases: Value =
            serde_json::from_str(include_str!("../tests/fixtures/elicitation.json")).unwrap();
        for case in cases.as_array().unwrap() {
            assert_eq!(
                validate_content(&case["schema"], &case["content"]).is_ok(),
                case["valid"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn false_and_zero_are_present_but_unknown_choices_and_out_of_range_values_are_rejected() {
        let schema = json!({"properties":{"approved":{"type":"boolean"},"count":{"type":"integer","minimum":0,"maximum":2},"choice":{"type":"string","enum":["a","b"]}},"required":["approved","count"]});
        assert!(validate_content(&schema, &json!({"approved":false,"count":0})).is_ok());
        assert!(validate_content(&schema, &json!({"approved":true,"count":3})).is_err());
        assert!(
            validate_content(&schema, &json!({"approved":true,"count":1,"choice":"c"})).is_err()
        );
        assert!(validate_content(&schema, &json!({"count":1})).is_err());
    }

    #[test]
    fn unsupported_properties_are_not_treated_as_text() {
        assert!(!schema_supported(
            &json!({"properties":{"secret":{"type":"future"}}})
        ));
        assert!(!safe_url("javascript:alert(1)"));
    }
}
