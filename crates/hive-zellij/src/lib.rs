//! ZJHive: Zellij's host WebAccess performs a one-shot operational status query.

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

#[derive(Default)]
struct ZjHive {
    url: String,
    status: String,
    permitted: bool,
}

register_plugin!(ZjHive);

impl ZjHive {
    fn refresh(&mut self) {
        if !self.permitted {
            return;
        }
        self.status = "Connecting to Hive…".into();
        web_request(
            format!("{}/hive/v1/status", self.url.trim_end_matches('/')),
            HttpVerb::Get,
            BTreeMap::new(),
            Vec::new(),
            BTreeMap::new(),
        );
    }
}

fn status_line(code: u16, body: &[u8]) -> String {
    if code != 200 {
        return format!("Hive unavailable (HTTP {code})");
    }
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => format!(
            "Hive {} — {} sessions",
            value["version"].as_str().unwrap_or("?"),
            value["sessions"]
        ),
        Err(_) => "Hive returned invalid status JSON".into(),
    }
}

impl ZellijPlugin for ZjHive {
    fn load(&mut self, config: BTreeMap<String, String>) {
        self.url = config
            .get("url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:8765".into());
        self.status = "Waiting for WebAccess permission".into();
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::WebRequestResult,
            EventType::Timer,
        ]);
        request_permission(&[PermissionType::WebAccess]);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permitted = true;
                self.refresh();
                set_timeout(5.0);
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.status = "WebAccess denied".into();
                true
            }
            Event::WebRequestResult(code, _, body, _) => {
                self.status = status_line(code, &body);
                true
            }
            Event::Timer(_) => {
                self.refresh();
                set_timeout(5.0);
                true
            }
            _ => false,
        }
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        println!("ZJHive | {}", self.status);
    }
}

#[cfg(test)]
mod tests {
    use super::status_line;

    #[test]
    fn an_unavailable_daemon_is_displayed_as_a_failure() {
        assert_eq!(status_line(503, b"{}"), "Hive unavailable (HTTP 503)");
        assert_eq!(
            status_line(200, b"broken"),
            "Hive returned invalid status JSON"
        );
        assert_eq!(
            status_line(200, br#"{"version":"0.1.0","sessions":2}"#),
            "Hive 0.1.0 — 2 sessions"
        );
    }
}
