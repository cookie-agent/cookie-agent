//! `attachment.delivery` (`docs/site/specs/attachment-handling.md` section
//! 10, decision of 2026-11-26): what a message sent now with a draft's
//! selection would give its model or ACP agent of each of the draft's
//! files, decided by the rules a request uses. A native model gets a file
//! [`crate::media::fit_media`] and [`crate::media::elide_excess_media`] keep
//! in its latest user message; an ACP agent one its prompt capabilities
//! take ([`crate::runtime::acp::prompt::prompt_misfit`]). Any other file is
//! its label, whose ending [`crate::media::missing_file_ending`] picks from
//! the tools the request offers, at once or deferred.

use std::sync::Arc;

use cookie_agent_protocol::{
    ArtifactRef, AttachmentDelivery, AttachmentDeliveryParams, AttachmentDeliveryResult,
    AttachmentEnding, FILE_UPLOAD_MEDIA_TYPE, InputMedia, SessionId, SessionOrigin,
    media_sniff::TEXT_MEDIA_TYPE,
    model_text::{FileLabelEnding, user_file_label},
    runtime_protocol::ToolInputMedia,
};

use super::uploads::MediaError;
use crate::{
    media::{FileContent, FileFacts, FileTools},
    policy::{self, FrozenRunPolicy},
    runtime::{Engine, EngineError, blocking_io},
};

/// Who a message goes to, as its delivery is decided.
enum Recipient {
    Model {
        name: String,
        user: Vec<InputMedia>,
        tool: Vec<ToolInputMedia>,
        tools: FileTools,
    },
    Agent {
        name: String,
        caps: crate::runtime::acp::wire::PromptCapabilities,
        tools: FileTools,
    },
}

impl Engine {
    /// `attachment.delivery`.
    pub async fn attachment_delivery(
        &self,
        params: AttachmentDeliveryParams,
    ) -> Result<AttachmentDeliveryResult, EngineError> {
        let session = params.session_id;
        for artifact in &params.files {
            artifact
                .validate()
                .map_err(|error| MediaError::InvalidRequest(error.to_string()))?;
        }
        self.inner.store.get(session)?;
        let contents = self.stored_contents(session, params.files.clone()).await?;
        let recipient = self.recipient(session, &params.selection)?;
        Ok(AttachmentDeliveryResult {
            files: self.deliveries(session, &params.files, &contents, &recipient),
        })
    }

