//! Files attached to the composer draft (spec 11.6): `/attach`, offering to
//! attach a pasted path, the chips the composer shows, and the chunked
//! upload. The TUI may run on another machine than the daemon, so a file is
//! always uploaded, never named by path.

use super::*;

use std::path::{Path, PathBuf};

use cookie_agent_protocol::{ArtifactRef, UploadKind};
use cookie_agent_runtime_api::media::{
    MAX_VIDEO_ATTACHMENT_BYTES, UploadRejection, validate_upload,
};

/// A pasted path the TUI offers to attach instead of typing it.
#[derive(Clone, Debug)]
pub(in crate::ui) struct AttachOffer {
    pub(in crate::ui) path: PathBuf,
    /// The pasted text, typed into the composer if the offer is declined.
    pub(in crate::ui) pasted: String,
    /// Why the file, which starts like media, could not be read as it: the
    /// offer is then to attach it as a file the model sees by its label
    /// (`docs/site/specs/attachment-handling.md` section 3.2).
    pub(in crate::ui) unreadable: Option<String>,
}

/// Why an upload did not join the draft.
#[derive(Debug)]
pub(in crate::ui) enum AttachError {
    /// It failed, for this reason.
    Failed(String),
    /// The file at `path` starts like media but does not parse: the user is
    /// asked before it goes up as a file.
    Unreadable { path: PathBuf, reason: String },
}

impl From<String> for AttachError {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

pub(in crate::ui) use cookie_agent_client_state::uploads::AcceptedMedia;
use cookie_agent_client_state::uploads::{attached_as_file, attached_by_name, too_large};

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
    force_file: bool,
) -> Result<Uploaded, AttachError> {
    let shown = path.display().to_string();
    let name = upload_name(&path).unwrap_or_else(|| shown.clone());
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("could not read {shown}: {error}"))?;
    if bytes.is_empty() {
        return Err(format!("{name} is empty").into());
    }
    // Media the model does not take goes up with its own type, and the
    // model sees its label (`docs/site/specs/attachment-handling.md`
    // question Q2b); what the daemon does not recognize, or takes only as a
    // file, is attached as a file, which `transfer` can copy to a machine.
    let mut by_name = None;
    let as_file = match validate_upload(&bytes) {
        Ok(upload) => {
            by_name = accepted
                .check(
                    &name,
                    upload.kind,
                    upload.media_type,
                    bytes.len() as u64,
                    upload.image,
                )
                .err();
            None
        }
        Err(UploadRejection::Unrecognized) => Some(format!(
            "{} takes {} inline",
            accepted.model,
            accepted.kinds()
        )),
        Err(UploadRejection::Malformed { media_type }) => {
            let reason = format!("{name} starts like {media_type} but could not be read");
            if !force_file {
                return Err(AttachError::Unreadable { path, reason });
            }
            Some(reason)
        }
        Err(UploadRejection::TooLarge {
            kind,
            byte_length,
            max_bytes,
            image,
        }) => Some(match image {
            Some((width, height)) => format!(
                "{width}×{height} pixels; images are taken up to {} pixels a side and {} pixels",
                cookie_agent_runtime_api::media::MAX_IMAGE_DIMENSION,
                cookie_agent_runtime_api::media::MAX_IMAGE_PIXELS
            ),
            None => too_large(&name, kind, byte_length, max_bytes),
        }),
    };
    let kind = if as_file.is_some() {
        UploadKind::File
    } else {
        UploadKind::Media
    };
    let artifact = cookie_agent_client_state::uploads::upload(
        &client,
        session_id,
        &name,
        upload_name(&path),
        &bytes,
        kind,
    )
    .await
    .map_err(|failure| AttachError::Failed(failure.message))?;
    Ok(Uploaded {
        artifact,
        note: as_file
            .map(|reason| attached_as_file(&name, &reason))
            .or_else(|| by_name.map(|reason| attached_by_name(&name, &reason))),
    })
}

