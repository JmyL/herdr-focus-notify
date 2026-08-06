use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

const PREVIEW_SIDE_MAX_CHARS: usize = 100;
const PREVIEW_SEPARATOR: &str = "\n...\n";
const EMPTY_ANSWER_PREVIEW: &str = "(empty)";

/// Prefer a prose preview of the latest Cursor assistant turn when a transcript exists.
///
/// Preview is `{first_prose...}\n...\n{last_prose...}` (100 chars each), after stripping
/// inline markdown and skipping fenced/code-like lines. A single prose line is used
/// alone. When the latest turn includes `AskQuestion` or `CreatePlan` tool_use parts,
/// their `title`/`questions[].prompt` or `overview` are preferred over plain text
/// (questions often live only in tool input). If the latest turn has no remaining
/// prose and no usable tool preview, the preview is `(empty)` instead of an older
/// turn. Session id resolution prefers the live Cursor chat opened by the pane's
/// agent process (`~/.cursor/chats/**/<session>/store.db` via `/proc` or `lsof`),
/// because Herdr's `agent_session` can stay stale after `/clear`. Falls back to the
/// Herdr-reported session id when live resolution is unavailable.
pub(crate) fn latest_answer_preview(
    agent: Option<&str>,
    herdr_session_id: Option<&str>,
    pane_id: Option<&str>,
    herdr_bin: Option<&str>,
) -> Option<String> {
    let agent = agent.map(str::trim).filter(|value| !value.is_empty())?;
    if !agent.eq_ignore_ascii_case("cursor") {
        return None;
    }

    let session_id = live_cursor_session_id(pane_id, herdr_bin).or_else(|| {
        herdr_session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|value| is_plausible_session_id(value))
            .map(str::to_string)
    })?;

    let path = find_cursor_transcript(&session_id)?;
    preview_from_transcript(&path)
}

fn live_cursor_session_id(pane_id: Option<&str>, herdr_bin: Option<&str>) -> Option<String> {
    let pane_id = pane_id.map(str::trim).filter(|value| !value.is_empty())?;
    let herdr_bin = herdr_bin.map(str::trim).filter(|value| !value.is_empty())?;
    let pids = cursor_agent_pids(herdr_bin, pane_id)?;
    pids.into_iter().find_map(session_id_from_process_fds)
}

fn cursor_agent_pids(herdr_bin: &str, pane_id: &str) -> Option<Vec<u32>> {
    let output = Command::new(herdr_bin)
        .arg("pane")
        .arg("process-info")
        .arg("--pane")
        .arg(pane_id)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json = String::from_utf8(output.stdout).ok()?;
    cursor_agent_pids_from_process_info_json(&json)
}

fn cursor_agent_pids_from_process_info_json(json: &str) -> Option<Vec<u32>> {
    let envelope: ProcessInfoEnvelope = serde_json::from_str(json).ok()?;
    let info = envelope.result?.process_info?;
    let mut pids = Vec::new();

    for process in info.foreground_processes.unwrap_or_default() {
        let pid = process.pid?;
        if pid == 0 {
            continue;
        }
        let cmdline = process.cmdline.unwrap_or_default();
        let argv0 = process
            .argv
            .as_ref()
            .and_then(|argv| argv.first())
            .map(String::as_str)
            .unwrap_or("");
        if looks_like_cursor_agent(&cmdline) || looks_like_cursor_agent(argv0) {
            pids.push(pid);
        }
    }

    if let Some(group_pid) = info.foreground_process_group_id {
        if group_pid != 0 && !pids.contains(&group_pid) {
            // Group leader is often the agent MainThread even when cmdline is sparse.
            pids.push(group_pid);
        }
    }

    if pids.is_empty() {
        None
    } else {
        Some(pids)
    }
}

fn looks_like_cursor_agent(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("cursor-agent")
        || lower.contains("/agent")
        || lower.ends_with(" agent")
        || lower.contains("bin/agent")
}

fn session_id_from_process_fds(pid: u32) -> Option<String> {
    session_id_from_linux_proc_fds(pid).or_else(|| session_id_from_lsof(pid))
}

