use insta::assert_snapshot;
use ratatui::style::{Color, Modifier, Style};

use super::{ColorLevel, DecisionTone, Theme, ThemeKind};

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
fn diff_styles_quantize_at_every_color_level() {
    for (level, expected) in [
        (ColorLevel::None, "none"),
        (ColorLevel::Ansi16, "ansi16"),
        (ColorLevel::Ansi256, "ansi256"),
        (ColorLevel::TrueColor, "truecolor"),
    ] {
        let theme = Theme::new(ThemeKind::Default, level);
        let added = theme.diff_added();
        let removed = theme.diff_removed();
        match expected {
            "none" => {
                assert!(added.fg.is_none());
                assert!(removed.fg.is_none());
            }
            "ansi16" => {
                assert!(
                    added
                        .fg
                        .is_some_and(|color| !matches!(color, Color::Rgb(..) | Color::Indexed(_)))
                );
                assert!(
                    removed
                        .fg
                        .is_some_and(|color| !matches!(color, Color::Rgb(..) | Color::Indexed(_)))
                );
            }
            "ansi256" => {
                assert!(matches!(added.fg, Some(Color::Indexed(_))));
                assert!(matches!(removed.fg, Some(Color::Indexed(_))));
            }
            "truecolor" => {
                assert!(matches!(added.fg, Some(Color::Rgb(..))));
                assert!(matches!(removed.fg, Some(Color::Rgb(..))));
            }
            _ => unreachable!(),
        }
        if level != ColorLevel::None {
            assert_ne!(added.fg, removed.fg);
        }
    }
}

