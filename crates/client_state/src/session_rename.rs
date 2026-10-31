//! Renaming a session (`session.rename`): the change a client sends for
//! what the user typed, the TUI's `/rename` arguments, and the call. Every
//! rename carries a fresh `client_rename_id`; the daemon answers a repeat
//! of the same ID and change with the session as it is, so a lost reply
//! can be retried. Clients set a title or go back to the generated one
//! (`Reset`); none of them offers `Clear`.

use std::fmt;

use cookie_agent_protocol::{
    ClientRenameId, EventSchemaError, SessionId, SessionMeta, SessionRenameChange,
    SessionRenameParams, SessionTitle,
};
use cookie_agent_protocol_transport::{Client, ClientError};

/// Why typed text is not a title.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TitleError {
    /// Nothing but whitespace: a client offers the automatic title instead.
    Empty,
    /// Longer than [`SessionTitle::MAX_BYTES`].
    TooLong,
    /// A line break, tab, or other control character.
    ControlCharacter,
}

impl fmt::Display for TitleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "A title cannot be empty.",
            Self::TooLong => "A title is at most 512 bytes.",
            Self::ControlCharacter => "A title cannot hold line breaks or control characters.",
        })
    }
}

impl std::error::Error for TitleError {}

/// The change that sets the title to `input`, trimmed.
pub fn title_change(input: &str) -> Result<SessionRenameChange, TitleError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(TitleError::Empty);
    }
    match SessionTitle::new(input) {
        Ok(title) => Ok(SessionRenameChange::Set { title }),
        Err(EventSchemaError::TitleTooLong) => Err(TitleError::TooLong),
        Err(EventSchemaError::EmptyTitle) => Err(TitleError::Empty),
        Err(_) => Err(TitleError::ControlCharacter),
    }
}

/// What `/rename` does with the text after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenameCommand {
    /// No argument: ask for the title, starting from the current one.
    Prompt,
    /// `--auto`, or a title.
    Change(SessionRenameChange),
}

/// The `/rename` arguments: none asks, `--auto` resets to the generated
/// title, and anything else is the title.
pub fn rename_command(arguments: &str) -> Result<RenameCommand, TitleError> {
    match arguments.trim() {
        "" => Ok(RenameCommand::Prompt),
        "--auto" => Ok(RenameCommand::Change(SessionRenameChange::Reset)),
        title => title_change(title).map(RenameCommand::Change),
    }
}

/// What a title field starts with: the session's title, else nothing.
pub fn current_title(session: &SessionMeta) -> String {
    session
        .title
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default()
}

/// The params that apply `change` to `session_id`. `unique` makes the
/// rename's idempotency key; a client passes a fresh UUID.
pub fn rename_params(
    session_id: SessionId,
    change: SessionRenameChange,
    unique: &str,
) -> SessionRenameParams {
    SessionRenameParams {
        session_id,
        client_rename_id: ClientRenameId::new(format!("rename-{unique}"))
            .unwrap_or_else(|_| ClientRenameId::new("rename").expect("a valid rename id")),
        change,
    }
}

/// Sends `params`, answering with the session as the rename left it.
pub async fn rename_session(
    client: &Client,
    params: SessionRenameParams,
) -> Result<SessionMeta, ClientError> {
    client
        .rename_session(params)
        .await
        .map(|result| result.session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(title: &str) -> SessionRenameChange {
        SessionRenameChange::Set {
            title: SessionTitle::new(title).unwrap(),
        }
    }

    #[test]
    fn typed_text_sets_a_trimmed_title_or_says_why_it_cannot() {
        assert_eq!(
            title_change("  Fix the parser \t"),
            Ok(set("Fix the parser"))
        );
        assert_eq!(title_change(""), Err(TitleError::Empty));
        assert_eq!(title_change(" \t "), Err(TitleError::Empty));
        assert_eq!(
            title_change("two\nlines"),
            Err(TitleError::ControlCharacter)
        );
        assert_eq!(title_change(&"é".repeat(256)), Ok(set(&"é".repeat(256))));
        assert_eq!(title_change(&"é".repeat(257)), Err(TitleError::TooLong));
    }

    #[test]
    fn rename_arguments_ask_reset_or_set() {
        assert_eq!(rename_command(""), Ok(RenameCommand::Prompt));
        assert_eq!(rename_command("   "), Ok(RenameCommand::Prompt));
        assert_eq!(
            rename_command(" --auto "),
            Ok(RenameCommand::Change(SessionRenameChange::Reset))
        );
        assert_eq!(
            rename_command("Ship --auto later"),
            Ok(RenameCommand::Change(set("Ship --auto later")))
        );
        assert_eq!(rename_command("a\u{7}b"), Err(TitleError::ControlCharacter));
    }

    #[test]
    fn each_rename_names_its_session_change_and_key() {
        let session_id = SessionId::new_v7();
        let params = rename_params(session_id, set("Docs"), "0192");
        assert_eq!(params.session_id, session_id);
        assert_eq!(params.change, set("Docs"));
        assert_eq!(params.client_rename_id.as_str(), "rename-0192");
        let reset = rename_params(session_id, SessionRenameChange::Reset, "0193");
        assert_eq!(reset.change, SessionRenameChange::Reset);
        assert_ne!(reset.client_rename_id, params.client_rename_id);
        // A key too long for the wire still makes a request.
        let long = rename_params(session_id, SessionRenameChange::Reset, &"x".repeat(300));
        assert_eq!(long.client_rename_id.as_str(), "rename");
    }
}