fn session_id_from_linux_proc_fds(pid: u32) -> Option<String> {
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let entries = fs::read_dir(fd_dir).ok()?;

    for entry in entries.flatten() {
        let target = fs::read_link(entry.path()).ok()?;
        if let Some(session_id) = session_id_from_chat_store_path(&target) {
            return Some(session_id);
        }
    }

    None
}

fn session_id_from_lsof(pid: u32) -> Option<String> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;

    if !output.status.success() && output.stdout.is_empty() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    for line in stdout.lines() {
        let Some(path) = line.strip_prefix('n') else {
            continue;
        };
        if let Some(session_id) = session_id_from_chat_store_path(Path::new(path)) {
            return Some(session_id);
        }
    }

    None
}

fn session_id_from_chat_store_path(path: &Path) -> Option<String> {
    let mut parts = path.components().rev();
    let file_name = parts.next()?.as_os_str().to_str()?;
    if file_name != "store.db" {
        return None;
    }

    let session_id = parts.next()?.as_os_str().to_str()?;
    if !is_plausible_session_id(session_id) {
        return None;
    }

    let chats = parts.nth(1)?.as_os_str().to_str()?;
    if chats != "chats" {
        // Expect .../chats/<workspace-hash>/<session-id>/store.db
        // nth(1) after session skips the hash component and lands on "chats".
        return None;
    }

    Some(session_id.to_string())
}

fn is_plausible_session_id(session_id: &str) -> bool {
    session_id.len() >= 8
        && session_id
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

fn find_cursor_transcript(session_id: &str) -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    let projects = Path::new(&home).join(".cursor").join("projects");
    let entries = fs::read_dir(projects).ok()?;

    for entry in entries.flatten() {
        let project_root = entry.path();
        let nested = project_root
            .join("agent-transcripts")
            .join(session_id)
            .join(format!("{session_id}.jsonl"));
        if nested.is_file() {
            return Some(nested);
        }

        let flat = project_root
            .join("agent-transcripts")
            .join(format!("{session_id}.jsonl"));
        if flat.is_file() {
            return Some(flat);
        }
    }

    None
}

fn preview_from_transcript(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut last_preview: Option<String> = None;

    for line in reader.lines() {
        let line = line.ok()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(entry) = serde_json::from_str::<TranscriptEntry>(trimmed) else {
            continue;
        };

        if entry.role.as_deref() != Some("assistant") {
            continue;
        }

        // Always bind preview to the latest assistant turn. An empty/code-only
        // latest turn must not inherit an older turn's prose.
        last_preview = Some(
            preview_from_message(entry.message.as_ref())
                .unwrap_or_else(|| EMPTY_ANSWER_PREVIEW.to_string()),
        );
    }

    last_preview
}

fn preview_from_message(message: Option<&TranscriptMessage>) -> Option<String> {
    let content = message?.content.as_ref()?;
    match content {
        TranscriptContent::Text(text) => prose_preview_from_text(text),
        TranscriptContent::Parts(parts) => {
            // AskQuestion / CreatePlan store the user-facing ask in tool input, often
            // with no text parts (or only status chatter). Prefer that over prose.
            if let Some(tool_text) = tool_preview_text(parts) {
                return prose_preview_from_text(&tool_text);
            }
            prose_preview_from_text(&message_text_from_parts(parts)?)
        }
    }
}

fn message_text_from_parts(parts: &[TranscriptPart]) -> Option<String> {
    let mut chunks = Vec::new();
    for part in parts {
        if part.kind.as_deref() != Some("text") {
            continue;
        }
        if let Some(text) = part
            .text
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            chunks.push(text);
        }
    }
    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n"))
    }
}

fn tool_preview_text(parts: &[TranscriptPart]) -> Option<String> {
    let mut ask_lines = Vec::new();
    let mut plan_lines = Vec::new();

    for part in parts {
        if part.kind.as_deref() != Some("tool_use") {
            continue;
        }
        let Some(name) = part.name.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
            continue;
        };
        let Some(input) = part.input.as_ref() else {
            continue;
        };

        if name.eq_ignore_ascii_case("AskQuestion") || name.eq_ignore_ascii_case("Ask_question") {
            if let Some(lines) = ask_question_lines(input) {
                ask_lines.extend(lines);
            }
        } else if name.eq_ignore_ascii_case("CreatePlan") {
            if let Some(lines) = create_plan_lines(input) {
                plan_lines.extend(lines);
            }
        }
    }

    let lines = if !ask_lines.is_empty() {
        ask_lines
    } else {
        plan_lines
    };
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn ask_question_lines(input: &serde_json::Value) -> Option<Vec<String>> {
    let parsed: AskQuestionInput = serde_json::from_value(input.clone()).ok()?;
    let mut lines = Vec::new();

    push_unique_line(&mut lines, first_non_empty_line(parsed.title.as_deref()));
    for question in parsed.questions.unwrap_or_default() {
        push_unique_line(&mut lines, first_non_empty_line(question.prompt.as_deref()));
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines)
    }
}

