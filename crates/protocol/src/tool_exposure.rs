//! Tool exposure (`docs/site/specs/session-tool-groups.md`): which tool
//! groups a session is offered, on, deferred, or off, and which skills it
//! loads. Agent documents, a project's defaults, and each session hold
//! rules; the highest layer with a rule matching a group decides.

use std::{borrow::Cow, fmt, str::FromStr};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{RuntimeId, SessionId};

pub const SESSION_TOOLS_GET_METHOD: &str = "session.tools.get";
pub const SESSION_TOOLS_SET_METHOD: &str = "session.tools.set";
pub const RUNTIMES_GROUPS_METHOD: &str = "runtimes.groups";

/// The most rules one list of tool group or skill rules holds.
pub const MAX_EXPOSURE_RULES: usize = 256;

/// What kind of group a selector names.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroupKind {
    /// A built-in provider's group (`coding`, `memory`), or one a runtime
    /// declares.
    Builtin,
    /// An MCP server's tools.
    Mcp,
    /// A plugin's tools.
    Plugin,
}

impl ToolGroupKind {
    /// The prefix a selector of this kind starts with.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Builtin => "",
            Self::Mcp => "mcp:",
            Self::Plugin => "plugin:",
        }
    }
}

/// A tool group as a rule names it: `coding@labpc`, `memory`, `mcp:exa`,
/// `mcp:playwright@local`, `plugin:browser`. The name may hold `*` and `?`
/// (a pattern) in an agent document; sessions and projects name groups
/// exactly.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ToolGroupSelector {
    kind: ToolGroupKind,
    name: String,
    runtime: Option<RuntimeId>,
}

/// Why a selector was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolGroupSelectorError(pub String);

impl fmt::Display for ToolGroupSelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ToolGroupSelectorError {}

impl ToolGroupSelector {
    /// The most bytes a group name holds.
    pub const MAX_NAME_BYTES: usize = 128;

    /// A selector of `kind` for `name`, on `runtime` when given.
    pub fn new(
        kind: ToolGroupKind,
        name: impl Into<String>,
        runtime: Option<RuntimeId>,
    ) -> Result<Self, ToolGroupSelectorError> {
        let name = name.into();
        if name.is_empty()
            || name.len() > Self::MAX_NAME_BYTES
            || name.chars().any(|character| {
                character.is_control() || character.is_whitespace() || character == '@'
            })
        {
            return Err(ToolGroupSelectorError(format!(
                "`{name}` is not a tool group name: 1 to {} bytes, without spaces, control characters, or `@`",
                Self::MAX_NAME_BYTES
            )));
        }
        Ok(Self {
            kind,
            name,
            runtime,
        })
    }

    #[must_use]
    pub const fn kind(&self) -> ToolGroupKind {
        self.kind
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn runtime(&self) -> Option<&RuntimeId> {
        self.runtime.as_ref()
    }

    /// Whether the name is a pattern (holds `*` or `?`).
    #[must_use]
    pub fn is_pattern(&self) -> bool {
        self.name.contains(['*', '?'])
    }

    /// Whether this selector matches the group `kind`/`name` offered on
    /// `runtime` (none for the base layer and `global`).
    #[must_use]
    pub fn matches(&self, kind: ToolGroupKind, name: &str, runtime: Option<&RuntimeId>) -> bool {
        self.kind == kind
            && self.runtime.as_ref().is_none_or(|own| Some(own) == runtime)
            && wildcard_match(&self.name, name)
    }

    /// How specific the selector is, for ranking matches within one layer:
    /// more literal characters, then fewer wildcards, then a runtime.
    #[must_use]
    pub fn specificity(&self) -> (usize, std::cmp::Reverse<usize>, bool) {
        let wildcards = self.name.chars().filter(|c| matches!(c, '*' | '?')).count();
        (
            self.name.chars().count() - wildcards,
            std::cmp::Reverse(wildcards),
            self.runtime.is_some(),
        )
    }
}

/// `*` matches any characters, `?` one.
fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let (mut p, mut v) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = v;
            p += 1;
        } else if let Some(at) = star {
            p = at + 1;
            mark += 1;
            v = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

impl fmt::Display for ToolGroupSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}{}", self.kind.prefix(), self.name)?;
        if let Some(runtime) = &self.runtime {
            write!(formatter, "@{runtime}")?;
        }
        Ok(())
    }
}

