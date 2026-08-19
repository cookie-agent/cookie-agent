//! Startup-only terminal background detection for automatic theme selection.

use std::{env, fmt, io};

use crate::{
    config::ThemePreference,
    theme::{Theme, ThemeKind},
};

const OSC11_QUERY: &[u8] = b"\x1b]11;?\x1b\\";
const OSC11_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(75);
const OSC11_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(20);
const OSC11_RESPONSE_CAP: usize = 512;
const OSC11_DRAIN_CAP: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeDetectionSource {
    Explicit,
    Osc11,
    ColorFgBg,
    Fallback,
}

impl fmt::Display for ThemeDetectionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Explicit => "explicit",
            Self::Osc11 => "osc11",
            Self::ColorFgBg => "colorfgbg",
            Self::Fallback => "fallback",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeDetection {
    pub kind: ThemeKind,
    pub source: ThemeDetectionSource,
}

trait Osc11Query {
    fn is_eligible(&self) -> bool;
    fn query(&mut self) -> Option<Vec<u8>>;
}

struct SystemOsc11Query;

impl Osc11Query for SystemOsc11Query {
    fn is_eligible(&self) -> bool {
        terminal_is_eligible()
    }

    fn query(&mut self) -> Option<Vec<u8>> {
        query_osc11()
    }
}

/// Resolve a TUI startup theme, querying the terminal only when neither config
/// nor `COOKIE_THEME` chooses an explicit theme.
pub fn detect_startup_theme(preference: Option<ThemePreference>) -> ThemeDetection {
    let cookie_theme = env::var("COOKIE_THEME").ok();
    let colorfgbg = env::var("COLORFGBG").ok();
    detect_with_query(
        preference,
        cookie_theme.as_deref(),
        colorfgbg.as_deref(),
        &mut SystemOsc11Query,
    )
}

/// Resolve themes for headless and test app construction without terminal I/O.
pub(crate) fn theme_without_terminal_detection(preference: Option<ThemePreference>) -> Theme {
    let cookie_theme = env::var("COOKIE_THEME").ok();
    let detection = detect_with_query(preference, cookie_theme.as_deref(), None, &mut NoOsc11Query);
    Theme::with_kind_from_env(detection.kind)
}

struct NoOsc11Query;

impl Osc11Query for NoOsc11Query {
    fn is_eligible(&self) -> bool {
        false
    }

    fn query(&mut self) -> Option<Vec<u8>> {
        None
    }
}

fn detect_with_query(
    preference: Option<ThemePreference>,
    cookie_theme: Option<&str>,
    colorfgbg: Option<&str>,
    query: &mut impl Osc11Query,
) -> ThemeDetection {
    match requested_theme(preference, cookie_theme) {
        RequestedTheme::Explicit(kind) => ThemeDetection {
            kind,
            source: ThemeDetectionSource::Explicit,
        },
        RequestedTheme::Auto => {
            let response = if query.is_eligible() {
                query.query()
            } else {
                None
            };
            if let Some(kind) = response
                .as_deref()
                .and_then(parse_osc11_response)
                .map(theme_for_rgb)
            {
                return ThemeDetection {
                    kind,
                    source: ThemeDetectionSource::Osc11,
                };
            }
            if let Some(kind) = colorfgbg.and_then(parse_colorfgbg) {
                return ThemeDetection {
                    kind,
                    source: ThemeDetectionSource::ColorFgBg,
                };
            }
            ThemeDetection {
                kind: ThemeKind::Default,
                source: ThemeDetectionSource::Fallback,
            }
        }
    }
}

enum RequestedTheme {
    Auto,
    Explicit(ThemeKind),
}

fn requested_theme(
    preference: Option<ThemePreference>,
    cookie_theme: Option<&str>,
) -> RequestedTheme {
    if let Some(preference) = preference {
        return preference
            .explicit_kind()
            .map_or(RequestedTheme::Auto, RequestedTheme::Explicit);
    }
    let cookie_theme = cookie_theme.map(str::to_ascii_lowercase);
    match cookie_theme.as_deref() {
        None | Some("auto") => RequestedTheme::Auto,
        Some("dark" | "dark-roast" | "darkroast") => RequestedTheme::Explicit(ThemeKind::Dark),
        Some("mono" | "monochrome") => RequestedTheme::Explicit(ThemeKind::Mono),
        Some("high-contrast" | "high_contrast" | "contrast") => {
            RequestedTheme::Explicit(ThemeKind::HighContrast)
        }
        Some("default") => RequestedTheme::Explicit(ThemeKind::Default),
        // Preserve the previous fail-open behavior for unknown explicit values.
        Some(_) => RequestedTheme::Explicit(ThemeKind::Default),
    }
}