fn create_plan_lines(input: &serde_json::Value) -> Option<Vec<String>> {
    let parsed: CreatePlanInput = serde_json::from_value(input.clone()).ok()?;
    if let Some(overview) = first_non_empty_line(parsed.overview.as_deref()) {
        return Some(vec![overview]);
    }
    if let Some(name) = first_non_empty_line(parsed.name.as_deref()) {
        return Some(vec![name]);
    }
    None
}

fn first_non_empty_line(value: Option<&str>) -> Option<String> {
    value?
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn push_unique_line(lines: &mut Vec<String>, line: Option<String>) {
    let Some(line) = line else {
        return;
    };
    if lines.last().map(String::as_str) != Some(line.as_str()) {
        lines.push(line);
    }
}

fn prose_preview_from_text(text: &str) -> Option<String> {
    let without_fences = strip_fenced_code(text);
    let cleaned = strip_inline_markdown(&without_fences);
    let prose_lines: Vec<String> = cleaned
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !looks_like_code_line(line))
        .map(str::to_string)
        .collect();

    match prose_lines.as_slice() {
        [] => None,
        [only] => Some(truncate_chars(only, PREVIEW_SIDE_MAX_CHARS)),
        [first, .., last] if first != last => Some(format!(
            "{}{}{}",
            truncate_chars(first, PREVIEW_SIDE_MAX_CHARS),
            PREVIEW_SEPARATOR,
            truncate_chars(last, PREVIEW_SIDE_MAX_CHARS)
        )),
        [first, ..] => Some(truncate_chars(first, PREVIEW_SIDE_MAX_CHARS)),
    }
}

fn strip_inline_markdown(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Links: [label](url) -> label
        if chars[i] == '[' {
            if let Some((label, next)) = parse_markdown_link(&chars, i) {
                output.push_str(&label);
                i = next;
                continue;
            }
        }

        // Double-delimiter wraps: **bold**, __bold__, ~~strike~~
        if let Some((delim_len, next)) = parse_delimited_span(&chars, i, &["**", "__", "~~"]) {
            let inner: String = chars[i + delim_len..next - delim_len].iter().collect();
            output.push_str(&strip_inline_markdown(&inner));
            i = next;
            continue;
        }

        // Backtick code: `code`
        if chars[i] == '`' {
            if let Some(next) = find_closing_single(&chars, i, '`') {
                let inner: String = chars[i + 1..next].iter().collect();
                output.push_str(&inner);
                i = next + 1;
                continue;
            }
        }

        // Single-delimiter wraps: *italic*, _italic_ (avoid snake_case mid-word)
        if chars[i] == '*' {
            if let Some(next) = find_closing_single(&chars, i, '*') {
                let inner: String = chars[i + 1..next].iter().collect();
                output.push_str(&strip_inline_markdown(&inner));
                i = next + 1;
                continue;
            }
        }
        if chars[i] == '_' && is_markdown_underscore_open(&chars, i) {
            if let Some(next) = find_closing_underscore(&chars, i) {
                let inner: String = chars[i + 1..next].iter().collect();
                output.push_str(&strip_inline_markdown(&inner));
                i = next + 1;
                continue;
            }
        }

        // ATX headings at line start: "# heading" -> "heading"
        if chars[i] == '#' && (i == 0 || chars[i - 1] == '\n') {
            let mut j = i;
            while j < chars.len() && chars[j] == '#' {
                j += 1;
            }
            if j < chars.len() && chars[j] == ' ' {
                i = j + 1;
                continue;
            }
        }

        output.push(chars[i]);
        i += 1;
    }

    output
}