impl FromStr for ToolGroupSelector {
    type Err = ToolGroupSelectorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (kind, rest) = if let Some(rest) = value.strip_prefix("mcp:") {
            (ToolGroupKind::Mcp, rest)
        } else if let Some(rest) = value.strip_prefix("plugin:") {
            (ToolGroupKind::Plugin, rest)
        } else {
            (ToolGroupKind::Builtin, value)
        };
        let (name, runtime) = match rest.rsplit_once('@') {
            Some((name, runtime)) => (
                name,
                Some(RuntimeId::new(runtime).map_err(|_| {
                    ToolGroupSelectorError(format!("`{runtime}` in `{value}` is not a runtime ID"))
                })?),
            ),
            None => (rest, None),
        };
        if kind == ToolGroupKind::Plugin && runtime.is_some() {
            return Err(ToolGroupSelectorError(format!(
                "`{value}`: a plugin's group takes no runtime"
            )));
        }
        Self::new(kind, name, runtime)
    }
}

impl Serialize for ToolGroupSelector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ToolGroupSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|error| de::Error::custom(format!("invalid tool group selector: {error}")))
    }
}

impl JsonSchema for ToolGroupSelector {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "ToolGroupSelector".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 168,
            "pattern": "^(?:mcp:|plugin:)?[^@\\s\\u0000-\\u001f\\u007f-\\u009f]+(?:@[a-z][a-z0-9_]{0,31})?$"
        })
    }
}

/// A skill as a rule names it: its published name (`release-check`,
/// `local_deploy`), with `*` and `?` in an agent document.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SkillPattern(String);

impl SkillPattern {
    pub const MAX_BYTES: usize = 128;

    pub fn new(value: impl Into<String>) -> Result<Self, ToolGroupSelectorError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > Self::MAX_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'*' | b'?')
            })
        {
            return Err(ToolGroupSelectorError(format!(
                "`{value}` is not a skill name: 1 to {} bytes of lowercase letters, digits, `-`, `_`, `*`, and `?`",
                Self::MAX_BYTES
            )));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_pattern(&self) -> bool {
        self.0.contains(['*', '?'])
    }

    #[must_use]
    pub fn matches(&self, skill: &str) -> bool {
        wildcard_match(&self.0, skill)
    }

    #[must_use]
    pub fn specificity(&self) -> (usize, std::cmp::Reverse<usize>) {
        let wildcards = self.0.chars().filter(|c| matches!(c, '*' | '?')).count();
        (
            self.0.chars().count() - wildcards,
            std::cmp::Reverse(wildcards),
        )
    }
}

impl fmt::Display for SkillPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for SkillPattern {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SkillPattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?)
            .map_err(|error| de::Error::custom(format!("invalid skill name: {error}")))
    }
}

impl JsonSchema for SkillPattern {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "SkillPattern".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "pattern": "^[a-z0-9_*?-]+$"
        })
    }
}

/// A tool group's state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroupState {
    /// Offered, sent up front.
    On,
    /// Offered deferred, loaded with `tool_search`; `on` on a model that
    /// does not defer and for an ACP agent.
    Deferred,
    /// Not offered.
    Off,
}

/// A skill's state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillState {
    On,
    Off,
}

/// One rule over tool groups.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ToolGroupRule {
    pub selector: ToolGroupSelector,
    pub state: ToolGroupState,
    /// Copied from the session's project's defaults when the session was
    /// created, and not changed since.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_project: bool,
}

/// One rule over skills.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SkillRule {
    pub skill: SkillPattern,
    pub state: SkillState,
    /// Copied from the session's project's defaults when the session was
    /// created, and not changed since.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_project: bool,
}

/// One layer's rules: an agent document's, a project's defaults, or a
/// session's.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ToolExposure {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub tool_groups: Vec<ToolGroupRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub skills: Vec<SkillRule>,
}

/// Why an exposure was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolExposureError {
    TooManyRules,
    DuplicateRule(String),
    Pattern(String),
    FixedGroup(String),
}

impl fmt::Display for ToolExposureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyRules => write!(
                formatter,
                "at most {MAX_EXPOSURE_RULES} tool group rules and {MAX_EXPOSURE_RULES} skill rules"
            ),
            Self::DuplicateRule(rule) => write!(formatter, "`{rule}` has more than one rule"),
            Self::Pattern(rule) => write!(
                formatter,
                "`{rule}` is a pattern; a session's and a project's choices name groups and skills exactly"
            ),
            Self::FixedGroup(rule) => write!(
                formatter,
                "`{rule}` is always offered and cannot be switched"
            ),
        }
    }
}

impl std::error::Error for ToolExposureError {}

/// The built-in groups that are never switched (spec section 4.3): their
/// tools follow what is offered. The loader's group is `loader`; a rule
/// may also call it by its tool, `tool_search`.
pub const FIXED_TOOL_GROUPS: [&str; 5] =
    ["artifacts", "background", "loader", "skills", "tool_search"];

