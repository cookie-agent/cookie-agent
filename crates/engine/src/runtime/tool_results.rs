use cookie_agent_protocol::{ArtifactReference, EventPayload, SessionId, ToolCallId};
use serde::Deserialize;

use super::Engine;
use crate::tool_api::ToolError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResultReadPage {
    pub content: String,
    pub next_offset_lines: Option<u64>,
    pub source: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashManifest {
    title: serde_json::Value,
    streams: BashStreams,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashStreams {
    stdout: CapturedArtifact,
    stderr: CapturedArtifact,
}

#[derive(Deserialize)]
struct CapturedArtifact {
    reference: ArtifactReference,
    sha256: String,
}

impl Engine {
    pub fn read_tool_result(
        &self,
        session: SessionId,
        tool_call_id: ToolCallId,
        stream: Option<&str>,
        offset_lines: u64,
        limit_lines: u64,
    ) -> Result<ToolResultReadPage, ToolError> {
        let projection = self
            .inner
            .store
            .get(session)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let events = projection.log.events();
        let result = events
            .iter()
            .rev()
            .find_map(|event| match &event.payload {
                EventPayload::ToolCallTerminated { termination }
                    if termination.tool_call_id == tool_call_id =>
                {
                    termination.result.as_ref()
                }
                _ => None,
            })
            .ok_or_else(|| {
                ToolError::execution(format!(
                    "tool result for call {tool_call_id} is not visible in this session"
                ))
            })?;

        if let Some(truncation) = &result.truncation {
            let digest = artifact_digest(&truncation.retained)?;
            if let Some(stream) = stream {
                let manifest = self
                    .inner
                    .artifacts
                    .read_paged(digest, 0, 1)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                let manifest: BashManifest = serde_json::from_str(&manifest.content)
                    .map_err(|_| ToolError::execution("retained result has no bash streams"))?;
                let _ = manifest.title;
                let artifact = match stream {
                    "stdout" => manifest.streams.stdout,
                    "stderr" => manifest.streams.stderr,
                    _ => return Err(ToolError::execution("stream must be stdout or stderr")),
                };
                if artifact.reference.uri != format!("artifact://sha256/{}", artifact.sha256) {
                    return Err(ToolError::execution("bash stream artifact is invalid"));
                }
                let page = self
                    .inner
                    .artifacts
                    .read_paged(&artifact.sha256, offset_lines, limit_lines)
                    .map_err(|error| ToolError::execution(error.to_string()))?;
                return Ok(ToolResultReadPage {
                    content: page.content,
                    next_offset_lines: page.next_offset_lines,
                    source: format!("truncation.{stream}"),
                });
            }
            let page = self
                .inner
                .artifacts
                .read_paged(digest, offset_lines, limit_lines)
                .map_err(|error| ToolError::execution(error.to_string()))?;
            return Ok(ToolResultReadPage {
                content: page.content,
                next_offset_lines: page.next_offset_lines,
                source: "truncation".into(),
            });
        }

        if stream.is_some() {
            return Err(ToolError::execution(
                "stream is only available for retained bash results",
            ));
        }
        if let Some(retained) = events.iter().rev().find_map(|event| match &event.payload {
            EventPayload::ToolOutputElided {
                tool_call_id: candidate,
                retained,
                ..
            } if *candidate == tool_call_id => Some(retained),
            _ => None,
        }) {
            let page = self
                .inner
                .artifacts
                .read_paged(artifact_digest(retained)?, offset_lines, limit_lines)
                .map_err(|error| ToolError::execution(error.to_string()))?;
            return Ok(ToolResultReadPage {
                content: page.content,
                next_offset_lines: page.next_offset_lines,
                source: "elision".into(),
            });
        }

        let page = page_inline(&result.output, offset_lines, limit_lines)?;
        Ok(ToolResultReadPage {
            content: page.content,
            next_offset_lines: page.next_offset_lines,
            source: "inline".into(),
        })
    }
}

fn artifact_digest(reference: &ArtifactReference) -> Result<&str, ToolError> {
    reference
        .uri
        .strip_prefix("artifact://sha256/")
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| ToolError::execution("tool result artifact reference is invalid"))
}

fn page_inline(
    content: &str,
    offset_lines: u64,
    limit_lines: u64,
) -> Result<super::artifacts::ArtifactPage, ToolError> {
    if limit_lines == 0 {
        return Err(ToolError::execution(
            "tool result page limit must be positive",
        ));
    }
    let offset = usize::try_from(offset_lines).unwrap_or(usize::MAX);
    let limit = usize::try_from(limit_lines).unwrap_or(usize::MAX);
    let mut lines = content.split_inclusive('\n').skip(offset);
    let content = lines.by_ref().take(limit).collect::<String>();
    let has_more = lines.next().is_some();
    Ok(super::artifacts::ArtifactPage {
        content,
        next_offset_lines: has_more.then_some(offset_lines.saturating_add(limit_lines)),
    })
}
