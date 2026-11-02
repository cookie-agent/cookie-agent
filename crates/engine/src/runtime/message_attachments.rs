//! Attachments in assistant messages
//! (`docs/site/specs/message-attachments.md`): each `<attach …/>` tag of a
//! committed model turn, native or an ACP agent's, resolved once, in the
//! background: a file read from a machine into the session tree's
//! artifacts, an artifact of the tree, or an asset shown. The outcome is an
//! `assistant_attachment` event; a tag whose rule asks waits for the user's
//! `attachment.decide`; a failure is told to the model before its next
//! request.

use std::{collections::HashMap, sync::Mutex};

use cookie_agent_protocol::{
    ASSET_MAX_BYTES, ATTACH_MAX_PER_MESSAGE, ApprovalResourceSource, AssetStamp, AttachSource,
    AttachTag, AttachmentDecideParams, AttachmentDecideResult, AttachmentOutcome,
    AttachmentSaveAssetParams, AttachmentSaveAssetResult, EventPayload as Event, MimeType,
    PermissionAction, PermissionEffect, PersistedAssistantPart, PersistedFilePart,
    PersistedFileSource, PersistedModelTurn, PreparedBindingLifetime, RuntimeId, SessionId,
    Sha256Digest,
    runtime_protocol::{FsDirection, FsKind},
};
use cookie_agent_runtime_api::authoring::{prepared_operation, prepared_resource};

use super::{Engine, EngineError, event_origin};
use crate::assets::AssetError;

/// A request about an attachment that cannot be answered.
fn refused(message: &str) -> EngineError {
    EngineError::Asset(AssetError::InvalidStore(message.to_owned()))
}

/// Where a tag stands: its turn, its text part, and its order there.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TagKey {
    session: SessionId,
    model_turn_seq: u64,
    content_index: u32,
    ordinal: u32,
}

/// A tag waiting for the user, with the machine its file is read from by
/// default.
#[derive(Clone, Debug)]
struct Waiting {
    tag: AttachTag,
    default_machine: Option<String>,
}

/// The engine's attachments in flight: those waiting for the user, and the
/// failures each session's model has yet to hear of.
#[derive(Debug, Default)]
pub(crate) struct MessageAttachments {
    waiting: Mutex<HashMap<TagKey, Waiting>>,
    notes: Mutex<HashMap<SessionId, Vec<String>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the rules said of a tag's read.
enum Checked {
    Allowed,
    Asks,
    Denied,
}

/// The line a failure is told to the model as.
fn failure_note(tag: &AttachTag, reason: &str) -> String {
    let named = match &tag.source {
        AttachSource::Path {
            path,
            machine: Some(machine),
        } => format!("{path} on {machine}"),
        AttachSource::Path { path, .. } => path.clone(),
        AttachSource::Artifact { sha256 } => format!("artifact {sha256}"),
        AttachSource::Asset { handle } => handle.clone(),
    };
    format!("[attachment not attached] {named}: {reason}")
}

impl Engine {
    /// Resolves, in the background, every attach tag of the committed turn
    /// `turn` of `session`, under the rules of `agent`. `default_machine`
    /// is where a tag without `machine` reads from: an ACP agent's own
    /// machine, else the session's workspace's.
    pub(crate) fn attach_turn(
        &self,
        session: SessionId,
        model_turn_seq: u64,
        turn: &PersistedModelTurn,
        agent: &cookie_agent_protocol::AgentSnapshot,
        default_machine: Option<String>,
    ) {
        let mut tags = Vec::new();
        for (index, part) in turn.content.iter().enumerate() {
            let PersistedAssistantPart::Text { text, .. } = part else {
                continue;
            };
            let content_index = u32::try_from(index).unwrap_or(u32::MAX);
            for (ordinal, tag) in cookie_agent_protocol::attach_tags(text)
                .into_iter()
                .enumerate()
            {
                let key = TagKey {
                    session,
                    model_turn_seq,
                    content_index,
                    ordinal: u32::try_from(ordinal).unwrap_or(u32::MAX),
                };
                tags.push((key, tag));
            }
        }
        if tags.is_empty() {
            return;
        }
        let engine = self.clone();
        let agent = agent.clone();
        tokio::spawn(async move {
            for (count, (key, tag)) in tags.into_iter().enumerate() {
                let outcome = if count >= ATTACH_MAX_PER_MESSAGE {
                    AttachmentOutcome::Failed {
                        reason: format!(
                            "too many attachments in one message (at most {ATTACH_MAX_PER_MESSAGE})"
                        ),
                    }
                } else {
                    engine
                        .resolve_attachment(key, &tag, default_machine.as_deref(), Some(&agent))
                        .await
                };
                engine
                    .record_attachment(key, tag, outcome, default_machine.clone())
                    .await;
            }
        });
    }