fn parse_osc11_response(response: &[u8]) -> Option<(f64, f64, f64)> {
    let response = std::str::from_utf8(response).ok()?;
    let channels = response.get(response.find("rgb:")? + 4..)?;
    let mut channels = channels.split('/');
    let (red, red_digits) = parse_rgb_component(channels.next()?)?;
    let (green, green_digits) = parse_rgb_component(channels.next()?)?;
    let (blue, blue_digits) = parse_rgb_component(channels.next()?)?;
    if red_digits != green_digits || red_digits != blue_digits {
        return None;
    }
    Some((red, green, blue))
}

fn parse_rgb_component(component: &str) -> Option<(f64, usize)> {
    let digits = component
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count();
    if digits != 2 && digits != 4 {
        return None;
    }
    let value = u16::from_str_radix(component.get(..digits)?, 16).ok()?;
    Some((
        f64::from(value) / if digits == 2 { 255.0 } else { 65_535.0 },
        digits,
    ))
}

fn theme_for_rgb(rgb: (f64, f64, f64)) -> ThemeKind {
    if relative_luminance(rgb) < 0.5 {
        ThemeKind::Dark
    } else {
        ThemeKind::Default
    }
}

fn relative_luminance((red, green, blue): (f64, f64, f64)) -> f64 {
    fn linearize(channel: f64) -> f64 {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linearize(red) + 0.7152 * linearize(green) + 0.0722 * linearize(blue)
}

fn parse_colorfgbg(value: &str) -> Option<ThemeKind> {
    let mut fields = value.split(';').map(str::trim);
    let foreground = fields.next()?;
    let second = fields.next()?;
    let third = fields.next();
    if fields.next().is_some() || foreground.parse::<u8>().ok()? > 15 {
        return None;
    }
    let background = match third {
        None => second,
        Some(background) if second.eq_ignore_ascii_case("default") => background,
        Some(_) => return None,
    };
    if background.eq_ignore_ascii_case("default") {
        return None;
    }
    match background.parse::<u8>().ok()? {
        0..=6 | 8 => Some(ThemeKind::Dark),
        7 | 9..=15 => Some(ThemeKind::Default),
        _ => None,
    }
}

fn terminal_is_eligible() -> bool {
    use std::io::IsTerminal as _;

    io::stdout().is_terminal() && io::stdin().is_terminal()
}

#[cfg(unix)]
fn query_osc11_io(
    input: &mut impl io::Read,
    output: &mut impl io::Write,
    query_timeout: std::time::Duration,
    drain_timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    use std::{thread, time::Instant};

    output.write_all(OSC11_QUERY).ok()?;
    output.flush().ok()?;

    let deadline = Instant::now() + query_timeout;
    let mut response = Vec::with_capacity(64);
    let mut buffer = [0_u8; 128];
    while Instant::now() < deadline && response.len() < OSC11_RESPONSE_CAP {
        match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response.contains(&b'\x07')
                    || response.windows(2).any(|window| window == b"\x1b\\")
                {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }

    // A terminal may begin its reply just after the main deadline or split it
    // across reads. Consume a short, bounded grace window before EventStream
    // takes ownership of stdin so those bytes cannot become key events.
    let drain_deadline = Instant::now() + drain_timeout;
    let mut drained = 0;
    while Instant::now() < drain_deadline && drained < OSC11_DRAIN_CAP {
        match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => drained += read,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }

    (!response.is_empty()).then_some(response)
}

#[cfg(unix)]
fn query_osc11() -> Option<Vec<u8>> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }

    enable_raw_mode().ok()?;
    let _raw_mode = RawModeGuard;
    let stdin = io::stdin();
    let original_flags = fcntl_getfl(&stdin).ok()?;
    fcntl_setfl(&stdin, original_flags | OFlags::NONBLOCK).ok()?;
    struct NonblockingGuard<'a> {
        input: &'a io::Stdin,
        original_flags: OFlags,
    }
    impl Drop for NonblockingGuard<'_> {
        fn drop(&mut self) {
            let _ = fcntl_setfl(self.input, self.original_flags);
        }
    }
    let _nonblocking = NonblockingGuard {
        input: &stdin,
        original_flags,
    };
    let mut input = stdin.lock();
    let mut output = io::stdout().lock();
    query_osc11_io(&mut input, &mut output, OSC11_TIMEOUT, OSC11_DRAIN_TIMEOUT)
}

