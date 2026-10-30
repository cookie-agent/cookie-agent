//! Cross-platform per-user path resolution for Cookie Agent.
//!
//! This module lives in `cookie_agent_protocol` because models, config, engine,
//! server, TUI, and tools already depend on that low-level crate. Placing it in
//! `cookie_agent_config` would create a dependency cycle with models.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors returned while resolving per-user paths.
#[derive(Debug, Error)]
pub enum PathError {
    #[error("home directory is unavailable")]
    HomeUnavailable,
}

/// Returns the current user's home directory.
///
/// # Errors
///
/// Returns [`PathError::HomeUnavailable`] when the standard library cannot
/// determine a home directory for the current user.
pub fn home_dir() -> Result<PathBuf, PathError> {
    std::env::home_dir().ok_or(PathError::HomeUnavailable)
}

/// Returns the current user's home directory canonicalized, as a runtime
/// reports it (`home` of `runtime/initialize`): see [`canonicalize`]. The
/// home as [`home_dir`] gives it when it cannot be canonicalized.
///
/// # Errors
///
/// Returns [`PathError::HomeUnavailable`] when the standard library cannot
/// determine a home directory for the current user.
pub fn canonical_home_dir() -> Result<PathBuf, PathError> {
    let home = home_dir()?;
    Ok(canonicalize(&home).unwrap_or(home))
}

/// Returns the unified Cookie Agent per-user data root.
///
/// # Errors
///
/// Returns [`PathError::HomeUnavailable`] when the current user's home
/// directory cannot be determined.
pub fn user_data_root() -> Result<PathBuf, PathError> {
    Ok(home_dir()?.join(".cookie-agent"))
}

/// A stable, opaque identity of this machine, so a client can tell whether a
/// daemon runs on its own machine: a SHA-256 of the operating system's machine
/// ID (`/etc/machine-id` or `/var/lib/dbus/machine-id`) or, where there is
/// none, of a random ID kept in `~/.cookie-agent/machine-id`, created on
/// first use. The fallback is per user, so two users of one machine without
/// an OS machine ID look like two machines. `None` when neither is
/// available.
#[must_use]
pub fn machine_id() -> Option<String> {
    static MACHINE_ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    MACHINE_ID
        .get_or_init(|| {
            // Debug builds under test: a runtime process of the test's own
            // machine reports another, as a runtime of another computer would.
            #[cfg(debug_assertions)]
            if let Some(id) = std::env::var("COOKIE_AGENT_TEST_MACHINE_ID")
                .ok()
                .filter(|id| !id.is_empty())
            {
                return Some(id);
            }
            let raw = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
                .into_iter()
                .find_map(|path| {
                    std::fs::read_to_string(path)
                        .ok()
                        .map(|id| id.trim().to_owned())
                        .filter(|id| !id.is_empty())
                })
                .or_else(user_machine_id)?;
            let digest = <sha2::Sha256 as sha2::Digest>::digest(
                format!("cookie-agent machine:{raw}").as_bytes(),
            );
            Some(
                digest
                    .iter()
                    .take(16)
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )
        })
        .clone()
}

/// This machine's host name; empty when it is unavailable.
#[must_use]
pub fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0_u8; 256];
        // SAFETY: the buffer is writable for its whole length, and
        // gethostname writes at most that many bytes.
        let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
        if result != 0 {
            return String::new();
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// The random machine ID kept in the user's data root, created when missing.
fn user_machine_id() -> Option<String> {
    let path = user_data_root().ok()?.join("machine-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim().to_owned();
        if !id.is_empty() {
            return Some(id);
        }
    }
    let id = uuid::Uuid::now_v7().simple().to_string();
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, format!("{id}\n")).ok()?;
    Some(id)
}