fn parse_markdown_link(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) != Some(&'[') {
        return None;
    }
    let mut i = start + 1;
    let mut label = String::new();
    while i < chars.len() {
        if chars[i] == ']' {
            break;
        }
        if chars[i] == '\n' {
            return None;
        }
        label.push(chars[i]);
        i += 1;
    }
    if i >= chars.len() || chars.get(i + 1) != Some(&'(') {
        return None;
    }
    i += 2;
    while i < chars.len() && chars[i] != ')' {
        if chars[i] == '\n' {
            return None;
        }
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    Some((strip_inline_markdown(&label), i + 1))
}

fn parse_delimited_span(chars: &[char], start: usize, delims: &[&str]) -> Option<(usize, usize)> {
    for delim in delims {
        let delim_chars: Vec<char> = delim.chars().collect();
        if chars[start..].starts_with(&delim_chars) {
            let mut i = start + delim_chars.len();
            while i + delim_chars.len() <= chars.len() {
                if chars[i..].starts_with(&delim_chars) {
                    if i == start + delim_chars.len() {
                        break;
                    }
                    return Some((delim_chars.len(), i + delim_chars.len()));
                }
                if chars[i] == '\n' {
                    break;
                }
                i += 1;
            }
        }
    }
    None
}

fn find_closing_single(chars: &[char], start: usize, delim: char) -> Option<usize> {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == delim {
            if i == start + 1 {
                return None;
            }
            return Some(i);
        }
        if chars[i] == '\n' {
            return None;
        }
        i += 1;
    }
    None
}

fn is_markdown_underscore_open(chars: &[char], i: usize) -> bool {
    let prev_ok = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
    let next_ok = chars
        .get(i + 1)
        .is_some_and(|ch| *ch != '_' && !ch.is_whitespace());
    prev_ok && next_ok
}

fn find_closing_underscore(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == '_' {
            let next_ok = i + 1 >= chars.len() || !chars[i + 1].is_ascii_alphanumeric();
            if i > start + 1 && next_ok {
                return Some(i);
            }
        }
        if chars[i] == '\n' {
            return None;
        }
        i += 1;
    }
    None
}

fn strip_fenced_code(text: &str) -> String {
    let mut output = String::new();
    let mut in_fence = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(line);
    }

    output
}

fn looks_like_code_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed.starts_with("```") {
        return true;
    }
    if trimmed.starts_with("$ ") || trimmed == "$" {
        return true;
    }
    if matches!(trimmed, "{" | "}" | "};" | "})" | "},") {
        return true;
    }
    if (trimmed.starts_with('{') || trimmed.starts_with('}')) && trimmed.len() <= 3 {
        return true;
    }

    // Indentation-heavy leftovers after fence stripping.
    if line.starts_with("    ") || line.starts_with('\t') {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    const PREFIXES: &[&str] = &[
        "import ",
        "from ",
        "const ",
        "let ",
        "var ",
        "fn ",
        "def ",
        "class ",
        "pub ",
        "function ",
        "package ",
        "#include",
        "use ",
        "export ",
        "return ",
        "async ",
        "await ",
        "#!/",
    ];
    PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let mut output: String = trimmed.chars().take(max_chars).collect();
    output.push_str("...");
    output
}

#[derive(Debug, Deserialize)]
struct ProcessInfoEnvelope {
    result: Option<ProcessInfoResult>,
}

#[derive(Debug, Deserialize)]
struct ProcessInfoResult {
    process_info: Option<ProcessInfo>,
}

#[derive(Debug, Deserialize)]
struct ProcessInfo {
    foreground_process_group_id: Option<u32>,
    foreground_processes: Option<Vec<ForegroundProcess>>,
}

