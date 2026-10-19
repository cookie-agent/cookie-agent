//! The memory file format (`docs/site/specs/memory.md` section 4.3) and
//! revisions (section 4.4).
//!
//! A memory file is YAML frontmatter between `---` lines, with a required
//! `description` and optional `keywords`, followed by a Markdown body that
//! may be empty. Parsing is strict: an unknown key, a wrong type, a
//! frontmatter that does not parse, or a file over `[memory]
//! max_file_bytes` makes the file invalid, with a reason. Control
//! characters other than newline and tab make it invalid too (section 13);
//! `\r\n` line endings are read as `\n`.

use std::fmt;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// Longest description, in characters.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 200;
/// Most keywords one memory may declare.
pub(crate) const MAX_KEYWORDS: usize = 32;
/// Longest keyword, in characters.
pub(crate) const MAX_KEYWORD_CHARS: usize = 64;

const FENCE: &str = "---";

/// A memory file's content: its frontmatter and its body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryDocument {
    description: String,
    keywords: Vec<String>,
    body: String,
}

/// The frontmatter keys a memory file may hold.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    description: String,
    #[serde(default)]
    keywords: Vec<String>,
}

/// Why a memory file or a memory to write is not valid.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct InvalidMemory(pub(crate) String);

fn invalid(reason: impl Into<String>) -> InvalidMemory {
    InvalidMemory(reason.into())
}

/// The first control character other than newline and tab in `text`.
fn control_character(text: &str) -> Option<char> {
    text.chars()
        .find(|character| character.is_control() && !matches!(character, '\n' | '\t'))
}

fn check_text(what: &str, text: &str) -> Result<(), InvalidMemory> {
    match control_character(text) {
        Some(character) => Err(invalid(format!(
            "{what} contains the control character U+{:04X}",
            u32::from(character)
        ))),
        None => Ok(()),
    }
}

impl MemoryDocument {
    /// A memory from its parts, checked against the rules of section 4.3.
    pub(crate) fn new(
        description: impl Into<String>,
        keywords: Vec<String>,
        body: impl Into<String>,
    ) -> Result<Self, InvalidMemory> {
        let document = Self {
            description: description.into(),
            keywords,
            body: body.into(),
        };
        document.check()?;
        Ok(document)
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    pub(crate) fn keywords(&self) -> &[String] {
        &self.keywords
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }

    /// This memory with `body` in place of its body, checked again.
    pub(crate) fn with_body(&self, body: String) -> Result<Self, InvalidMemory> {
        Self::new(self.description.clone(), self.keywords.clone(), body)
    }

    fn check(&self) -> Result<(), InvalidMemory> {
        let characters = self.description.chars().count();
        if !(1..=MAX_DESCRIPTION_CHARS).contains(&characters) {
            return Err(invalid(format!(
                "description must be 1 to {MAX_DESCRIPTION_CHARS} characters, not {characters}"
            )));
        }
        if self.description.contains('\n') {
            return Err(invalid("description must be one line"));
        }
        check_text("description", &self.description)?;
        if self.keywords.len() > MAX_KEYWORDS {
            return Err(invalid(format!(
                "keywords must have at most {MAX_KEYWORDS} entries, not {}",
                self.keywords.len()
            )));
        }
        for keyword in &self.keywords {
            let characters = keyword.chars().count();
            if !(1..=MAX_KEYWORD_CHARS).contains(&characters) {
                return Err(invalid(format!(
                    "keyword `{keyword}` must be 1 to {MAX_KEYWORD_CHARS} characters, not {characters}"
                )));
            }
            if keyword.contains('\n') {
                return Err(invalid("keywords must not contain line breaks"));
            }
            check_text("a keyword", keyword)?;
        }
        check_text("the body", &self.body)
    }

    /// Parses a memory file's bytes. `max_file_bytes` is `[memory]
    /// max_file_bytes`.
    pub(crate) fn parse(bytes: &[u8], max_file_bytes: usize) -> Result<Self, InvalidMemory> {
        if bytes.len() > max_file_bytes {
            return Err(too_large(bytes.len(), max_file_bytes));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| invalid("the file is not UTF-8"))?;
        let text = text.replace("\r\n", "\n");
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| invalid("the file must start with a `---` line"))?;
        let (yaml, body) = if let Some(body) = rest.strip_prefix("---\n") {
            ("", body)
        } else if rest == FENCE {
            ("", "")
        } else if let Some(end) = rest.find("\n---\n") {
            (&rest[..=end], &rest[end + "\n---\n".len()..])
        } else if let Some(yaml) = rest.strip_suffix("\n---") {
            (yaml, "")
        } else {
            return Err(invalid("the frontmatter must end with a `---` line"));
        };
        check_text("the file", &text)?;
        let frontmatter: Frontmatter =
            serde_yaml::from_str(yaml).map_err(|error| invalid(format!("frontmatter: {error}")))?;
        Self::new(frontmatter.description, frontmatter.keywords, body)
    }

