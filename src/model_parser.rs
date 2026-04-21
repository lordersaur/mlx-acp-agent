use regex::Regex;

/// Strip thought/reasoning blocks from model output text.
///
/// Returns `(thoughts, cleaned_text)` where `thoughts` is a vec of extracted
/// reasoning strings and `cleaned_text` has all thought markers removed.
pub fn extract_thought_blocks(text: &str) -> (Vec<String>, String) {
    let mut thoughts = Vec::new();

    let balanced_patterns = [
        (
            Regex::new(r"(?s)<\|channel>thought\n(.*?)<channel\|>").unwrap(),
            1usize,
        ),
        (Regex::new(r"(?s)<thinking>(.*?)</thinking>").unwrap(), 1),
        (Regex::new(r"(?s)<think>(.*?)</think>").unwrap(), 1),
        (Regex::new(r"(?s)<\|think\|>(.*?)<\|/think\|>").unwrap(), 1),
    ];

    let mut working = text.to_owned();
    for (re, group) in balanced_patterns {
        working = re
            .replace_all(&working, |caps: &regex::Captures<'_>| {
                if let Some(body) = caps.get(group).map(|m| m.as_str().trim()) {
                    if !body.is_empty() {
                        thoughts.push(body.to_owned());
                    }
                }
                ""
            })
            .into_owned();
    }

    let prefilled_close_patterns = [
        (Regex::new(r"(?s)^(.*?)</think>(.*)$").unwrap(), 1usize),
        (Regex::new(r"(?s)^(.*?)</thinking>(.*)$").unwrap(), 1usize),
        (Regex::new(r"(?s)^(.*?)<\|/think\|>(.*)$").unwrap(), 1usize),
        (Regex::new(r"(?s)^(.*?)<channel\|>(.*)$").unwrap(), 1usize),
    ];

    for (re, group) in prefilled_close_patterns {
        if re.is_match(&working) {
            working = re
                .replace_all(&working, |caps: &regex::Captures<'_>| {
                    let body = caps
                        .get(group)
                        .map(|m| m.as_str().trim())
                        .unwrap_or_default();
                    if !body.is_empty() {
                        thoughts.push(body.to_owned());
                    }
                    caps.get(2)
                        .map(|m| m.as_str().to_owned())
                        .unwrap_or_default()
                })
                .into_owned();
        }
    }

    let orphan_open_patterns = [
        Regex::new(r"(?s)<\|channel>thought\n(.*)$").unwrap(),
        Regex::new(r"(?s)<thinking>(.*)$").unwrap(),
        Regex::new(r"(?s)<think>(.*)$").unwrap(),
        Regex::new(r"(?s)<\|think\|>(.*)$").unwrap(),
    ];

    for re in orphan_open_patterns {
        if re.is_match(&working) {
            working = re
                .replace_all(&working, |caps: &regex::Captures<'_>| {
                    let body = caps.get(1).map(|m| m.as_str().trim()).unwrap_or_default();
                    if !body.is_empty() {
                        thoughts.push(body.to_owned());
                    }
                    ""
                })
                .into_owned();
        }
    }

    // Drop any leftover orphan tags and collapse excess blank lines.
    let stripped = strip_residual_meta(&working);
    let newline_re = Regex::new(r"\n{3,}").unwrap();
    let cleaned = newline_re.replace_all(&stripped, "\n\n").trim().to_owned();

    (thoughts, cleaned)
}

fn strip_residual_meta(text: &str) -> String {
    let drop_meta_lines = Regex::new(
        r"(?m)^\s*(?:<\|channel>|<channel\|>|<thinking>|</thinking>|<think>|</think>|<\|think\|>|<\|/think\|>|<\|turn\|>|<turn\|>|</turn>).*$",
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
        .replace("<|think|>", "")
        .replace("<|/think|>", "")
        .replace("<|turn|>", "")
        .replace("<turn|>", "")
        .replace("</turn>", "")
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
    fn strips_gemma_think_token_pair() {
        let raw = "<|think|>some reasoning<|/think|>final answer";
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
    fn strips_prefilled_close_form() {
        let raw = "I will check the repo.</thinking>Here is the result.";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(thoughts, vec!["I will check the repo."]);
        assert_eq!(text, "Here is the result.");
    }

    #[test]
    fn strips_orphan_channel_tokens() {
        let raw = "Final answer <channel|> with stray token";
        let (_thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(text, "with stray token");
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