/// Whether `selector` names a fixed group exactly.
#[must_use]
pub fn names_fixed_group(selector: &ToolGroupSelector) -> bool {
    selector.kind() == ToolGroupKind::Builtin
        && !selector.is_pattern()
        && FIXED_TOOL_GROUPS.contains(&selector.name())
}

impl ToolExposure {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tool_groups.is_empty() && self.skills.is_empty()
    }

    /// Checks bounds, duplicates, and fixed groups; patterns are allowed.
    pub fn validate(&self) -> Result<(), ToolExposureError> {
        if self.tool_groups.len() > MAX_EXPOSURE_RULES || self.skills.len() > MAX_EXPOSURE_RULES {
            return Err(ToolExposureError::TooManyRules);
        }
        let mut seen = std::collections::BTreeSet::new();
        for rule in &self.tool_groups {
            if names_fixed_group(&rule.selector) {
                return Err(ToolExposureError::FixedGroup(rule.selector.to_string()));
            }
            if !seen.insert(rule.selector.to_string()) {
                return Err(ToolExposureError::DuplicateRule(rule.selector.to_string()));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for rule in &self.skills {
            if !seen.insert(rule.skill.as_str()) {
                return Err(ToolExposureError::DuplicateRule(rule.skill.to_string()));
            }
        }
        Ok(())
    }

    /// [`Self::validate`], and no patterns: a session's and a project's
    /// choices.
    pub fn validate_exact(&self) -> Result<(), ToolExposureError> {
        self.validate()?;
        if let Some(rule) = self
            .tool_groups
            .iter()
            .find(|rule| rule.selector.is_pattern())
        {
            return Err(ToolExposureError::Pattern(rule.selector.to_string()));
        }
        if let Some(rule) = self.skills.iter().find(|rule| rule.skill.is_pattern()) {
            return Err(ToolExposureError::Pattern(rule.skill.to_string()));
        }
        Ok(())
    }

    /// The state the most specific rule matching the group gives, with the
    /// rule; later rules win ties.
    #[must_use]
    pub fn group_rule(
        &self,
        kind: ToolGroupKind,
        name: &str,
        runtime: Option<&RuntimeId>,
    ) -> Option<&ToolGroupRule> {
        self.tool_groups
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.selector.matches(kind, name, runtime))
            .max_by_key(|(index, rule)| (rule.selector.specificity(), *index))
            .map(|(_, rule)| rule)
    }

    /// The most specific rule matching `skill`; later rules win ties.
    #[must_use]
    pub fn skill_rule(&self, skill: &str) -> Option<&SkillRule> {
        self.skills
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.skill.matches(skill))
            .max_by_key(|(index, rule)| (rule.skill.specificity(), *index))
            .map(|(_, rule)| rule)
    }
}

/// The layer that decided a group's or a skill's state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureLayer {
    /// The defaults: on, deferral as configured.
    Default,
    /// The agent document's `tool_groups` or `skills`.
    Agent,
    /// The session's choices, copied from its project's defaults.
    Project,
    /// The session's own choices.
    Session,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionToolsGetParams {
    pub session_id: SessionId,
}

/// One group of a session, on one runtime.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionToolGroup {
    /// The group's exact selector, as a choice for it names it.
    pub selector: ToolGroupSelector,
    pub kind: ToolGroupKind,
    pub name: String,
    /// The runtime that offers it; absent for the base layer, `global`,
    /// and top-level MCP servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeId>,
    /// The session's attachment that offers it; absent for the base layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub state: ToolGroupState,
    pub layer: ExposureLayer,
    /// What the layers below the session's choice give: what `Reset`
    /// returns to.
    pub below: ToolGroupState,
    /// The layer `below` comes from.
    pub below_layer: ExposureLayer,
    /// Permissions hide every tool of it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden_by_permissions: bool,
    /// Its tools, by the names the model sees.
    pub tools: Vec<String>,
    /// An estimate of the tokens its definitions cost sent up front.
    pub token_estimate: u64,
}

/// One skill of a session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionSkill {
    /// The name the session knows it by.
    pub name: String,
    pub description: String,
    pub source: crate::SkillSource,
    /// The attachment that contributes a project skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<String>,
    pub state: SkillState,
    pub layer: ExposureLayer,
    pub below: SkillState,
    pub below_layer: ExposureLayer,
    /// Shadowed by a skill of the same name: it cannot be switched.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shadowed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionToolsGetResult {
    pub groups: Vec<SessionToolGroup>,
    pub skills: Vec<SessionSkill>,
    /// The session's own choices.
    pub exposure: ToolExposure,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionToolsSetParams {
    pub session_id: SessionId,
    /// The session's choices afterwards, whole: exact selectors only.
    pub exposure: ToolExposure,
}