    /// Records `outcome` for the tag at `key`: what waits for the user is
    /// remembered, and a failure kept for the model's next request.
    async fn record_attachment(
        &self,
        key: TagKey,
        tag: AttachTag,
        outcome: AttachmentOutcome,
        default_machine: Option<String>,
    ) {
        {
            let state = &self.inner.message_attachments;
            match &outcome {
                AttachmentOutcome::PendingApproval => {
                    lock(&state.waiting).insert(
                        key,
                        Waiting {
                            tag: tag.clone(),
                            default_machine,
                        },
                    );
                }
                AttachmentOutcome::Failed { reason } => {
                    lock(&state.waiting).remove(&key);
                    lock(&state.notes)
                        .entry(key.session)
                        .or_default()
                        .push(failure_note(&tag, reason));
                }
                AttachmentOutcome::Attached { .. } | AttachmentOutcome::Shown { .. } => {
                    lock(&state.waiting).remove(&key);
                }
            }
        }
        let event = Event::AssistantAttachment {
            model_turn_seq: key.model_turn_seq,
            content_index: key.content_index,
            ordinal: key.ordinal,
            tag,
            outcome,
        };
        let session = key.session;
        let appended = self
            .on_actor(session, move |engine| {
                engine.append_direct(session, None, event_origin("engine:attachments"), event)
            })
            .await;
        if let Err(error) = appended {
            eprintln!("cookie-agent: an attachment of session {session} was not recorded: {error}");
        }
    }

    /// The failures of `session`'s attachments its model has not heard of,
    /// as one message, which they are forgotten for.
    pub(crate) fn take_attachment_notes(&self, session: SessionId) -> Option<String> {
        let notes = lock(&self.inner.message_attachments.notes).remove(&session)?;
        (!notes.is_empty()).then(|| notes.join("\n"))
    }

    /// Records `note`, failures of attachments, as user history: the
    /// model reads it with its next request, and the transcript shows it.
    pub(crate) async fn record_attachment_notes(
        &self,
        session: SessionId,
        run: Option<cookie_agent_protocol::RunId>,
        note: &str,
    ) -> Result<(), EngineError> {
        let text = format!(
            "{}\n{note}",
            cookie_agent_protocol::model_text::TOOL_EMITTED_SYSTEM_USER_MARKER
        );
        self.append(
            session,
            run,
            event_origin("engine:attachments"),
            Event::MessageInjected {
                role: cookie_agent_protocol::ExtensionMessageRole::User,
                content: cookie_agent_protocol::UserContent::text(text),
            },
        )
        .await?;
        Ok(())
    }

    /// Resolves `tag`: under `agent`'s rules, or without any when the user
    /// allowed it (`agent` is `None`).
    async fn resolve_attachment(
        &self,
        key: TagKey,
        tag: &AttachTag,
        default_machine: Option<&str>,
        agent: Option<&cookie_agent_protocol::AgentSnapshot>,
    ) -> AttachmentOutcome {
        let result = match &tag.source {
            AttachSource::Path { path, machine } => {
                self.attach_file(
                    key.session,
                    path,
                    machine.as_deref().or(default_machine),
                    agent,
                )
                .await
            }
            AttachSource::Artifact { sha256 } => self.attach_artifact(key.session, tag, sha256),
            AttachSource::Asset { handle } => self.show_asset_tag(key.session, handle, agent).await,
        };
        result.unwrap_or_else(|reason| AttachmentOutcome::Failed { reason })
    }