/// `path` canonicalized for showing and comparing: on Windows without the
/// `\\?\` verbatim prefix `std::fs::canonicalize` adds, whenever the path
/// means the same without it (see [`simplified`]); elsewhere exactly what
/// `canonicalize` returns. Every canonical path a person or a model reads,
/// such as a workspace root or a session's directory, comes from here, so
/// they all compare equal.
///
/// # Errors
///
/// Fails as `canonicalize` does, for example when the path does not exist.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(simplified)
}

/// `path` without the Windows verbatim prefix (`\\?\C:\…` becomes `C:\…`,
/// `\\?\UNC\server\share\…` becomes `\\server\share\…`) when the path
/// means the same without it: shorter than `MAX_PATH`, and with no component
/// a non-verbatim path would read differently (a device name such as `CON`,
/// or a name ending in a dot or a space). Other paths, and every path on
/// other platforms, are returned unchanged.
#[must_use]
pub fn simplified(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    match path.to_str().and_then(simplified_windows) {
        Some(simple) => PathBuf::from(simple),
        None => path,
    }
}

/// The non-verbatim spelling of the Windows path `path`, when it has one
/// that means the same.
fn simplified_windows(path: &str) -> Option<String> {
    const MAX_PATH: usize = 260;
    let simple = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        let rest = path.strip_prefix(r"\\?\")?;
        let bytes = rest.as_bytes();
        if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != b":\\" {
            return None;
        }
        rest.to_owned()
    };
    if simple.len() >= MAX_PATH || simple.contains('/') {
        return None;
    }
    let reserved = |name: &str| {
        let stem = name
            .split('.')
            .next()
            .unwrap_or(name)
            .trim_end()
            .to_ascii_uppercase();
        matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
    };
    let components = simple
        .trim_start_matches('\\')
        .split('\\')
        .skip(1)
        .filter(|component| !component.is_empty());
    for component in components {
        if component.ends_with('.') || component.ends_with(' ') || reserved(component) {
            return None;
        }
    }
    Some(simple)
}

/// The first `name` on `PATH`, found as [`search_path_candidates`] finds it.
///
/// `std::process::Command` looks only for `<name>.exe` on `PATH`, so it
/// misses the `.cmd` launchers many tools install, such as VS Code's `code`.
/// Like `where.exe` minus its current-directory search, which would let a
/// repository plant an executable.
#[cfg(windows)]
#[must_use]
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pathext = std::env::var_os("PATHEXT");
    search_path_candidates(
        name,
        &path.to_string_lossy(),
        pathext
            .as_deref()
            .map(|value| value.to_string_lossy())
            .as_deref(),
        Path::is_file,
    )
    .into_iter()
    .next()
}

