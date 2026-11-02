//! The agents panel's rules every client shares
//! (`docs/site/specs/acp-onboarding.md` section 7): how a row of
//! `agents.documents` is labelled, which group it is in, and which actions
//! it offers.

use cookie_agent_protocol::{
    AgentDescriptor, AgentDocumentInfo, AgentDocumentKind, AgentDocumentOrigin,
};

/// What the panel can do to a row.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentDocumentAction {
    /// Starts the user's document inheriting the official one, or opens it.
    Customize,
    /// Opens the user's document.
    Open,
    Disable,
    Enable,
    /// Deletes the user's document.
    Reset,
    /// Copies the official document whole.
    Replace,
}

impl AgentDocumentAction {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Customize => "Customize",
            Self::Open => "Open",
            Self::Disable => "Turn off",
            Self::Enable => "Turn on",
            Self::Reset => "Reset",
            Self::Replace => "Replace",
        }
    }

    /// Whether the panel asks before doing it: it deletes or overwrites the
    /// user's document.
    #[must_use]
    pub const fn confirms(self) -> bool {
        matches!(self, Self::Reset | Self::Replace)
    }
}

/// The panel's groups, in order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AgentDocumentGroup {
    Official,
    Acp,
    Templates,
    Yours,
}

impl AgentDocumentGroup {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Official => "Official agents",
            Self::Acp => "ACP agents",
            Self::Templates => "Templates",
            Self::Yours => "Your agents",
        }
    }
}

/// The group `info` is listed in.
#[must_use]
pub const fn group(info: &AgentDocumentInfo) -> AgentDocumentGroup {
    match (info.kind, info.origin) {
        (AgentDocumentKind::Template, _) => AgentDocumentGroup::Templates,
        (AgentDocumentKind::Acp, _) => AgentDocumentGroup::Acp,
        (_, AgentDocumentOrigin::Own) => AgentDocumentGroup::Yours,
        _ => AgentDocumentGroup::Official,
    }
}

/// Where the row's document comes from, as a short label.
#[must_use]
pub fn origin_label(info: &AgentDocumentInfo) -> String {
    let mut label = match info.origin {
        AgentDocumentOrigin::Official => match &info.official_version {
            Some(version) => format!("official {version}"),
            None => "official".to_owned(),
        },
        AgentDocumentOrigin::Customized => "customized".to_owned(),
        AgentDocumentOrigin::Replaced => "replaced".to_owned(),
        AgentDocumentOrigin::Own => "yours".to_owned(),
        AgentDocumentOrigin::FollowsTemplate => "follows _acp".to_owned(),
        AgentDocumentOrigin::Bare => "no document".to_owned(),
        AgentDocumentOrigin::Unknown => "unknown".to_owned(),
    };
    if info.disabled {
        label.push_str(" · off");
    }
    label
}

/// The actions `info` offers.
#[must_use]
pub fn actions(info: &AgentDocumentInfo) -> Vec<AgentDocumentAction> {
    let mut actions = Vec::new();
    let own = info.origin == AgentDocumentOrigin::Own;
    match &info.path {
        Some(_) => actions.push(AgentDocumentAction::Open),
        None if info.origin != AgentDocumentOrigin::Bare => {
            actions.push(AgentDocumentAction::Customize);
        }
        None => {}
    }
    if info.kind != AgentDocumentKind::Template && info.origin != AgentDocumentOrigin::Bare {
        actions.push(if info.disabled {
            AgentDocumentAction::Enable
        } else {
            AgentDocumentAction::Disable
        });
    }
    if info.path.is_some() && !own {
        actions.push(AgentDocumentAction::Reset);
    }
    if matches!(
        info.origin,
        AgentDocumentOrigin::Official | AgentDocumentOrigin::Customized
    ) && info.kind != AgentDocumentKind::Acp
    {
        actions.push(AgentDocumentAction::Replace);
    }
    actions
}

/// Why the agent of `info` cannot run, from the agent listing.
#[must_use]
pub fn unavailable_line(info: &AgentDocumentInfo, agents: &[AgentDescriptor]) -> Option<String> {
    agents
        .iter()
        .find(|agent| agent.id.as_str() == info.id && agent.preset == info.preset)
        .and_then(|agent| agent.unavailable.as_ref())
        .map(cookie_agent_protocol::AgentUnavailable::summary)
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{AgentDocumentInfo, AgentDocumentKind, AgentDocumentOrigin};

    use super::{AgentDocumentAction as Action, AgentDocumentGroup, actions, group, origin_label};

    fn info(kind: AgentDocumentKind, origin: AgentDocumentOrigin, path: bool) -> AgentDocumentInfo {
        AgentDocumentInfo {
            id: "primary".into(),
            preset: None,
            kind,
            origin,
            inherits: None,
            path: path.then(|| "agents/primary.md".to_owned()),
            official_version: Some("1.2.0".into()),
            disabled: false,
        }
    }

    #[test]
    fn rows_offer_what_their_origin_allows() {
        use AgentDocumentKind::*;
        use AgentDocumentOrigin::*;
        assert_eq!(
            actions(&info(Agent, Official, false)),
            [Action::Customize, Action::Disable, Action::Replace]
        );
        assert_eq!(
            actions(&info(Agent, Customized, true)),
            [
                Action::Open,
                Action::Disable,
                Action::Reset,
                Action::Replace
            ]
        );
        assert_eq!(
            actions(&info(Agent, Replaced, true)),
            [Action::Open, Action::Disable, Action::Reset]
        );
        assert_eq!(
            actions(&info(Agent, Own, true)),
            [Action::Open, Action::Disable]
        );
        assert_eq!(
            actions(&info(Acp, FollowsTemplate, false)),
            [Action::Customize, Action::Disable]
        );
        assert_eq!(actions(&info(Acp, Bare, false)), []);
        assert_eq!(
            actions(&info(Template, Official, false)),
            [Action::Customize, Action::Replace]
        );
        let mut off = info(Agent, Customized, true);
        off.disabled = true;
        assert!(actions(&off).contains(&Action::Enable));
        assert!(origin_label(&off).ends_with("· off"));
        assert_eq!(
            origin_label(&info(Agent, Official, false)),
            "official 1.2.0"
        );
    }

    #[test]
    fn rows_are_grouped_by_kind_and_origin() {
        use AgentDocumentKind::*;
        use AgentDocumentOrigin::*;
        assert_eq!(
            group(&info(Agent, Customized, true)),
            AgentDocumentGroup::Official
        );
        assert_eq!(group(&info(Agent, Own, true)), AgentDocumentGroup::Yours);
        assert_eq!(group(&info(Acp, Own, true)), AgentDocumentGroup::Acp);
        assert_eq!(
            group(&info(Template, Official, false)),
            AgentDocumentGroup::Templates
        );
    }
}