    /// What `agent`'s rules say of `action` on `label` (bound as
    /// `binding`) in `scope`; allowed without rules.
    fn check_attachment(
        &self,
        session: SessionId,
        agent: Option<&cookie_agent_protocol::AgentSnapshot>,
        action: PermissionAction,
        label: &str,
        binding: &str,
        scope: &cookie_agent_runtime_api::ResourceScope,
    ) -> Result<Checked, String> {
        let Some(agent) = agent else {
            return Ok(Checked::Allowed);
        };
        let resource = prepared_resource(
            action.clone(),
            "path",
            label.as_bytes(),
            binding.as_bytes(),
            PreparedBindingLifetime::ProcessLocal,
            ApprovalResourceSource::PrimaryOperation,
        )
        .map_err(|error| error.to_string())?;
        let operation = prepared_operation(
            "attach",
            &serde_json::json!({ "resource": label }),
            vec![(action, "attach")],
            vec![resource],
            session.to_string().as_bytes(),
        )
        .map_err(|error| error.to_string())?;
        let overlay = self
            .inner
            .store
            .get(session)
            .map_err(|error| error.to_string())?
            .permission_overlay
            .clone();
        let decision = self.inner.permissions.decide_operation_with_grants(
            agent,
            Some(&overlay),
            None,
            &operation,
            &[Some(label.to_owned())],
            crate::permissions::PermissionScope::runtime(
                &scope.runtime,
                scope.workspace.as_deref(),
            )
            .with_home(scope.home.as_deref())
            .with_platform(scope.platform.as_deref()),
        );
        Ok(match decision.effect {
            PermissionEffect::Allow => Checked::Allowed,
            PermissionEffect::Ask => Checked::Asks,
            PermissionEffect::Deny => Checked::Denied,
        })
    }

    /// Reads the file `path` of `machine` once into the session tree's
    /// artifacts.
    async fn attach_file(
        &self,
        session: SessionId,
        path: &str,
        machine: Option<&str>,
        agent: Option<&cookie_agent_protocol::AgentSnapshot>,
    ) -> Result<AttachmentOutcome, String> {
        let machine = match machine {
            Some(machine) => machine.to_owned(),
            None => self
                .default_asset_machine(session)
                .map_err(|error| error.to_string())?,
        };
        let endpoint = self.transfer_endpoint(session, &machine)?;
        let prepared = endpoint
            .connection
            .fs_prepare(path, FsDirection::Read)
            .await?;
        let client = endpoint.connection.client.clone();
        let close = |client: crate::runtimes::RuntimeClient, route: u64| async move {
            client.fs_close(route).await;
        };
        match prepared.kind {
            None => {
                close(client, prepared.route).await;
                return Err(format!(
                    "{} on {} does not exist",
                    prepared.path, endpoint.machine
                ));
            }
            Some(FsKind::Folder) => {
                close(client, prepared.route).await;
                return Err(format!(
                    "{} on {} is a folder",
                    prepared.path, endpoint.machine
                ));
            }
            Some(_) => {}
        }
        if let Some(size) = prepared.size
            && size > ASSET_MAX_BYTES
        {
            close(client, prepared.route).await;
            return Err(format!(
                "it holds {}, more than an attachment's {}",
                cookie_agent_protocol::human_bytes(size),
                cookie_agent_protocol::human_bytes(ASSET_MAX_BYTES)
            ));
        }
        match self.check_attachment(
            session,
            agent,
            PermissionAction::READ,
            &prepared.label,
            &prepared.binding,
            &endpoint.scope,
        )? {
            Checked::Allowed => {}
            Checked::Asks => {
                close(client, prepared.route).await;
                return Ok(AttachmentOutcome::PendingApproval);
            }
            Checked::Denied => {
                close(client, prepared.route).await;
                return Err("reading it is not allowed".into());
            }
        }
        let bytes = crate::asset_tools::read_route(&client, prepared.route).await?;
        let name = prepared
            .path
            .rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or("file")
            .to_owned();
        self.store_attachment(session, &bytes, &name)
    }