/// What `attachment.delivery` answered for the draft's files, by digest, and
/// what it was last asked: the session, the draft's selection and files,
/// and the session's runtimes (`docs/site/specs/attachment-handling.md`
/// section 10).
#[derive(Debug, Default)]
pub(in crate::ui) struct Deliveries {
    pub(in crate::ui) files:
        std::collections::HashMap<String, cookie_agent_protocol::AttachmentDelivery>,
    asked: Option<DeliveryQuestion>,
    request: u64,
    /// The file just attached, whose reason the status line says once the
    /// answer comes.
    announce: Option<String>,
}

type DeliveryQuestion = (
    SessionId,
    cookie_agent_protocol::RunSelection,
    Vec<ArtifactRef>,
    Vec<cookie_agent_protocol::RuntimeId>,
);

/// A file uploaded, and what the status line says of it when the model does
/// not take it inline.
pub(in crate::ui) struct Uploaded {
    pub(in crate::ui) artifact: ArtifactRef,
    pub(in crate::ui) note: Option<String>,
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
                media: cookie_agent_client_state::acp::input_media(
                    agent,
                    &self.agent_runtimes(),
                    self.agent_host().as_ref(),
                ),
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

    /// Whether attaching is offered at all: a model is selected. A file it
    /// does not take inline goes up as a file, which it sees as its label.
    pub(in crate::ui) fn attachments_offered(&self) -> bool {
        self.accepted_media().is_some()
    }

    /// Upload the file at `path` and attach it to the draft once the daemon
    /// stored it. Refuses, without reading it, a path that is no file and a
    /// file larger than the daemon's upload limit; what the model does not
    /// take inline goes up as a file.
    pub(in crate::ui) async fn attach_file(&mut self, path: &str) {
        self.attach_file_as(path, false).await;
    }