    /// What the stored copy of each of `files` holds: a `file` upload's
    /// content sniffed from its head, any other's its own type.
    async fn stored_contents(
        &self,
        session: SessionId,
        files: Vec<ArtifactRef>,
    ) -> Result<Vec<FileContent>, EngineError> {
        let artifacts = Arc::clone(&self.inner.artifacts);
        blocking_io::run(move || {
            let store = artifacts.read_store(session)?;
            let mut contents = Vec::with_capacity(files.len());
            for artifact in &files {
                // A folder's reference names its manifest and its main
                // file's type; the model only ever gets its label.
                if artifact.folder.is_some() {
                    let held = match &store {
                        Some(store) => {
                            matches!(
                                super::read_folder_manifest(store, &artifact.sha256),
                                Ok(Some(_))
                            )
                        }
                        None => false,
                    };
                    contents.push(match held {
                        false => FileContent::Missing,
                        true if artifact.media_type == TEXT_MEDIA_TYPE => FileContent::Text,
                        true => FileContent::Media(artifact.media_type.clone()),
                    });
                    continue;
                }
                let range = match &store {
                    Some(store) => match store.read_range(&artifact.sha256, 0, 1) {
                        Ok(range) if range.total_bytes == artifact.byte_length => Some(range),
                        Ok(_) => None,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
                            ) =>
                        {
                            None
                        }
                        Err(error) => return Err(error),
                    },
                    None => None,
                };
                contents.push(match range {
                    None => FileContent::Missing,
                    Some(range) if artifact.media_type == FILE_UPLOAD_MEDIA_TYPE => {
                        crate::media::sniffed_content(&range.head, range.total_bytes)
                    }
                    Some(_) if artifact.media_type == TEXT_MEDIA_TYPE => FileContent::Text,
                    Some(_) => FileContent::Media(artifact.media_type.clone()),
                });
            }
            Ok(contents)
        })
        .await
        .map_err(|error| EngineError::ToolFailed(error.to_string()))?
        .map_err(EngineError::from)
    }

    /// Who a message of `session` sent with `selection` goes to, frozen as
    /// a run start freezes it: the model the session's fallback reached, or
    /// the ACP agent on the runtime that runs it for the session.
    fn recipient(
        &self,
        session: SessionId,
        selection: &cookie_agent_protocol::RunSelection,
    ) -> Result<Recipient, EngineError> {
        let projection = self.inner.store.get(session)?;
        let mut selection = selection.clone();
        if selection.acp_runtime.is_none()
            && let Ok(current) = self.session_model_selection(session)
            && current.agent == selection.agent
        {
            selection.acp_runtime = current.acp_runtime;
        }
        let memory = self.memory_variables(session);
        let policy = match &projection.meta.origin {
            SessionOrigin::Root => self.freeze_root_selection(&selection, false, &memory)?.0,
            SessionOrigin::Delegated { .. } => {
                self.freeze_delegated_selection(
                    &projection.creation_agent,
                    projection.meta.creation_selection.preset.as_deref(),
                    &selection,
                    policy::ResultLimits::from_config(&self.inner.config.load().runtime),
                    &memory,
                )?
                .0
            }
        };
        if let Some(offers) = policy.acp_agent() {
            let (recorded, _) = self.acp_host_hints(session, &offers.id)?;
            let agent = offers
                .place(
                    &self.session_runtime_ids(session)?,
                    recorded.as_ref(),
                    selection.acp_runtime.as_ref(),
                )
                .map_err(EngineError::Acp)?;
            return Ok(Recipient::Agent {
                name: offers.id.to_string(),
                caps: crate::runtime::acp::prompt::caps_of(&agent.known.input_media),
                tools: self.acp_file_tools(session, Some(&policy)),
            });
        }
        let remembered = projection.log.state().model().clone();
        let effective = remembered.continuation(
            &selection,
            &policy
                .selected_suffix
                .iter()
                .map(|binding| binding.selection.clone())
                .collect::<Vec<_>>(),
            false,
        );
        let binding = policy
            .selected_suffix
            .iter()
            .find(|binding| binding.selection == effective.model)
            .or_else(|| policy.selected_suffix.first())
            .ok_or(EngineError::NoRunnableModel)?;
        let capabilities = policy
            .model_capabilities(binding)
            .ok_or(EngineError::NoRunnableModel)?;
        let adapter = binding.descriptor.adapter_id.as_str();
        Ok(Recipient::Model {
            name: binding.selection.model.to_string(),
            user: policy::user_input_media(adapter, &capabilities),
            tool: policy::tool_input_media(adapter, &capabilities),
            tools: self.native_file_tools(session, &policy)?,
        })
    }

    /// What a native request of `policy` would offer of the tools that do
    /// something with a file, at once or deferred.
    fn native_file_tools(
        &self,
        session: SessionId,
        policy: &FrozenRunPolicy,
    ) -> Result<FileTools, EngineError> {
        let composition = self.compose_for_policy(session, policy)?;
        let offers = |name: &str| composition.tools.iter().any(|tool| tool.name == name);
        Ok(FileTools {
            read_artifact: offers(crate::base_tools::READ_ARTIFACT),
            transfer: offers(crate::machines_tool::TRANSFER),
        })
    }

    /// Each file's delivery to `recipient`.
    fn deliveries(
        &self,
        session: SessionId,
        files: &[ArtifactRef],
        contents: &[FileContent],
        recipient: &Recipient,
    ) -> Vec<AttachmentDelivery> {
        let (misfits, tool, tools) = match recipient {
            Recipient::Model {
                name,
                user,
                tool,
                tools,
            } => {
                let kinds = files
                    .iter()
                    .map(|artifact| {
                        (
                            crate::media::user_file_kind(&artifact.media_type),
                            artifact.media_type.as_str(),
                            artifact.byte_length,
                        )
                    })
                    .collect::<Vec<_>>();
                (
                    crate::media::user_files_fit(&kinds, name, user),
                    tool.clone(),
                    *tools,
                )
            }
            Recipient::Agent { name, caps, tools } => (
                files
                    .iter()
                    .map(|artifact| {
                        crate::runtime::acp::prompt::prompt_misfit(artifact, name, caps)
                    })
                    .collect(),
                crate::runtime::acp::prompt::bridge_input_media(caps),
                *tools,
            ),
        };
        let takes_transfers = self.session_takes_transfers(session);
        files
            .iter()
            .zip(contents)
            .zip(misfits)
            .map(|((artifact, content), misfit)| {
                let facts = FileFacts {
                    byte_length: artifact.byte_length,
                    content: content.clone(),
                };
                let why = match content {
                    FileContent::Missing => Some("its stored copy is missing or damaged".into()),
                    _ if artifact.media_type == FILE_UPLOAD_MEDIA_TYPE => {
                        Some("it was attached as a file".to_owned())
                    }
                    _ => match &artifact.folder {
                        Some(folder) => Some(format!("it is a folder of {} files", folder.files)),
                        None => misfit,
                    },
                };
                let Some(why) = why else {
                    return AttachmentDelivery {
                        sha256: artifact.sha256.clone(),
                        ending: AttachmentEnding::Included,
                        label: user_file_label(artifact, FileLabelEnding::IncludedBelow),
                        reason: None,
                    };
                };
                let ending = crate::media::missing_file_ending(&facts, &tool, tools);
                let reason = if ending == FileLabelEnding::OnlyName
                    && facts.content != FileContent::Missing
                {
                    let mut reason = why;
                    if crate::media::returns_file(&facts, &tool) && !tools.read_artifact {
                        reason.push_str("; read_artifact is off in this session");
                    }
                    reason.push_str(if takes_transfers {
                        "; transfer is off in this session"
                    } else {
                        "; no machine is attached to copy it to"
                    });
                    reason
                } else {
                    why
                };
                AttachmentDelivery {
                    sha256: artifact.sha256.clone(),
                    ending: AttachmentEnding::of(ending),
                    label: user_file_label(artifact, ending),
                    reason: Some(reason),
                }
            })
            .collect()
    }
}
