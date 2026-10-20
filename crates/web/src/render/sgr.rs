//! SGR colour sequences in `terminal` display blocks, as styled spans. The
//! daemon strips every control sequence from terminal output except SGR
//! (`ESC [ <params> m`), so the only state is the graphic rendition those
//! sequences set, which carries across lines as in a terminal.

use super::push_escaped;

/// A colour SGR names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Colour {
    /// One of the 16 standard colours, 0–15 (8–15 bright).
    Standard(u8),
    /// A colour of the 256-colour palette beyond the standard 16.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The graphic rendition SGR sequences have set so far.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rendition {
    pub fg: Option<Colour>,
    pub bg: Option<Colour>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strike: bool,
}

impl Rendition {
    /// Applies the parameters of one SGR sequence (the text between `ESC [`
    /// and `m`).
    pub fn apply(&mut self, parameters: &str) {
        let mut parameters = parameters
            .split([';', ':'])
            .map(|parameter| parameter.parse::<u16>().unwrap_or(0))
            .peekable();
        if parameters.peek().is_none() {
            *self = Self::default();
            return;
        }
        while let Some(parameter) = parameters.next() {
            match parameter {
                0 => *self = Self::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                7 => self.inverse = true,
                9 => self.strike = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                27 => self.inverse = false,
                29 => self.strike = false,
                30..=37 => self.fg = Some(Colour::Standard((parameter - 30) as u8)),
                39 => self.fg = None,
                40..=47 => self.bg = Some(Colour::Standard((parameter - 40) as u8)),
                49 => self.bg = None,
                90..=97 => self.fg = Some(Colour::Standard((parameter - 90 + 8) as u8)),
                100..=107 => self.bg = Some(Colour::Standard((parameter - 100 + 8) as u8)),
                38 | 48 => {
                    let colour = match parameters.next() {
                        Some(5) => parameters.next().map(|index| {
                            let index = index.min(255) as u8;
                            if index < 16 {
                                Colour::Standard(index)
                            } else {
                                Colour::Indexed(index)
                            }
                        }),
                        Some(2) => {
                            let mut channel = || parameters.next().unwrap_or(0).min(255) as u8;
                            Some(Colour::Rgb(channel(), channel(), channel()))
                        }
                        _ => None,
                    };
                    if parameter == 38 {
                        self.fg = colour;
                    } else {
                        self.bg = colour;
                    }
                }
                _ => {}
            }
        }
    }

    fn is_plain(&self) -> bool {
        *self == Self::default()
    }

    /// The span's `class` and `style` attributes. Standard colours are
    /// classes the palette defines for light and dark; others are inline.
    fn attributes(&self) -> String {
        let (fg, bg) = if self.inverse {
            (self.bg, self.fg)
        } else {
            (self.fg, self.bg)
        };
        let mut classes = Vec::new();
        let mut styles = Vec::new();
        for (colour, kind) in [(fg, "fg"), (bg, "bg")] {
            match colour {
                Some(Colour::Standard(index)) => classes.push(format!("sgr-{kind}-{index}")),
                Some(Colour::Indexed(index)) => {
                    let (red, green, blue) = indexed_rgb(index);
                    styles.push(css_colour(kind, red, green, blue));
                }
                Some(Colour::Rgb(red, green, blue)) => {
                    styles.push(css_colour(kind, red, green, blue))
                }
                None => {}
            }
        }
        for (on, class) in [
            (self.bold, "font-bold"),
            (self.dim, "opacity-70"),
            (self.italic, "italic"),
            (self.underline, "underline"),
            (self.strike, "line-through"),
        ] {
            if on {
                classes.push(class.to_owned());
            }
        }
        let mut attributes = String::new();
        if !classes.is_empty() {
            attributes.push_str(&format!(" class=\"{}\"", classes.join(" ")));
        }
        if !styles.is_empty() {
            attributes.push_str(&format!(" style=\"{}\"", styles.join(";")));
        }
        attributes
    }
}

