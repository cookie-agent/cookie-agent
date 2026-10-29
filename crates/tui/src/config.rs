//! Strict, independent TUI configuration (`tui.toml`).
//!
//! The file lives at `~/.cookie-agent/tui.toml`. There is no workspace layer or
//! environment variable override. A missing file yields defaults; a malformed
//! file or any unknown key fails with an actionable path/key error. Error
//! messages quote the file's location and the offending key or TOML parse
//! context, never file contents.
//!
//! The TUI writes one key itself, `transcript`, when the user changes the
//! transcript mode ([`save_transcript`]); everything else in the file stays
//! as the user wrote it.

use std::path::{Path, PathBuf};

use cookie_agent_protocol::paths;
use serde::Deserialize;

use cookie_agent_client_state::transcript_mode::TranscriptMode;

use crate::state::EventLevel;
use crate::theme::ThemeKind;

pub const CONFIG_FILE_NAME: &str = "tui.toml";

/// Parsed TUI configuration with all defaults applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TuiConfig {
    /// Minimum diagnostic level rendered in the conversation. Rows below the
    /// threshold stay in the session projection and reappear when the level
    /// is lowered at runtime.
    pub minimum_event_level: EventLevel,
    /// Optional theme preference; `None` and `Auto` use terminal/environment
    /// detection (`COOKIE_THEME`, OSC 11, `COLORFGBG`).
    pub theme: Option<ThemePreference>,
    /// How the conversation shows agent work: each block's thinking and
    /// tool calls folded into one row, or all of them.
    pub transcript: TranscriptMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemePreference {
    Auto,
    Default,
    Dark,
    Mono,
    HighContrast,
}

impl ThemePreference {
    pub const fn explicit_kind(self) -> Option<ThemeKind> {
        match self {
            Self::Auto => None,
            Self::Default => Some(ThemeKind::Default),
            Self::Dark => Some(ThemeKind::Dark),
            Self::Mono => Some(ThemeKind::Mono),
            Self::HighContrast => Some(ThemeKind::HighContrast),
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            minimum_event_level: EventLevel::DEFAULT_MINIMUM,
            theme: None,
            transcript: TranscriptMode::default(),
        }
    }
}

#[derive(Debug)]
pub enum TuiConfigError {
    Io {
        path: PathBuf,
        message: String,
    },
    Malformed {
        path: PathBuf,
        message: String,
    },
    /// The file could not be written.
    Write {
        path: PathBuf,
        message: String,
    },
}
impl std::fmt::Display for TuiConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => {
                write!(f, "cannot read TUI config {}: {message}", path.display())
            }
            Self::Malformed { path, message } => write!(
                f,
                "invalid TUI config {}: {message} (see docs/tui.toml.example)",
                path.display()
            ),
            Self::Write { path, message } => {
                write!(f, "cannot write TUI config {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for TuiConfigError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TuiConfigFile {
    minimum_event_level: Option<LevelName>,
    theme: Option<ThemeName>,
    transcript: Option<TranscriptName>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum TranscriptName {
    Reduced,
    Full,
}

impl From<TranscriptName> for TranscriptMode {
    fn from(name: TranscriptName) -> Self {
        match name {
            TranscriptName::Reduced => Self::Reduced,
            TranscriptName::Full => Self::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum LevelName {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum ThemeName {
    Auto,
    Default,
    Dark,
    Mono,
    HighContrast,
}

impl From<LevelName> for EventLevel {
    fn from(level: LevelName) -> Self {
        match level {
            LevelName::Debug => Self::Debug,
            LevelName::Info => Self::Info,
            LevelName::Warning => Self::Warning,
            LevelName::Error => Self::Error,
        }
    }
}

impl From<ThemeName> for ThemePreference {
    fn from(theme: ThemeName) -> Self {
        match theme {
            ThemeName::Auto => Self::Auto,
            ThemeName::Default => Self::Default,
            ThemeName::Dark => Self::Dark,
            ThemeName::Mono => Self::Mono,
            ThemeName::HighContrast => Self::HighContrast,
        }
    }
}

/// The platform config path for this process environment.
pub fn config_path() -> Option<PathBuf> {
    paths::user_data_root()
        .ok()
        .map(|root| root.join(CONFIG_FILE_NAME))
}

/// Parse TOML text strictly. Unknown keys, wrong types, and invalid enum
/// values are errors naming the offending key; the input text is never
/// echoed back.
pub fn parse(text: &str, path: &Path) -> Result<TuiConfig, TuiConfigError> {
    let file: TuiConfigFile = toml::from_str(text).map_err(|error| TuiConfigError::Malformed {
        path: path.to_owned(),
        message: sanitize_toml_error(text, &error),
    })?;
    let defaults = TuiConfig::default();
    Ok(TuiConfig {
        minimum_event_level: file
            .minimum_event_level
            .map(EventLevel::from)
            .unwrap_or(defaults.minimum_event_level),
        theme: file.theme.map(ThemePreference::from),
        transcript: file
            .transcript
            .map(TranscriptMode::from)
            .unwrap_or(defaults.transcript),
    })
}

/// Load the config from an explicit path. A missing file yields defaults.
pub fn load_from(path: &Path) -> Result<TuiConfig, TuiConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(TuiConfig::default()),
        Err(error) => Err(TuiConfigError::Io {
            path: path.to_owned(),
            message: error.to_string(),
        }),
    }
}

/// Load from the platform config path, or an explicit override path.
pub fn load(override_path: Option<&Path>) -> Result<TuiConfig, TuiConfigError> {
    match override_path {
        Some(path) => load_from(path),
        None => match config_path() {
            Some(path) => load_from(&path),
            None => Ok(TuiConfig::default()),
        },
    }
}

/// Saves `mode` as the `transcript` key of the platform config file; see
/// [`save_transcript_to`]. Returns the path it wrote.
pub fn save_transcript(mode: TranscriptMode) -> Result<PathBuf, TuiConfigError> {
    let path = config_path().ok_or_else(|| TuiConfigError::Write {
        path: PathBuf::from(CONFIG_FILE_NAME),
        message: "no user data directory".into(),
    })?;
    save_transcript_to(&path, mode)?;
    Ok(path)
}

/// Sets the `transcript` key of the file at `path` to `mode`, keeping its
/// comments, other keys, and their order as written; a missing file is
/// created holding only that key. The file is read again just before the
/// write and replaced in one rename, and one that no longer parses is left
/// as it is.
pub fn save_transcript_to(path: &Path, mode: TranscriptMode) -> Result<(), TuiConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(TuiConfigError::Io {
                path: path.to_owned(),
                message: error.to_string(),
            });
        }
    };
    parse(&text, path)?;
    let mut document =
        text.parse::<toml_edit::DocumentMut>()
            .map_err(|_| TuiConfigError::Malformed {
                path: path.to_owned(),
                message: "malformed TOML".into(),
            })?;
    document["transcript"] = toml_edit::value(mode.name());
    let write_error = |error: std::io::Error| TuiConfigError::Write {
        path: path.to_owned(),
        message: error.to_string(),
    };
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory).map_err(write_error)?;
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(write_error)?;
    // The new file keeps the old one's permissions, not a temporary file's.
    if let Ok(metadata) = std::fs::metadata(path) {
        file.as_file()
            .set_permissions(metadata.permissions())
            .map_err(write_error)?;
    }
    std::io::Write::write_all(&mut file, document.to_string().as_bytes()).map_err(write_error)?;
    file.persist(path)
        .map_err(|error| write_error(error.error))?;
    Ok(())
}

