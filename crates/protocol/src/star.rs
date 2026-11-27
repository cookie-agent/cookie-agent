//! Asking for a GitHub star (`docs/site/specs/github-star.md`, client
//! protocol 1.106.0): once the daemon has been used for 7 days the web page
//! asks whether the user would star the project, and the daemon stars it
//! with a GitHub token already on its machine when it has one.
//!
//! The daemon decides when the card shows (`star.status`) and records the
//! user's answer (`star.answer`); `star.changed` tells every owner
//! connection to ask again. Guests get none of them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const STAR_STATUS_METHOD: &str = "star.status";
pub const STAR_ANSWER_METHOD: &str = "star.answer";
pub const STAR_CHANGED_METHOD: &str = "star.changed";

/// The repository the card asks to star.
pub const STAR_REPOSITORY: &str = "cookie-agent/cookie-agent";
/// The repository's page, which the card opens when the daemon cannot star.
pub const STAR_REPOSITORY_URL: &str = "https://github.com/cookie-agent/cookie-agent";

/// Whether the card shows now.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StarStatusParams {}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct StarStatusResult {
    /// The card shows.
    pub show: bool,
    /// The GitHub account the daemon stars as, when it can star: the card
    /// offers **Star as @login**, and otherwise a link to
    /// [`STAR_REPOSITORY_URL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
}

crate::open_enum! {
    /// What the user did with the card.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum StarAnswer {
        /// **Star as @login**: the daemon stars with its token. The card
        /// never shows again, whether starring worked or not.
        Star,
        /// The page opened the repository's page. The card never shows
        /// again.
        Opened,
        /// **Later**, or the card's ×: hidden for 7 days.
        Later,
        /// **Don't remind me**: the card never shows again.
        Dismiss,
    }
}

/// Records the user's answer.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StarAnswerParams {
    pub answer: StarAnswer,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct StarAnswerResult {
    /// GitHub accepted the star: only for [`StarAnswer::Star`].
    pub starred: bool,
}

/// The card's state changed; ask `star.status` again.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct StarChangedNotification {}
