//! An agent document read whole, for showing: the document after every
//! `inherit:` and `{{default_prompt}}`, with the documents' comments, where
//! each frontmatter key came from, and where the inherited prompt is
//! (`docs/site/specs/agent-shortcuts.md` section 4).
//!
//! The frontmatter is put together from the text of each document of the
//! chain, a block at a time: a top-level key with its lines, or one action
//! under `permissions`, each with the comments above it. The blocks are
//! picked by the loader's rules ([`crate::agent_inherit`]), and the result
//! is checked against the loader's own merge: when the two disagree, or a
//! document does not read as blocks, the loader's merge is shown without
//! comments. What an agent runs is always the loader's.

use crate::agent_inherit::{self, Resolved};
use crate::{ConfigError, ConfigTree, agent_document};

/// An agent document after every `inherit`, for reading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedAgentDocument {
    /// The frontmatter and body.
    pub text: String,
    /// Whether `text` keeps the documents' comments.
    pub comments: bool,
    /// The tree paths of the documents merged, the one asked for first.
    pub chain: Vec<String>,
    pub keys: Vec<ResolvedKey>,
    pub prompt: Option<ResolvedPrompt>,
}

/// Where a frontmatter key of a resolved document came from: a top-level
/// key, or a permission action as `permissions.<action>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedKey {
    pub key: String,
    /// The tree path of the document of the chain it came from.
    pub source: String,
    /// Its first and last line in the text, 1-based.
    pub first_line: u32,
    pub last_line: u32,
    /// The inherited block it replaced, as written.
    pub replaced: Option<String>,
}

/// The inherited prompt in a resolved document's body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPrompt {
    /// The tree path of the document whose prompt it is.
    pub source: String,
    /// Its first and last line in the text, 1-based.
    pub first_line: u32,
    pub last_line: u32,
}

/// One document of the chain.
struct Link {
    path: String,
    yaml: String,
    body: String,
    mapping: serde_yaml::Mapping,
}

/// A block of frontmatter: its key, and its lines, the comments and blank
/// lines above it (`lead` of them) first.
#[derive(Clone)]
struct Block {
    key: String,
    lines: Vec<String>,
    lead: usize,
}

impl Block {
    /// Its lines without the comments above it.
    fn content(&self) -> String {
        self.lines[self.lead..].join("\n")
    }
}

/// A top-level entry of the frontmatter.
enum Top {
    Plain(Block),
    /// `permissions:` (with the comments above it), and its actions.
    Permissions {
        header: Block,
        actions: Vec<Block>,
    },
}

/// A block placed in the resolved frontmatter.
struct Placed {
    block: Block,
    /// The index in the chain of the document it came from.
    source: usize,
    replaced: Option<String>,
}

enum Entry {
    Plain(Placed),
    Permissions {
        header: Placed,
        actions: Vec<Placed>,
    },
}

/// The document at tree path `path` (an agent document or template of the
/// tree, an official one among them) read whole. Official agents are read
/// only when `officials` is set, as the loader reads them.
///
/// # Errors
///
/// An unknown document, a document that does not parse, and an `inherit`
/// the loader refuses or cannot follow (yet).
pub fn resolve_for_reading(
    tree: &ConfigTree,
    path: &str,
    officials: bool,
) -> Result<ResolvedAgentDocument, ConfigError> {
    let shown = agent_inherit::tree_path(tree, path);
    let bytes = agent_inherit::lookup(tree, path, officials).ok_or_else(|| {
        agent_document::invalid_document(&shown, "there is no such agent document")
    })?;
    // The loader's merge: what the agent runs.
    let merged = match agent_inherit::resolve(tree, path, &bytes, officials)? {
        Resolved::AsWritten => None,
        Resolved::Merged(merged) => Some(merged),
        Resolved::Missing(target) => {
            return Err(agent_document::invalid_document(
                &shown,
                format!("it inherits `{target}`, which there is no document for (yet)"),
            ));
        }
    };
    let (truth_yaml, truth_body) =
        agent_document::split_document(merged.as_deref().unwrap_or(&bytes), &shown)?;
    let mut truth = agent_inherit::mapping_of(&truth_yaml, &shown)?;
    truth.remove(agent_inherit::INHERIT_KEY);
    let chain = links(tree, path, &bytes, officials)?;
    let (body, prompt) = match body_of(tree, &chain, officials)? {
        // An inherited body is the loader's; one written as is may still
        // take the prompt of the document it replaces.
        (body, prompt) if merged.is_none() || body == truth_body => (body, prompt),
        _ => (truth_body, None),
    };
    let blocks = chain
        .iter()
        .map(|link| blocks(&link.yaml))
        .collect::<Option<Vec<_>>>();
    let with_comments = blocks.map(|blocks| pick(&blocks)).and_then(|entries| {
        let (lines, keys) = render(&entries, &chain);
        let yaml = lines.join("\n");
        let mapping = agent_inherit::mapping_of(&yaml, &shown).ok()?;
        (mapping == truth).then_some((lines, keys))
    });
    let (lines, keys, comments) = match with_comments {
        Some((lines, keys)) => (lines, keys, true),
        None => {
            let (lines, keys) = without_comments(&truth, &chain);
            (lines, keys, false)
        }
    };
    let front = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let mut text = String::from("---\n");
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str("---\n");
    text.push_str(&body);
    // The body starts after the frontmatter and its two fences.
    let prompt = prompt.map(|prompt| ResolvedPrompt {
        first_line: prompt.first_line + front + 2,
        last_line: prompt.last_line + front + 2,
        ..prompt
    });
    Ok(ResolvedAgentDocument {
        text,
        comments,
        chain: chain.into_iter().map(|link| link.path).collect(),
        keys,
        prompt,
    })
}

