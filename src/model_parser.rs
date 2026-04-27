use regex::Regex;

/// Strip thought/reasoning blocks from model output text.
///
/// Returns `(thoughts, cleaned_text)` where `thoughts` is a vec of extracted
/// reasoning strings and `cleaned_text` has all thought markers removed.
pub fn extract_thought_blocks(raw_text: &str) -> (Vec<String>, String) {
    let mut thoughts = Vec::new();

    let balanced_patterns = [
        (
            Regex::new(r"(?s)<\|channel>thought(?:[ \t]*\r?\n|[ \t]+)?(.*?)<channel\|>").unwrap(),
            1usize,
        ),
        (Regex::new(r"(?s)<\|think\|>(.*?)<\|/think\|>").unwrap(), 1),
    ];

    let mut working = raw_text.to_owned();
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
        Regex::new(r"(?s)<\|channel>thought(?:[ \t]*\r?\n|[ \t]+)?(.*)$").unwrap(),
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

    // Drop leftover orphan thought tags and collapse excess blank lines.
    let stripped = strip_residual_meta(&working);
    let newline_re = Regex::new(r"\n{3,}").unwrap();
    let cleaned = newline_re.replace_all(&stripped, "\n\n").trim().to_owned();

    (thoughts, cleaned)
}

/// Clean accumulated answer text that has already been separated from a thinking block
/// by the streaming state machine.
///
/// Unlike `clean_model_text`, this does NOT apply the `prefilled_close_patterns` step.
/// That step matches `^(.*?)<channel\|>(.*)$`, so if the answer text mentions
/// `<channel|>` in prose (e.g. the model explaining Gemma token format), it treats
/// everything before that tag as a thought and discards it — silently wiping the answer.
///
/// Instead: strip only complete balanced thought blocks, then remove stray markers.
pub fn clean_streaming_answer(text: &str) -> String {
    let re_channel =
        Regex::new(r"(?s)<\|channel>thought(?:[ \t]*\r?\n|[ \t]+)?(.*?)<channel\|>").unwrap();
    let re_think = Regex::new(r"(?s)<\|think\|>(.*?)<\|/think\|>").unwrap();
    let s = re_channel.replace_all(text, "");
    let s = re_think.replace_all(&s, "");
    strip_residual_meta(&s).trim().to_owned()
}

/// Like `clean_streaming_answer` but without the final trim.
///
/// Use this for individual streaming chunks where whitespace tokens (spaces, newlines
/// between words) must be preserved — trimming would silently drop the space in "Hello ".
pub fn clean_streaming_chunk(text: &str) -> String {
    let re_channel =
        Regex::new(r"(?s)<\|channel>thought(?:[ \t]*\r?\n|[ \t]+)?(.*?)<channel\|>").unwrap();
    let re_think = Regex::new(r"(?s)<\|think\|>(.*?)<\|/think\|>").unwrap();
    let s = re_channel.replace_all(text, "");
    let s = re_think.replace_all(&s, "");
    strip_residual_meta(&s).to_owned()
}

fn strip_residual_meta(text: &str) -> String {
    let drop_meta_lines =
        Regex::new(r"(?m)^\s*(?:<\|channel>|<channel\|>|<\|think\|>|<\|/think\|>).*$").unwrap();
    let stripped_lines = drop_meta_lines.replace_all(text, "");
    stripped_lines
        .replace("<|channel>", "")
        .replace("<channel|>", "")
        .replace("<|think|>", "")
        .replace("<|/think|>", "")
}

#[cfg(test)]
mod tests {
    use super::extract_thought_blocks;

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
    fn strips_channel_block_without_newline_after_label() {
        let raw = "<|channel>thought I will check the repo.<channel|>Here is the result.";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(thoughts, vec!["I will check the repo."]);
        assert_eq!(text, "Here is the result.");
    }

    #[test]
    fn strips_prefilled_close_form() {
        let raw = "I will check the repo.<channel|>Here is the result.";
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
    fn preserves_non_gemma_thinking_aliases() {
        let raw = "<thinking>I will check the repo.</thinking>\nHere is the result.";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert!(thoughts.is_empty());
        assert_eq!(text, raw);
    }

    #[test]
    fn strips_unclosed_thinking_block() {
        let raw = "<|channel>thought\nI should call a tool but never close the tag.";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(
            thoughts,
            vec!["I should call a tool but never close the tag."]
        );
        assert_eq!(text, "");
    }

    #[test]
    fn preserves_non_thought_gemma_control_tokens() {
        let raw = "<|think|>\nI will search.\n<|/think|>\nFinal answer.\n<|turn|>user\nWhat's next?\n<turn|>";
        let (thoughts, text) = extract_thought_blocks(raw);
        assert_eq!(thoughts, vec!["I will search."]);
        assert_eq!(text, "Final answer.\n<|turn|>user\nWhat's next?\n<turn|>");
    }
}
