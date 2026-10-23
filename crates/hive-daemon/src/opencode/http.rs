//! A small authenticated HTTP/SSE client for the OpenCode server.
//!
//! Hand-rolled SSE parsing keeps `reqwest`'s optional `stream` feature (and a new dependency)
//! out of the workspace: `Response::chunk()` is all the framing needs.

use std::{collections::VecDeque, time::Duration};

use reqwest::{Method, Response, Url};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::error::OpencodeError;
use crate::config::Profile;

/// Ordinary calls must not hang a prompt or a startup forever on a wedged server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A single line larger than this cannot be a real OpenCode event; dropping it bounds memory
/// against a misbehaving or hostile endpoint.
const MAX_BUFFER: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Client {
    http: reqwest::Client,
    base: String,
    // Kept out of `Debug`/errors: this is the only place the secret lives after startup.
    auth: Option<(String, String)>,
}

impl Client {
    pub(crate) fn new(profile: &Profile) -> Result<Self, OpencodeError> {
        let endpoint = profile
            .endpoint
            .as_deref()
            .ok_or(OpencodeError::Protocol { what: "endpoint" })?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &profile.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                OpencodeError::Protocol {
                    what: "header name",
                }
            })?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
                OpencodeError::Protocol {
                    what: "header value",
                }
            })?;
            headers.insert(name, value);
        }
        let auth = match &profile.password_env {
            Some(var) => {
                let password = std::env::var(var)
                    .ok()
                    .filter(|password| !password.is_empty())
                    .ok_or_else(|| OpencodeError::MissingCredentials { var: var.clone() })?;
                Some((
                    profile
                        .username
                        .clone()
                        .unwrap_or_else(|| "opencode".into()),
                    password,
                ))
            }
            None => None,
        };
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| OpencodeError::Protocol {
                what: "client setup",
            })?;
        Ok(Self {
            http,
            base: endpoint.trim_end_matches('/').to_owned(),
            auth,
        })
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url, OpencodeError> {
        let mut url =
            Url::parse(&format!("{}{path}", self.base)).map_err(|_| OpencodeError::Protocol {
                what: "endpoint URL",
            })?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<reqwest::RequestBuilder, OpencodeError> {
        let mut request = self.http.request(method, self.url(path, query)?);
        if let Some((user, password)) = &self.auth {
            request = request.basic_auth(user, Some(password));
        }
        Ok(request)
    }

    /// Sends a request and maps transport and status failures to typed errors.
    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Response, OpencodeError> {
        let mut request = self.request(method, path, query)?.timeout(REQUEST_TIMEOUT);
        if let Some(body) = body {
            request = request.json(body);
        }
        check(request.send().await.map_err(classify)?)
    }

    pub(crate) async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<T, OpencodeError> {
        self.send(method, path, query, body)
            .await?
            .json()
            .await
            .map_err(|_| OpencodeError::Protocol { what: "JSON body" })
    }

    pub(crate) async fn ok(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(), OpencodeError> {
        self.send(method, path, query, body).await.map(|_| ())
    }

    /// Opens an SSE stream. There is deliberately no total timeout: the stream is long-lived.
    pub(crate) async fn stream(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Sse, OpencodeError> {
        let request = self
            .request(Method::GET, path, query)?
            .header(reqwest::header::ACCEPT, "text/event-stream");
        let response = check(request.send().await.map_err(classify)?)?;
        Ok(Sse {
            response,
            parser: SseParser::default(),
            ready: VecDeque::new(),
        })
    }
}

fn classify(error: reqwest::Error) -> OpencodeError {
    // Never keep the reqwest error: its Display embeds the URL.
    if error.is_timeout() {
        OpencodeError::Unreachable {
            reason: "timed out",
        }
    } else if error.is_connect() {
        OpencodeError::Unreachable {
            reason: "connection failed",
        }
    } else {
        OpencodeError::Unreachable {
            reason: "request failed",
        }
    }
}

fn check(response: Response) -> Result<Response, OpencodeError> {
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else if matches!(status.as_u16(), 401 | 403) {
        Err(OpencodeError::Unauthorized)
    } else {
        Err(OpencodeError::HttpStatus {
            status: status.as_u16(),
        })
    }
}

/// One dispatched server-sent event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub event: Option<String>,
    pub data: String,
}

pub(crate) struct Sse {
    response: Response,
    parser: SseParser,
    ready: VecDeque<Frame>,
}

impl Sse {
    /// `Ok(None)` is a clean end of stream; `Err` is a broken connection. Callers treat both
    /// as "reconnect", but the distinction keeps logs honest.
    pub(crate) async fn next(&mut self) -> Result<Option<Frame>, OpencodeError> {
        loop {
            if let Some(frame) = self.ready.pop_front() {
                return Ok(Some(frame));
            }
            match self.response.chunk().await {
                Ok(Some(bytes)) => self.ready.extend(self.parser.push(&bytes)),
                Ok(None) => return Ok(None),
                Err(_) => return Err(OpencodeError::StreamLost),
            }
        }
    }
}

/// Incremental SSE framing per the WHATWG spec subset OpenCode uses.
#[derive(Default)]
pub(crate) struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            // Servers may use CRLF; a stray CR would otherwise corrupt the last field value.
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                // A blank line dispatches; an event with no data lines is not an event.
                if !self.data.is_empty() {
                    frames.push(Frame {
                        event: self.event.take(),
                        data: self.data.join("\n"),
                    });
                }
                self.event = None;
                self.data.clear();
            } else if line.starts_with(':') {
                // Comment lines carry OpenCode's `: heartbeat` keep-alives.
            } else {
                let (field, value) = line.split_once(':').unwrap_or((&line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "data" => self.data.push(value.to_owned()),
                    "event" => self.event = Some(value.to_owned()),
                    // `id` and `retry` are unused: OpenCode's v1 stream has no replay cursor.
                    _ => {}
                }
            }
        }
        if self.buffer.len() > MAX_BUFFER {
            self.buffer.clear();
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(frames: &[Frame]) -> Vec<&str> {
        frames.iter().map(|frame| frame.data.as_str()).collect()
    }

    #[test]
    fn a_frame_split_across_chunks_is_emitted_once_complete() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"data: {\"a\"").is_empty());
        assert!(parser.push(b":1}\n").is_empty());
        let frames = parser.push(b"\n");
        assert_eq!(data(&frames), ["{\"a\":1}"]);
    }

    #[test]
    fn heartbeat_comments_and_crlf_line_endings_are_tolerated() {
        let mut parser = SseParser::default();
        let frames = parser.push(b": heartbeat\r\n\r\nevent: x\r\ndata: one\r\n\r\n");
        assert_eq!(data(&frames), ["one"]);
        assert_eq!(frames[0].event.as_deref(), Some("x"));
    }

    #[test]
    fn multiple_data_lines_are_joined_with_newlines() {
        let mut parser = SseParser::default();
        let frames = parser.push(b"data: a\ndata: b\n\n");
        assert_eq!(data(&frames), ["a\nb"]);
    }

    #[test]
    fn two_frames_in_one_chunk_are_both_emitted_in_order() {
        let mut parser = SseParser::default();
        let frames = parser.push(b"data: 1\n\ndata: 2\n\n");
        assert_eq!(data(&frames), ["1", "2"]);
    }

    #[test]
    fn a_blank_line_with_no_data_does_not_emit_an_empty_event() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"\n\n: only a comment\n\n").is_empty());
    }

    #[test]
    fn multibyte_text_split_mid_character_survives_reassembly() {
        let mut parser = SseParser::default();
        let bytes = "data: héllo\n\n".as_bytes();
        // Split inside the two-byte é.
        assert!(parser.push(&bytes[..8]).is_empty());
        assert_eq!(data(&parser.push(&bytes[8..])), ["héllo"]);
    }
}
