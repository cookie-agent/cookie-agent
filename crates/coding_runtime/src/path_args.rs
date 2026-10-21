//! Lexical expansion at the file-path argument boundary, before preparation or authorization.
//! `~` expands against the home the runtime reports ([`crate::runtime_home`]),
//! so a path under it gets the label a `~/` permission rule expands to.

use std::path::PathBuf;

use cookie_agent_runtime_api::ToolError;
use serde::{Deserialize, Deserializer, de::Error as _};

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let path = String::deserialize(deserializer)?;
    expand_home(path, || {
        crate::runtime_home().map(std::path::Path::to_owned)
    })
    .map_err(D::Error::custom)
}

fn expand_home(
    path: String,
    home_dir: impl FnOnce() -> Option<PathBuf>,
) -> Result<String, ToolError> {
    if !path.starts_with('~') {
        return Ok(path);
    }
    let suffix = if path == "~" {
        ""
    } else if let Some(suffix) = path.strip_prefix("~/") {
        suffix
    } else {
        return Err(ToolError::execution(
            "unsupported tilde path: use ~ or ~/path; ~otheruser expansion is not supported",
        ));
    };
    let home = home_dir()
        .filter(|home| home.is_absolute())
        .ok_or_else(|| {
            ToolError::execution(
                "cannot expand tilde path: home directory is unavailable or not absolute",
            )
        })?;
    // Leading repeated separators must not cause PathBuf::push to replace the home directory.
    let expanded = home.join(suffix.trim_start_matches(std::path::is_separator));
    expanded.into_os_string().into_string().map_err(|_| {
        ToolError::execution("cannot expand tilde path: home directory is not valid UTF-8")
    })
}

#[cfg(test)]
mod tests {
    use super::expand_home;

    #[test]
    fn expansion_is_lexical_and_only_applies_to_home_prefixes() {
        let home = std::env::current_dir().unwrap().join("home");
        for (input, suffix) in [
            ("~", ""),
            ("~/file", "file"),
            ("~//file", "file"),
            ("~/a/../b", "a/../b"),
        ] {
            assert_eq!(
                expand_home(input.into(), || Some(home.clone())).unwrap(),
                home.join(suffix).to_str().unwrap()
            );
        }
        for input in [
            "relative/file",
            "/absolute/file",
            "./~otheruser/x",
            "dir/~/file",
            "",
        ] {
            assert_eq!(
                expand_home(input.into(), || panic!("home lookup for unchanged path")).unwrap(),
                input
            );
        }
        assert!(
            expand_home("~nosuchuser/x".into(), || panic!("named user lookup"))
                .unwrap_err()
                .message()
                .contains("~otheruser expansion is not supported")
        );
    }

    #[test]
    fn unavailable_or_relative_home_is_a_clear_error() {
        for home in [None, Some("".into()), Some("relative/home".into())] {
            for input in ["~", "~/file"] {
                assert!(
                    expand_home(input.into(), || home.clone())
                        .unwrap_err()
                        .message()
                        .contains("home directory is unavailable or not absolute")
                );
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tool_tests;