/// TOML errors do not always name the offending key; recover it from the
/// error span by taking the line's key token. Values are never included.
fn sanitize_toml_error(text: &str, error: &toml::de::Error) -> String {
    if let Some(span) = error.span() {
        let mut offset = 0;
        for line in text.split('\n') {
            let line_end = offset + line.len();
            if span.start >= offset && span.start <= line_end {
                if let Some(key) = line.split('=').next().map(str::trim)
                    && !key.is_empty()
                {
                    let kind = if error.message().contains("unknown field") {
                        "unknown key"
                    } else {
                        "invalid value for key"
                    };
                    return format!("{kind} `{key}`");
                }
                return format!("malformed TOML at byte offset {}", span.start);
            }
            offset = line_end + 1;
        }
    }
    "malformed TOML".into()
}

#[cfg(test)]
mod tests {
    use super::{
        EventLevel, ThemePreference, TranscriptMode, TuiConfig, load_from, parse,
        save_transcript_to,
    };
    use std::path::Path;

    #[test]
    fn defaults_hide_debug_and_info_diagnostics() {
        let config = TuiConfig::default();
        assert_eq!(config.minimum_event_level, EventLevel::Warning);
        assert_eq!(config.theme, None);
    }

    #[test]
    fn full_document_and_empty_document_parse() {
        let path = Path::new("tui.toml");
        let config = parse(
            "minimum_event_level = \"debug\"\ntheme = \"high-contrast\"\n",
            path,
        )
        .expect("full config");
        assert_eq!(config.minimum_event_level, EventLevel::Debug);
        assert_eq!(config.theme, Some(ThemePreference::HighContrast));
        assert_eq!(parse("", path).expect("empty"), TuiConfig::default());
        assert_eq!(
            parse("theme = \"mono\"\n", path).expect("theme only").theme,
            Some(ThemePreference::Mono)
        );
        assert_eq!(
            parse("theme = \"dark\"\n", path).expect("dark theme").theme,
            Some(ThemePreference::Dark)
        );
        assert_eq!(
            parse("theme = \"auto\"\n", path).expect("auto theme").theme,
            Some(ThemePreference::Auto)
        );
    }

