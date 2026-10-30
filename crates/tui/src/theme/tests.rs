use ratatui::style::Color;

use super::{ColorLevel, Theme, ThemeKind};

#[test]
fn environment_selects_themes_and_disables_color_safely() {
    assert_eq!(
        Theme::from_environment("high-contrast", false, "xterm-256color", "").key(),
        super::ThemeKey {
            kind: ThemeKind::HighContrast,
            colors: ColorLevel::Ansi16,
        }
    );
    for requested in ["dark", "dark-roast", "darkroast"] {
        assert_eq!(
            Theme::from_environment(requested, false, "xterm-256color", "").key(),
            super::ThemeKey {
                kind: ThemeKind::Dark,
                colors: ColorLevel::Ansi256,
            }
        );
    }
    for theme in [
        Theme::from_environment("mono", false, "xterm-256color", "truecolor"),
        Theme::from_environment("default", true, "xterm-256color", "truecolor"),
        Theme::from_environment("default", false, "dumb", "truecolor"),
        Theme::from_environment("dark", true, "xterm-256color", "truecolor"),
        Theme::from_environment("dark-roast", false, "dumb", "truecolor"),
        Theme::new(ThemeKind::Mono, ColorLevel::TrueColor),
    ] {
        assert_eq!(theme.key().kind, ThemeKind::Mono);
        assert_eq!(theme.key().colors, ColorLevel::None);
        assert!(theme.assistant().fg.is_none());
    }
}

#[test]
fn colors_quantize_for_terminal_capabilities() {
    assert!(matches!(
        Theme::new(ThemeKind::Default, ColorLevel::TrueColor).quantize_rgb(12, 34, 56),
        Some(Color::Rgb(12, 34, 56))
    ));
    assert!(matches!(
        Theme::new(ThemeKind::Default, ColorLevel::Ansi256).quantize_rgb(12, 34, 56),
        Some(Color::Indexed(_))
    ));
    assert!(matches!(
        Theme::new(ThemeKind::Default, ColorLevel::Ansi16).quantize_rgb(12, 34, 56),
        Some(Color::Black | Color::Blue | Color::DarkGray)
    ));
}

#[test]
fn known_truecolor_terminals_fill_in_a_missing_colorterm() {
    use super::colorterm_with_hints;
    // Windows Terminal (and WSL inside it) never sets COLORTERM.
    assert_eq!(colorterm_with_hints(String::new(), true, ""), "truecolor");
    assert_eq!(
        colorterm_with_hints(String::new(), false, "vscode"),
        "truecolor"
    );
    assert_eq!(
        colorterm_with_hints(String::new(), false, "Apple_Terminal"),
        ""
    );
    assert_eq!(colorterm_with_hints(String::new(), false, ""), "");
    // An explicit COLORTERM always wins.
    assert_eq!(colorterm_with_hints("256".into(), true, "vscode"), "256");
    let theme = Theme::from_kind_environment(
        ThemeKind::Default,
        false,
        "xterm-256color",
        &colorterm_with_hints(String::new(), true, ""),
    );
    assert_eq!(theme.key().colors, ColorLevel::TrueColor);
}
