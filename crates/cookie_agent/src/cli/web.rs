//! `cookie web`: the daemon's web page with its token.

use anyhow::Context as _;
use zeroize::Zeroizing;

use super::daemon_target;

/// `cookie web`: prints the daemon's web page with its token, and opens it
/// in the system browser with `open`.
pub(super) fn run_web(
    url: Option<String>,
    token: Option<String>,
    open: bool,
) -> anyhow::Result<()> {
    let target = daemon_target(url, token)?;
    let link = web_link(&target.url, &target.token)?;
    println!("{}", link.as_str());
    if open {
        open_in_browser(&link)?;
    }
    Ok(())
}

/// The daemon's web page with `token` in its fragment, for the daemon whose
/// client endpoint is `url`: `ws`→`http`, `wss`→`https`, the endpoint's
/// `ws` dropped. The fragment never reaches a server or its logs.
pub(super) fn web_link(url: &str, token: &str) -> anyhow::Result<Zeroizing<String>> {
    let page = cookie_agent_server::page_url(url)
        .with_context(|| format!("{url} is not a daemon client URL ending in /ws"))?;
    Ok(Zeroizing::new(format!("{page}#token={token}")))
}

/// Opens `link` in the system browser.
pub(super) fn open_in_browser(link: &str) -> anyhow::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    let status = command
        .arg(link)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("open the system browser")?;
    anyhow::ensure!(
        status.success(),
        "the system browser did not open ({status})"
    );
    Ok(())
}
