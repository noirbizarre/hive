//! Request identity and delivery outcomes, deliberately independent of agent-owned history.

use crate::PromptFailure;
use agent_client_protocol::schema::v2;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("prompt content could not be fingerprinted")]
    Encoding,
    #[error("invalid or unsupported prompt fingerprint")]
    Fingerprint,
    #[error("invalid legacy prompt content")]
    LegacyContent,
    #[error("invalid recorded prompt outcome")]
    Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint(String);

impl Fingerprint {
    pub fn content(content: &[v2::ContentBlock]) -> Result<Self, JournalError> {
        let value = serde_json::to_value(content).map_err(|_| JournalError::Encoding)?;
        let mut bytes = Vec::new();
        canonical(&value, &mut bytes).map_err(|_| JournalError::Encoding)?;
        let mut digest = Sha256::new();
        // Changing the canonical encoding must introduce a new fingerprint version.
        digest.update(b"hive:prompt-content:sha256-v1\0");
        digest.update(bytes);
        Ok(Self(format!("sha256-v1:{:x}", digest.finalize())))
    }

    pub fn legacy(content: &str) -> Result<Self, JournalError> {
        // Decode exactly as live requests are decoded; raw JSON ordering is not identity.
        let content: Vec<v2::ContentBlock> =
            serde_json::from_str(content).map_err(|_| JournalError::LegacyContent)?;
        Self::content(&content)
    }

    pub fn parse(value: String) -> Result<Self, JournalError> {
        let valid = value.strip_prefix("sha256-v1:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if !valid {
            return Err(JournalError::Fingerprint);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn canonical(value: &Value, bytes: &mut Vec<u8>) -> serde_json::Result<()> {
    match value {
        Value::Object(fields) => {
            bytes.push(b'{');
            // preserve_order is enabled by ACP: sort recursively, including extension metadata.
            let sorted: std::collections::BTreeMap<_, _> = fields.iter().collect();
            for (index, (key, value)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    bytes.push(b',');
                }
                serde_json::to_writer(&mut *bytes, key)?;
                bytes.push(b':');
                canonical(value, bytes)?;
            }
            bytes.push(b'}');
        }
        Value::Array(values) => {
            bytes.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    bytes.push(b',');
                }
                canonical(value, bytes)?;
            }
            bytes.push(b']');
        }
        // JSON numbers retain their serialized representation, including signed floating zero.
        _ => serde_json::to_writer(bytes, value)?,
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Acknowledged {
        #[serde(rename = "messageId")]
        message_id: v2::MessageId,
    },
    Failed {},
    Cancelled {},
}

impl Outcome {
    pub fn recorded(result: &Result<v2::PromptResponse, PromptFailure>) -> Self {
        match result {
            Ok(response) => Self::Acknowledged {
                message_id: response.message_id.clone(),
            },
            Err(PromptFailure::Cancelled) => Self::Cancelled {},
            Err(PromptFailure::Failed(_)) => Self::Failed {},
        }
    }

    pub fn legacy(value: &str) -> Result<Self, JournalError> {
        let result: Result<v2::PromptResponse, String> =
            serde_json::from_str(value).map_err(|_| JournalError::Outcome)?;
        Ok(Self::recorded(&result.map_err(PromptFailure::from)))
    }

    pub fn decode(value: &str) -> Result<Self, JournalError> {
        serde_json::from_str(value).map_err(|_| JournalError::Outcome)
    }

    pub fn encode(&self) -> Result<String, JournalError> {
        serde_json::to_string(self).map_err(|_| JournalError::Outcome)
    }