/// The program to hand `std::process::Command` (or tokio's) for a
/// user-configured command: MCP stdio servers, ACP agents, runtime launch
/// commands, and plugins all go through here. `env` is the child's
/// configured environment; a `PATH` in it is searched instead of the
/// daemon's, as `Command` itself does.
///
/// On Windows, `Command` appends only `.exe` to a bare name, so `npx`, which
/// npm installs as `npx.cmd`, is "program not found". A `program` with no
/// path separator that does not already end in a `PATHEXT` extension is
/// therefore looked up on `PATH` with each `PATHEXT` extension, as
/// [`search_path_candidates`] does, and the first hit `CreateProcess` can
/// start (`.com`, `.exe`, `.bat`, `.cmd`; never `.ps1`) replaces it. A
/// `.bat` or `.cmd` hit goes to `Command` as is: since Rust 1.77.2
/// (CVE-2024-24576, "BatBadBut") the standard library runs a batch file
/// through `cmd.exe /e:ON /v:OFF /d /c` with escaping that keeps every
/// argument literal, and fails the spawn when an argument cannot be passed
/// safely. Never build that `cmd.exe /c` line by hand.
///
/// Elsewhere, and when nothing is found, `program` comes back unchanged.
#[must_use]
pub fn resolve_program<K, V>(program: &str, env: impl IntoIterator<Item = (K, V)>) -> PathBuf
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    #[cfg(windows)]
    {
        let path = env
            .into_iter()
            .find(|(key, _)| key.as_ref().eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| value.as_ref().to_owned())
            .or_else(|| std::env::var_os("PATH").map(|value| value.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let pathext = std::env::var_os("PATHEXT");
        windows_program(
            program,
            &path,
            pathext
                .as_deref()
                .map(|value| value.to_string_lossy())
                .as_deref(),
            Path::is_file,
        )
        .unwrap_or_else(|| PathBuf::from(program))
    }
    #[cfg(not(windows))]
    {
        let _ = env;
        PathBuf::from(program)
    }
}

/// The Windows lookup behind [`resolve_program`]; `None` leaves `program`
/// to `Command`.
#[cfg(any(windows, test))]
fn windows_program(
    program: &str,
    path: &str,
    pathext: Option<&str>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    const RUNNABLE: [&str; 4] = ["com", "exe", "bat", "cmd"];
    if program.is_empty() || program.contains(['/', '\\', ':']) {
        return None;
    }
    let has_extension = pathext
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .any(|extension| {
            program.len() > extension.len()
                && program.is_char_boundary(program.len() - extension.len())
                && program[program.len() - extension.len()..].eq_ignore_ascii_case(extension)
        });
    if has_extension {
        return None;
    }
    search_path_candidates(program, path, pathext, exists)
        .into_iter()
        .find(|candidate| {
            candidate
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    RUNNABLE
                        .iter()
                        .any(|runnable| extension.eq_ignore_ascii_case(runnable))
                })
        })
}