fn css_colour(kind: &str, red: u8, green: u8, blue: u8) -> String {
    let property = if kind == "fg" {
        "color"
    } else {
        "background-color"
    };
    format!("{property}:#{red:02x}{green:02x}{blue:02x}")
}

/// The xterm 256-colour palette entry `index` (16–255) as RGB.
fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let level = 8 + (index - 232) * 10;
        return (level, level, level);
    }
    let cube = index - 16;
    let level = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
    (level(cube / 36), level((cube / 6) % 6), level(cube % 6))
}

/// Appends `text` with its SGR sequences turned into spans, continuing from
/// and updating `rendition`. Other escape sequences are dropped.
pub fn push_sgr_html(out: &mut String, text: &str, rendition: &mut Rendition) {
    let mut rest = text;
    let mut run = String::new();
    let flush = |out: &mut String, run: &mut String, rendition: &Rendition| {
        if run.is_empty() {
            return;
        }
        if rendition.is_plain() {
            push_escaped(out, run);
        } else {
            out.push_str(&format!("<span{}>", rendition.attributes()));
            push_escaped(out, run);
            out.push_str("</span>");
        }
        run.clear();
    };
    while let Some(start) = rest.find('\u{1b}') {
        run.push_str(&rest[..start]);
        let sequence = &rest[start + 1..];
        let Some(parameters) = sequence.strip_prefix('[') else {
            rest = sequence;
            continue;
        };
        let end = parameters
            .find(|character: char| !(character.is_ascii_digit() || matches!(character, ';' | ':')))
            .unwrap_or(parameters.len());
        let final_byte = parameters[end..].chars().next();
        if final_byte == Some('m') {
            flush(out, &mut run, rendition);
            rendition.apply(&parameters[..end]);
        }
        rest = &parameters[(end + final_byte.map_or(0, char::len_utf8)).min(parameters.len())..];
    }
    run.push_str(rest);
    flush(out, &mut run, rendition);
}

#[cfg(test)]
mod tests {
    use super::{Colour, Rendition, push_sgr_html};

    fn html(text: &str) -> String {
        let mut out = String::new();
        push_sgr_html(&mut out, text, &mut Rendition::default());
        out
    }

    #[test]
    fn sgr_sequences_become_spans_and_text_is_escaped() {
        assert_eq!(html("plain <b>"), "plain &lt;b&gt;");
        assert_eq!(
            html("\u{1b}[1;31merror\u{1b}[0m done"),
            "<span class=\"sgr-fg-1 font-bold\">error</span> done"
        );
        assert_eq!(
            html("\u{1b}[92mok\u{1b}[39m"),
            "<span class=\"sgr-fg-10\">ok</span>"
        );
        assert_eq!(
            html("\u{1b}[38;5;208morange\u{1b}[m"),
            "<span style=\"color:#ff8700\">orange</span>"
        );
        assert_eq!(
            html("\u{1b}[48;2;1;2;3mbg"),
            "<span style=\"background-color:#010203\">bg</span>"
        );
        // Other sequences are dropped, not shown.
        assert_eq!(html("a\u{1b}[2Kb"), "ab");
    }

    #[test]
    fn the_rendition_carries_across_calls() {
        let mut rendition = Rendition::default();
        let mut out = String::new();
        push_sgr_html(&mut out, "\u{1b}[4mone", &mut rendition);
        push_sgr_html(&mut out, "two", &mut rendition);
        assert!(rendition.underline);
        assert_eq!(
            out,
            "<span class=\"underline\">one</span><span class=\"underline\">two</span>"
        );
        rendition.apply("7;34;43");
        assert_eq!(rendition.fg, Some(Colour::Standard(4)));
        let mut inverse = String::new();
        push_sgr_html(&mut inverse, "x", &mut rendition);
        assert!(inverse.contains("sgr-fg-3 sgr-bg-4"), "{inverse}");
    }
}