    /// The file text of this memory: `description`, `keywords` when there
    /// are any, and the body as it is.
    pub(crate) fn render(&self) -> String {
        let mut text = String::from("---\ndescription: ");
        text.push_str(&yaml_scalar(&self.description, false));
        text.push('\n');
        if !self.keywords.is_empty() {
            text.push_str("keywords: [");
            for (index, keyword) in self.keywords.iter().enumerate() {
                if index > 0 {
                    text.push_str(", ");
                }
                text.push_str(&yaml_scalar(keyword, true));
            }
            text.push_str("]\n");
        }
        text.push_str("---\n");
        text.push_str(&self.body);
        text
    }
}

pub(crate) fn too_large(bytes: usize, max_file_bytes: usize) -> InvalidMemory {
    invalid(format!(
        "the file is {bytes} bytes, more than [memory] max_file_bytes ({max_file_bytes})"
    ))
}

/// `value` as a YAML scalar: plain when it reads back as the same string,
/// double-quoted otherwise. `in_flow` is for an entry of a `[...]` list.
fn yaml_scalar(value: &str, in_flow: bool) -> String {
    let plain_reads_back = || {
        let probe = if in_flow {
            serde_yaml::from_str::<Vec<String>>(&format!("[{value}]"))
                .is_ok_and(|values| values.len() == 1 && values[0] == value)
        } else {
            serde_yaml::from_str::<String>(value).is_ok_and(|read| read == value)
        };
        probe && !value.contains('#') && !value.starts_with(['\'', '"'])
    };
    if !value.is_empty() && plain_reads_back() {
        value.to_owned()
    } else {
        // A JSON string is a valid YAML double-quoted scalar.
        serde_json::to_string(value).expect("strings serialize to JSON")
    }
}

/// A memory file's revision: the first 16 hex digits of the SHA-256 of its
/// bytes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct MemoryRevision(String);

impl MemoryRevision {
    const DIGITS: usize = 16;

    pub(crate) fn of(bytes: &[u8]) -> Self {
        Self::from_digest(Sha256::digest(bytes).as_slice())
    }

    pub(crate) fn from_digest(digest: &[u8]) -> Self {
        let mut text = String::with_capacity(Self::DIGITS);
        for byte in &digest[..Self::DIGITS / 2] {
            text.push_str(&format!("{byte:02x}"));
        }
        Self(text)
    }