#[derive(Debug, Deserialize)]
struct ForegroundProcess {
    pid: Option<u32>,
    cmdline: Option<String>,
    argv: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TranscriptEntry {
    role: Option<String>,
    message: Option<TranscriptMessage>,
}

#[derive(Debug, Deserialize)]
struct TranscriptMessage {
    content: Option<TranscriptContent>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TranscriptContent {
    Parts(Vec<TranscriptPart>),
    Text(String),
}

#[derive(Debug, Deserialize)]
struct TranscriptPart {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct AskQuestionInput {
    title: Option<String>,
    questions: Option<Vec<AskQuestionItem>>,
}

#[derive(Debug, Deserialize)]
struct AskQuestionItem {
    prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreatePlanInput {
    name: Option<String>,
    overview: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_transcript(contents: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = env::temp_dir().join(format!("herdr-focus-notify-transcript-{nanos}.jsonl"));
        let mut file = File::create(&path).expect("create transcript");
        file.write_all(contents.as_bytes())
            .expect("write transcript");
        path
    }

    #[test]
    fn joins_first_and_last_prose_lines() {
        let path = temp_transcript(
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"First prose line.\nMiddle detail.\nLast prose line."}]}}
"#,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some("First prose line.\n...\nLast prose line.")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn skips_fenced_code_at_start_and_end() {
        let text = "```bash\necho hi\n```\nHere is the summary.\nDone for now.\n```\ncode\n```";
        assert_eq!(
            prose_preview_from_text(text).as_deref(),
            Some("Here is the summary.\n...\nDone for now.")
        );
    }

    #[test]
    fn skips_code_like_lines_around_prose() {
        let text = "import os\nActual answer starts here.\nconst x = 1\nWrap-up sentence.";
        assert_eq!(
            prose_preview_from_text(text).as_deref(),
            Some("Actual answer starts here.\n...\nWrap-up sentence.")
        );
    }

    #[test]
    fn strips_inline_markdown_formatting() {
        let text = "**Bold start** with a [link](https://example.com).\nMiddle.\nDone with `code` and *emphasis*.";
        assert_eq!(
            prose_preview_from_text(text).as_deref(),
            Some("Bold start with a link.\n...\nDone with code and emphasis.")
        );
    }

    #[test]
    fn single_prose_line_has_no_separator() {
        assert_eq!(
            prose_preview_from_text("Just one line.").as_deref(),
            Some("Just one line.")
        );
    }

    #[test]
    fn all_code_returns_none() {
        assert_eq!(
            prose_preview_from_text("```rust\nfn main() {}\n```\nconst x = 1;"),
            None
        );
    }

    #[test]
    fn truncates_each_side_to_one_hundred_chars() {
        let first = "A".repeat(120);
        let last = "B".repeat(120);
        let text = format!("{first}\nmiddle\n{last}");
        let preview = prose_preview_from_text(&text).unwrap();
        let (left, right) = preview.split_once(PREVIEW_SEPARATOR).unwrap();
        assert_eq!(left.chars().count(), PREVIEW_SIDE_MAX_CHARS + 3);
        assert_eq!(right.chars().count(), PREVIEW_SIDE_MAX_CHARS + 3);
        assert!(left.ends_with("..."));
        assert!(right.ends_with("..."));
        assert!(left.starts_with(&"A".repeat(PREVIEW_SIDE_MAX_CHARS)));
    }