/// `runtimes.groups`: the tool groups and project skills one runtime
/// capability brings at a workspace, live when it runs, else as of the
/// last time it ran (session tool groups spec, section 4.4).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGroupsParams {
    pub runtime_id: RuntimeId,
    /// The capability; the runtime's only one when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<crate::CapabilityName>,
    /// The workspace, as typed: `~/api` or an absolute path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// One tool group a runtime capability brings.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeToolGroup {
    /// The group's exact selector, as a choice for it names it.
    pub selector: ToolGroupSelector,
    pub kind: ToolGroupKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub tools: Vec<String>,
    /// An estimate of the tokens its definitions cost sent up front.
    pub token_estimate: u64,
    /// Its state before any choice: `on` or `deferred`.
    pub default: ToolGroupState,
}

/// One project skill a runtime capability brings at a workspace.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeSkill {
    pub name: String,
    pub description: String,
    pub source: crate::SkillSource,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeGroupsResult {
    /// Whether anything is known: the capability runs now, or ran before.
    pub known: bool,
    /// The groups and skills come from a running instance.
    pub live: bool,
    /// When a running instance last reported them, when not `live`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<jiff::Timestamp>,
    /// The workspace they were seen at, when another than the one asked
    /// for: the capability never ran there, so its workspace's own MCP
    /// servers and project skills are not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_at: Option<String>,
    pub groups: Vec<RuntimeToolGroup>,
    pub skills: Vec<RuntimeSkill>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector(value: &str) -> ToolGroupSelector {
        value.parse().unwrap()
    }

    #[test]
    fn selectors_parse_and_print() {
        for value in [
            "coding",
            "coding@labpc",
            "mcp:exa",
            "mcp:playwright@local",
            "plugin:browser",
            "mcp:*",
            "*@labpc",
        ] {
            assert_eq!(selector(value).to_string(), value);
        }
        assert_eq!(selector("mcp:exa@labpc").kind(), ToolGroupKind::Mcp);
        assert!("plugin:browser@labpc".parse::<ToolGroupSelector>().is_err());
        assert!("coding@Labpc".parse::<ToolGroupSelector>().is_err());
        assert!("".parse::<ToolGroupSelector>().is_err());
        assert!("mcp:".parse::<ToolGroupSelector>().is_err());
    }

    #[test]
    fn selectors_match_by_kind_name_and_runtime() {
        let labpc = RuntimeId::new("labpc").unwrap();
        assert!(selector("coding").matches(ToolGroupKind::Builtin, "coding", Some(&labpc)));
        assert!(selector("coding@labpc").matches(ToolGroupKind::Builtin, "coding", Some(&labpc)));
        assert!(!selector("coding@labpc").matches(ToolGroupKind::Builtin, "coding", None));
        assert!(!selector("coding").matches(ToolGroupKind::Mcp, "coding", None));
        assert!(selector("mcp:*").matches(ToolGroupKind::Mcp, "exa", None));
        assert!(!selector("mcp:exa").matches(ToolGroupKind::Mcp, "exa_pro", None));
    }

    #[test]
    fn the_most_specific_rule_of_a_layer_decides_and_later_wins_ties() {
        let rule = |value: &str, state| ToolGroupRule {
            selector: selector(value),
            state,
            from_project: false,
        };
        let exposure = ToolExposure {
            tool_groups: vec![
                rule("mcp:github", ToolGroupState::On),
                rule("mcp:*", ToolGroupState::Off),
                rule("mcp:git*", ToolGroupState::Deferred),
            ],
            skills: Vec::new(),
        };
        let decided = |name: &str| {
            exposure
                .group_rule(ToolGroupKind::Mcp, name, None)
                .map(|rule| rule.state)
        };
        assert_eq!(decided("github"), Some(ToolGroupState::On));
        assert_eq!(decided("gitlab"), Some(ToolGroupState::Deferred));
        assert_eq!(decided("exa"), Some(ToolGroupState::Off));
        assert_eq!(
            exposure.group_rule(ToolGroupKind::Builtin, "coding", None),
            None
        );
    }

    #[test]
    fn sessions_refuse_patterns_and_fixed_groups() {
        let exposure = |value: &str| ToolExposure {
            tool_groups: vec![ToolGroupRule {
                selector: selector(value),
                state: ToolGroupState::Off,
                from_project: false,
            }],
            skills: Vec::new(),
        };
        assert!(exposure("mcp:*").validate().is_ok());
        assert!(matches!(
            exposure("mcp:*").validate_exact(),
            Err(ToolExposureError::Pattern(_))
        ));
        assert!(matches!(
            exposure("artifacts").validate(),
            Err(ToolExposureError::FixedGroup(_))
        ));
        assert!(exposure("coding@labpc").validate_exact().is_ok());
    }
}