#[cfg(not(unix))]
fn query_osc11() -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CannedQuery {
        response: Option<Vec<u8>>,
        calls: usize,
        eligible: bool,
    }

    impl Default for CannedQuery {
        fn default() -> Self {
            Self {
                response: None,
                calls: 0,
                eligible: true,
            }
        }
    }

    impl Osc11Query for CannedQuery {
        fn is_eligible(&self) -> bool {
            self.eligible
        }

        fn query(&mut self) -> Option<Vec<u8>> {
            self.calls += 1;
            self.response.take()
        }
    }

    #[test]
    fn parses_osc11_rgb_responses() {
        assert_eq!(
            parse_osc11_response(b"\x1b]11;rgb:ffff/8080/0000\x1b\\"),
            Some((1.0, 32_896.0 / 65_535.0, 0.0))
        );
        assert_eq!(
            parse_osc11_response(b"\x1b]11;rgb:ff/80/00\x07"),
            Some((1.0, 128.0 / 255.0, 0.0))
        );
        assert_eq!(parse_osc11_response(b"garbage"), None);
        assert_eq!(parse_osc11_response(b""), None);
        assert_eq!(parse_osc11_response(b"rgb:fff/0000/0000"), None);
        assert_eq!(parse_osc11_response(b"rgb:ff/0000/0000"), None);
    }

    #[test]
    fn colorfgbg_classifies_documented_indices() {
        for index in [0, 1, 2, 3, 4, 5, 6, 8] {
            assert_eq!(
                parse_colorfgbg(&format!("15;{index}")),
                Some(ThemeKind::Dark)
            );
        }
        for index in [7, 9, 10, 11, 12, 13, 14, 15] {
            assert_eq!(
                parse_colorfgbg(&format!("0;default;{index}")),
                Some(ThemeKind::Default)
            );
        }
        for value in [
            "0",
            "0;default",
            "default",
            "0;16",
            "0;nope",
            "0;default;1;7",
            "0;wrong;7",
            "x;7",
            "16;7",
            "",
        ] {
            assert_eq!(parse_colorfgbg(value), None, "{value:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn delayed_osc_reply_is_drained_before_event_handling() {
        use std::{
            io::{Read as _, Write as _},
            os::unix::net::UnixStream,
            thread,
            time::Duration,
        };

        let (mut app_input, mut terminal) = UnixStream::pair().expect("terminal pair");
        app_input
            .set_nonblocking(true)
            .expect("nonblocking app input");
        let mut query_output = app_input.try_clone().expect("query output");
        let terminal_reply = thread::spawn(move || {
            let mut query = [0_u8; OSC11_QUERY.len()];
            terminal.read_exact(&mut query).expect("read query");
            assert_eq!(&query, OSC11_QUERY);
            thread::sleep(Duration::from_millis(15));
            terminal
                .write_all(b"\x1b]11;rgb:0000/0000/0000\x1b\\")
                .expect("write delayed reply");
            // Keep the peer open until the assertion so an empty read is
            // distinguishable from EOF.
            thread::sleep(Duration::from_millis(80));
        });

        let _ = query_osc11_io(
            &mut app_input,
            &mut query_output,
            Duration::from_millis(10),
            OSC11_DRAIN_TIMEOUT,
        );
        let mut event_bytes = [0_u8; 64];
        let read = app_input.read(&mut event_bytes);
        assert!(
            matches!(read, Err(ref error) if error.kind() == io::ErrorKind::WouldBlock),
            "delayed OSC bytes reached event handling: {read:?}"
        );
        terminal_reply.join().expect("terminal reply thread");
    }

    #[test]
    fn luminance_threshold_selects_dark_below_half() {
        assert_eq!(theme_for_rgb((0.0, 0.0, 0.0)), ThemeKind::Dark);
        assert_eq!(theme_for_rgb((1.0, 1.0, 1.0)), ThemeKind::Default);
        assert_eq!(theme_for_rgb((0.735, 0.735, 0.735)), ThemeKind::Dark);
        assert_eq!(theme_for_rgb((0.736, 0.736, 0.736)), ThemeKind::Default);
    }

    #[test]
    fn explicit_theme_beats_detection_and_reports_source() {
        let mut query = CannedQuery {
            response: Some(b"rgb:0000/0000/0000".to_vec()),
            ..CannedQuery::default()
        };
        let detection = detect_with_query(
            Some(ThemePreference::Default),
            Some("dark"),
            Some("0;0"),
            &mut query,
        );
        assert_eq!(
            detection,
            ThemeDetection {
                kind: ThemeKind::Default,
                source: ThemeDetectionSource::Explicit,
            }
        );
        assert_eq!(query.calls, 0);
        assert_eq!(ThemeDetectionSource::Explicit.to_string(), "explicit");

        let mut cookie_query = CannedQuery {
            response: Some(b"rgb:ffff/ffff/ffff".to_vec()),
            ..CannedQuery::default()
        };
        assert_eq!(
            detect_with_query(None, Some("dark"), Some("0;15"), &mut cookie_query),
            ThemeDetection {
                kind: ThemeKind::Dark,
                source: ThemeDetectionSource::Explicit,
            }
        );
        assert_eq!(cookie_query.calls, 0);
    }

    #[test]
    fn non_tty_skips_the_query_entirely() {
        let mut query = CannedQuery {
            response: Some(b"rgb:0000/0000/0000".to_vec()),
            eligible: false,
            ..CannedQuery::default()
        };
        assert_eq!(
            detect_with_query(None, None, Some("0;15"), &mut query),
            ThemeDetection {
                kind: ThemeKind::Default,
                source: ThemeDetectionSource::ColorFgBg,
            }
        );
        assert_eq!(query.calls, 0, "ineligible terminal was queried");
    }

    #[test]
    fn auto_uses_osc_then_colorfgbg_then_fallback() {
        let mut osc = CannedQuery {
            response: Some(b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec()),
            ..CannedQuery::default()
        };
        assert_eq!(
            detect_with_query(None, None, Some("0;15"), &mut osc),
            ThemeDetection {
                kind: ThemeKind::Dark,
                source: ThemeDetectionSource::Osc11,
            }
        );

        let mut absent = CannedQuery::default();
        assert_eq!(
            detect_with_query(
                Some(ThemePreference::Auto),
                Some("dark"),
                Some("0;15"),
                &mut absent
            ),
            ThemeDetection {
                kind: ThemeKind::Default,
                source: ThemeDetectionSource::ColorFgBg,
            }
        );
        let mut invalid = CannedQuery {
            response: Some(b"not an OSC response".to_vec()),
            ..CannedQuery::default()
        };
        assert_eq!(
            detect_with_query(None, None, Some("0;0"), &mut invalid),
            ThemeDetection {
                kind: ThemeKind::Dark,
                source: ThemeDetectionSource::ColorFgBg,
            }
        );
        assert_eq!(
            detect_with_query(None, Some("auto"), Some("default"), &mut absent),
            ThemeDetection {
                kind: ThemeKind::Default,
                source: ThemeDetectionSource::Fallback,
            }
        );
    }

    #[test]
    fn no_color_coercion_happens_after_detection() {
        let mut query = CannedQuery {
            response: Some(b"rgb:0000/0000/0000".to_vec()),
            ..CannedQuery::default()
        };
        let detection = detect_with_query(None, None, None, &mut query);
        assert_eq!(detection.kind, ThemeKind::Dark);
        let theme =
            Theme::from_kind_environment(detection.kind, true, "xterm-256color", "truecolor");
        assert_eq!(theme.key().kind, ThemeKind::Mono);
    }
}