#[test]
fn semantic_theme_snapshot_is_deterministic_with_and_without_color() {
    fn signature(name: &str, style: Style) -> String {
        format!(
            "{name}: fg={:?} bg={:?} bold={} italic={} underline={} dim={} reverse={}",
            style.fg,
            style.bg,
            style.add_modifier.contains(Modifier::BOLD),
            style.add_modifier.contains(Modifier::ITALIC),
            style.add_modifier.contains(Modifier::UNDERLINED),
            style.add_modifier.contains(Modifier::DIM),
            style.add_modifier.contains(Modifier::REVERSED),
        )
    }

    let default = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
    let contrast = Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16);
    let mono = Theme::new(ThemeKind::Mono, ColorLevel::None);
    let snapshot = [
        signature("default.surface", default.surface()),
        signature("default.panel", default.panel()),
        signature("default.user", default.user()),
        signature("default.assistant", default.assistant()),
        signature("default.tool_running", default.tool_running()),
        signature("default.tool_success", default.tool_success()),
        signature("default.tool_failure", default.tool_failure()),
        signature("default.selected", default.selected()),
        signature("default.hover", default.hover()),
        signature("default.block_hover", default.block_hover()),
        signature(
            "default.terminal_background",
            Style::default().bg(default.terminal_background().unwrap()),
        ),
        signature(
            "default.decision.allow",
            default.decision(DecisionTone::Allow, false),
        ),
        signature(
            "default.decision.deny.active",
            default.decision(DecisionTone::Deny, true),
        ),
        signature("contrast.user", contrast.user()),
        signature("contrast.assistant", contrast.assistant()),
        signature("contrast.tool_success", contrast.tool_success()),
        signature("contrast.error", contrast.error()),
        signature("contrast.selected", contrast.selected()),
        signature("default.warning", default.warning()),
        signature("contrast.warning", contrast.warning()),
        signature("mono.warning", mono.warning()),
        signature("contrast.heading", contrast.heading()),
        signature("default.inline_code", default.inline_code()),
        signature("contrast.inline_code", contrast.inline_code()),
        signature("mono.link", mono.link()),
        signature("mono.inline_code", mono.inline_code()),
        signature("mono.selected", mono.selected()),
        signature("mono.hover", mono.hover()),
        signature("mono.block_hover", mono.block_hover()),
        signature(
            "mono.terminal_background",
            Style {
                bg: mono.terminal_background(),
                ..Style::default()
            },
        ),
        signature("contrast.block_hover", contrast.block_hover()),
        signature(
            "contrast.terminal_background",
            Style {
                bg: contrast.terminal_background(),
                ..Style::default()
            },
        ),
    ]
    .join("\n");
    assert_snapshot!(snapshot, @"
        default.surface: fg=Some(Rgb(70, 48, 31)) bg=Some(Rgb(251, 244, 230)) bold=false italic=false underline=false dim=false reverse=false
        default.panel: fg=None bg=Some(Rgb(251, 244, 230)) bold=false italic=false underline=false dim=false reverse=false
        default.user: fg=Some(Rgb(156, 90, 16)) bg=None bold=true italic=false underline=false dim=false reverse=false
        default.assistant: fg=Some(Rgb(78, 122, 52)) bg=None bold=true italic=false underline=false dim=false reverse=false
        default.tool_running: fg=Some(Rgb(156, 74, 18)) bg=None bold=false italic=false underline=false dim=false reverse=false
        default.tool_success: fg=Some(Rgb(47, 107, 56)) bg=None bold=false italic=false underline=false dim=false reverse=false
        default.tool_failure: fg=Some(Rgb(174, 51, 39)) bg=None bold=true italic=false underline=false dim=false reverse=false
        default.selected: fg=Some(Rgb(70, 48, 31)) bg=Some(Rgb(230, 206, 158)) bold=true italic=false underline=false dim=false reverse=false
        default.hover: fg=None bg=Some(Rgb(235, 216, 174)) bold=false italic=false underline=false dim=false reverse=false
        default.block_hover: fg=None bg=Some(Rgb(235, 216, 174)) bold=false italic=false underline=false dim=false reverse=false
        default.terminal_background: fg=None bg=Some(Rgb(249, 241, 225)) bold=false italic=false underline=false dim=false reverse=false
        default.decision.allow: fg=Some(Rgb(47, 107, 56)) bg=None bold=true italic=false underline=false dim=false reverse=false
        default.decision.deny.active: fg=Some(Rgb(174, 51, 39)) bg=Some(Rgb(243, 213, 201)) bold=true italic=false underline=false dim=false reverse=false
        contrast.user: fg=Some(LightCyan) bg=None bold=true italic=false underline=false dim=false reverse=false
        contrast.assistant: fg=Some(White) bg=None bold=true italic=false underline=false dim=false reverse=false
        contrast.tool_success: fg=Some(LightGreen) bg=None bold=true italic=false underline=false dim=false reverse=false
        contrast.error: fg=Some(LightRed) bg=None bold=true italic=false underline=false dim=false reverse=false
        contrast.selected: fg=Some(Black) bg=Some(LightYellow) bold=true italic=false underline=false dim=false reverse=false
        default.warning: fg=Some(Rgb(122, 82, 6)) bg=None bold=true italic=false underline=false dim=false reverse=false
        contrast.warning: fg=Some(LightYellow) bg=None bold=true italic=false underline=false dim=false reverse=false
        mono.warning: fg=None bg=None bold=true italic=false underline=false dim=false reverse=false
        contrast.heading: fg=Some(White) bg=None bold=true italic=false underline=true dim=false reverse=false
        default.inline_code: fg=Some(Rgb(98, 82, 64)) bg=Some(Rgb(242, 229, 204)) bold=false italic=false underline=false dim=false reverse=false
        contrast.inline_code: fg=Some(Black) bg=Some(LightYellow) bold=true italic=false underline=false dim=false reverse=false
        mono.link: fg=None bg=None bold=true italic=false underline=true dim=false reverse=false
        mono.inline_code: fg=None bg=None bold=true italic=false underline=false dim=false reverse=false
        mono.selected: fg=None bg=None bold=false italic=false underline=false dim=false reverse=true
        mono.hover: fg=None bg=None bold=false italic=false underline=true dim=false reverse=false
        mono.block_hover: fg=None bg=None bold=true italic=false underline=false dim=false reverse=false
        mono.terminal_background: fg=None bg=None bold=false italic=false underline=false dim=false reverse=false
        contrast.block_hover: fg=None bg=Some(DarkGray) bold=false italic=false underline=false dim=false reverse=false
        contrast.terminal_background: fg=None bg=None bold=false italic=false underline=false dim=false reverse=false
        ");
}

