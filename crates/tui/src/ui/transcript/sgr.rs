//! SGR colour sequences in `terminal` display blocks, as ratatui styles.
//!
//! The daemon strips every control sequence from terminal output except SGR
//! (`ESC [ <params> m`), so the only state to track is the graphic rendition
//! those sequences set. It carries across lines, as it does in a terminal.

use ratatui::style::{Color, Modifier, Style};

use crate::theme::{ColorLevel, Theme};

/// The graphic rendition of one terminal stream: what its SGR sequences
/// have set so far.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Rendition {
    fg: Option<Color>,
    bg: Option<Color>,
    modifier: Modifier,
}

impl Rendition {
    /// `base` with this rendition on top of it.
    pub(super) fn style(self, base: Style) -> Style {
        let mut style = base.add_modifier(self.modifier);
        if let Some(fg) = self.fg {
            style = style.fg(fg);
        }
        if let Some(bg) = self.bg {
            style = style.bg(bg);
        }
        style
    }

    /// Applies the parameters of one SGR sequence (the text between `ESC [`
    /// and `m`). Colours are fitted to the theme's colour level; a theme
    /// without colour keeps only the modifiers.
    pub(super) fn apply(&mut self, parameters: &str, theme: &Theme) {
        let mut parameters = parameters.split(';').peekable();
        while let Some(parameter) = parameters.next() {
            // `38:5:n` and `38:2::r:g:b` carry their colour in sub-parameters.
            if parameter.contains(':') {
                let mut parts = parameter.split(':');
                let target = parts.next().and_then(|part| part.parse::<u16>().ok());
                let rest = parts.collect::<Vec<_>>();
                let color = match rest.as_slice() {
                    ["5", index, ..] => index.parse().ok().map(ExtendedColor::Indexed),
                    ["2", "", red, green, blue, ..] | ["2", red, green, blue] => {
                        rgb(red, green, blue).map(ExtendedColor::Rgb)
                    }
                    _ => None,
                };
                if let (Some(target), Some(color)) = (target, color) {
                    self.set_extended(target, color, theme);
                }
                continue;
            }
            let Some(code) = (if parameter.is_empty() {
                Some(0)
            } else {
                parameter.parse::<u16>().ok()
            }) else {
                continue;
            };
            match code {
                0 => *self = Self::default(),
                1 => self.modifier.insert(Modifier::BOLD),
                2 => self.modifier.insert(Modifier::DIM),
                3 => self.modifier.insert(Modifier::ITALIC),
                4 => self.modifier.insert(Modifier::UNDERLINED),
                7 => self.modifier.insert(Modifier::REVERSED),
                9 => self.modifier.insert(Modifier::CROSSED_OUT),
                21 | 22 => self.modifier.remove(Modifier::BOLD | Modifier::DIM),
                23 => self.modifier.remove(Modifier::ITALIC),
                24 => self.modifier.remove(Modifier::UNDERLINED),
                27 => self.modifier.remove(Modifier::REVERSED),
                29 => self.modifier.remove(Modifier::CROSSED_OUT),
                30..=37 => self.fg = basic(code - 30, theme),
                39 => self.fg = None,
                40..=47 => self.bg = basic(code - 40, theme),
                49 => self.bg = None,
                90..=97 => self.fg = basic(code - 90 + 8, theme),
                100..=107 => self.bg = basic(code - 100 + 8, theme),
                38 | 48 => {
                    let color = match parameters.next() {
                        Some("5") => parameters
                            .next()
                            .and_then(|index| index.parse().ok())
                            .map(ExtendedColor::Indexed),
                        Some("2") => {
                            let (red, green, blue) = (
                                parameters.next().unwrap_or_default(),
                                parameters.next().unwrap_or_default(),
                                parameters.next().unwrap_or_default(),
                            );
                            rgb(red, green, blue).map(ExtendedColor::Rgb)
                        }
                        _ => None,
                    };
                    if let Some(color) = color {
                        self.set_extended(code, color, theme);
                    }
                }
                // Blinking, hidden text, fonts, and the rest draw as plain.
                _ => {}
            }
        }
    }