/// The documents from the one at `path` to the one that inherits nothing.
fn links(
    tree: &ConfigTree,
    path: &str,
    bytes: &[u8],
    officials: bool,
) -> Result<Vec<Link>, ConfigError> {
    let mut chain: Vec<Link> = Vec::new();
    let (mut path, mut bytes) = (path.to_owned(), bytes.to_vec());
    loop {
        let shown = agent_inherit::tree_path(tree, &path);
        let (yaml, body) = agent_document::split_document(&bytes, &shown)?;
        let mapping = agent_inherit::mapping_of(&yaml, &shown)?;
        let inherit = mapping
            .get(agent_inherit::INHERIT_KEY)
            .and_then(serde_yaml::Value::as_str)
            .map(str::to_owned);
        chain.push(Link {
            path: path.clone(),
            yaml,
            body,
            mapping,
        });
        let Some(inherit) = inherit else {
            return Ok(chain);
        };
        let refused = |message: &str| agent_document::invalid_document(&shown, message);
        let target = agent_inherit::target_path(&path, &inherit)
            .ok_or_else(|| refused("`inherit` names no agent document"))?;
        if chain.iter().any(|link| link.path == target) || chain.len() >= agent_inherit::MAX_CHAIN {
            return Err(refused("`inherit` makes a cycle or a chain too long"));
        }
        bytes = agent_inherit::lookup(tree, &target, officials)
            .ok_or_else(|| refused("`inherit` names a document there is none of (yet)"))?;
        path = target;
    }
}

/// The body the loader gives the first document of `chain`, and where the
/// inherited prompt is in it (lines of the body, 1-based).
fn body_of(
    tree: &ConfigTree,
    chain: &[Link],
    officials: bool,
) -> Result<(String, Option<ResolvedPrompt>), ConfigError> {
    let mut links = chain.iter().rev();
    let root = links.next().expect("a chain has a document");
    let mut body = root.body.clone();
    let mut prompt = None;
    let official = root
        .path
        .starts_with(&format!("{}/", crate::OFFICIAL_AGENTS_DIRECTORY));
    if !official {
        let mut walked = chain.iter().map(|link| link.path.clone()).collect();
        if let Some((source, inherited)) =
            agent_inherit::replaced_prompt(tree, &root.path, officials, &mut walked)?
        {
            (body, prompt) = expand(&root.body, &inherited, source);
        }
    }
    let mut base = root;
    for link in links {
        if link.body.trim().is_empty() {
            // Its body is the inherited one, whole.
            if prompt.is_none() {
                prompt = Some(ResolvedPrompt {
                    source: base.path.clone(),
                    first_line: 1,
                    last_line: line_count(&body),
                });
            }
        } else {
            (body, prompt) = expand(&link.body, body.trim_end_matches('\n'), base.path.clone());
        }
        base = link;
    }
    Ok((body, prompt))
}

/// `template` with `{{default_prompt}}` replaced by `inherited`, and where
/// the first replacement is, by line.
fn expand(template: &str, inherited: &str, source: String) -> (String, Option<ResolvedPrompt>) {
    const MARK: &str = "\u{0}default_prompt\u{0}";
    let body = crate::prompt_template::replace_default_prompt(template, inherited);
    let marked = crate::prompt_template::replace_default_prompt(template, MARK);
    let prompt = marked.find(MARK).map(|at| {
        let first_line = line_count(&marked[..at]).max(1);
        ResolvedPrompt {
            source,
            first_line,
            last_line: first_line + line_count(inherited).max(1) - 1,
        }
    });
    (body, prompt)
}

