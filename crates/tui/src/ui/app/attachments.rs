//! Files attached to the composer draft (spec 11.6): `/attach`, offering to
//! attach a pasted path, the chips the composer shows, and the chunked
//! upload. The TUI may run on another machine than the daemon, so a file is
//! always uploaded, never named by path.

use super::*;

use std::path::{Path, PathBuf};

use cookie_agent_protocol::ArtifactRef;
use cookie_agent_runtime_api::media::{UploadRejection, validate_upload};

/// A pasted path the TUI offers to attach instead of typing it.
#[derive(Clone, Debug)]
pub(in crate::ui) struct AttachOffer {
    pub(in crate::ui) path: PathBuf,
    /// The pasted text, typed into the composer if the offer is declined.
    pub(in crate::ui) pasted: String,
}

pub(in crate::ui) use cookie_agent_client_state::uploads::AcceptedMedia;
use cookie_agent_client_state::uploads::too_large;

/// The file a paste names when it is exactly one existing file path, as
/// terminals paste a file dropped on them: absolute, `~/…`, or a `file://`
/// URL, optionally quoted or with backslash-escaped spaces.
pub(in crate::ui) fn pasted_file_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() || text.contains(['\n', '\r']) {
        return None;
    }
    let unquoted = ['\'', '"'].iter().find_map(|quote| {
        text.strip_prefix(*quote)
            .and_then(|rest| rest.strip_suffix(*quote))
    });
    let path = match unquoted {
        Some(path) => path.to_owned(),
        None => match text.strip_prefix("file://") {
            Some(url) => percent_decoded(url)?,
            None if cfg!(windows) => text.to_owned(),
            None => unescaped(text),
        },
    };
    let path = expand_home(&path)?;
    (path.is_absolute() && path.is_file()).then_some(path)
}

fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `text` with shell backslash escapes (`\ `, `\(`) undone.
fn unescaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => out.extend(characters.next()),
            character => out.push(character),
        }
    }
    out
}

/// `path` with a leading `~/` replaced by the home directory.
fn expand_home(path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => Some(PathBuf::from(std::env::var_os("HOME")?).join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// The name a file is attached under: its file name, as uploads take it.
fn upload_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && name.len() <= ArtifactRef::MAX_FILENAME_BYTES)
        .map(str::to_owned)
}

/// Reads, checks, and uploads one file into `session_id`'s store in chunks
/// sent one after another, returning the reference a `file` part names.
async fn upload_file(
    client: Client,
    session_id: SessionId,
    path: PathBuf,
    accepted: AcceptedMedia,
) -> Result<ArtifactRef, String> {
    let shown = path.display().to_string();
    let name = upload_name(&path).unwrap_or_else(|| shown.clone());
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("could not read {shown}: {error}"))?;
    if bytes.is_empty() {
        return Err(format!("{name} is empty"));
    }
    match validate_upload(&bytes) {
        Ok(upload) => accepted.check(
            &name,
            upload.kind,
            upload.media_type,
            bytes.len() as u64,
            upload.image,
        )?,
        Err(UploadRejection::Unrecognized) => {
            return Err(format!(
                "{name} is not a file the model can take; {} accepts {}",
                accepted.model,
                accepted.kinds()
            ));
        }
        Err(UploadRejection::Malformed { media_type }) => {
            return Err(format!("{name} starts like {media_type} but is not valid"));
        }
        Err(UploadRejection::TooLarge {
            kind,
            byte_length,
            max_bytes,
            image,
        }) => {
            return Err(match image {
                Some((width, height)) => format!(
                    "{name} is {width}×{height} pixels; images are accepted up to {} pixels a side and {} pixels",
                    cookie_agent_runtime_api::media::MAX_IMAGE_DIMENSION,
                    cookie_agent_runtime_api::media::MAX_IMAGE_PIXELS
                ),
                None => too_large(&name, kind, byte_length, max_bytes),
            });
        }
    }
    cookie_agent_client_state::uploads::upload(
        &client,
        session_id,
        &name,
        upload_name(&path),
        &bytes,
    )
    .await
    .map_err(|failure| failure.message)
}

