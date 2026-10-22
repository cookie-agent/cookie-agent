//! The text editor the CLI and the TUI hand a document to.

use std::{path::Path, process::Command};

/// The editor to run, which may carry arguments (`code --wait`):
/// `$VISUAL`, else `$EDITOR`, else `vi`. Windows has no `vi`: there the
/// default is VS Code, waiting for its tab to close, when `code` is on
/// `PATH`, else Notepad, which waits for its window.
#[must_use]
pub fn editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|editor| !editor.trim().is_empty())
        .unwrap_or_else(default_editor)
}

#[cfg(windows)]
fn default_editor() -> String {
    if crate::paths::find_executable("code").is_some() {
        "code --wait".to_owned()
    } else {
        "notepad".to_owned()
    }
}

#[cfg(not(windows))]
fn default_editor() -> String {
    "vi".to_owned()
}

/// `editor`, which may carry arguments (`code --wait`), on `path`.
#[must_use]
pub fn editor_command(editor: &str, path: &Path) -> Command {
    #[cfg(unix)]
    {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(path);
        command
    }
    #[cfg(not(unix))]
    {
        let mut words = editor.split_whitespace();
        let program = words.next().unwrap_or("notepad");
        // `Command` finds only `.exe` files on `PATH`; editors such as
        // VS Code install a `.cmd` launcher.
        #[cfg(windows)]
        let mut command = match crate::paths::find_executable(program) {
            Some(found) => Command::new(found),
            None => Command::new(program),
        };
        #[cfg(not(windows))]
        let mut command = Command::new(program);
        command.args(words).arg(path);
        command
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn a_cmd_editor_runs_with_its_arguments_on_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let editor = directory.path().join("cookie-editor.cmd");
        std::fs::write(&editor, "@echo %1>%2\r\n").unwrap();
        let file = directory.path().join("document.toml");
        std::fs::write(&file, "before").unwrap();
        let status = super::editor_command(&format!("{} --wait", editor.display()), &file)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), "--wait");
    }

    #[test]
    fn notepad_is_on_path() {
        assert!(crate::paths::find_executable("notepad").is_some());
    }
}
