//! The GitHub star card (`docs/site/specs/github-star.md` section 2): what
//! the daemon says about it (`star.status`, again on `star.changed`), and
//! the user's answer (`star.answer`).

use cookie_agent_protocol::StarAnswer;
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

/// How long the card thanks the user before it closes.
const THANKS: std::time::Duration = std::time::Duration::from_millis(2200);

/// Where the card is.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum StarPhase {
    #[default]
    Hidden,
    /// Asking. `login` is the account the daemon stars as, else the card
    /// offers the repository's page.
    Ask { login: Option<String> },
    /// The daemon is starring as `login`.
    Starring { login: String },
    /// Starred, or GitHub opened: thanks, then hidden.
    Thanked { opened: bool },
    /// The daemon could not star as `login`; the card offers the page.
    Failed { login: String },
}

impl Workspace {
    /// Asks the daemon whether the card shows. An answer the card is
    /// showing the outcome of is left alone.
    pub fn refresh_star(self) {
        if !self.has_star.get_untracked() {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let Ok(status) = client.star_status().await else {
                return;
            };
            self.star.update(|phase| {
                if matches!(phase, StarPhase::Hidden | StarPhase::Ask { .. }) {
                    *phase = if status.show {
                        StarPhase::Ask {
                            login: status.login,
                        }
                    } else {
                        StarPhase::Hidden
                    };
                }
            });
        });
    }

    /// Records `answer` and shows what follows.
    pub fn answer_star(self, answer: StarAnswer) {
        let Some(client) = self.client() else {
            return;
        };
        let login = match self.star.get_untracked() {
            StarPhase::Ask { login } => login,
            _ => None,
        };
        self.star.set(match (answer, login) {
            (StarAnswer::Star, Some(login)) => StarPhase::Starring { login },
            (StarAnswer::Opened, _) => StarPhase::Thanked { opened: true },
            _ => StarPhase::Hidden,
        });
        if answer == StarAnswer::Opened {
            self.close_star_later();
        }
        spawn_local(async move {
            let starred = client
                .star_answer(answer)
                .await
                .is_ok_and(|result| result.starred);
            if answer != StarAnswer::Star {
                return;
            }
            let failed = self.star.with_untracked(|phase| match phase {
                StarPhase::Starring { login } => (!starred).then(|| login.clone()),
                _ => None,
            });
            match failed {
                Some(login) => self.star.set(StarPhase::Failed { login }),
                None => {
                    self.star.set(StarPhase::Thanked { opened: false });
                    self.close_star_later();
                }
            }
        });
    }

    /// Hides the card without an answer: after a failed star, which the
    /// daemon already recorded.
    pub fn close_star(self) {
        self.star.set(StarPhase::Hidden);
    }

    fn close_star_later(self) {
        set_timeout(
            move || {
                if matches!(self.star.get_untracked(), StarPhase::Thanked { .. }) {
                    self.star.set(StarPhase::Hidden);
                }
            },
            THANKS,
        );
    }
}
