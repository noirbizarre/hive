//! Only standardized MCP identifiers are rewritten; application JSON remains opaque.

use super::runtime::RuntimeError;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const MAX_NAMES: usize = 4096;
const MAX_NAME_BYTES: usize = 1024;
const MAX_URI_BYTES: usize = 32768;

#[derive(Default)]
pub(super) struct Names(HashMap<String, (String, String, String)>);

impl Names {
    pub fn expose(
        &mut self,
        kind: &str,
        backend: &str,
        name: &str,
    ) -> Result<String, RuntimeError> {
        if name.is_empty() || name.len() > MAX_NAME_BYTES {
            return Err(RuntimeError::Response);
        }
        let tuple = (kind.to_owned(), backend.to_owned(), name.to_owned());
        // Tuple serialization is unambiguous even when original names contain separators.
        let digest = Sha256::digest(
            serde_json::to_vec(&("hive-mcp-name-v1", &tuple)).expect("string tuple serializes"),
        );
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        let alias = format!("hive_{kind}_{hex}");
        if let Some(old) = self.0.get(&alias) {
            if old != &tuple {
                return Err(RuntimeError::Response);
            }
        } else {
            if self.0.len() >= MAX_NAMES {
                return Err(RuntimeError::Busy);
            }
            self.0.insert(alias.clone(), tuple);
        }
        Ok(alias)
    }

    pub fn resolve(&self, kind: &str, alias: &str) -> Result<(String, String), RuntimeError> {
        let (stored_kind, backend, name) = self.0.get(alias).ok_or(RuntimeError::Request)?;
        if stored_kind != kind {
            return Err(RuntimeError::Request);
        }
        Ok((backend.clone(), name.clone()))
    }
}

fn prefix(backend: &str) -> String {
    let hex: String = backend
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("hive-mcp://v1-b{hex}/")
}

pub(super) fn wrap(backend: &str, uri: &str) -> Result<String, RuntimeError> {
    if uri.is_empty() || uri.len() > MAX_URI_BYTES {
        return Err(RuntimeError::Response);
    }
    let wrapped = format!("{}{uri}", prefix(backend));
    if wrapped.len() > MAX_URI_BYTES {
        return Err(RuntimeError::Response);
    }
    Ok(wrapped)
}

pub(super) fn unwrap<'a>(
    backends: impl Iterator<Item = &'a str>,
    uri: &str,
) -> Result<(String, String), RuntimeError> {
    if uri.len() > MAX_URI_BYTES {
        return Err(RuntimeError::Request);
    }
    for backend in backends {
        if let Some(original) = uri.strip_prefix(&prefix(backend))
            && !original.is_empty()
        {
            // Strip exactly once: nested Hive URIs may be legitimate backend identifiers.
            return Ok((backend.into(), original.into()));
        }
    }
    Err(RuntimeError::Request)
}

pub(super) fn field(backend: &str, value: &mut Value, key: &str) -> Result<(), RuntimeError> {
    let uri = value
        .get(key)
        .and_then(Value::as_str)
        .ok_or(RuntimeError::Response)?;
    value[key] = Value::String(wrap(backend, uri)?);
    Ok(())
}

fn content(backend: &str, value: &mut Value) -> Result<(), RuntimeError> {
    match value.get("type").and_then(Value::as_str) {
        Some("resource_link") => field(backend, value, "uri"),
        Some("resource") => resource(
            backend,
            value.get_mut("resource").ok_or(RuntimeError::Response)?,
        ),
        Some("text") if value.get("text").is_some_and(Value::is_string) => Ok(()),
        Some("image" | "audio")
            if value.get("data").is_some_and(Value::is_string)
                && value.get("mimeType").is_some_and(Value::is_string) =>
        {
            Ok(())
        }
        _ => Err(RuntimeError::Response),
    }
}

fn resource(backend: &str, value: &mut Value) -> Result<(), RuntimeError> {
    if !(value.get("text").is_some_and(Value::is_string)
        ^ value.get("blob").is_some_and(Value::is_string))
    {
        return Err(RuntimeError::Response);
    }
    field(backend, value, "uri")
}

pub(super) fn rewrite_result(
    backend: &str,
    method: &str,
    result: &mut Value,
) -> Result<(), RuntimeError> {
    match method {
        "resources/read" => {
            for resource in result
                .get_mut("contents")
                .and_then(Value::as_array_mut)
                .ok_or(RuntimeError::Response)?
            {
                self::resource(backend, resource)?;
            }
        }
        "tools/call" => {
            if result.get("isError").is_some_and(|v| !v.is_boolean())
                || result
                    .get("structuredContent")
                    .is_some_and(|v| !v.is_object())
            {
                return Err(RuntimeError::Response);
            }
            for block in result
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .ok_or(RuntimeError::Response)?
            {
                content(backend, block)?;
            }
        }
        "prompts/get" => {
            for message in result
                .get_mut("messages")
                .and_then(Value::as_array_mut)
                .ok_or(RuntimeError::Response)?
            {
                if !matches!(
                    message.get("role").and_then(Value::as_str),
                    Some("user" | "assistant")
                ) {
                    return Err(RuntimeError::Response);
                }
                content(
                    backend,
                    message.get_mut("content").ok_or(RuntimeError::Response)?,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_are_collision_safe_and_resources_preserve_exact_bytes() {
        let mut names = Names::default();
        let a = names.expose("t", "a", "same").unwrap();
        let b = names.expose("t", "b", "same").unwrap();
        assert_ne!(a, b);
        assert_eq!(names.resolve("t", &a).unwrap(), ("a".into(), "same".into()));
        assert!(names.resolve("p", &a).is_err());
        for original in [
            "file:///a/../b?x=%2f#fragment",
            "hive-mcp://nested/x",
            "urn:test:x",
            "file:///{+path}{?revision}",
        ] {
            let exposed = wrap("a", original).unwrap();
            assert_eq!(
                unwrap(["a", "b"].into_iter(), &exposed).unwrap(),
                ("a".into(), original.into())
            );
        }
    }
    #[test]
    fn only_standard_resource_content_is_rewritten_and_aliases_never_change_target() {
        let mut result = serde_json::json!({"content":[
            {"type":"resource_link","uri":"file:///unlisted"},
            {"type":"resource","resource":{"uri":"urn:embedded","blob":"YQ=="}},
            {"type":"text","text":"file:///opaque"}],
            "structuredContent":{"uri":"file:///application"},"isError":true});
        rewrite_result("backend", "tools/call", &mut result).unwrap();
        assert!(
            result["content"][0]["uri"]
                .as_str()
                .unwrap()
                .starts_with("hive-mcp://")
        );
        assert_eq!(result["content"][1]["resource"]["blob"], "YQ==");
        assert_eq!(result["content"][2]["text"], "file:///opaque");
        assert_eq!(result["structuredContent"]["uri"], "file:///application");
        assert_eq!(result["isError"], true);
        let mut names = Names::default();
        let alias = names.expose("t", "backend", "name").unwrap();
        names
            .0
            .insert(alias, ("t".into(), "foreign".into(), "name".into()));
        assert_eq!(
            names.expose("t", "backend", "name"),
            Err(RuntimeError::Response)
        );
    }
}
