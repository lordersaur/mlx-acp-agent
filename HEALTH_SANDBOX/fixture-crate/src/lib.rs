pub const SYSTEM_PROMPT: &str = r#"You are a coding agent.

## Output
- Be concise.
- If you can't do something, say so.
"#;

pub fn file_chunk_lines(content: &str, start_line: usize, limit: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if start_line > total {
        return format!("[start_line {start_line} is past end of file ({total} lines)]");
    }
    let from = start_line - 1;
    let to = (from + limit).min(total);
    let chunk = lines[from..to]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{line_number}: {line}", line_number = from + i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    if to >= total {
        chunk
    } else {
        format!("{chunk}\n[Showing lines {start_line}-{to}. Continue at {}.]", to + 1)
    }
}

pub fn run_command_status(exit_code: i32, stderr: &str) -> String {
    if exit_code == 0 {
        "ok".to_owned()
    } else {
        format!("failed with code {exit_code}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_chunk_lines_formats_first_page() {
        let output = file_chunk_lines("one\ntwo\nthree", 1, 2);
        assert_eq!(output, "1: one\n2: two\n[Showing lines 1-2. Continue at 3.]");
    }

    #[test]
    fn run_command_status_reports_success() {
        assert_eq!(run_command_status(0, ""), "ok");
    }
}