/// The lines `text` starts or holds: its newlines plus one for a last line
/// without one; 0 for none.
fn line_count(text: &str) -> u32 {
    let newlines = text.matches('\n').count();
    let partial = usize::from(!text.is_empty() && !text.ends_with('\n'));
    u32::try_from(newlines + partial).unwrap_or(u32::MAX)
}

/// A key at the start of `line`: plain, or in quotes; `None` when the
/// line holds no `key:`.
fn key_of(line: &str) -> Option<(String, &str)> {
    let line = line.trim_start();
    let (key, rest) = match line.chars().next()? {
        quote @ ('"' | '\'') => {
            let end = line[1..].find(quote)? + 1;
            (line[1..end].to_owned(), &line[end + 1..])
        }
        _ => {
            let end = line.find(':')?;
            (line[..end].trim_end().to_owned(), &line[end..])
        }
    };
    let rest = rest.trim_start().strip_prefix(':')?;
    // `key:value` is a plain scalar, not a key.
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    Some((key, rest.trim()))
}

/// Whether `value` (after a key's colon) is empty but for a comment: a
/// block follows on the next lines.
fn opens_block(value: &str) -> bool {
    value.is_empty() || value.starts_with('#')
}

/// `yaml` as blocks; `None` when it does not read as them (flow-style
/// `permissions`, content before the first key, an odd indentation).
fn blocks(yaml: &str) -> Option<Vec<Top>> {
    let mut tops: Vec<Top> = Vec::new();
    // Comments and blank lines, kept for the block that follows them.
    let mut pending: Vec<String> = Vec::new();
    // The indentation of `permissions`' actions, once one is read.
    let mut action_indent: Option<usize> = None;
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            pending.push(line.to_owned());
            continue;
        }
        if indent == 0 && !trimmed.starts_with('-') {
            let (key, value) = key_of(line)?;
            let lead = pending.len();
            let mut lines = std::mem::take(&mut pending);
            lines.push(line.to_owned());
            let block = Block { key, lines, lead };
            if block.key == "permissions" {
                if !opens_block(value) {
                    return None;
                }
                action_indent = None;
                tops.push(Top::Permissions {
                    header: block,
                    actions: Vec::new(),
                });
            } else {
                tops.push(Top::Plain(block));
            }
            continue;
        }
        match tops.last_mut()? {
            Top::Plain(block) => {
                block.lines.append(&mut pending);
                block.lines.push(line.to_owned());
            }
            Top::Permissions { actions, .. } => {
                if indent == 0 {
                    return None;
                }
                let starts = *action_indent.get_or_insert(indent);
                if indent < starts {
                    return None;
                }
                if indent == starts && !trimmed.starts_with('-') {
                    let (key, _) = key_of(line)?;
                    let lead = pending.len();
                    let mut lines = std::mem::take(&mut pending);
                    lines.push(line.to_owned());
                    actions.push(Block { key, lines, lead });
                } else {
                    let action = actions.last_mut()?;
                    action.lines.append(&mut pending);
                    action.lines.push(line.to_owned());
                }
            }
        }
    }
    // Comments after the last key stay with it.
    if !pending.is_empty() {
        match tops.last_mut() {
            Some(Top::Plain(block)) => block.lines.append(&mut pending),
            Some(Top::Permissions { header, actions }) => match actions.last_mut() {
                Some(action) => action.lines.append(&mut pending),
                None => header.lines.append(&mut pending),
            },
            None => {}
        }
    }
    Some(tops)
}

/// The permission action a key names: `bash@labserver` is `bash`.
fn action(key: &str) -> &str {
    key.split('@').next().unwrap_or(key)
}