#[test]
fn dark_semantic_theme_snapshot_is_deterministic() {
    fn signature(name: &str, style: Style) -> String {
        format!(
            "{name}: fg={:?} bg={:?} bold={} italic={} underline={} dim={} reverse={}",
            style.fg,
            style.bg,
            style.add_modifier.contains(Modifier::BOLD),
            style.add_modifier.contains(Modifier::ITALIC),
            style.add_modifier.contains(Modifier::UNDERLINED),
            style.add_modifier.contains(Modifier::DIM),
            style.add_modifier.contains(Modifier::REVERSED),
        )
    }

    let dark = Theme::new(ThemeKind::Dark, ColorLevel::TrueColor);
    let snapshot = [
        signature("dark.surface", dark.surface()),
        signature("dark.panel", dark.panel()),
        signature("dark.user", dark.user()),
        signature("dark.assistant", dark.assistant()),
        signature("dark.tool_running", dark.tool_running()),
        signature("dark.tool_success", dark.tool_success()),
        signature("dark.tool_failure", dark.tool_failure()),
        signature("dark.selected", dark.selected()),
        signature("dark.hover", dark.hover()),
        signature("dark.block_hover", dark.block_hover()),
        signature(
            "dark.terminal_background",
            Style::default().bg(dark.terminal_background().unwrap()),
        ),
        signature(
            "dark.decision.allow",
            dark.decision(DecisionTone::Allow, false),
        ),
        signature(
            "dark.decision.deny.active",
            dark.decision(DecisionTone::Deny, true),
        ),
        signature("dark.warning", dark.warning()),
        signature("dark.inline_code", dark.inline_code()),
    ]
    .join("\n");
    assert_snapshot!(snapshot, @"
        dark.surface: fg=Some(Rgb(237, 199, 171)) bg=Some(Rgb(32, 28, 22)) bold=false italic=false underline=false dim=false reverse=false
        dark.panel: fg=None bg=Some(Rgb(32, 28, 22)) bold=false italic=false underline=false dim=false reverse=false
        dark.user: fg=Some(Rgb(199, 119, 30)) bg=None bold=true italic=false underline=false dim=false reverse=false
        dark.assistant: fg=Some(Rgb(96, 151, 64)) bg=None bold=true italic=false underline=false dim=false reverse=false
        dark.tool_running: fg=Some(Rgb(217, 130, 70)) bg=None bold=false italic=false underline=false dim=false reverse=false
        dark.tool_success: fg=Some(Rgb(74, 170, 89)) bg=None bold=false italic=false underline=false dim=false reverse=false
        dark.tool_failure: fg=Some(Rgb(242, 110, 97)) bg=None bold=true italic=false underline=false dim=false reverse=false
        dark.selected: fg=Some(Rgb(237, 199, 171)) bg=Some(Rgb(99, 86, 59)) bold=true italic=false underline=false dim=false reverse=false
        dark.hover: fg=None bg=Some(Rgb(76, 67, 48)) bold=false italic=false underline=false dim=false reverse=false
        dark.block_hover: fg=None bg=Some(Rgb(76, 67, 48)) bold=false italic=false underline=false dim=false reverse=false
        dark.terminal_background: fg=None bg=Some(Rgb(41, 37, 29)) bold=false italic=false underline=false dim=false reverse=false
        dark.decision.allow: fg=Some(Rgb(74, 170, 89)) bg=None bold=true italic=false underline=false dim=false reverse=false
        dark.decision.deny.active: fg=Some(Rgb(32, 28, 22)) bg=Some(Rgb(191, 123, 95)) bold=true italic=false underline=false dim=false reverse=false
        dark.warning: fg=Some(Rgb(204, 149, 45)) bg=None bold=true italic=false underline=false dim=false reverse=false
        dark.inline_code: fg=Some(Rgb(201, 181, 156)) bg=Some(Rgb(57, 51, 38)) bold=false italic=false underline=false dim=false reverse=false
        ");
}

#[test]
fn inline_code_is_tinted_in_bakery_palettes_and_bold_elsewhere() {
    for (theme, has_background, bold) in [
        (
            Theme::new(ThemeKind::Default, ColorLevel::TrueColor),
            true,
            false,
        ),
        (
            Theme::new(ThemeKind::Default, ColorLevel::Ansi256),
            true,
            false,
        ),
        (
            Theme::new(ThemeKind::Default, ColorLevel::Ansi16),
            true,
            false,
        ),
        (
            Theme::new(ThemeKind::Dark, ColorLevel::TrueColor),
            true,
            false,
        ),
        (
            Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
            true,
            true,
        ),
        (Theme::new(ThemeKind::Mono, ColorLevel::None), false, true),
    ] {
        let style = theme.inline_code();
        assert_eq!(
            style.bg.is_some(),
            has_background,
            "background: {:?}",
            theme.key()
        );
        assert!(
            !style.add_modifier.contains(Modifier::REVERSED),
            "no reverse video: {:?}",
            theme.key()
        );
        // Distinction is never color-only: the bakery tint is the code-block
        // parchment, and without a tint bold (plus backticks) carries it.
        assert_eq!(
            style.add_modifier.contains(Modifier::BOLD),
            bold,
            "weight: {:?}",
            theme.key()
        );
        if has_background && !bold {
            assert_eq!(style.bg, theme.code_background(), "{:?}", theme.key());
        }
    }
}

#[test]
fn block_hover_never_underlines_at_any_capability() {
    for kind in [
        ThemeKind::Default,
        ThemeKind::Dark,
        ThemeKind::Mono,
        ThemeKind::HighContrast,
    ] {
        for level in [
            ColorLevel::None,
            ColorLevel::Ansi16,
            ColorLevel::Ansi256,
            ColorLevel::TrueColor,
        ] {
            let theme = Theme::new(kind, level);
            let hover = theme.block_hover();
            assert_eq!(hover.fg, None);
            assert_eq!(hover.bg, theme.hover_fill().bg);
            assert!(!hover.add_modifier.contains(Modifier::UNDERLINED));
            assert!(
                !Style::default()
                    .add_modifier(Modifier::UNDERLINED)
                    .patch(hover)
                    .add_modifier
                    .contains(Modifier::UNDERLINED)
            );
            if level == ColorLevel::None {
                assert!(hover.add_modifier.contains(Modifier::BOLD));
            }
            if matches!(kind, ThemeKind::Mono | ThemeKind::HighContrast)
                || level == ColorLevel::None
            {
                assert_eq!(theme.terminal_background(), None);
            }
        }
    }
}

#[test]
fn terminal_bands_stay_close_to_the_surface_and_below_hover() {
    for (kind, rgb, indexed, ansi16) in [
        (ThemeKind::Default, (249, 241, 225), 230, Color::White),
        (ThemeKind::Dark, (41, 37, 29), 235, Color::Black),
    ] {
        for (level, expected) in [
            (ColorLevel::TrueColor, Some(Color::Rgb(rgb.0, rgb.1, rgb.2))),
            (ColorLevel::Ansi256, Some(Color::Indexed(indexed))),
            (ColorLevel::Ansi16, Some(ansi16)),
            (ColorLevel::None, None),
        ] {
            let theme = Theme::new(kind, level);
            assert_eq!(theme.terminal_background(), expected);
            if level != ColorLevel::None {
                assert_ne!(theme.terminal_background(), theme.block_hover().bg);
            }
            for flat in [ThemeKind::Mono, ThemeKind::HighContrast] {
                assert_eq!(Theme::new(flat, level).terminal_background(), None);
            }
        }
        let palette = Theme::new(kind, ColorLevel::TrueColor).palette();
        for (band, surface) in [rgb.0, rgb.1, rgb.2].into_iter().zip([
            palette.cream.rgb.0,
            palette.cream.rgb.1,
            palette.cream.rgb.2,
        ]) {
            assert!(band.abs_diff(surface) <= 10);
        }
    }
}

#[test]
fn warm_background_bands_hand_pick_ansi256_cells() {
    // Each light band names its cell explicitly, and the ladder steps
    // from light to deep: cream surface, parchment code band, glaze
    // hover, toasted selection.
    let theme = Theme::new(ThemeKind::Default, ColorLevel::Ansi256);
    assert_eq!(theme.surface().bg, Some(Color::Indexed(231)));
    assert_eq!(theme.panel().bg, Some(Color::Indexed(231)));
    assert_eq!(theme.code_background(), Some(Color::Indexed(230)));
    assert_eq!(theme.terminal_background(), Some(Color::Indexed(230)));
    assert_eq!(theme.hover().bg, Some(Color::Indexed(223)));
    assert_eq!(theme.hover_fill().bg, Some(Color::Indexed(223)));
    assert_eq!(theme.selected().bg, Some(Color::Indexed(222)));
    assert_eq!(
        theme.quantize_rgb(0xEB, 0xD8, 0xAE),
        Some(Color::Indexed(223))
    );
    assert_eq!(
        theme.quantize_rgb(0xE6, 0xCE, 0x9E),
        Some(Color::Indexed(222))
    );
    // Pane chrome uses the hand-picked walnut cell.
    assert_eq!(theme.panel_border().fg, Some(Color::Indexed(240)));
    assert_eq!(
        Theme::new(ThemeKind::Default, ColorLevel::TrueColor)
            .panel_border()
            .fg,
        Some(Color::Rgb(0x6B, 0x4F, 0x2C))
    );
    // The default theme's hover on a capable terminal is the quiet
    // background alone — no underline, no foreground shift.
    assert!(!theme.hover().add_modifier.contains(Modifier::UNDERLINED));
    assert_eq!(theme.hover().fg, None);
    let truecolor = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
    assert!(
        !truecolor
            .hover()
            .add_modifier
            .contains(Modifier::UNDERLINED)
    );
}

#[test]
fn palette_colors_use_hand_picked_ansi256_cells() {
    let theme = Theme::new(ThemeKind::Default, ColorLevel::Ansi256);
    for (rgb, cell) in [
        (super::LIGHT.cream.rgb, 231),
        (super::LIGHT.terminal.rgb, 230),
        (super::LIGHT.parchment.rgb, 230),
        (super::LIGHT.glaze.rgb, 223),
        (super::LIGHT.toasted.rgb, 222),
        (super::LIGHT.crust.rgb, 180),
        (super::LIGHT.border.rgb, 240),
        (super::LIGHT.tan.rgb, 240),
        (super::LIGHT.honey.rgb, 94),
        (super::LIGHT.espresso.rgb, 236),
        (super::LIGHT.cocoa.rgb, 239),
        (super::LIGHT.latte.rgb, 95),
        (super::LIGHT.ash.rgb, 241),
        (super::LIGHT.quote.rgb, 243),
        (super::LIGHT.caramel.rgb, 130),
        (super::LIGHT.cinnamon.rgb, 131),
        (super::LIGHT.cranberry.rgb, 88),
        (super::LIGHT.maple.rgb, 58),
        (super::LIGHT.sage.rgb, 65),
        (super::LIGHT.basil.rgb, 29),
        (super::LIGHT.slate.rgb, 24),
        (super::LIGHT.plum.rgb, 96),
        (super::LIGHT.terracotta.rgb, 124),
        (super::LIGHT.allow_tint.rgb, 194),
        (super::LIGHT.deny_tint.rgb, 224),
        (super::LIGHT.neutral_tint.rgb, 187),
    ] {
        assert_eq!(
            theme.quantize_rgb(rgb.0, rgb.1, rgb.2),
            Some(Color::Indexed(cell)),
            "palette color {rgb:?}"
        );
    }
}

#[test]
fn dark_palette_uses_hand_picked_ansi256_cells() {
    let theme = Theme::new(ThemeKind::Dark, ColorLevel::Ansi256);
    for swatch in super::DARK.swatches() {
        assert_eq!(
            theme.quantize_rgb(swatch.rgb.0, swatch.rgb.1, swatch.rgb.2),
            Some(Color::Indexed(swatch.ansi256)),
            "dark palette color {:?}",
            swatch.rgb
        );
    }
}

#[test]
fn dark_palette_preserves_hues() {
    fn hsv_hue(rgb: (u8, u8, u8)) -> f64 {
        let red = f64::from(rgb.0) / 255.0;
        let green = f64::from(rgb.1) / 255.0;
        let blue = f64::from(rgb.2) / 255.0;
        let max = red.max(green).max(blue);
        let min = red.min(green).min(blue);
        let delta = max - min;
        if delta == 0.0 {
            0.0
        } else if max == red {
            60.0 * ((green - blue) / delta).rem_euclid(6.0)
        } else if max == green {
            60.0 * ((blue - red) / delta + 2.0)
        } else {
            60.0 * ((red - green) / delta + 4.0)
        }
    }

    let names = [
        "cream",
        "terminal",
        "parchment",
        "glaze",
        "toasted",
        "crust",
        "border",
        "tan",
        "espresso",
        "cocoa",
        "latte",
        "ash",
        "quote",
        "caramel",
        "sage",
        "basil",
        "cinnamon",
        "cranberry",
        "honey",
        "maple",
        "slate",
        "plum",
        "terracotta",
        "allow_tint",
        "deny_tint",
        "neutral_tint",
        "ink",
    ];
    for ((name, light), dark) in names
        .into_iter()
        .zip(super::LIGHT.swatches())
        .zip(super::DARK.swatches())
    {
        let light_hue = hsv_hue(light.rgb);
        let dark_hue = hsv_hue(dark.rgb);
        let delta = (light_hue - dark_hue).abs();
        let delta = delta.min(360.0 - delta);
        // At LIGHT cream's 8% saturation, one 8-bit channel step moves
        // HSV hue by almost 3 degrees; allow that quantization interval.
        let tolerance = if name == "cream" { 4.0 } else { 2.0 };
        assert!(
            delta <= tolerance + 1e-9,
            "{name}: light={light_hue:.2} dark={dark_hue:.2} delta={delta:.2}"
        );
    }
}

#[test]
fn default_quiet_roles_use_values_without_dim() {
    let truecolor = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
    for style in [
        truecolor.panel_border(),
        truecolor.code_border(),
        truecolor.muted(),
        truecolor.internal(),
    ] {
        assert!(!style.add_modifier.contains(Modifier::DIM));
    }
    assert_eq!(
        truecolor.input_border(true).fg,
        Some(Color::Rgb(0x7A, 0x52, 0x06))
    );
    assert!(
        truecolor
            .input_border(true)
            .add_modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        !truecolor
            .input_border(true)
            .add_modifier
            .contains(Modifier::DIM)
    );

    let mono = Theme::new(ThemeKind::Mono, ColorLevel::None);
    assert!(mono.panel_border().add_modifier.contains(Modifier::DIM));

    for colors in [
        ColorLevel::None,
        ColorLevel::Ansi16,
        ColorLevel::Ansi256,
        ColorLevel::TrueColor,
    ] {
        let dark = Theme::new(ThemeKind::Dark, colors);
        for (role, style) in [
            ("panel_border", dark.panel_border()),
            ("code_border", dark.code_border()),
            ("muted", dark.muted()),
            ("internal", dark.internal()),
            ("scrollbar_thumb", dark.scrollbar_thumb()),
        ] {
            assert!(
                !style.add_modifier.contains(Modifier::DIM),
                "Dark {role} retained DIM at {colors:?}"
            );
        }
    }
}

#[test]
fn default_ansi16_uses_revised_quiet_roles() {
    let theme = Theme::new(ThemeKind::Default, ColorLevel::Ansi16);
    for style in [
        theme.panel_border(),
        theme.code_border(),
        theme.internal(),
        theme.muted(),
    ] {
        assert_eq!(style.fg, Some(Color::DarkGray));
        assert!(!style.add_modifier.contains(Modifier::DIM));
    }
    assert_eq!(theme.quote().fg, Some(Color::DarkGray));
    assert!(theme.quote().add_modifier.contains(Modifier::ITALIC));
    assert_eq!(theme.heading().fg, Some(Color::Black));
    assert!(theme.heading().add_modifier.contains(Modifier::BOLD));
}

#[test]
fn dark_ansi16_uses_specified_roles() {
    let theme = Theme::new(ThemeKind::Dark, ColorLevel::Ansi16);
    assert_eq!(theme.surface().fg, Some(Color::White));
    assert_eq!(theme.surface().bg, Some(Color::Black));
    assert_eq!(theme.muted().fg, Some(Color::Gray));
    assert_eq!(theme.internal().fg, Some(Color::DarkGray));
    assert_eq!(theme.quote().fg, Some(Color::DarkGray));
    assert!(theme.quote().add_modifier.contains(Modifier::ITALIC));
    assert_eq!(theme.panel_border().fg, Some(Color::DarkGray));
    assert_eq!(theme.code_border().fg, Some(Color::DarkGray));
    assert_eq!(theme.input_border(true).fg, Some(Color::LightYellow));
    assert_eq!(theme.heading().fg, Some(Color::White));
    assert_eq!(theme.error().fg, Some(Color::LightRed));
    assert_eq!(theme.assistant().fg, Some(Color::LightGreen));
    assert_eq!(theme.tool_success().fg, Some(Color::LightGreen));
    assert_eq!(theme.thinking().fg, Some(Color::LightMagenta));
    assert_eq!(theme.link().fg, Some(Color::LightBlue));
    // Inline code keeps the body colour on the grey tint: sixteen colours
    // have no muted ink that stays distinct from that tint.
    assert_eq!(theme.inline_code().fg, Some(Color::White));
    assert_eq!(theme.inline_code().bg, Some(Color::DarkGray));
    assert_eq!(theme.user().fg, Some(Color::Yellow));
    assert_eq!(theme.tool().fg, Some(Color::Yellow));
    assert_eq!(theme.warning().fg, Some(Color::Yellow));
    assert_eq!(theme.selected().fg, Some(Color::Black));
    assert_eq!(theme.selected().bg, Some(Color::Gray));
    assert_eq!(theme.text_selection().fg, Some(Color::Black));
    assert_eq!(theme.text_selection().bg, Some(Color::LightCyan));
    assert_eq!(theme.hover().bg, Some(Color::DarkGray));
    assert!(theme.hover().add_modifier.contains(Modifier::UNDERLINED));
    assert_eq!(theme.code_background(), Some(Color::DarkGray));

    for (tone, idle_fg, active_bg) in [
        (DecisionTone::Allow, Color::LightGreen, Color::LightGreen),
        (DecisionTone::Deny, Color::LightRed, Color::LightRed),
        (DecisionTone::Neutral, Color::LightCyan, Color::Gray),
    ] {
        let idle = theme.decision(tone, false);
        let active = theme.decision(tone, true);
        assert_eq!(idle.fg, Some(idle_fg));
        assert_eq!(idle.bg, None);
        assert_eq!(active.fg, Some(Color::Black));
        assert_eq!(active.bg, Some(active_bg));
        assert!(idle.add_modifier.contains(Modifier::BOLD));
        assert!(active.add_modifier.contains(Modifier::BOLD));
    }
}

#[test]
fn text_selection_is_background_only_except_on_crude_targets() {
    // The documented contract: where subtle color exists the wash
    // touches only the background, so selected code keeps its syntax
    // colors.
    for theme in [
        Theme::new(ThemeKind::Default, ColorLevel::Ansi256),
        Theme::new(ThemeKind::Default, ColorLevel::TrueColor),
    ] {
        let style = theme.text_selection();
        assert_eq!(style.fg, None, "foreground preserved: {:?}", theme.key());
        assert!(style.bg.is_some(), "crust wash: {:?}", theme.key());
        assert!(!style.add_modifier.contains(Modifier::BOLD));
    }
    // The one documented exception: ANSI-16 and high-contrast text is
    // always bright, which a light-cyan wash would swallow, so the
    // pair is pinned black-on-light-cyan.
    for theme in [
        Theme::new(ThemeKind::Default, ColorLevel::Ansi16),
        Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
    ] {
        let style = theme.text_selection();
        assert_eq!(
            style.fg,
            Some(Color::Black),
            "pinned pair: {:?}",
            theme.key()
        );
        assert_eq!(
            style.bg,
            Some(Color::LightCyan),
            "pinned pair: {:?}",
            theme.key()
        );
    }
    // No color at all: bold reverse video, no color channels set.
    let mono = Theme::new(ThemeKind::Mono, ColorLevel::None).text_selection();
    assert_eq!(mono.fg, None);
    assert_eq!(mono.bg, None);
    assert!(
        mono.add_modifier
            .contains(Modifier::REVERSED | Modifier::BOLD)
    );
}

#[test]
fn surfaces_and_interactions_degrade_gracefully_without_color() {
    let mono = Theme::new(ThemeKind::Mono, ColorLevel::None);
    assert_eq!(mono.surface(), Style::default());
    assert_eq!(mono.panel(), Style::default());
    assert_eq!(mono.code_background(), None);
    assert!(mono.selected().add_modifier.contains(Modifier::REVERSED));
    assert!(mono.hover().add_modifier.contains(Modifier::UNDERLINED));
    let active = mono.decision(DecisionTone::Allow, true);
    assert!(active.add_modifier.contains(Modifier::REVERSED));
    assert!(active.add_modifier.contains(Modifier::BOLD));

    let contrast = Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16);
    assert_eq!(contrast.surface(), Style::default());
    assert_eq!(contrast.code_background(), None);
    let ansi16 = Theme::new(ThemeKind::Default, ColorLevel::Ansi16);
    assert_eq!(ansi16.surface().fg, Some(Color::Black));
    assert_eq!(ansi16.surface().bg, Some(Color::White));
    // Hand-picked light-safe ANSI backgrounds, never nearest-color guesses.
    assert_eq!(ansi16.code_background(), Some(Color::Gray));
    assert_eq!(ansi16.selected().bg, Some(Color::Gray));
    assert_eq!(ansi16.hover().bg, Some(Color::Gray));
    assert!(ansi16.hover().add_modifier.contains(Modifier::UNDERLINED));

    for theme in [
        Theme::new(ThemeKind::Default, ColorLevel::TrueColor),
        Theme::new(ThemeKind::Dark, ColorLevel::TrueColor),
    ] {
        for tone in [
            DecisionTone::Allow,
            DecisionTone::Deny,
            DecisionTone::Neutral,
        ] {
            let idle = theme.decision(tone, false);
            let active = theme.decision(tone, true);
            assert!(idle.bg.is_none(), "idle button has no fill: {tone:?}");
            assert!(active.bg.is_some(), "active button is filled: {tone:?}");
            if theme.key().kind == ThemeKind::Dark {
                assert_eq!(active.fg, theme.surface().bg, "surface on chip: {tone:?}");
            } else {
                assert_eq!(idle.fg, active.fg, "tone survives hover: {tone:?}");
            }
        }
    }
}

#[test]
fn inline_code_ink_is_muted_but_readable_on_its_tint() {
    fn luminance(rgb: (u8, u8, u8)) -> f64 {
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgb.0) + 0.7152 * channel(rgb.1) + 0.0722 * channel(rgb.2)
    }
    fn contrast(first: (u8, u8, u8), second: (u8, u8, u8)) -> f64 {
        let (high, low) = {
            let (a, b) = (luminance(first), luminance(second));
            if a > b { (a, b) } else { (b, a) }
        };
        (high + 0.05) / (low + 0.05)
    }
    for palette in [&super::LIGHT, &super::DARK] {
        let ink = contrast(palette.ink.rgb, palette.parchment.rgb);
        let body = contrast(palette.espresso.rgb, palette.parchment.rgb);
        // WCAG AA for body-size text, yet quieter than the body text.
        assert!(ink >= 4.5, "ink contrast {ink:.2}");
        assert!(
            ink < body,
            "ink {ink:.2} must be quieter than body {body:.2}"
        );
    }
}