    /// Keeps `bytes` in the session tree's artifacts, as the file `name`.
    fn store_attachment(
        &self,
        session: SessionId,
        bytes: &[u8],
        name: &str,
    ) -> Result<AttachmentOutcome, String> {
        let (reference, digest) = self
            .inner
            .artifacts
            .retain(session, bytes)
            .map_err(|error| format!("the file was not stored: {error}"))?;
        let filename = name.to_owned();
        let media_type = crate::assets::media_type(&filename, bytes);
        Ok(AttachmentOutcome::Attached {
            file: PersistedFilePart {
                media_type: MimeType::new(media_type).map_err(|error| error.to_string())?,
                filename: Some(filename),
                source: PersistedFileSource::Artifact {
                    byte_length: bytes.len() as u64,
                    sha256: Sha256Digest::new(digest).map_err(|error| error.to_string())?,
                    reference,
                },
                metadata: None,
            },
        })
    }

    /// The session tree's artifact `sha256`, attached as it is.
    fn attach_artifact(
        &self,
        session: SessionId,
        tag: &AttachTag,
        sha256: &str,
    ) -> Result<AttachmentOutcome, String> {
        let digest = Sha256Digest::new(sha256)
            .map_err(|_| "an artifact is named by its 64-character hex SHA-256".to_owned())?;
        let store = self
            .inner
            .artifacts
            .read_store(session)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "this session has no artifacts".to_owned())?;
        let range = store
            .read_range(digest.as_str(), 0, 0)
            .map_err(|_| "this session has no such artifact".to_owned())?;
        if range.total_bytes > ASSET_MAX_BYTES {
            return Err(format!(
                "it holds {}, more than an attachment's {}",
                cookie_agent_protocol::human_bytes(range.total_bytes),
                cookie_agent_protocol::human_bytes(ASSET_MAX_BYTES)
            ));
        }
        let filename = tag.label();
        let media_type = crate::assets::media_type(&filename, &range.head);
        Ok(AttachmentOutcome::Attached {
            file: PersistedFilePart {
                media_type: MimeType::new(media_type).map_err(|error| error.to_string())?,
                filename: Some(filename),
                source: PersistedFileSource::Artifact {
                    byte_length: range.total_bytes,
                    sha256: digest.clone(),
                    reference: cookie_agent_protocol::ArtifactReference {
                        uri: format!("artifact://sha256/{}", digest.as_str()),
                    },
                },
                metadata: None,
            },
        })
    }

    /// The asset `handle`, shown when `agent`'s `asset` rule allows reading
    /// it.
    async fn show_asset_tag(
        &self,
        session: SessionId,
        handle: &str,
        agent: Option<&cookie_agent_protocol::AgentSnapshot>,
    ) -> Result<AttachmentOutcome, String> {
        let reference = handle.to_owned();
        let asset = self
            .assets_blocking(move |engine, _| {
                let stores = engine.session_asset_stores(session);
                crate::asset_tools::resolve(engine, &stores, &reference).map_err(|error| {
                    let message = error.to_string();
                    let message = message
                        .strip_prefix("tool failed: ")
                        .unwrap_or(&message)
                        .to_owned();
                    EngineError::Asset(AssetError::InvalidStore(message))
                })
            })
            .await
            .map_err(|error| match error {
                EngineError::Asset(AssetError::InvalidStore(message)) => message,
                error => error.to_string(),
            })?;
        let scope = cookie_agent_runtime_api::ResourceScope {
            runtime: RuntimeId::GLOBAL.to_owned(),
            ..cookie_agent_runtime_api::ResourceScope::default()
        };
        match self.check_attachment(
            session,
            agent,
            PermissionAction::ASSET,
            "read",
            "read",
            &scope,
        )? {
            Checked::Allowed => Ok(AttachmentOutcome::Shown {
                asset: asset.block(),
            }),
            Checked::Asks => Ok(AttachmentOutcome::PendingApproval),
            Checked::Denied => Err("showing assets is not allowed".into()),
        }
    }

    /// The latest record of the tag at `key`: from memory while it waits,
    /// else from the session's log.
    fn attachment_record(&self, key: TagKey) -> Option<(AttachTag, AttachmentOutcome)> {
        let events = self.inner.store.log(key.session).ok()?.event_snapshot();
        events.iter().rev().find_map(|event| match &event.payload {
            Event::AssistantAttachment {
                model_turn_seq,
                content_index,
                ordinal,
                tag,
                outcome,
            } if *model_turn_seq == key.model_turn_seq
                && *content_index == key.content_index
                && *ordinal == key.ordinal =>
            {
                Some((tag.clone(), outcome.clone()))
            }
            _ => None,
        })
    }