    /// [`Self::attach_file`], uploading a file that starts like media but
    /// does not parse as a file when `force_file`, else asking first.
    async fn attach_file_as(&mut self, path: &str, force_file: bool) {
        let Some(path) = expand_home(path.trim()) else {
            self.status = "cannot expand ~ without HOME".into();
            return;
        };
        let shown = path.display().to_string();
        let Some(accepted) = self.accepted_media() else {
            self.status = "select a model before attaching a file".into();
            return;
        };
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
        // A file the model does not take still goes up as a file, up to
        // the daemon's upload limit.
        if metadata.len() > MAX_VIDEO_ATTACHMENT_BYTES {
            self.status = format!(
                "{name} is {}; files are attached up to {}",
                cookie_agent_protocol::human_bytes(metadata.len()),
                cookie_agent_protocol::human_bytes(MAX_VIDEO_ATTACHMENT_BYTES)
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
            let result = upload_file(client, session_id, path, accepted, force_file).await;
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
        result: Result<Uploaded, AttachError>,
    ) {
        match result {
            Ok(Uploaded { artifact, note }) if self.selected == Some(session_id) => {
                self.status = note.unwrap_or_else(|| {
                    format!("attached {}", crate::state::attachment_chip(&artifact))
                });
                if !self
                    .attachments
                    .iter()
                    .any(|attached| attached.sha256 == artifact.sha256)
                {
                    self.file_deliveries.announce = Some(artifact.sha256.clone());
                    self.attachments.push(artifact);
                }
            }
            Ok(_) => {
                self.status = format!("{name} was uploaded for another session; attach it again");
            }
            Err(AttachError::Unreadable { path, reason }) => {
                if self.selected == Some(session_id) {
                    self.attach_offer = Some(AttachOffer {
                        path,
                        pasted: String::new(),
                        unreadable: Some(reason),
                    });
                    self.modal = Modal::AttachConfirm;
                }
            }
            Err(AttachError::Failed(error)) => {
                self.session_errors.record(&error);
                self.status = error;
            }
        }
    }

    /// Asks `attachment.delivery` about the draft's files when they, the
    /// draft's model or agent, or the session's runtimes changed since it
    /// was last asked. A new session's draft, and a daemon before client
    /// protocol 1.105.0, leave the chips to judge from the model's accepted
    /// media.
    pub(super) fn follow_deliveries(&mut self) {
        let question = self
            .selected
            .filter(|_| self.new_session_draft.is_none() && !self.attachments.is_empty())
            .filter(|_| {
                cookie_agent_client_state::uploads::daemon_answers_delivery(
                    self.client.daemon_version(),
                )
            })
            .zip(self.draft.clone())
            .map(|(session_id, selection)| {
                (
                    session_id,
                    selection,
                    self.attachments.clone(),
                    self.agent_runtimes(),
                )
            });
        if question == self.file_deliveries.asked {
            return;
        }
        self.file_deliveries.asked = question.clone();
        self.file_deliveries.request += 1;
        let Some((session_id, selection, files, _)) = question else {
            self.file_deliveries.files.clear();
            return;
        };
        let request = self.file_deliveries.request;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .attachment_delivery(cookie_agent_protocol::AttachmentDeliveryParams {
                    session_id,
                    files,
                    selection,
                })
                .await
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::AttachmentDelivery { request, result });
        });
    }

    /// Installs the answer to `attachment.delivery` request `request` when
    /// it is the latest; a failure leaves the chips to judge for
    /// themselves. The file just attached has its reason said.
    pub(super) fn install_deliveries(
        &mut self,
        request: u64,
        result: Result<cookie_agent_protocol::AttachmentDeliveryResult, String>,
    ) {
        if request != self.file_deliveries.request {
            return;
        }
        self.file_deliveries.files = result
            .map(|answer| {
                answer
                    .files
                    .into_iter()
                    .map(|file| (file.sha256.clone(), file))
                    .collect()
            })
            .unwrap_or_default();
        let Some(announce) = self.file_deliveries.announce.take() else {
            return;
        };
        let attached = self
            .attachments
            .iter()
            .find(|artifact| artifact.sha256 == announce);
        if let Some((artifact, reason)) = attached.and_then(|artifact| {
            let delivery = self.file_deliveries.files.get(&artifact.sha256)?;
            Some((artifact, delivery.reason.clone()?))
        }) {
            self.status = format!("attached {}: {reason}", artifact.display_name());
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
                self.attach_file_as(&offer.path.to_string_lossy(), offer.unreadable.is_some())
                    .await;
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
        if let Some(reason) = offer.unreadable.clone() {
            let body = |width: u16| {
                let mut lines = wrapped_line(
                    Line::styled(format!("{reason}. Attach it as a file?"), theme.heading()),
                    width,
                );
                lines.push(Line::default());
                lines.extend(wrapped_line(
                    Line::styled(path.clone(), theme.muted()),
                    width,
                ));
                lines.push(Line::default());
                lines.extend(wrapped_line(
                    Line::styled(
                        "The model sees its name and size, and can copy it to a machine.",
                        theme.muted_text(),
                    ),
                    width,
                ));
                lines
            };
            self.hit_map.confirm_buttons =
                render_confirm_panel(frame, screen, "Attach as file", body, theme);
            return;
        }
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
    /// `📎 name · type word · size` each, `· name only` after one the model sees
    /// by its label alone and `· can't read` after one nothing it has can
    /// read, as many as fit and a count of the rest.
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
        let accepted = self.accepted_media();
        for (index, artifact) in self.attachments.iter().enumerate() {
            // A file the model sees by its label alone says so, and one
            // nothing the model has can read warns.
            use cookie_agent_client_state::uploads::{ChipBadge, chip_badge};
            let name_only = match chip_badge(
                artifact,
                self.file_deliveries.files.get(&artifact.sha256),
                accepted.as_ref(),
            ) {
                None => "",
                Some(ChipBadge::NameOnly(_)) => " · name only",
                Some(ChipBadge::CantRead(_)) => " · can't read",
            };
            let chip = format!(
                " 📎 {} · {} · {}{name_only} ",
                artifact.display_name(),
                cookie_agent_client_state::media_type_word(&artifact.media_type),
                cookie_agent_protocol::human_bytes(artifact.byte_length),
            );
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
