//! Files the next message attaches: from `/attach` and the file picker,
//! the camera, drag and drop, or paste. Each file's content is sniffed in
//! the page as the daemon sniffs it, checked against what the selected
//! model accepts, and uploaded at once in chunks (the shared upload
//! policy); the draft shows it as a chip. A file the model does not take
//! inline goes up as a `file` upload, which the model sees as its label. A new session's draft keeps its
//! files in the page until the first message created the session, then
//! uploads them. An image too large is offered downscaled through a
//! canvas; video never is.

use cookie_agent_client_state::uploads::{AcceptedMedia, UploadFailure, attached_as_file, upload};
use cookie_agent_protocol::{
    ArtifactRef, Client, SessionId, UploadKind, media_sniff::validate_upload,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::attach::{
    Assessment, ImageLimits, assess, downscale_target, image_limits, shrink_again, tightened,
};

/// How many smaller sizes a downscale tries before it gives up.
const DOWNSCALE_TRIES: usize = 6;

/// A file a new session's draft attaches: a chip at once, under a stand-in
/// reference, and uploaded once the first message created the session.
#[derive(Clone)]
pub(super) struct DraftFile {
    artifact: ArtifactRef,
    name: String,
    bytes: Vec<u8>,
    kind: UploadKind,
}

/// An image the page offers to downscale and attach.
#[derive(Clone, Debug, PartialEq)]
pub struct DownscaleOffer {
    pub name: String,
    /// Why it cannot be attached as it is.
    pub reason: String,
    pub limits: ImageLimits,
}

impl Workspace {
    /// What the model of the next message accepts: the draft's model, as
    /// the runtime snapshot describes it.
    pub fn accepted_media(self) -> Option<AcceptedMedia> {
        let draft = self.draft.get()?;
        // An ACP agent takes what it advertised when it last started on the
        // runtime it runs on.
        if let Some(agent) = self.draft_acp_agent() {
            return Some(AcceptedMedia {
                model: agent.id.to_string(),
                media: cookie_agent_client_state::acp::input_media(&agent, &self.agent_runtimes()),
            });
        }
        self.with_runtime(|runtime| {
            let media = runtime
                .snapshot()?
                .models
                .iter()
                .find(|model| model.key == draft.model.model)?
                .input_media
                .clone();
            Some(AcceptedMedia {
                model: runtime.model_names().short_label(&draft.model.model),
                media,
            })
        })
    }

    /// Reads and attaches each of `files`.
    pub fn attach_files(self, files: Vec<web_sys::File>) {
        for file in files {
            spawn_local(async move {
                let name = upload_name(&file.name());
                match crate::files::read_file(&file).await {
                    Ok(bytes) => self.attach_bytes(name, bytes),
                    Err(error) => self.status.set(format!("could not read {name}: {error}")),
                }
            });
        }
    }

    /// Attaches `bytes` as `name`: uploads them when they fit, offers a
    /// downscale for an image that does not, and says why otherwise.
    pub fn attach_bytes(self, name: String, bytes: Vec<u8>) {
        if !self.writable() {
            self.status
                .set("cannot attach files in a read-only session".into());
            return;
        }
        let Some(accepted) = self.accepted_media_untracked() else {
            self.status
                .set("select a model before attaching a file".into());
            return;
        };
        match assess(&name, &bytes, &accepted) {
            Assessment::Attach => self.upload(name, bytes, UploadKind::Media, None),
            Assessment::Downscale { reason, limits } => {
                self.offer_downscale(name, reason, limits, bytes)
            }
            Assessment::AsFile(reason) => {
                let note = attached_as_file(&name, &reason);
                self.upload(name, bytes, UploadKind::File, Some(note));
            }
            Assessment::Refuse(reason) => self.status.set(reason),
        }
    }

    fn accepted_media_untracked(self) -> Option<AcceptedMedia> {
        leptos::prelude::untrack(|| self.accepted_media())
    }

    fn offer_downscale(self, name: String, reason: String, limits: ImageLimits, bytes: Vec<u8>) {
        self.offer_bytes.set_value(bytes);
        self.downscale_offer.set(Some(DownscaleOffer {
            name,
            reason,
            limits,
        }));
    }

    /// Declines the downscale offer.
    pub fn dismiss_downscale(self) {
        self.downscale_offer.set(None);
        self.offer_bytes.set_value(Vec::new());
    }

    /// Accepts the downscale offer: re-encodes the image smaller until it
    /// fits, then attaches it.
    pub fn accept_downscale(self) {
        let Some(offer) = self.downscale_offer.get_untracked() else {
            return;
        };
        let bytes = self
            .offer_bytes
            .try_update_value(std::mem::take)
            .unwrap_or_default();
        self.downscale_offer.set(None);
        let Some(accepted) = self.accepted_media_untracked() else {
            return;
        };
        let media_type = validate_upload(&bytes)
            .map(|upload| upload.media_type)
            .unwrap_or("image/png");
        // JPEG where the model takes it: it shrinks photos the most.
        let jpeg = accepted
            .check(&offer.name, "image", "image/jpeg", 0, None)
            .is_ok();
        let (encoding, extension) = if jpeg {
            ("image/jpeg", "jpg")
        } else {
            ("image/png", "png")
        };
        let name = renamed(&offer.name, extension);
        self.status.set(format!("downscaling {}…", offer.name));
        spawn_local(async move {
            let image = match crate::files::decode_image(&bytes, media_type).await {
                Ok(image) => image,
                Err(error) => {
                    self.status.set(format!(
                        "could not read {} as an image: {error}",
                        offer.name
                    ));
                    return;
                }
            };
            let (width, height) = (image.natural_width(), image.natural_height());
            let mut size = downscale_target(width, height, bytes.len() as u64, offer.limits)
                .unwrap_or_else(|| shrink_again(width, height));
            for _ in 0..DOWNSCALE_TRIES {
                let encoded = match crate::files::encode_image(&image, size, encoding) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        self.status.set(format!("could not downscale: {error}"));
                        return;
                    }
                };
                let fits = encoded.len() as u64 <= offer.limits.max_bytes;
                match assess(&name, &encoded, &accepted) {
                    Assessment::Attach if fits => {
                        self.status.set(format!(
                            "downscaled {} to {}×{}",
                            offer.name, size.0, size.1
                        ));
                        self.upload(name, encoded, UploadKind::Media, None);
                        return;
                    }
                    Assessment::Refuse(reason) | Assessment::AsFile(reason) => {
                        self.status.set(reason);
                        return;
                    }
                    Assessment::Attach | Assessment::Downscale { .. } => {
                        size = shrink_again(size.0, size.1);
                    }
                }
            }
            self.status
                .set(format!("{} could not be made small enough", offer.name));
        });
    }

    /// Uploads `bytes` into the viewed session's store and adds the file
    /// to the draft; a new session's draft keeps them until its first
    /// message.
    fn upload(self, name: String, bytes: Vec<u8>, kind: UploadKind, note: Option<String>) {
        if self.new_session.get_untracked() {
            self.hold_draft_file(name, bytes, kind);
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        let Some(session_id) = self.selected.get_untracked() else {
            return;
        };
        self.status.set(format!("attaching {name}…"));
        spawn_local(async move {
            match upload(&client, session_id, &name, Some(name.clone()), &bytes, kind).await {
                // It joins its session's draft, shown or put aside.
                Ok(artifact) => {
                    self.status.set(note.unwrap_or_else(|| {
                        format!(
                            "attached {}",
                            cookie_agent_client_state::attachment_chip(&artifact)
                        )
                    }));
                    self.attach_to_draft(session_id, artifact);
                }
                Err(failure) => self.refuse_upload(name, bytes, failure),
            }
        });
    }

    fn add_attachment(self, artifact: ArtifactRef) {
        self.attachments.update(|attachments| {
            if !attachments
                .iter()
                .any(|attached| attached.sha256 == artifact.sha256)
            {
                attachments.push(artifact);
            }
        });
    }

    /// Says why `name` was not uploaded, offering a smaller copy of an
    /// image the daemon found too large.
    fn refuse_upload(self, name: String, bytes: Vec<u8>, failure: UploadFailure) {
        let image = validate_upload(&bytes).is_ok_and(|upload| upload.kind == "image");
        let limits = self
            .accepted_media_untracked()
            .as_ref()
            .and_then(image_limits);
        match (failure.too_large, limits) {
            (Some(refused), Some(limits)) if image => {
                self.offer_downscale(name, failure.message, tightened(limits, refused), bytes);
            }
            _ => self.status.set(failure.message),
        }
    }

    /// Keeps a new session's file in the page, as a chip under a stand-in
    /// reference, until the first message created the session.
    fn hold_draft_file(self, name: String, bytes: Vec<u8>, kind: UploadKind) {
        self.next_draft_file.update_value(|next| *next += 1);
        let media_type = match kind {
            UploadKind::File => cookie_agent_protocol::FILE_UPLOAD_MEDIA_TYPE.to_owned(),
            UploadKind::Media => validate_upload(&bytes).map_or_else(
                |_| cookie_agent_protocol::FILE_UPLOAD_MEDIA_TYPE.to_owned(),
                |upload| upload.media_type.to_owned(),
            ),
        };
        let artifact = ArtifactRef {
            sha256: format!("{:064x}", self.next_draft_file.get_value()),
            byte_length: bytes.len() as u64,
            media_type,
            filename: Some(name.clone()),
        };
        self.status.set(format!(
            "attached {}; it is uploaded with the first message",
            cookie_agent_client_state::attachment_chip(&artifact)
        ));
        self.add_attachment(artifact.clone());
        self.draft_files.update_value(|held| {
            held.push(DraftFile {
                artifact,
                name,
                bytes,
                kind,
            });
        });
    }

    /// Whether `files` holds a file of the new session's draft.
    pub(super) fn holds_draft_files(self, files: &[ArtifactRef]) -> bool {
        self.draft_files.with_value(|held| {
            files
                .iter()
                .any(|file| held.iter().any(|draft| draft.artifact == *file))
        })
    }

    /// Uploads the draft's files among `files` into `session_id`, which the
    /// first message just created: `files` with each one's own reference,
    /// or, when one was not uploaded (which the status line says), the
    /// ones that were.
    pub(super) async fn upload_draft_files(
        self,
        client: &Client,
        session_id: SessionId,
        files: Vec<ArtifactRef>,
    ) -> Result<Vec<ArtifactRef>, Vec<ArtifactRef>> {
        let held = self.draft_files.with_value(Clone::clone);
        let mut uploaded = Vec::new();
        let mut refused = false;
        for file in files {
            let Some(draft) = held.iter().find(|draft| draft.artifact == file) else {
                uploaded.push(file);
                continue;
            };
            match upload(
                client,
                session_id,
                &draft.name,
                Some(draft.name.clone()),
                &draft.bytes,
                draft.kind,
            )
            .await
            {
                Ok(artifact) => uploaded.push(artifact),
                Err(failure) => {
                    refused = true;
                    self.refuse_upload(draft.name.clone(), draft.bytes.clone(), failure);
                }
            }
        }
        self.draft_files.set_value(Vec::new());
        if refused { Err(uploaded) } else { Ok(uploaded) }
    }

    /// Forgets the files a new session's draft kept.
    /// Takes the file `sha256` off the draft.
    pub fn remove_attachment(self, sha256: &str) {
        self.attachments
            .update(|attachments| attachments.retain(|artifact| artifact.sha256 != sha256));
        self.draft_files
            .update_value(|held| held.retain(|draft| draft.artifact.sha256 != sha256));
    }
}

/// The name a file is attached under: its name without a directory, cut to
/// what an upload takes; `file` when it has none.
fn upload_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    if name.is_empty() {
        return "file".into();
    }
    let mut end = name.len().min(ArtifactRef::MAX_FILENAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// `name` with its extension replaced by `extension`.
fn renamed(name: &str, extension: &str) -> String {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    upload_name(&format!("{stem}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::{renamed, upload_name};

    #[test]
    fn attached_names_drop_directories_and_downscales_take_their_encoding() {
        assert_eq!(upload_name("C:\\photos\\cat.png"), "cat.png");
        assert_eq!(upload_name("  "), "file");
        assert_eq!(upload_name(&"é".repeat(200)).len(), 254);
        assert_eq!(renamed("cat.photo.png", "jpg"), "cat.photo.jpg");
        assert_eq!(renamed("scan", "png"), "scan.png");
    }
}