    fn set_extended(&mut self, target: u16, color: ExtendedColor, theme: &Theme) {
        let color = match color {
            ExtendedColor::Indexed(index) => indexed(index, theme),
            ExtendedColor::Rgb((red, green, blue)) => theme.quantize_rgb(red, green, blue),
        };
        match target {
            38 => self.fg = color,
            48 => self.bg = color,
            _ => {}
        }
    }
}

enum ExtendedColor {
    Indexed(u8),
    Rgb((u8, u8, u8)),
}

fn rgb(red: &str, green: &str, blue: &str) -> Option<(u8, u8, u8)> {
    Some((red.parse().ok()?, green.parse().ok()?, blue.parse().ok()?))
}

/// One of the sixteen ANSI colours; the terminal's palette decides how it
/// looks, as it would for the program's own output.
fn basic(index: u16, theme: &Theme) -> Option<Color> {
    if theme.key().colors == ColorLevel::None {
        return None;
    }
    Some(match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    })
}

/// A 256-colour palette entry: itself where the terminal has the palette,
/// else the nearest colour the theme can draw.
fn indexed(index: u8, theme: &Theme) -> Option<Color> {
    match theme.key().colors {
        ColorLevel::None => None,
        _ if index < 16 => basic(u16::from(index), theme),
        ColorLevel::Ansi256 | ColorLevel::TrueColor => Some(Color::Indexed(index)),
        ColorLevel::Ansi16 => {
            let (red, green, blue) = palette_rgb(index);
            theme.quantize_rgb(red, green, blue)
        }
    }
}

/// The RGB value of xterm's 256-colour entry `index` (16 and up).
fn palette_rgb(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let level = 8 + (index - 232) * 10;
        return (level, level, level);
    }
    let cube = index.saturating_sub(16);
    let level = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
    (level(cube / 36), level(cube / 6 % 6), level(cube % 6))
}

/// `text` as runs of one rendition each, starting from `rendition`, which
/// its SGR sequences move on; the sequences themselves are not part of any
/// run. An escape that is not SGR (the daemon removes those) is dropped.
pub(super) fn styled_runs(
    text: &str,
    rendition: &mut Rendition,
    theme: &Theme,
) -> Vec<(Rendition, String)> {
    let mut runs: Vec<(Rendition, String)> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let Some(escape) = rest.find('\u{1b}') else {
            push_run(&mut runs, *rendition, rest);
            break;
        };
        push_run(&mut runs, *rendition, &rest[..escape]);
        rest = &rest[escape + 1..];
        let Some(sequence) = rest.strip_prefix('[') else {
            continue;
        };
        let end = sequence.find(|character: char| {
            !(character.is_ascii_digit() || matches!(character, ';' | ':'))
        });
        match end {
            Some(end) if sequence[end..].starts_with('m') => {
                rendition.apply(&sequence[..end], theme);
                rest = &sequence[end + 1..];
            }
            _ => rest = sequence,
        }
    }
    runs
}

