//! Bounded clipboard transactions; only the UI owner supplies terminal output.

use crate::clipboard::{ClipboardError, Completion, Request};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{
    collections::BTreeMap,
    io::Write,
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
#[error("Rich clipboard {0}; retry paste/copy or use a supported terminal clipboard")]
pub struct ProtocolError(pub String);

struct Packet {
    fields: BTreeMap<String, String>,
    data: Vec<u8>,
}

impl Packet {
    fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let text =
            std::str::from_utf8(bytes).map_err(|_| ProtocolError("reply is not ASCII".into()))?;
        let (meta, payload) = text.split_once(';').unwrap_or((text, ""));
        let mut fields = BTreeMap::new();
        for field in meta.split(':') {
            let (key, value) = field
                .split_once('=')
                .ok_or_else(|| ProtocolError("metadata is malformed".into()))?;
            if fields.insert(key.to_owned(), value.to_owned()).is_some() {
                return Err(ProtocolError("metadata repeats a key".into()));
            }
        }
        if payload.len() > 4096_usize.div_ceil(3) * 4 {
            return Err(ProtocolError("data chunk exceeds 4096 bytes".into()));
        }
        let data = STANDARD
            .decode(payload)
            .map_err(|_| ProtocolError("data has invalid Base64".into()))?;
        if data.len() > 4096 {
            return Err(ProtocolError("data chunk exceeds 4096 bytes".into()));
        }
        Ok(Self { fields, data })
    }
    fn get(&self, key: &str) -> &str {
        self.fields.get(key).map_or("", String::as_str)
    }
    fn decoded(&self, key: &str, limit: usize) -> Result<String, ProtocolError> {
        let value = self.get(key);
        if value.len() > limit.div_ceil(3) * 4 {
            return Err(ProtocolError(format!("{key} exceeds its limit")));
        }
        let value = STANDARD
            .decode(value)
            .map_err(|_| ProtocolError(format!("{key} has invalid Base64")))?;
        if value.len() > limit {
            return Err(ProtocolError(format!("{key} exceeds its limit")));
        }
        String::from_utf8(value).map_err(|_| ProtocolError(format!("{key} is not UTF-8")))
    }
}

enum Phase {
    Listing,
    Reading(String),
    Writing {
        image: std::sync::Arc<crate::draft::Attachment>,
        offset: usize,
        finished: bool,
    },
}
struct Transfer {
    request: Request,
    id: String,
    phase: Phase,
    started: Instant,
    updated: Instant,
    opened: bool,
    data: Vec<u8>,
    password: String,
    primary: bool,
}

#[derive(Default)]
pub struct Protocol {
    transfer: Option<Transfer>,
    pub native_paste: bool,
}

pub enum Outcome {
    Complete(Completion),
    Image(Request, String, Vec<u8>),
}

fn send(output: &mut impl Write, metadata: &str, bytes: &[u8]) -> Result<(), ClipboardError> {
    write!(
        output,
        "\x1b]5522;{metadata};{}\x1b\\",
        STANDARD.encode(bytes)
    )
    .and_then(|()| output.flush())
    .map_err(ClipboardError::Terminal)
}

impl Protocol {
    pub fn abort(&mut self) {
        self.transfer = None;
    }
    pub fn cancel(&mut self) {
        self.transfer = None;
        self.native_paste = false;
    }

