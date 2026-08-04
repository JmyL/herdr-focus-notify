use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// Prefer the first line of the latest Cursor assistant turn when a transcript exists.
///
/// Session id resolution prefers the live Cursor chat opened by the pane's
/// agent process (`~/.cursor/chats/**/<session>/store.db` via `/proc` or `lsof`),
/// because Herdr's `agent_session` can stay stale after `/clear`. Falls back to
/// the Herdr-reported session id when live resolution is unavailable.
pub(crate) fn latest_answer_first_line(
    agent: Option<&str>,
    herdr_session_id: Option<&str>,
    pane_id: Option<&str>,
    herdr_bin: Option<&str>,
) -> Option<String> {
    let agent = agent.map(str::trim).filter(|value| !value.is_empty())?;
    if !agent.eq_ignore_ascii_case("cursor") {
        return None;
    }

    let session_id = live_cursor_session_id(pane_id, herdr_bin)
        .or_else(|| {
            herdr_session_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .filter(|value| is_plausible_session_id(value))
                .map(str::to_string)
        })?;

    let path = find_cursor_transcript(&session_id)?;
    first_line_from_transcript(&path)
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
            latest_answer_first_line(
                Some("codex"),
                Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
                None,
                None
            ),
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
        let preview = latest_answer_first_line(Some("cursor"), Some(session_id), None, None);
        match previous_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }

        assert_eq!(preview.as_deref(), Some("Fresh preview line"));
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
