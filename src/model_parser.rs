use regex::Regex;

/// Strip thought/reasoning blocks from model output text.
///
/// Returns `(thoughts, cleaned_text)` where `thoughts` is a vec of extracted
/// reasoning strings and `cleaned_text` has all thought markers removed.
pub fn extract_thought_blocks(text: &str) -> (Vec<String>, String) {
    let mut thoughts = Vec::new();

    let channel_re = Regex::new(r"(?s)<\|channel>([^\n]*)\n(.*?)<channel\|>").unwrap();
    let without_channels = channel_re
        .replace_all(text, |caps: &regex::Captures<'_>| {
            let header = caps.get(1).map(|m| m.as_str().trim()).unwrap_or_default();
            let body = caps.get(2).map(|m| m.as_str().trim()).unwrap_or_default();
            if !body.is_empty() {
                thoughts.push(body.to_owned());
            } else if !header.is_empty() {
                thoughts.push(header.to_owned());
            }
            ""
        })
        .into_owned();

    let thinking_re = Regex::new(
        r"(?s)<thinking>(.*?)</thinking>|<think>(.*?)</think>|^(.*?)</think>|^(.*?)</thinking>",
    )
    .unwrap();
    let without_thinking = thinking_re
        .replace_all(&without_channels, |caps: &regex::Captures<'_>| {
            let body = caps
                .get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .or_else(|| caps.get(4))
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            if !body.is_empty() {
                thoughts.push(body.to_owned());
            }
            ""
        })
        .into_owned();

    let orphan_thinking_re =
        Regex::new(r"(?s)<thinking>(.*)$|<think>(.*)$|<\|channel>thought\n(.*)$").unwrap();
    let without_orphan_thinking = orphan_thinking_re
        .replace_all(&without_thinking, |caps: &regex::Captures<'_>| {
            let body = caps
                .get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            if !body.is_empty() {
                thoughts.push(body.to_owned());
            }
            ""
        })
        .into_owned();

    // Drop any leftover orphan tags and collapse excess blank lines.
    let stripped = strip_residual_meta(&without_orphan_thinking);
    let newline_re = Regex::new(r"\n{3,}").unwrap();
    let cleaned = newline_re.replace_all(&stripped, "\n\n").trim().to_owned();

    (thoughts, cleaned)
}

fn strip_residual_meta(text: &str) -> String {
    let drop_meta_lines = Regex::new(
        r"(?m)^\s*(?:<\|channel>|<channel\|>|<thinking>|</thinking>|<think>|</think>).*$",
    )
    .unwrap();
    let stripped_lines = drop_meta_lines.replace_all(text, "");
    stripped_lines
        .replace("<|channel>", "")
        .replace("<channel|>", "")
        .replace("<thinking>", "")
        .replace("</thinking>", "")
        .replace("<think>", "")
        .replace("</think>", "")
}

#[cfg(test)]
mod tests {
    use super::extract_thought_blocks;

    #[test]
    fn strips_thinking_block_and_returns_thought() {
        let raw = "<thinking>\nI should inspect the file first.\n</thinking>\nThe answer is 42.";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(thoughts, vec!["I should inspect the file first."]);
        assert_eq!(text, "The answer is 42.");
    }

    #[test]
    fn strips_think_alias() {
        let raw = "<think>some reasoning</think>final answer";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(thoughts, vec!["some reasoning"]);
        assert_eq!(text, "final answer");
    }

    #[test]
    fn strips_channel_block() {
        let raw = "<|channel>thought\nI will check the repo.\n<channel|>\nHere is the result.";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(thoughts, vec!["I will check the repo."]);
        assert_eq!(text, "Here is the result.");
    }

    #[test]
    fn strips_orphan_channel_tokens() {
        let raw = "Final answer <channel|> with stray token";
        let (_thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(text, "Final answer  with stray token");
    }

    #[test]
    fn passthrough_plain_text() {
        let raw = "Hello, world!";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert!(thoughts.is_empty());
        assert_eq!(text, "Hello, world!");
    }

    #[test]
    fn strips_unclosed_thinking_block() {
        let raw = "<thinking>\nI should call a tool but never close the tag.";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(
            thoughts,
            vec!["I should call a tool but never close the tag."]
        );
        assert_eq!(text, "");
    }
}