    pub fn into_response(self) -> Result<v2::PromptResponse, PromptFailure> {
        match self {
            Self::Acknowledged { message_id } => Ok(v2::PromptResponse::new(message_id)),
            Self::Cancelled {} => Err(PromptFailure::Cancelled),
            Self::Failed {} => Err("prompt previously failed; this key will not be sent again; inspect agent state before submitting new work with a new key".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cancellation_survives_durable_replay_without_becoming_a_generic_failure() {
        let outcome = Outcome::recorded(&Err(PromptFailure::Cancelled));
        assert!(matches!(
            Outcome::decode(&outcome.encode().unwrap())
                .unwrap()
                .into_response(),
            Err(PromptFailure::Cancelled)
        ));
    }

    fn fingerprint(value: Value) -> Fingerprint {
        Fingerprint::legacy(&value.to_string()).unwrap()
    }

    #[test]
    fn fingerprints_have_a_stable_versioned_encoding_and_reject_unknown_encodings() {
        let empty = Fingerprint::content(&[]).unwrap();
        assert_eq!(
            empty.as_str(),
            "sha256-v1:f8edd0d31cc3b2dc29f713081a548677cae8f62da13aae23dfbe9576680b4738"
        );
        assert_eq!(Fingerprint::parse(empty.as_str().into()).unwrap(), empty);
        for invalid in ["", "sha256-v1:00", "sha256-v2:00", "not a fingerprint"] {
            assert!(Fingerprint::parse(invalid.into()).is_err());
        }
    }

    #[test]
    fn nested_object_order_does_not_change_content_identity() {
        let first =
            r#"[{"type":"text","text":"hello","_meta":{"z":1,"a":{"second":2,"first":1}}}]"#;
        let reordered =
            r#"[{"_meta":{"a":{"first":1,"second":2},"z":1},"text":"hello","type":"text"}]"#;
        assert_eq!(
            Fingerprint::legacy(first).unwrap(),
            Fingerprint::legacy(reordered).unwrap()
        );
    }

    #[test]
    fn every_content_component_and_array_boundary_participates_in_identity() {
        for (first, changed) in [
            (
                json!([{"type":"text","text":"hello"}]),
                json!([{"type":"text","text":"hello "}]),
            ),
            (
                json!([{"type":"image","data":"AA==","mimeType":"image/png"}]),
                json!([{"type":"image","data":"AQ==","mimeType":"image/png"}]),
            ),
            (
                json!([{"type":"text","text":"a","annotations":{"priority":0.5}}]),
                json!([{"type":"text","text":"a","annotations":{"priority":0.6}}]),
            ),
            (
                json!([{"type":"text","text":"a","_meta":{"note":"one"}}]),
                json!([{"type":"text","text":"a","_meta":{"note":"two"}}]),
            ),
            (
                json!([{"type":"custom","payload":{"a":[1,2]}}]),
                json!([{"type":"custom","payload":{"a":[2,1]}}]),
            ),
            (
                json!([{"type":"text","text":"a"},{"type":"text","text":"b"}]),
                json!([{"type":"text","text":"ab"}]),
            ),
            (
                json!([{"type":"resource","resource":{"uri":"file:///one","text":"a"}}]),
                json!([{"type":"resource","resource":{"uri":"file:///one","text":"b"}}]),
            ),
        ] {
            assert_ne!(fingerprint(first), fingerprint(changed));
        }
    }

    #[test]
    fn serialized_numeric_distinctions_including_signed_annotation_zero_are_preserved() {
        for (first, changed) in [
            (
                r#"[{"type":"text","text":"a","annotations":{"priority":0.0}}]"#,
                r#"[{"type":"text","text":"a","annotations":{"priority":-0.0}}]"#,
            ),
            (
                r#"[{"type":"text","text":"a","_meta":{"n":1}}]"#,
                r#"[{"type":"text","text":"a","_meta":{"n":1.0}}]"#,
            ),
        ] {
            assert_ne!(
                Fingerprint::legacy(first).unwrap(),
                Fingerprint::legacy(changed).unwrap()
            );
        }
    }

    #[test]
    fn request_credentials_and_keys_are_not_prompt_content() {
        let request = |key| {
            serde_json::from_value::<v2::PromptRequest>(json!({
                "sessionId":"session", "prompt":[{"type":"text","text":"hello"}],
                "_meta":{"hive":{"key":key,"token":key,"generation":1}}
            }))
            .unwrap()
        };
        assert_eq!(
            Fingerprint::content(&request("one").prompt).unwrap(),
            Fingerprint::content(&request("two").prompt).unwrap()
        );
    }

    #[test]
    fn durable_outcomes_discard_arbitrary_metadata_and_error_text() {
        let acknowledged = Outcome::legacy(
            r#"{"Ok":{"messageId":"original","_meta":{"private":"secret metadata"}}}"#,
        )
        .unwrap();
        assert_eq!(
            acknowledged.encode().unwrap(),
            r#"{"status":"acknowledged","messageId":"original"}"#
        );
        let response = acknowledged.into_response().unwrap();
        assert_eq!(response.message_id.to_string(), "original");
        assert!(response.meta.is_none());
        let failed = Outcome::legacy(r#"{"Err":"private downstream payload"}"#).unwrap();
        assert_eq!(failed.encode().unwrap(), r#"{"status":"failed"}"#);
        assert!(
            !failed
                .into_response()
                .unwrap_err()
                .to_string()
                .contains("private downstream")
        );
        assert!(Outcome::decode(r#"{"status":"failed","payload":"unexpected"}"#).is_err());
    }
}