    /// A revision a caller sent: 16 lowercase hex digits.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        (value.len() == Self::DIGITS
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then(|| Self(value.to_owned()))
    }

    /// The revision as text, as `memory.*` sends it.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MemoryRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 4096;

    fn parse(text: &str) -> Result<MemoryDocument, InvalidMemory> {
        MemoryDocument::parse(text.as_bytes(), LIMIT)
    }

    fn reason(text: &str) -> String {
        parse(text).expect_err(text).0
    }

    #[test]
    fn a_file_parses_into_description_keywords_and_body() {
        let document = parse(
            "---\ndescription: Build and test conventions for cookie-agent\nkeywords: [cargo, clippy, msrv, no-fail-fast]\n---\n- Run `cargo test --no-fail-fast`.\n",
        )
        .unwrap();
        assert_eq!(
            document.description(),
            "Build and test conventions for cookie-agent"
        );
        assert_eq!(
            document.keywords(),
            ["cargo", "clippy", "msrv", "no-fail-fast"]
        );
        assert_eq!(document.body(), "- Run `cargo test --no-fail-fast`.\n");
    }

    #[test]
    fn keywords_are_optional_and_the_body_may_be_empty() {
        for text in [
            "---\ndescription: x\n---\n",
            "---\ndescription: x\n---",
            "---\r\ndescription: x\r\n---\r\n",
        ] {
            let document = parse(text).unwrap_or_else(|error| panic!("{text:?}: {error}"));
            assert_eq!(document.description(), "x");
            assert!(document.keywords().is_empty());
            assert_eq!(document.body(), "");
        }
        let document = parse("---\ndescription: x\nkeywords:\n  - a\n  - b c\n---\nbody").unwrap();
        assert_eq!(document.keywords(), ["a", "b c"]);
        assert_eq!(document.body(), "body");
    }

    #[test]
    fn invalid_files_say_why() {
        for (text, expected) in [
            ("description: x\n", "must start with a `---` line"),
            ("---\ndescription: x\n", "must end with a `---` line"),
            ("---\n---\n", "frontmatter"),
            ("---\nkeywords: [a]\n---\n", "missing field `description`"),
            (
                "---\ndescription: x\ntags: [a]\n---\n",
                "unknown field `tags`",
            ),
            ("---\ndescription: [x]\n---\n", "invalid type"),
            ("---\ndescription: x\nkeywords: a\n---\n", "invalid type"),
            (
                "---\ndescription: x\nkeywords: [[a]]\n---\n",
                "invalid type",
            ),
            ("---\ndescription: x\ndescription: y\n---\n", "duplicate"),
            ("---\ndescription: [x\n---\n", "frontmatter"),
            (
                "---\ndescription: \"\"\n---\n",
                "1 to 200 characters, not 0",
            ),
            ("---\ndescription: \"a\\nb\"\n---\n", "one line"),
            (
                "---\ndescription: x\nkeywords: [\"\"]\n---\n",
                "1 to 64 characters",
            ),
            (
                "---\ndescription: x\nkeywords: [\"a\\nb\"]\n---\n",
                "line breaks",
            ),
            ("---\ndescription: x\n---\nbell \u{7}", "U+0007"),
            ("---\ndescription: x\n---\nlone \r return", "U+000D"),
        ] {
            let found = reason(text);
            assert!(found.contains(expected), "{text:?}: {found}");
        }
        let found = MemoryDocument::parse(b"---\ndescription: \xff\n---\n", LIMIT)
            .unwrap_err()
            .0;
        assert!(found.contains("UTF-8"), "{found}");
    }

    #[test]
    fn description_and_keyword_limits_are_inclusive() {
        let at = |description: &str, keywords: Vec<String>| {
            MemoryDocument::new(description, keywords, "")
        };
        assert!(at(&"é".repeat(200), vec![]).is_ok());
        assert!(at(&"é".repeat(201), vec![]).is_err());
        assert!(at("x", vec!["k".repeat(64)]).is_ok());
        assert!(at("x", vec!["ü".repeat(64)]).is_ok());
        assert!(at("x", vec!["k".repeat(65)]).is_err());
        assert!(at("x", vec!["k".to_owned(); 32]).is_ok());
        assert!(at("x", vec!["k".to_owned(); 33]).is_err());
        assert!(MemoryDocument::new("x", vec![], "tab\tand\nnewline").is_ok());
        assert!(MemoryDocument::new("x", vec![], "carriage\r\nreturn").is_err());
        assert!(MemoryDocument::new("x\u{1b}[31m", vec![], "").is_err());
        assert!(MemoryDocument::new("x", vec!["a\u{85}".to_owned()], "").is_err());
    }

    #[test]
    fn the_size_limit_counts_the_whole_file() {
        let head = "---\ndescription: x\n---\n";
        let fits = format!("{head}{}", "b".repeat(LIMIT - head.len()));
        assert!(parse(&fits).is_ok());
        let over = format!("{fits}b");
        assert!(reason(&over).contains("more than [memory] max_file_bytes (4096)"));
    }

    #[test]
    fn rendering_reads_back_as_the_same_memory() {
        for (description, keywords) in [
            ("Build and test conventions", vec!["cargo", "no-fail-fast"]),
            (
                "Who the user is: role, expertise",
                vec!["a, b", "[x]", "{y}", "c: d", "# not a comment", "'q'"],
            ),
            (
                "123",
                vec!["true", "null", "~", "1.5", "-", "- x", "*a", "&b", "!t"],
            ),
            (
                "  padded  ",
                vec![" lead", "trail ", "\"quoted\"", "back\\slash"],
            ),
            (
                "- looks like a list",
                vec!["?", ":", "%x", "@y", "`z`", "a #b"],
            ),
            ("ends with a colon:", vec!["tab\tinside", "ünïcödé"]),
        ] {
            let keywords = keywords.into_iter().map(str::to_owned).collect();
            let document = MemoryDocument::new(description, keywords, "line\n\n---\nafter\n")
                .unwrap_or_else(|error| panic!("{description}: {error}"));
            let text = document.render();
            assert_eq!(
                parse(&text).unwrap_or_else(|error| panic!("{text}: {error}")),
                document,
                "{text}"
            );
        }
        let plain = MemoryDocument::new("Build notes", vec!["cargo".to_owned()], "")
            .unwrap()
            .render();
        assert_eq!(
            plain,
            "---\ndescription: Build notes\nkeywords: [cargo]\n---\n"
        );
        let bare = MemoryDocument::new("x", vec![], "body").unwrap().render();
        assert_eq!(bare, "---\ndescription: x\n---\nbody");
    }

    #[test]
    fn revisions_are_sixteen_hex_digits_of_sha256() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb924…
        assert_eq!(MemoryRevision::of(b"").as_str(), "e3b0c44298fc1c14");
        assert_ne!(MemoryRevision::of(b"a"), MemoryRevision::of(b"b"));
        assert!(MemoryRevision::parse("e3b0c44298fc1c14").is_some());
        assert!(MemoryRevision::parse("E3B0C44298FC1C14").is_none());
        assert!(MemoryRevision::parse("e3b0c44298fc1c1").is_none());
        assert!(MemoryRevision::parse("e3b0c44298fc1c14a").is_none());
        assert!(MemoryRevision::parse("g3b0c44298fc1c14").is_none());
    }
}