    pub fn read(&mut self, request: Request, output: &mut impl Write) -> Option<Completion> {
        if self.transfer.is_some() {
            return Some(failure(request, "is busy"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        if let Err(error) = send(output, &format!("type=read:id={id}"), b".") {
            return Some(Completion {
                request,
                result: Err(error),
            });
        }
        self.transfer = Some(Transfer::new(request, id, Phase::Listing));
        None
    }

    pub fn write(
        &mut self,
        request: Request,
        image: std::sync::Arc<crate::draft::Attachment>,
        output: &mut impl Write,
    ) -> Option<Completion> {
        if self.transfer.is_some() {
            return Some(failure(request, "is busy"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        if let Err(error) = send(output, &format!("type=write:id={id}"), &[]) {
            return Some(Completion {
                request,
                result: Err(error),
            });
        }
        self.transfer = Some(Transfer::new(
            request,
            id,
            Phase::Writing {
                image,
                offset: 0,
                finished: false,
            },
        ));
        None
    }

    pub fn tick(&mut self, output: &mut impl Write) -> Option<Completion> {
        let transfer = self.transfer.as_mut()?;
        if transfer.started.elapsed() > Duration::from_secs(30)
            || transfer.updated.elapsed() > Duration::from_secs(5)
        {
            return self
                .transfer
                .take()
                .map(|t| failure(t.request, "timed out"));
        }
        if let Phase::Writing {
            image,
            offset,
            finished,
        } = &mut transfer.phase
        {
            // Four chunks per tick bound output latency and leave time for redraws and cancellation.
            let result = (|| {
                for _ in 0..4 {
                    if *finished {
                        break;
                    }
                    if *offset == image.bytes.len() {
                        send(output, &format!("type=wdata:id={}", transfer.id), &[])?;
                        *finished = true;
                    } else {
                        // Terminals decode concatenated wdata Base64, so only the final chunk
                        // may carry padding. A multiple of three also stays under 4096 bytes.
                        let end = (*offset + 4095).min(image.bytes.len());
                        send(
                            output,
                            &format!(
                                "type=wdata:id={}:mime={}",
                                transfer.id,
                                STANDARD.encode(&image.mime)
                            ),
                            &image.bytes[*offset..end],
                        )?;
                        *offset = end;
                    }
                    transfer.updated = Instant::now();
                }
                Ok(())
            })();
            if let Err(error) = result {
                return self.transfer.take().map(|t| Completion {
                    request: t.request,
                    result: Err(error),
                });
            }
        }
        None
    }

    pub fn receive(
        &mut self,
        bytes: &[u8],
        offer: impl FnOnce() -> Option<Request>,
        output: &mut impl Write,
    ) -> Option<Outcome> {
        let packet = match Packet::parse(bytes) {
            Ok(packet) => packet,
            Err(error) => {
                return self.transfer.take().map(|t| {
                    Outcome::Complete(Completion {
                        request: t.request,
                        result: Err(error.into()),
                    })
                });
            }
        };
        if packet.get("id").is_empty()
            && self.transfer.is_none()
            && self.native_paste
            && packet.get("type") == "read"
            && packet.get("status") == "OK"
        {
            self.transfer =
                offer().map(|request| Transfer::new(request, String::new(), Phase::Listing));
        }
        let transfer = self.transfer.as_mut()?;
        if transfer.id != packet.get("id") {
            return None;
        }
        let result = transfer.apply(packet, output);
        match result {
            Ok(false) => None,
            Err(error) => self.transfer.take().map(|t| {
                Outcome::Complete(Completion {
                    request: t.request,
                    result: Err(error),
                })
            }),
            Ok(true) => {
                let t = self.transfer.take()?;
                let result = match t.phase {
                    Phase::Reading(mime) if mime.starts_with("image/") => {
                        return Some(Outcome::Image(t.request, mime, t.data));
                    }
                    Phase::Reading(_) => String::from_utf8(t.data)
                        .map(|text| Some(text.into()))
                        .map_err(|_| ProtocolError("text is not UTF-8".into()).into()),
                    Phase::Writing { .. } => Ok(None),
                    Phase::Listing => unreachable!("listing transitions to a content read"),
                };
                Some(Outcome::Complete(Completion {
                    request: t.request,
                    result,
                }))
            }
        }
    }
}

impl Transfer {
    fn new(request: Request, id: String, phase: Phase) -> Self {
        Self {
            request,
            id,
            phase,
            started: Instant::now(),
            updated: Instant::now(),
            opened: false,
            data: Vec::new(),
            password: String::new(),
            primary: false,
        }
    }

    fn apply(&mut self, packet: Packet, output: &mut impl Write) -> Result<bool, ClipboardError> {
        let invalid = || ProtocolError("reply has an invalid transaction state".into());
        let writing = matches!(self.phase, Phase::Writing { .. });
        if packet.get("type") != if writing { "write" } else { "read" } {
            return Err(invalid().into());
        }
        self.updated = Instant::now();
        if !packet.get("pw").is_empty() {
            let password = packet.decoded("pw", 1024)?;
            if !self.password.is_empty() && self.password != password {
                return Err(invalid().into());
            }
            self.password = password;
        }
        match packet.get("loc") {
            "primary" => self.primary = true,
            "" | "clipboard" => {}
            _ => return Err(invalid().into()),
        }
        match packet.get("status") {
            "OK" if !writing && !self.opened && packet.data.is_empty() => {
                self.opened = true;
                Ok(false)
            }
            "DATA" if !writing && self.opened => {
                let mime = packet.decoded("mime", 256)?;
                let (expected, limit) = match &self.phase {
                    Phase::Listing => (".", 16 * 1024),
                    Phase::Reading(mime) => (mime.as_str(), crate::draft::MAX_IMAGE_BYTES),
                    _ => return Err(invalid().into()),
                };
                if mime != expected || packet.data.len() > limit.saturating_sub(self.data.len()) {
                    return Err(ProtocolError(
                        "payload exceeds limits or has an unexpected MIME type".into(),
                    )
                    .into());
                }
                self.data.extend_from_slice(&packet.data);
                Ok(false)
            }
            "DONE" if writing => {
                if !matches!(self.phase, Phase::Writing { finished: true, .. }) {
                    return Err(invalid().into());
                }
                Ok(true)
            }
            "DONE" if self.opened && packet.data.is_empty() => {
                if matches!(self.phase, Phase::Listing) {
                    let list = std::str::from_utf8(&self.data).map_err(|_| invalid())?;
                    let types: Vec<_> = list.split_ascii_whitespace().collect();
                    if types.len() > 128 || types.iter().any(|mime| mime.len() > 256) {
                        return Err(invalid().into());
                    }
                    let images = matches!(self.request.target, crate::clipboard::Target::Chat);
                    let mime = [
                        "image/png",
                        "image/jpeg",
                        "image/webp",
                        "text/plain;charset=utf-8",
                        "text/plain",
                    ]
                    .into_iter()
                    .find(|mime| (images || !mime.starts_with("image/")) && types.contains(mime))
                    .ok_or_else(|| {
                        ProtocolError(
                            "has no supported image or plain text; paste images into a chat draft"
                                .into(),
                        )
                    })?
                    .to_owned();
                    // Give the follow-up read its own ID; late listing packets cannot complete it.
                    self.id = uuid::Uuid::new_v4().to_string();
                    let mut meta = format!("type=read:id={}", self.id);
                    if self.primary {
                        meta.push_str(":loc=primary");
                    }
                    if !self.password.is_empty() {
                        meta.push_str(&format!(
                            ":pw={}:name={}",
                            STANDARD.encode(&self.password),
                            STANDARD.encode("Paste event")
                        ));
                    }
                    send(output, &meta, mime.as_bytes())?;
                    self.phase = Phase::Reading(mime);
                    self.data.clear();
                    self.opened = false;
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            "EPERM" => Err(ProtocolError(
                "permission denied; allow clipboard access in your terminal settings".into(),
            )
            .into()),
            "ENOSYS" | "EBUSY" | "EIO" | "EINVAL" | "EFBIG" => {
                Err(ProtocolError(format!("operation failed ({})", packet.get("status"))).into())
            }
            _ => Err(invalid().into()),
        }
    }
}

fn failure(request: Request, message: &str) -> Completion {
    Completion {
        request,
        result: Err(ProtocolError(message.into()).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        Request {
            operation: crate::clipboard::Operation::Paste,
            session: Some("session".into()),
            original: String::new(),
            editor: Default::default(),
            target: crate::clipboard::Target::Chat,
            revision: 0,
        }
    }
    fn packet(id: &str, status: &str, mime: Option<&str>, data: &[u8]) -> Vec<u8> {
        let mut meta = format!("type=read:id={id}:status={status}");
        if let Some(mime) = mime {
            meta.push_str(&format!(":mime={}", STANDARD.encode(mime)));
        }
        format!("{meta};{}", STANDARD.encode(data)).into_bytes()
    }
    fn receive(
        protocol: &mut Protocol,
        id: &str,
        status: &str,
        mime: Option<&str>,
        data: &[u8],
        output: &mut Vec<u8>,
    ) -> Option<Outcome> {
        protocol.receive(&packet(id, status, mime, data), || None, output)
    }
    #[test]
    fn malformed_and_oversized_base64_chunks_are_rejected() {
        for packet in [
            b"type=read:status=DATA;!".as_slice(),
            b"type=read:type=write",
            b"type=read;YQ",
            b"type=read;Y Q=",
        ] {
            assert!(Packet::parse(packet).is_err());
        }
        assert!(
            Packet::parse(format!("type=read;{}", STANDARD.encode(vec![0; 4097])).as_bytes())
                .is_err()
        );
    }

    #[test]
    fn explicit_reads_list_formats_then_assemble_only_the_preferred_image() {
        let mut protocol = Protocol::default();
        let mut output = Vec::new();
        assert!(protocol.read(request(), &mut output).is_none());
        let id = protocol.transfer.as_ref().unwrap().id.clone();
        assert!(receive(&mut protocol, "other", "OK", None, &[], &mut output).is_none());
        receive(&mut protocol, &id, "OK", None, &[], &mut output);
        receive(
            &mut protocol,
            &id,
            "DATA",
            Some("."),
            b"text/plain image/png image/jpeg\n",
            &mut output,
        );
        assert!(receive(&mut protocol, &id, "DONE", None, &[], &mut output).is_none());
        let id = protocol.transfer.as_ref().unwrap().id.clone();
        receive(&mut protocol, &id, "OK", None, &[], &mut output);
        receive(
            &mut protocol,
            &id,
            "DATA",
            Some("image/png"),
            b"first",
            &mut output,
        );
        receive(
            &mut protocol,
            &id,
            "DATA",
            Some("image/png"),
            b"second",
            &mut output,
        );
        assert!(
            matches!(receive(&mut protocol, &id, "DONE", None, &[], &mut output), Some(Outcome::Image(_, mime, bytes)) if mime == "image/png" && bytes == b"firstsecond")
        );
    }

    #[test]
    fn native_paste_tokens_and_primary_location_are_used_only_for_the_followup_read() {
        let mut protocol = Protocol {
            native_paste: true,
            ..Default::default()
        };
        let mut output = Vec::new();
        let opening = format!(
            "type=read:status=OK:loc=primary:pw={}",
            STANDARD.encode("one-time")
        );
        protocol.receive(opening.as_bytes(), || Some(request()), &mut output);
        receive(
            &mut protocol,
            "",
            "DATA",
            Some("."),
            b"image/png",
            &mut output,
        );
        receive(&mut protocol, "", "DONE", None, &[], &mut output);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(":loc=primary:pw=b25lLXRpbWU=:name=UGFzdGUgZXZlbnQ="));
        assert!(output.ends_with(";aW1hZ2UvcG5n\x1b\\"));
    }

    #[test]
    fn denied_timed_out_and_out_of_order_reads_complete_as_errors() {
        for status in ["EPERM", "DATA", "DONE"] {
            let mut protocol = Protocol::default();
            let mut output = Vec::new();
            protocol.read(request(), &mut output);
            let id = protocol.transfer.as_ref().unwrap().id.clone();
            assert!(matches!(
                receive(&mut protocol, &id, status, None, &[], &mut output),
                Some(Outcome::Complete(Completion { result: Err(_), .. }))
            ));
            assert!(protocol.transfer.is_none());
        }
        let mut protocol = Protocol::default();
        protocol.read(request(), &mut Vec::new());
        protocol.transfer.as_mut().unwrap().updated = Instant::now() - Duration::from_secs(6);
        assert!(protocol.tick(&mut Vec::new()).unwrap().result.is_err());
    }

    #[test]
    fn image_writes_are_chunked_and_success_requires_a_final_acknowledgement() {
        // Incompressible pixels force several DATA packets, including a padded final chunk.
        let mut random = 42_u32;
        let pixels = (0..64 * 64 * 4)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                random as u8
            })
            .collect::<Vec<_>>();
        let image = crate::draft::Attachment::from_native(arboard::ImageData {
            width: 64,
            height: 64,
            bytes: pixels.into(),
        })
        .unwrap();
        let mut protocol = Protocol::default();
        let mut output = Vec::new();
        protocol.write(request(), image.clone(), &mut output);
        let id = protocol.transfer.as_ref().unwrap().id.clone();
        while !matches!(
            protocol.transfer.as_ref().unwrap().phase,
            Phase::Writing { finished: true, .. }
        ) {
            assert!(protocol.tick(&mut output).is_none());
        }
        assert!(protocol.transfer.is_some());
        let mut reconstructed = Vec::new();
        let mut encoded = String::new();
        let output_text = std::str::from_utf8(&output).unwrap();
        let mut chunks = 0;
        for sequence in output_text.split("\x1b\\") {
            if sequence.contains(":mime=") {
                encoded.push_str(sequence.rsplit_once(';').unwrap().1);
                let packet =
                    Packet::parse(sequence.strip_prefix("\x1b]5522;").unwrap().as_bytes()).unwrap();
                assert!(packet.data.len() <= 4096);
                reconstructed.extend_from_slice(&packet.data);
                chunks += 1;
            }
        }
        assert!(chunks > 1);
        assert_eq!(reconstructed, image.bytes.as_ref());
        assert_eq!(STANDARD.decode(encoded).unwrap(), image.bytes.as_ref());
        let ack = format!("type=write:id={id}:status=DONE");
        assert!(matches!(
            protocol.receive(ack.as_bytes(), || None, &mut output),
            Some(Outcome::Complete(Completion {
                result: Ok(None),
                ..
            }))
        ));
    }
}