    /// `attachment.decide`.
    pub async fn decide_attachment(
        &self,
        params: AttachmentDecideParams,
    ) -> Result<AttachmentDecideResult, EngineError> {
        let key = TagKey {
            session: params.session_id,
            model_turn_seq: params.model_turn_seq,
            content_index: params.content_index,
            ordinal: params.ordinal,
        };
        let waiting = lock(&self.inner.message_attachments.waiting).remove(&key);
        let waiting = match waiting {
            Some(waiting) => waiting,
            // Asked before this daemon started: the log says what.
            None => match self.attachment_record(key) {
                Some((tag, AttachmentOutcome::PendingApproval)) => Waiting {
                    tag,
                    default_machine: None,
                },
                _ => {
                    return Err(refused("that attachment does not wait for an answer"));
                }
            },
        };
        let outcome = if params.allow {
            self.resolve_attachment(key, &waiting.tag, waiting.default_machine.as_deref(), None)
                .await
        } else {
            AttachmentOutcome::Failed {
                reason: "the user did not allow it".into(),
            }
        };
        self.record_attachment(key, waiting.tag, outcome, waiting.default_machine)
            .await;
        Ok(AttachmentDecideResult {})
    }

    /// `attachment.save_asset`: an asset of an attached file's bytes, in the
    /// scope the user chose. The user's own action: no rule is checked.
    pub async fn save_attachment_asset(
        &self,
        params: AttachmentSaveAssetParams,
    ) -> Result<AttachmentSaveAssetResult, EngineError> {
        let key = TagKey {
            session: params.session_id,
            model_turn_seq: params.model_turn_seq,
            content_index: params.content_index,
            ordinal: params.ordinal,
        };
        let Some((tag, AttachmentOutcome::Attached { file })) = self.attachment_record(key) else {
            return Err(refused("only an attached file can be saved as an asset"));
        };
        let PersistedFileSource::Artifact { sha256, .. } = &file.source else {
            return Err(refused("that attachment holds no stored file"));
        };
        let store = self
            .inner
            .artifacts
            .read_store(key.session)
            .map_err(|error| EngineError::Asset(AssetError::Store(error.to_string())))?
            .ok_or_else(|| refused("this session has no artifacts"))?;
        let range = store
            .read_range(sha256.as_str(), 0, ASSET_MAX_BYTES)
            .map_err(|error| EngineError::Asset(AssetError::Store(error.to_string())))?;
        let session = key.session;
        let scope = params.scope;
        let write = crate::assets::AssetWrite {
            replace: None,
            title: Some(tag.label()),
            filename: file.filename.clone(),
            media_type: file.media_type.as_str().to_owned(),
            bytes: range.data,
            stamp: AssetStamp {
                at: jiff::Timestamp::now(),
                session,
                agent: None,
            },
        };
        let asset = self
            .assets_blocking(move |engine, stores| {
                let target = engine.session_asset_stores(session).of(scope).clone();
                let (asset, _) = stores.write(&target, write, &std::collections::HashSet::new())?;
                Ok(asset)
            })
            .await?;
        Ok(AttachmentSaveAssetResult { asset })
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::AttachSource;

    use super::*;

    #[test]
    fn a_failure_names_what_the_tag_named_and_why() {
        let tag = |source| AttachTag {
            source,
            title: None,
        };
        assert_eq!(
            failure_note(
                &tag(AttachSource::Path {
                    path: "runs/loss.png".into(),
                    machine: Some("labpc".into())
                }),
                "no such file"
            ),
            "[attachment not attached] runs/loss.png on labpc: no such file"
        );
        assert_eq!(
            failure_note(
                &tag(AttachSource::Asset {
                    handle: "asset_3f9a1c2e".into()
                }),
                "denied"
            ),
            "[attachment not attached] asset_3f9a1c2e: denied"
        );
    }
}
