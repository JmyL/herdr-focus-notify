use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Prefer the first line of the latest Cursor assistant turn when a transcript exists.
pub(crate) fn latest_answer_first_line(agent: Option<&str>, session_id: Option<&str>) -> Option<String> {
    let agent = agent.map(str::trim).filter(|value| !value.is_empty())?;
    if !agent.eq_ignore_ascii_case("cursor") {
        return None;
    }

    let session_id = session_id.map(str::trim).filter(|value| !value.is_empty())?;
    if !is_plausible_session_id(session_id) {
        return None;
    }

    let path = find_cursor_transcript(session_id)?;
    first_line_from_transcript(&path)
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

fn first_line_from_transcript(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut last_first_line: Option<String> = None;

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

        if let Some(first_line) = first_line_from_message(entry.message.as_ref()) {
            last_first_line = Some(first_line);
        }
    }

    last_first_line
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
}

fn first_line_from_message(message: Option<&TranscriptMessage>) -> Option<String> {
    let content = message?.content.as_ref()?;
    match content {
        TranscriptContent::Text(text) => first_non_empty_line(text),
        TranscriptContent::Parts(parts) => parts.iter().find_map(|part| {
            if part.kind.as_deref() != Some("text") {
                return None;
            }
            first_non_empty_line(part.text.as_deref()?)
        }),
    }
}

fn first_non_empty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
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
        file.write_all(contents.as_bytes()).expect("write transcript");
        path
    }

    #[test]
    fn extracts_first_line_of_latest_assistant_text() {
        let path = temp_transcript(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":"First answer.\nMore."}]}}
{"role":"user","message":{"content":[{"type":"text","text":"again"}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":"  Latest answer first line.\nSecond."}]}}
{"type":"turn_ended","status":"success"}
"#,
        );

        assert_eq!(
            first_line_from_transcript(&path).as_deref(),
            Some("Latest answer first line.")
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn skips_non_cursor_agents() {
        assert_eq!(
            latest_answer_first_line(Some("codex"), Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")),
            None
        );
    }

    #[test]
    fn finds_transcript_under_fake_home_and_returns_latest_line() {
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
{"role":"assistant","message":{"content":[{"type":"text","text":"Fresh preview line\nmore"}]}}
"#,
        )
        .expect("write");

        let previous_home = env::var_os("HOME");
        env::set_var("HOME", &home);
        let preview = latest_answer_first_line(Some("cursor"), Some(session_id));
        match previous_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }

        assert_eq!(preview.as_deref(), Some("Fresh preview line"));
        let _ = fs::remove_dir_all(home);
    }
}