impl App {
    /// What the model of the next message accepts: the draft's model (a new
    /// session's draft first), as the catalog view states it. `None` while
    /// no model is selected.
    pub(in crate::ui) fn accepted_media(&self) -> Option<AcceptedMedia> {
        let selection = self.new_session_draft.as_ref().or(self.draft.as_ref())?;
        // An ACP agent takes what it advertised when it last started on the
        // runtime it runs on.
        if let Some(agent) = self.draft_acp_agent() {
            return Some(AcceptedMedia {
                model: agent.id.to_string(),
                media: cookie_agent_client_state::acp::input_media(agent, &self.agent_runtimes()),
            });
        }
        let key = &selection.model.model;
        let media = self
            .model_descriptor(key)
            .map(|model| model.input_media.clone())
            .unwrap_or_default();
        Some(AcceptedMedia {
            model: self.runtime.model_names().short_label(key),
            media,
        })
    }

    /// Whether attaching is offered at all: the selected model accepts some
    /// kind of file.
    pub(in crate::ui) fn attachments_offered(&self) -> bool {
        self.accepted_media()
            .is_some_and(|accepted| !accepted.media.is_empty())
    }

    /// Upload the file at `path` and attach it to the draft once the daemon
    /// stored it. Refuses, without reading it, a path that is no file, a
    /// model that accepts no files, and a file larger than anything the
    /// model accepts; the rest of the checks run on the file's content.
    pub(in crate::ui) async fn attach_file(&mut self, path: &str) {
        let Some(path) = expand_home(path.trim()) else {
            self.status = "cannot expand ~ without HOME".into();
            return;
        };
        let shown = path.display().to_string();
        let Some(accepted) = self.accepted_media() else {
            self.status = "select a model before attaching a file".into();
            return;
        };
        if accepted.media.is_empty() {
            self.status = format!("{} accepts no attachments", accepted.model);
            return;
        }
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                self.status = format!("{shown} is not a file");
                return;
            }
            Err(error) => {
                self.status = format!("cannot attach {shown}: {error}");
                return;
            }
        };
        let name = upload_name(&path).unwrap_or_else(|| shown.clone());
        if metadata.len() > accepted.largest() {
            self.status = format!(
                "{name} is {}; {} accepts files up to {}",
                cookie_agent_protocol::human_bytes(metadata.len()),
                accepted.model,
                cookie_agent_protocol::human_bytes(accepted.largest())
            );
            return;
        }
        // Uploads land in the session tree's store: a new session is
        // created first.
        if let Some(selection) = self.new_session_draft.clone() {
            self.create_root_session(selection).await;
            if self.new_session_draft.is_some() {
                return;
            }
        }
        let Some(session_id) = self.selected else {
            self.status = "create or select a session first".into();
            return;
        };
        self.status = format!("attaching {name}…");
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = upload_file(client, session_id, path, accepted).await;
            let _ = updates.send(RpcUpdate::AttachmentUploaded {
                session_id,
                name,
                result,
            });
        });
    }

    /// An upload finished: its file joins the draft, unless the composer
    /// moved to another session meanwhile (the file lives in the first
    /// session's tree, so it could not be sent from this one).
    pub(super) fn finish_attachment(
        &mut self,
        session_id: SessionId,
        name: String,
        result: Result<ArtifactRef, String>,
    ) {
        match result {
            Ok(artifact) if self.selected == Some(session_id) => {
                self.status = format!("attached {}", crate::state::attachment_chip(&artifact));
                if !self
                    .attachments
                    .iter()
                    .any(|attached| attached.sha256 == artifact.sha256)
                {
                    self.attachments.push(artifact);
                }
            }
            Ok(_) => {
                self.status = format!("{name} was uploaded for another session; attach it again");
            }
            Err(error) => {
                self.session_errors.record(&error);
                self.status = error;
            }
        }
    }

    /// Remove the draft's newest attachment (Backspace at the start of the
    /// composer). Returns whether there was one.
    pub(super) fn remove_last_attachment(&mut self) -> bool {
        let Some(artifact) = self.attachments.pop() else {
            return false;
        };
        self.status = format!("removed {}", artifact.display_name());
        true
    }

    /// Answer the offer to attach a pasted path: attach it, or type the
    /// pasted text into the composer as it was.
    pub(super) async fn answer_attach_offer(&mut self, button: ConfirmButton) {
        self.modal = Modal::None;
        let Some(offer) = self.attach_offer.take() else {
            return;
        };
        match button {
            ConfirmButton::Confirm => {
                self.attach_file(&offer.path.to_string_lossy()).await;
            }
            ConfirmButton::Cancel => {
                self.mutate_input(|input| input.insert_text(&offer.pasted));
            }
        }
    }

    pub(super) async fn handle_attach_offer_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                self.answer_attach_offer(ConfirmButton::Cancel).await;
            }
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                self.answer_attach_offer(ConfirmButton::Confirm).await;
            }
            _ => {}
        }
    }

    pub(super) fn render_attach_offer(&mut self, frame: &mut ratatui::Frame, screen: Rect) {
        let Some(offer) = self.attach_offer.as_ref() else {
            return;
        };
        let theme = &self.theme;
        let name = upload_name(&offer.path).unwrap_or_else(|| offer.path.display().to_string());
        let size = std::fs::metadata(&offer.path)
            .map(|metadata| cookie_agent_protocol::human_bytes(metadata.len()))
            .unwrap_or_default();
        let kinds = self
            .accepted_media()
            .map(|accepted| format!("{} accepts {}.", accepted.model, accepted.kinds()))
            .unwrap_or_default();
        let path = offer.path.display().to_string();
        let body = |width: u16| {
            let mut lines = vec![
                Line::styled(format!("Attach {name} ({size})?"), theme.heading()),
                Line::default(),
            ];
            lines.extend(wrapped_line(Line::styled(path, theme.muted()), width));
            lines.push(Line::default());
            lines.extend(wrapped_line(
                Line::styled(
                    format!("{kinds} Cancel pastes the path as text instead."),
                    theme.muted_text(),
                ),
                width,
            ));
            lines
        };
        self.hit_map.confirm_buttons =
            render_confirm_panel(frame, screen, "Attach a file", body, theme);
    }

    /// The draft's attachments as chips on the composer's bottom border,
    /// `📎 name · type · size` each, as many as fit and a count of the rest.
    /// Draws the attachment chips into the composer's bottom border, and
    /// returns how many columns they take.
    pub(super) fn render_attachment_chips(&self, frame: &mut ratatui::Frame, area: Rect) -> u16 {
        if self.attachments.is_empty() || area.height < 2 || area.width < 8 {
            return 0;
        }
        let room = usize::from(area.width - 4);
        let style = self.theme.user();
        let mut spans = Vec::new();
        let mut used = 0;
        for (index, artifact) in self.attachments.iter().enumerate() {
            let chip = format!(" {} ", crate::state::attachment_chip(artifact));
            let rest = self.attachments.len() - index - 1;
            let more = if rest > 0 {
                format!(" +{rest} ")
            } else {
                String::new()
            };
            let width = UnicodeWidthStr::width(chip.as_str());
            if used + width + UnicodeWidthStr::width(more.as_str()) > room {
                let shown = format!(" +{} ", self.attachments.len() - index);
                if used + UnicodeWidthStr::width(shown.as_str()) <= room {
                    spans.push(Span::styled(shown, style));
                } else if spans.is_empty() {
                    spans.push(Span::styled(truncate_with_ellipsis(&chip, room), style));
                }
                break;
            }
            used += width;
            spans.push(Span::styled(chip, style));
        }
        let line = Line::from(spans);
        let taken = u16::try_from(line.width()).unwrap_or(u16::MAX);
        let row = Rect::new(area.x + 2, area.bottom() - 1, area.width - 4, 1);
        frame.render_widget(Paragraph::new(line), row);
        taken
    }
}

/// [`pasted_file_path`], for tests outside this module.
#[cfg(test)]
pub(crate) fn pasted_file_path_for_test(text: &str) -> Option<PathBuf> {
    pasted_file_path(text)
}
