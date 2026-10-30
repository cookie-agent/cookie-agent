//! Uploading a file a message attaches (spec 11.6): what the model accepts,
//! and the chunked `artifact.upload.*` exchange. A client reads the file and
//! sniffs its content itself; this checks and sends the bytes.

use cookie_agent_protocol::{ArtifactRef, Client, ClientError, InputMedia, SessionId};

/// What the model a message goes to accepts: its name and its input media.
#[derive(Clone, Debug)]
pub struct AcceptedMedia {
    pub model: String,
    pub media: Vec<InputMedia>,
}

impl AcceptedMedia {
    /// The accepted kinds, as a person reads them: `image, pdf`.
    pub fn kinds(&self) -> String {
        self.media
            .iter()
            .map(|entry| entry.kind.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The largest file of any kind the model accepts.
    pub fn largest(&self) -> u64 {
        self.media
            .iter()
            .map(|entry| entry.max_bytes)
            .max()
            .unwrap_or(0)
    }

    /// Whether `bytes` of media type `media_type` and kind `kind` (and, for
    /// an image, `image` pixels) fit; otherwise why not.
    pub fn check(
        &self,
        name: &str,
        kind: &str,
        media_type: &str,
        bytes: u64,
        image: Option<(u32, u32)>,
    ) -> Result<(), String> {
        let model = &self.model;
        let Some(entry) = self.media.iter().find(|entry| entry.kind.as_str() == kind) else {
            return Err(format!(
                "{model} does not accept {kind} files; it accepts {}",
                self.kinds()
            ));
        };
        if !entry.mime_types.iter().any(|accepted| {
            accepted == media_type
                || accepted
                    .strip_suffix('*')
                    .is_some_and(|prefix| media_type.starts_with(prefix))
        }) {
            return Err(format!(
                "{model} does not accept {media_type}; it accepts {}",
                entry.mime_types.join(", ")
            ));
        }
        if bytes > entry.max_bytes {
            return Err(too_large(name, kind, bytes, entry.max_bytes));
        }
        if let Some((width, height)) = image {
            if let Some(limit) = entry.max_dimension
                && width.max(height) > limit
            {
                return Err(format!(
                    "{name} is {width}×{height} pixels; {model} accepts images up to {limit} pixels a side"
                ));
            }
            if let Some(limit) = entry.max_pixels
                && u64::from(width) * u64::from(height) > limit
            {
                return Err(format!(
                    "{name} is {width}×{height} pixels; {model} accepts images of up to {limit} pixels"
                ));
            }
        }
        Ok(())
    }
}

/// What the status line says of `name` attached as a file because the
/// model does not take it inline (`reason`): the model sees its label and
/// can copy it to a machine, not read it.
pub fn attached_as_file(name: &str, reason: &str) -> String {
    format!(
        "attached {name} as a file ({reason}): the model sees its name and can copy it to a machine"
    )
}

/// Why a file of `bytes` is refused for being over `max_bytes`.
pub fn too_large(name: &str, kind: &str, bytes: u64, max_bytes: u64) -> String {
    format!(
        "{name} is {}; {kind} files are accepted up to {}",
        cookie_agent_protocol::human_bytes(bytes),
        cookie_agent_protocol::human_bytes(max_bytes)
    )
}

/// The status line for a daemon refusal of an upload: a `media_too_large`
/// says the limit (the TUI does not downscale), anything else its message.
pub fn upload_refusal(name: &str, error: &ClientError) -> String {
    if error.is(&cookie_agent_protocol::errors::MEDIA_TOO_LARGE)
        && let Some(data) = error.error_data()
    {
        let number = |field: &str| data.fields.get(field).and_then(serde_json::Value::as_u64);
        let kind = data.text("kind").unwrap_or("file");
        if let (Some(width), Some(height)) = (number("width"), number("height")) {
            let mut limits = Vec::new();
            if let Some(side) = number("max_dimension") {
                limits.push(format!("{side} pixels a side"));
            }
            if let Some(pixels) = number("max_pixels") {
                limits.push(format!("{pixels} pixels"));
            }
            if !limits.is_empty() {
                return format!(
                    "{name} is {width}×{height} pixels; images are accepted up to {}",
                    limits.join(" and ")
                );
            }
        }
        if let (Some(bytes), Some(max_bytes)) = (number("byte_length"), number("max_bytes")) {
            return too_large(name, kind, bytes, max_bytes);
        }
    }
    format!("could not attach {name}: {error}")
}

/// The limits a `media_too_large` refusal names, so a client can offer to
/// shrink the file to them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefusedLimits {
    pub max_bytes: Option<u64>,
    pub max_dimension: Option<u32>,
    pub max_pixels: Option<u64>,
}

impl RefusedLimits {
    /// The limits of `error` when it is a `media_too_large` refusal.
    pub fn of(error: &ClientError) -> Option<Self> {
        if !error.is(&cookie_agent_protocol::errors::MEDIA_TOO_LARGE) {
            return None;
        }
        let data = error.error_data()?;
        let number = |field: &str| data.fields.get(field).and_then(serde_json::Value::as_u64);
        Some(Self {
            max_bytes: number("max_bytes"),
            max_dimension: number("max_dimension").and_then(|side| u32::try_from(side).ok()),
            max_pixels: number("max_pixels"),
        })
    }
}

/// Why an upload failed: the status line, and the limits when the daemon
/// refused the file as too large.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadFailure {
    pub message: String,
    pub too_large: Option<RefusedLimits>,
}

impl UploadFailure {
    fn of(name: &str, error: &ClientError) -> Self {
        Self {
            message: upload_refusal(name, error),
            too_large: RefusedLimits::of(error),
        }
    }
}

/// Uploads `bytes` into `session_id`'s store in chunks sent one after
/// another, as `filename` when given and as an upload of `kind`, returning
/// the reference a `file` part names. A refusal says why in terms of `name`; a failed chunk cancels the
/// upload.
pub async fn upload(
    client: &Client,
    session_id: SessionId,
    name: &str,
    filename: Option<String>,
    bytes: &[u8],
    kind: cookie_agent_protocol::UploadKind,
) -> Result<ArtifactRef, UploadFailure> {
    let begun = client
        .begin_upload(cookie_agent_protocol::ArtifactUploadBeginParams {
            session_id,
            byte_length: bytes.len() as u64,
            filename,
            kind,
        })
        .await
        .map_err(|error| UploadFailure::of(name, &error))?;
    let chunk = usize::try_from(
        begun
            .chunk_bytes
            .clamp(1, cookie_agent_protocol::ARTIFACT_CHUNK_BYTES),
    )
    .unwrap_or(usize::MAX);
    let mut offset = 0;
    for piece in bytes.chunks(chunk) {
        use base64::Engine as _;
        let sent = client
            .upload_chunk(cookie_agent_protocol::ArtifactUploadChunkParams {
                upload_id: begun.upload_id.clone(),
                offset: offset as u64,
                data: base64::engine::general_purpose::STANDARD.encode(piece),
            })
            .await;
        if let Err(error) = sent {
            let _ = client
                .cancel_upload(cookie_agent_protocol::ArtifactUploadCancelParams {
                    upload_id: begun.upload_id.clone(),
                })
                .await;
            return Err(UploadFailure::of(name, &error));
        }
        offset += piece.len();
    }
    client
        .finish_upload(cookie_agent_protocol::ArtifactUploadFinishParams {
            upload_id: begun.upload_id,
            sha256: cookie_agent_protocol::Sha256Digest::of_bytes(bytes).to_string(),
        })
        .await
        .map(|finished| finished.artifact)
        .map_err(|error| UploadFailure::of(name, &error))
}