/// Pure candidate selection behind `where.exe`-style lookups: for each
/// `PATH` entry in order, tries `name` with each `PATHEXT` extension in order
/// (default `.COM;.EXE;.BAT;.CMD`). Like `where`, a name that already ends in
/// a `PATHEXT` extension (case-insensitive) is tried as written instead.
/// Entries are `;`-separated, may be double-quoted, and empty ones are
/// skipped. No filtering happens here; callers vet the candidates.
pub fn search_path_candidates(
    name: &str,
    path: &str,
    pathext: Option<&str>,
    exists: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let extensions: Vec<&str> = pathext
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .collect();
    let has_extension = extensions.iter().any(|extension| {
        name.len() > extension.len()
            && name.is_char_boundary(name.len() - extension.len())
            && name[name.len() - extension.len()..].eq_ignore_ascii_case(extension)
    });
    let names: Vec<String> = if has_extension {
        vec![name.to_owned()]
    } else {
        extensions
            .iter()
            .map(|extension| format!("{name}{extension}"))
            .collect()
    };

    let mut entries = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in path.chars() {
        match character {
            '"' => quoted = !quoted,
            ';' if !quoted => entries.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    entries.push(current);

    let mut found = Vec::new();
    for entry in entries.iter().filter(|entry| !entry.is_empty()) {
        for file in &names {
            let candidate = Path::new(entry).join(file);
            if exists(&candidate) {
                found.push(candidate);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_verbatim_path_is_simplified_when_it_means_the_same() {
        let simple = |path: &str| super::simplified_windows(path);
        assert_eq!(
            simple(r"\\?\C:\Users\me\work").as_deref(),
            Some(r"C:\Users\me\work")
        );
        assert_eq!(
            simple(r"\\?\UNC\server\share\work").as_deref(),
            Some(r"\\server\share\work")
        );
        assert_eq!(simple(r"C:\Users\me"), None, "not verbatim");
        assert_eq!(simple(r"\\?\C:\work\con.txt"), None, "a device name");
        assert_eq!(simple(r"\\?\C:\work\trailing."), None, "a trailing dot");
        assert_eq!(simple(r"\\?\Volume{0}\work"), None, "not a drive");
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(simple(&long), None, "longer than MAX_PATH");
    }

    #[test]
    fn user_data_root_is_below_home() {
        let home = super::home_dir().expect("home directory");
        assert_eq!(
            super::user_data_root().expect("user data root"),
            home.join(".cookie-agent")
        );
    }

    #[test]
    fn the_machine_id_is_a_stable_opaque_digest() {
        let Some(id) = super::machine_id() else {
            return;
        };
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()), "{id}");
        assert_eq!(super::machine_id().as_deref(), Some(id.as_str()));
    }
}

#[cfg(test)]
mod path_search_tests {
    use std::path::Path;

    use super::search_path_candidates;

    // `Path::join` uses the host separator, so compare with separators normalized.
    fn norm(path: &str) -> String {
        path.replace('\\', "/")
    }

    fn search(name: &str, path: &str, pathext: Option<&str>, present: &[&str]) -> Vec<String> {
        let present: Vec<String> = present.iter().map(|file| norm(file)).collect();
        search_path_candidates(name, path, pathext, |candidate: &Path| {
            present.contains(&norm(&candidate.to_string_lossy()))
        })
        .iter()
        .map(|candidate| norm(&candidate.to_string_lossy()))
        .collect()
    }

    fn expected(files: &[&str]) -> Vec<String> {
        files.iter().map(|file| norm(file)).collect()
    }

    #[test]
    fn candidates_follow_path_order_then_extension_order() {
        let found = search(
            "bash",
            r"C:\b;C:\a",
            Some(".EXE;.CMD"),
            &[
                r"C:\a\bash.EXE",
                r"C:\b\bash.CMD",
                r"C:\b\bash.EXE",
                r"C:\a\bash.CMD",
            ],
        );
        assert_eq!(
            found,
            expected(&[
                r"C:\b\bash.EXE",
                r"C:\b\bash.CMD",
                r"C:\a\bash.EXE",
                r"C:\a\bash.CMD"
            ])
        );
    }

    #[test]
    fn pathext_extensions_are_used_verbatim_and_default_applies() {
        assert_eq!(
            search(
                "bash",
                r"C:\a",
                Some(".exe"),
                &[r"C:\a\bash.exe", r"C:\a\bash.EXE"]
            ),
            expected(&[r"C:\a\bash.exe"])
        );
        assert_eq!(
            search("bash", r"C:\a", None, &[r"C:\a\bash.EXE", r"C:\a\bash.BAT"]),
            expected(&[r"C:\a\bash.EXE", r"C:\a\bash.BAT"])
        );
        assert!(search("bash", r"C:\a", Some(".EXE"), &[r"C:\a\bash"]).is_empty());
    }

    #[test]
    fn name_with_known_extension_is_tried_as_written_case_insensitively() {
        assert_eq!(
            search(
                "git.exe",
                r"C:\a",
                Some(".EXE"),
                &[r"C:\a\git.exe", r"C:\a\git.exe.EXE"]
            ),
            expected(&[r"C:\a\git.exe"])
        );
        assert_eq!(
            search("GIT.Exe", r"C:\a", Some(".exe"), &[r"C:\a\GIT.Exe"]),
            expected(&[r"C:\a\GIT.Exe"])
        );
    }

    #[test]
    fn system32_candidate_is_kept_in_order_for_the_validator_to_reject() {
        let found = search(
            "bash",
            r"C:\Windows\System32;C:\Git\bin",
            None,
            &[r"C:\Windows\System32\bash.EXE", r"C:\Git\bin\bash.EXE"],
        );
        assert_eq!(
            found,
            expected(&[r"C:\Windows\System32\bash.EXE", r"C:\Git\bin\bash.EXE"])
        );
    }

    #[test]
    fn quoted_and_empty_entries_are_handled() {
        let found = search(
            "bash",
            r#";;"C:\Program Files\Git\bin";"C:\semi;colon";"#,
            Some(".EXE"),
            &[
                r"C:\Program Files\Git\bin\bash.EXE",
                r"C:\semi;colon\bash.EXE",
            ],
        );
        assert_eq!(
            found,
            expected(&[
                r"C:\Program Files\Git\bin\bash.EXE",
                r"C:\semi;colon\bash.EXE"
            ])
        );
    }

    #[test]
    fn no_match_and_empty_path_yield_nothing() {
        assert!(search("bash", r"C:\a;C:\b", None, &[]).is_empty());
        assert!(search("bash", "", None, &["bash.EXE"]).is_empty());
    }

    fn program(name: &str, path: &str, pathext: Option<&str>, present: &[&str]) -> Option<String> {
        let present: Vec<String> = present.iter().map(|file| norm(file)).collect();
        super::windows_program(name, path, pathext, |candidate: &Path| {
            present.contains(&norm(&candidate.to_string_lossy()))
        })
        .map(|found| norm(&found.to_string_lossy()))
    }

    #[test]
    fn a_bare_program_resolves_to_its_first_runnable_launcher() {
        assert_eq!(
            program(
                "npx",
                r"C:\empty;C:\node",
                None,
                &[r"C:\node\npx", r"C:\node\npx.CMD"]
            ),
            Some(norm(r"C:\node\npx.CMD"))
        );
        // `CreateProcess` cannot start a PowerShell script, even when
        // `PATHEXT` lists it first.
        assert_eq!(
            program(
                "npx",
                r"C:\node",
                Some(".PS1;.EXE;.CMD"),
                &[r"C:\node\npx.PS1", r"C:\node\npx.CMD"]
            ),
            Some(norm(r"C:\node\npx.CMD"))
        );
        assert_eq!(
            program("npx", r"C:\node", Some(".PS1"), &[r"C:\node\npx.PS1"]),
            None
        );
    }

    #[test]
    fn paths_extensions_and_misses_are_left_to_command() {
        let present = [r"C:\node\npx.CMD", r"C:\node\npx.cmd.CMD"];
        assert_eq!(program(r"C:\node\npx", r"C:\node", None, &present), None);
        assert_eq!(program(r"node\npx", r"C:", None, &present), None);
        assert_eq!(program("./npx", r"C:\node", None, &present), None);
        assert_eq!(program("C:npx", r"C:\node", None, &present), None);
        assert_eq!(program("npx.cmd", r"C:\node", None, &present), None);
        assert_eq!(program("", r"C:\node", None, &present), None);
        assert_eq!(program("uvx", r"C:\node", None, &present), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn programs_are_unchanged_off_windows() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("cookie-shim.cmd"), "").unwrap();
        let path = directory.path().to_string_lossy().into_owned();
        assert_eq!(
            super::resolve_program("cookie-shim", [("PATH", path.as_str())]),
            Path::new("cookie-shim")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_cmd_launcher_on_the_child_path_runs_with_literal_arguments() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("cookie-shim.cmd"),
            "@echo off\r\necho %*> \"%~dp0out.txt\"\r\n",
        )
        .unwrap();
        let path = directory.path().to_string_lossy().into_owned();
        let program = super::resolve_program("cookie-shim", [("Path", path.as_str())]);
        assert_eq!(
            program.to_string_lossy().to_lowercase(),
            directory
                .path()
                .join("cookie-shim.cmd")
                .to_string_lossy()
                .to_lowercase()
        );
        // The BatBadBut shape: a quote that would end the argument and an
        // `&` that would start a second command.
        let status = std::process::Command::new(&program)
            .args(["hello world", "x\" & echo injected> pwned.txt & \""])
            .current_dir(directory.path())
            .status()
            .unwrap();
        assert!(status.success());
        let out = std::fs::read_to_string(directory.path().join("out.txt")).unwrap();
        assert!(out.contains("hello world"), "{out}");
        assert!(!directory.path().join("pwned.txt").exists(), "{out}");
    }

    #[cfg(windows)]
    #[test]
    fn a_missing_program_is_left_to_command() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_string_lossy().into_owned();
        assert_eq!(
            super::resolve_program("cookie-missing", [("PATH", path.as_str())]),
            Path::new("cookie-missing")
        );
    }
}