    #[test]
    fn prefers_latest_assistant_turn() {
        let path = temp_transcript(
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Older first.\nOlder last."}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":"Fresh first.\nFresh last."}]}}
"#,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some("Fresh first.\n...\nFresh last.")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn empty_latest_turn_returns_empty_marker() {
        let path = temp_transcript(
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Older first.\nOlder last."}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":"```rust\nfn main() {}\n```\nconst x = 1;"}]}}
"#,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some(EMPTY_ANSWER_PREVIEW)
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn empty_message_latest_turn_returns_empty_marker() {
        let path = temp_transcript(
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Older prose."}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":""}]}}
"#,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some(EMPTY_ANSWER_PREVIEW)
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn ask_question_only_uses_title_and_prompt() {
        let path = temp_transcript(
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Older prose."}]}}
{"role":"assistant","message":{"content":[{"type":"tool_use","name":"AskQuestion","input":{"title":"Bluetooth 끊김 시 기본 장치 동작","questions":[{"id":"reconnect","prompt":"BT 헤드셋이 다시 연결되면 기본 장치를 어떻게 할까요?\n\n세부 설명","options":[{"id":"auto","label":"자동"}]}]}}]}}
"#,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some(
                "Bluetooth 끊김 시 기본 장치 동작\n...\nBT 헤드셋이 다시 연결되면 기본 장치를 어떻게 할까요?"
            )
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn ask_question_prefers_prompt_over_status_text() {
        let path = temp_transcript(
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"설정을 먼저 확인하겠습니다."},{"type":"tool_use","name":"AskQuestion","input":{"title":"Clarify format-on-save behavior","questions":[{"id":"how","prompt":"저장할 때 어떤 식으로 바뀌나요? (원인 좁히기용)"}]}}]}}
"#,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some(
                "Clarify format-on-save behavior\n...\n저장할 때 어떤 식으로 바뀌나요? (원인 좁히기용)"
            )
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn ask_question_multiple_prompts_use_first_and_last() {
        let path = temp_transcript(
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"AskQuestion","input":{"questions":[{"id":"a","prompt":"알림 body를 어떻게 바꿀까요?"},{"id":"b","prompt":"중간 질문"},{"id":"c","prompt":"“답변의 첫번째 줄”은 어느 쪽을 말하는 건가요?"}]}}]}}
"#,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some(
                "알림 body를 어떻게 바꿀까요?\n...\n“답변의 첫번째 줄”은 어느 쪽을 말하는 건가요?"
            )
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn create_plan_only_uses_overview() {
        let path = temp_transcript(
            r##"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"CreatePlan","input":{"name":"fuzzel size followup","overview":"지금 활성 모니터는 scale 1.0이라 DPI만으로는 부족합니다.","plan":"# long plan body\n\nmore detail"}}]}}
"##,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some("지금 활성 모니터는 scale 1.0이라 DPI만으로는 부족합니다.")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn non_ask_tool_use_only_returns_empty_marker() {
        let path = temp_transcript(
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Older prose."}]}}
{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Shell","input":{"command":"echo hi"}}]}}
"#,
        );

        assert_eq!(
            preview_from_transcript(&path).as_deref(),
            Some(EMPTY_ANSWER_PREVIEW)
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn skips_non_cursor_agents() {
        assert_eq!(
            latest_answer_preview(
                Some("codex"),
                Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
                None,
                None
            ),
            None
        );
    }

    #[test]
    fn finds_transcript_under_fake_home_and_returns_preview() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let home = env::temp_dir().join(format!("herdr-focus-notify-home-{nanos}"));
        let session_id = "11111111-2222-3333-4444-555555555555";
        let transcript_dir = home
            .join(".cursor")
            .join("projects")
            .join("demo")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&transcript_dir).expect("mkdir");
        let path = transcript_dir.join(format!("{session_id}.jsonl"));
        fs::write(
            &path,
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Older line"}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":"Fresh preview line\nmore detail here"}]}}
"#,
        )
        .expect("write");

        let previous_home = env::var_os("HOME");
        env::set_var("HOME", &home);
        let preview = latest_answer_preview(Some("cursor"), Some(session_id), None, None);
        match previous_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }

        assert_eq!(
            preview.as_deref(),
            Some("Fresh preview line\n...\nmore detail here")
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn parses_session_id_from_chat_store_path() {
        let path = Path::new(
            "/home/sungsik/.cursor/chats/d42b9c57d24cf5db3bd8d332dc35437f/0d5f7b87-55e7-4904-b4a5-908d3d3f634c/store.db",
        );
        assert_eq!(
            session_id_from_chat_store_path(path).as_deref(),
            Some("0d5f7b87-55e7-4904-b4a5-908d3d3f634c")
        );
        assert_eq!(
            session_id_from_chat_store_path(Path::new("/tmp/store.db")),
            None
        );
    }

    #[test]
    fn extracts_cursor_agent_pids_from_process_info() {
        let json = r#"{
          "id":"cli:pane:process_info",
          "result":{
            "type":"pane_process_info",
            "process_info":{
              "pane_id":"w1J:p1",
              "shell_pid":223036,
              "foreground_process_group_id":290942,
              "foreground_processes":[
                {
                  "pid":290942,
                  "name":"MainThread",
                  "cwd":"/tmp",
                  "cmdline":"/home/sungsik/.local/bin/agent --use-system-ca /home/sungsik/.local/share/cursor-agent/versions/x/index.js",
                  "argv":["/home/sungsik/.local/bin/agent","--use-system-ca","/home/sungsik/.local/share/cursor-agent/versions/x/index.js"]
                }
              ]
            }
          }
        }"#;

        assert_eq!(
            cursor_agent_pids_from_process_info_json(json),
            Some(vec![290942])
        );
    }
}