    #[test]
    fn unknown_keys_and_invalid_values_fail_with_actionable_key_names() {
        let path = Path::new("tui.toml");
        let error = parse("minimum_event_level = \"trace\"\n", path).expect_err("bad level");
        let message = error.to_string();
        assert!(message.contains("tui.toml"));
        assert!(message.contains("minimum_event_level"), "{message}");
        assert!(!message.contains("trace"), "no raw value echo: {message}");

        let error = parse("unknown_key = 1\n", path).expect_err("unknown key");
        assert!(error.to_string().contains("unknown_key"));

        let error = parse("minimum_event_level = 3\n", path).expect_err("wrong type");
        assert!(error.to_string().contains("minimum_event_level"));

        let error = parse("not = [toml\n", path).expect_err("syntax error");
        assert!(error.to_string().contains("tui.toml"));
    }

    #[test]
    fn missing_file_uses_defaults_and_explicit_path_loads() {
        let directory = tempfile::tempdir().expect("tempdir");
        let missing = directory.path().join("absent.toml");
        assert_eq!(
            super::load_from(&missing).expect("missing"),
            TuiConfig::default()
        );
        let present = directory.path().join("tui.toml");
        std::fs::write(&present, "minimum_event_level = \"info\"\n").expect("write");
        assert_eq!(
            super::load_from(&present)
                .expect("load")
                .minimum_event_level,
            EventLevel::Info
        );
    }

    #[test]
    fn malformed_unknown_and_permission_errors_preserve_path_without_values() {
        let directory = tempfile::tempdir().expect("tempdir");
        let malformed = directory.path().join("malformed.toml");
        std::fs::write(&malformed, "theme = \"secret-invalid-value\"\n").expect("write");
        let message = load_from(&malformed).expect_err("malformed").to_string();
        assert!(message.contains(&malformed.display().to_string()));
        assert!(message.contains("theme"));
        assert!(!message.contains("secret-invalid-value"));

        let unknown = directory.path().join("unknown.toml");
        std::fs::write(&unknown, "unexpected_setting = \"secret-value\"\n").expect("write");
        let message = load_from(&unknown).expect_err("unknown key").to_string();
        assert!(message.contains(&unknown.display().to_string()));
        assert!(message.contains("unexpected_setting"));
        assert!(!message.contains("secret-value"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let unreadable = directory.path().join("unreadable.toml");
            std::fs::write(&unreadable, "theme = \"mono\"\n").expect("write");
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
                .expect("remove permissions");
            let error = load_from(&unreadable).expect_err("permission error");
            assert!(
                error
                    .to_string()
                    .contains(&unreadable.display().to_string())
            );
        }
    }

    #[test]
    fn transcript_takes_both_modes_and_nothing_else() {
        let path = Path::new("tui.toml");
        for mode in [TranscriptMode::Reduced, TranscriptMode::Full] {
            let text = format!("transcript = \"{}\"\n", mode.name());
            assert_eq!(parse(&text, path).expect("mode").transcript, mode);
        }
        for text in ["transcript = \"compact\"\n", "transcript = true\n"] {
            let message = parse(text, path).expect_err("bad mode").to_string();
            assert!(message.contains("transcript"), "{message}");
            assert!(!message.contains("compact"), "no raw value echo: {message}");
        }
    }

    #[test]
    fn saving_the_transcript_changes_only_its_key() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("tui.toml");
        let written =
            "# my settings\ntheme = \"mono\" # keep\n\n# level\nminimum_event_level = \"info\"\n";
        std::fs::write(&path, written).expect("write");
        save_transcript_to(&path, TranscriptMode::Full).expect("save");
        let saved = std::fs::read_to_string(&path).expect("read");
        assert_eq!(saved, format!("{written}transcript = \"full\"\n"));
        assert_eq!(
            load_from(&path).expect("load").transcript,
            TranscriptMode::Full
        );
        // Saving again replaces the value where it stands.
        save_transcript_to(&path, TranscriptMode::Reduced).expect("save again");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("{written}transcript = \"reduced\"\n")
        );
    }

    #[test]
    fn saving_the_transcript_creates_a_missing_file_and_its_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("home").join("tui.toml");
        save_transcript_to(&path, TranscriptMode::Full).expect("save");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "transcript = \"full\"\n"
        );
    }

    #[test]
    fn saving_the_transcript_leaves_a_file_that_does_not_parse() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("tui.toml");
        for written in ["theme = \"secret-value\"\n", "not = [toml\n", "other = 1\n"] {
            std::fs::write(&path, written).expect("write");
            let message = save_transcript_to(&path, TranscriptMode::Full)
                .expect_err("invalid file")
                .to_string();
            assert!(message.contains(&path.display().to_string()), "{message}");
            assert!(!message.contains("secret-value"), "{message}");
            assert_eq!(std::fs::read_to_string(&path).expect("read"), written);
        }
    }

    #[cfg(unix)]
    #[test]
    fn saving_the_transcript_keeps_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("tui.toml");
        std::fs::write(&path, "").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        save_transcript_to(&path, TranscriptMode::Full).expect("save");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }
}
