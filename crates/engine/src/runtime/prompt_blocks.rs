/// Boundaries between composed system-prompt blocks are rendered as a markdown
/// horizontal rule so block transitions are visible in the assembled prompt.
pub(crate) const PROMPT_BLOCK_SEPARATOR: &str = "\n\n---\n\n";

pub(crate) fn push_prompt_block(prompt: &mut String, block: &str) {
    if !prompt.is_empty() {
        prompt.push_str(PROMPT_BLOCK_SEPARATOR);
    }
    prompt.push_str(block);
}

#[cfg(test)]
mod tests {
    use super::{PROMPT_BLOCK_SEPARATOR, push_prompt_block};

    #[test]
    fn separator_is_a_markdown_horizontal_rule() {
        assert_eq!(PROMPT_BLOCK_SEPARATOR, "\n\n---\n\n");
    }

    #[test]
    fn empty_prompt_gets_no_leading_separator() {
        let mut prompt = String::new();
        push_prompt_block(&mut prompt, "first");
        assert_eq!(prompt, "first");
    }

    #[test]
    fn blocks_are_joined_by_exactly_one_separator() {
        let mut prompt = String::new();
        push_prompt_block(&mut prompt, "first");
        push_prompt_block(&mut prompt, "second");
        assert_eq!(prompt, "first\n\n---\n\nsecond");
        assert_eq!(prompt.matches(PROMPT_BLOCK_SEPARATOR).count(), 1);
    }
}
