use std::collections::HashSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{ArtifactReference, Sha256Digest};

pub const MAX_TOOL_STREAMS: usize = 8;
pub const MAX_TOOL_STREAM_NAME_BYTES: usize = 64;
pub const MAX_TOOL_DELTA_BYTES: usize = 64 * 1024;
pub const MAX_TOOL_DISPLAY_BYTES: usize = 64 * 1024;

pub(crate) fn validate_display(text: &str, maximum: usize) -> bool {
    text.len() <= maximum
        && text
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
}

pub(crate) fn deserialize_delta_display<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let display = Option::<String>::deserialize(deserializer)?;
    if display
        .as_ref()
        .is_some_and(|text| !validate_display(text, crate::SafeDisplayText::MAX_BYTES))
    {
        return Err(serde::de::Error::custom(
            "invalid or oversized display delta",
        ));
    }
    Ok(display)
}

/// The same name grammar is used for declarations, output chunks, and artifact URI suffixes.
pub fn validate_tool_stream_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > MAX_TOOL_STREAM_NAME_BYTES
        || matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(
            "stream names must be 1..64 URI-safe ASCII characters, excluding . and ..".into(),
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolOutputDeclaration {
    #[default]
    Single,
    Named {
        #[serde(deserialize_with = "deserialize_named_streams")]
        #[schemars(length(min = 1, max = 8))]
        streams: Vec<String>,
    },
}

fn deserialize_named_streams<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    let declaration = ToolOutputDeclaration::Named {
        streams: Vec::<String>::deserialize(deserializer)?,
    };
    declaration.validate().map_err(serde::de::Error::custom)?;
    let ToolOutputDeclaration::Named { streams } = declaration else {
        unreachable!()
    };
    Ok(streams)
}

pub(crate) fn deserialize_stream_name<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let name = String::deserialize(deserializer)?;
    validate_tool_stream_name(&name).map_err(serde::de::Error::custom)?;
    Ok(name)
}

impl ToolOutputDeclaration {
    pub fn validate(&self) -> Result<(), String> {
        if let Self::Named { streams } = self {
            if streams.is_empty() || streams.len() > MAX_TOOL_STREAMS {
                return Err("named output must declare between 1 and 8 streams".into());
            }
            let mut names = HashSet::new();
            for name in streams {
                validate_tool_stream_name(name)?;
                if !names.insert(name) {
                    return Err("output stream names must be unique".into());
                }
            }
        }
        Ok(())
    }

    pub fn channels(&self) -> Vec<Option<String>> {
        match self {
            Self::Single => vec![None],
            Self::Named { streams } => streams.iter().cloned().map(Some).collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputChunk {
    pub stream: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolCompletionOutput {
    Single { text: String },
    Named { streams: Vec<ToolOutputChunk> },
    Streamed,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RetainedToolStream {
    pub name: Option<String>,
    pub reference: ArtifactReference,
    pub sha256: Sha256Digest,
    pub byte_length: u64,
    pub line_count: u64,
    pub truncated: bool,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RetainedToolOutput {
    pub reference: ArtifactReference,
    pub streams: Vec<RetainedToolStream>,
    pub incomplete: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputManifest {
    pub streams: Vec<RetainedToolStream>,
}

impl ToolOutputManifest {
    pub fn validate(&self) -> Result<(), String> {
        let names = self
            .streams
            .iter()
            .map(|stream| {
                stream
                    .name
                    .clone()
                    .ok_or_else(|| "manifest stream must be named".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        ToolOutputDeclaration::Named { streams: names }.validate()?;
        for stream in &self.streams {
            if stream.reference.uri != format!("artifact://sha256/{}", stream.sha256) {
                return Err("stream reference and digest do not match".into());
            }
        }
        Ok(())
    }
}

/// Public artifact routes are deliberately distinct from stored ArtifactReference URIs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactReadPath {
    pub digest: Sha256Digest,
    pub stream: Option<String>,
}

impl ArtifactReadPath {
    pub fn parse(path: &str) -> Result<Self, String> {
        let path = path
            .strip_prefix("artifact://")
            .ok_or_else(|| "expected artifact:// URI".to_owned())?;
        let (digest, stream) = path
            .split_once('/')
            .map_or((path, None), |(digest, stream)| (digest, Some(stream)));
        let digest = Sha256Digest::new(digest).map_err(|_| {
            "artifact URI requires a 64-character lowercase SHA-256 digest".to_owned()
        })?;
        if let Some(stream) = stream {
            validate_tool_stream_name(stream)?;
        }
        Ok(Self {
            digest,
            stream: stream.map(str::to_owned),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_declarations_share_bounded_uri_safe_names() {
        for streams in [
            vec![],
            vec!["a".into(); 9],
            vec!["same".into(), "same".into()],
            vec!["../bad".into()],
            vec![".".into()],
            vec!["..".into()],
            vec!["a/b".into()],
            vec!["a\\b".into()],
            vec!["bad?query".into()],
            vec!["bad#fragment".into()],
            vec!["bad\n".into()],
            vec!["x".repeat(65)],
        ] {
            let declaration = ToolOutputDeclaration::Named { streams };
            assert!(declaration.validate().is_err());
            assert!(
                serde_json::from_value::<ToolOutputDeclaration>(
                    serde_json::to_value(declaration).unwrap()
                )
                .is_err()
            );
        }
        let declaration = ToolOutputDeclaration::Named {
            streams: vec!["results".into(), "diagnostics.v2".into(), "EMPTY_1".into()],
        };
        declaration.validate().unwrap();
        assert_eq!(
            declaration.channels(),
            vec![
                Some("results".into()),
                Some("diagnostics.v2".into()),
                Some("EMPTY_1".into())
            ]
        );
    }

    #[test]
    fn public_artifact_uri_parser_is_exact() {
        let digest = "a".repeat(64);
        for path in [
            format!("artifact://{digest}"),
            format!("artifact://{digest}/diagnostics.v2"),
        ] {
            let parsed = ArtifactReadPath::parse(&path).unwrap();
            assert_eq!(parsed.digest.as_str(), digest);
        }
        for path in [
            digest.clone(),
            format!("artifact://sha256/{digest}"),
            format!("artifact://{}", "A".repeat(64)),
            format!("artifact://{}", "a".repeat(63)),
            format!("artifact://{digest}/"),
            format!("artifact://{digest}/../file"),
            format!("artifact://{digest}/.."),
            format!("artifact://{digest}/out/more"),
            format!("artifact://{digest}?query"),
            format!("artifact://{digest}/out#fragment"),
            format!("artifact://{digest}/%2e%2e"),
            format!("artifact://{digest}/bad\\stream"),
        ] {
            assert!(ArtifactReadPath::parse(&path).is_err(), "{path}");
        }
    }
}