fn push_run(runs: &mut Vec<(Rendition, String)>, rendition: Rendition, text: &str) {
    if text.is_empty() {
        return;
    }
    match runs.last_mut() {
        Some((last, run)) if *last == rendition => run.push_str(text),
        _ => runs.push((rendition, text.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(text: &str, theme: &Theme) -> Vec<(Style, String)> {
        let mut rendition = Rendition::default();
        styled_runs(text, &mut rendition, theme)
            .into_iter()
            .map(|(rendition, text)| (rendition.style(Style::default()), text))
            .collect()
    }

    fn truecolor() -> Theme {
        Theme::new(crate::theme::ThemeKind::Default, ColorLevel::TrueColor)
    }

    #[test]
    fn basic_and_bright_colours_map_to_the_ansi_palette() {
        let theme = truecolor();
        assert_eq!(
            runs("\u{1b}[31mred\u{1b}[0m plain", &theme),
            vec![
                (Style::default().fg(Color::Red), "red".to_owned()),
                (Style::default(), " plain".to_owned()),
            ]
        );
        assert_eq!(
            runs("\u{1b}[94;103mx", &theme),
            vec![(
                Style::default().fg(Color::LightBlue).bg(Color::LightYellow),
                "x".to_owned()
            )]
        );
        // Default foreground and background undo only themselves.
        assert_eq!(
            runs("\u{1b}[1;32;41mx\u{1b}[39my\u{1b}[49mz", &theme),
            vec![
                (
                    Style::default()
                        .fg(Color::Green)
                        .bg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                    "x".to_owned()
                ),
                (
                    Style::default().bg(Color::Red).add_modifier(Modifier::BOLD),
                    "y".to_owned()
                ),
                (
                    Style::default().add_modifier(Modifier::BOLD),
                    "z".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn modifiers_turn_on_and_off() {
        let theme = truecolor();
        let styled = runs("\u{1b}[1;3;4;7;9ma\u{1b}[22;23;24;27;29mb", &theme);
        assert_eq!(
            styled[0].0.add_modifier,
            Modifier::BOLD
                | Modifier::ITALIC
                | Modifier::UNDERLINED
                | Modifier::REVERSED
                | Modifier::CROSSED_OUT
        );
        assert_eq!(styled[1].0, Style::default());
        // `ESC[m` is a reset like `ESC[0m`.
        assert_eq!(
            runs("\u{1b}[2mdim\u{1b}[mplain", &theme)[1],
            (Style::default(), "plain".to_owned())
        );
    }

    #[test]
    fn extended_colours_take_both_spellings_and_fit_the_colour_level() {
        let theme = truecolor();
        for text in ["\u{1b}[38;5;196mx", "\u{1b}[38:5:196mx"] {
            assert_eq!(
                runs(text, &theme)[0].0.fg,
                Some(Color::Indexed(196)),
                "{text:?}"
            );
        }
        for text in [
            "\u{1b}[38;2;10;20;30mx",
            "\u{1b}[38:2::10:20:30mx",
            "\u{1b}[38:2:10:20:30mx",
        ] {
            assert_eq!(
                runs(text, &theme)[0].0.fg,
                Some(Color::Rgb(10, 20, 30)),
                "{text:?}"
            );
        }
        assert_eq!(
            runs("\u{1b}[48;2;1;2;3mx", &theme)[0].0.bg,
            Some(Color::Rgb(1, 2, 3))
        );
        // Palette entries below 16 are the named colours.
        assert_eq!(
            runs("\u{1b}[38;5;9mx", &theme)[0].0.fg,
            Some(Color::LightRed)
        );
        // A sixteen-colour terminal gets the nearest colour it has.
        let ansi16 = Theme::new(crate::theme::ThemeKind::Default, ColorLevel::Ansi16);
        let fitted = runs("\u{1b}[38;2;250;10;10mx", &ansi16)[0].0.fg;
        assert!(
            !matches!(fitted, Some(Color::Rgb(..) | Color::Indexed(_)) | None),
            "{fitted:?}"
        );
        assert!(!matches!(
            runs("\u{1b}[38;5;196mx", &ansi16)[0].0.fg,
            Some(Color::Indexed(_)) | None
        ));
    }

    #[test]
    fn a_theme_without_colour_keeps_only_the_modifiers() {
        let mono = Theme::new(crate::theme::ThemeKind::Mono, ColorLevel::None);
        assert_eq!(
            runs("\u{1b}[1;31;44;38;5;196;48;2;1;2;3mx", &mono),
            vec![(
                Style::default().add_modifier(Modifier::BOLD),
                "x".to_owned()
            )]
        );
    }

    #[test]
    fn the_rendition_carries_across_calls_and_other_escapes_are_dropped() {
        let theme = truecolor();
        let mut rendition = Rendition::default();
        styled_runs("\u{1b}[33mstart", &mut rendition, &theme);
        let next = styled_runs("next line", &mut rendition, &theme);
        assert_eq!(next[0].0.style(Style::default()).fg, Some(Color::Yellow));
        // Unknown codes leave the rendition as it was; a stray escape that
        // is not SGR draws nothing.
        assert_eq!(
            runs("\u{1b}[5;8;60mx\u{1b}y", &theme),
            vec![(Style::default(), "xy".to_owned())]
        );
    }
}
