//! Opening a link in the system browser, as `cookie web --open` does.

/// Opens `link` with the system opener (`open` on macOS, `start` on
/// Windows, `xdg-open` elsewhere), without waiting for the browser.
pub(in crate::ui) fn open(link: &str) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    let mut child = command
        .arg(link)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // Reaped in the background, so it never stays a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