/// The blocks the loader's rules keep, from `blocks` (one list per document
/// of the chain, the asked-for document first): each document's keys over
/// the ones it inherits, `permissions` per action, `inherit` left out.
fn pick(blocks: &[Vec<Top>]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for (source, tops) in blocks.iter().enumerate().rev() {
        for top in tops {
            match top {
                Top::Plain(block) if block.key == agent_inherit::INHERIT_KEY => {}
                Top::Plain(block) => {
                    let placed = |replaced| Placed {
                        block: block.clone(),
                        source,
                        replaced,
                    };
                    let at = entries.iter().position(
                        |entry| matches!(entry, Entry::Plain(old) if old.block.key == block.key),
                    );
                    match at {
                        Some(at) => {
                            let Entry::Plain(old) = &entries[at] else {
                                unreachable!()
                            };
                            let replaced = old.block.content();
                            entries[at] = Entry::Plain(placed(Some(replaced)));
                        }
                        None => entries.push(Entry::Plain(placed(None))),
                    }
                }
                Top::Permissions { header, actions } => {
                    let at = entries
                        .iter()
                        .position(|entry| matches!(entry, Entry::Permissions { .. }));
                    let at = at.unwrap_or_else(|| {
                        entries.push(Entry::Permissions {
                            header: Placed {
                                block: header.clone(),
                                source,
                                replaced: None,
                            },
                            actions: Vec::new(),
                        });
                        entries.len() - 1
                    });
                    let Entry::Permissions {
                        actions: placed, ..
                    } = &mut entries[at]
                    else {
                        unreachable!()
                    };
                    // As the loader does: each action removes every key of
                    // its action kept so far, then is added.
                    for block in actions {
                        let name = action(&block.key);
                        let mut replaced = Vec::new();
                        let mut first = None;
                        let mut index = 0;
                        placed.retain(|old| {
                            let keep = action(&old.block.key) != name;
                            if !keep {
                                first.get_or_insert(index);
                                replaced.push(old.block.content());
                            } else {
                                index += 1;
                            }
                            keep
                        });
                        let new = Placed {
                            block: block.clone(),
                            source,
                            replaced: (!replaced.is_empty()).then(|| replaced.join("\n")),
                        };
                        match first {
                            Some(at) => placed.insert(at, new),
                            None => placed.push(new),
                        }
                    }
                }
            }
        }
    }
    entries
}

/// The frontmatter's lines, and its keys with their lines in the text
/// (which starts with the `---` fence).
fn render(entries: &[Entry], chain: &[Link]) -> (Vec<String>, Vec<ResolvedKey>) {
    let mut lines = Vec::new();
    let mut keys = Vec::new();
    let mut place = |lines: &mut Vec<String>, placed: &Placed, key: String| {
        let start = lines.len();
        lines.extend(placed.block.lines.iter().cloned());
        let text_line = |index: usize| u32::try_from(index + 2).unwrap_or(u32::MAX);
        keys.push(ResolvedKey {
            key,
            source: chain[placed.source].path.clone(),
            first_line: text_line(start + placed.block.lead),
            last_line: text_line(lines.len() - 1),
            replaced: placed.replaced.clone(),
        });
    };
    for entry in entries {
        match entry {
            Entry::Plain(placed) => place(&mut lines, placed, placed.block.key.clone()),
            Entry::Permissions { header, actions } => {
                lines.extend(header.block.lines.iter().cloned());
                for placed in actions {
                    place(
                        &mut lines,
                        placed,
                        format!("permissions.{}", placed.block.key),
                    );
                }
            }
        }
    }
    (lines, keys)
}

/// The loader's frontmatter written out, with no comments, and its keys,
/// each from the nearest document of the chain that writes it.
fn without_comments(
    truth: &serde_yaml::Mapping,
    chain: &[Link],
) -> (Vec<String>, Vec<ResolvedKey>) {
    let yaml = if truth.is_empty() {
        String::new()
    } else {
        serde_yaml::to_string(truth).unwrap_or_default()
    };
    let lines = yaml.lines().map(str::to_owned).collect::<Vec<_>>();
    let writes = |key: &str| {
        chain
            .iter()
            .position(|link| match key.strip_prefix("permissions.") {
                Some(action) => link
                    .mapping
                    .get("permissions")
                    .and_then(serde_yaml::Value::as_mapping)
                    .is_some_and(|permissions| permissions.contains_key(action)),
                None => link.mapping.contains_key(key),
            })
    };
    let entries = blocks(&yaml)
        .unwrap_or_default()
        .into_iter()
        .map(|top| match top {
            Top::Plain(block) => Entry::Plain(Placed {
                block,
                source: 0,
                replaced: None,
            }),
            Top::Permissions { header, actions } => Entry::Permissions {
                header: Placed {
                    block: header,
                    source: 0,
                    replaced: None,
                },
                actions: actions
                    .into_iter()
                    .map(|block| Placed {
                        block,
                        source: 0,
                        replaced: None,
                    })
                    .collect(),
            },
        })
        .collect::<Vec<_>>();
    let (_, mut keys) = render(&entries, chain);
    for key in &mut keys {
        if let Some(source) = writes(&key.key) {
            key.source.clone_from(&chain[source].path);
        }
    }
    (lines, keys)
}
