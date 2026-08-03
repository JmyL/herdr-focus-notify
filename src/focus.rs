use serde::Deserialize;
use std::path::Path;
use std::process::Command;

use crate::answer_preview::latest_answer_first_line;
use crate::config::{activate_app, is_linux_notifier};
use crate::notification::FocusNotification;
use crate::notifier::host_command;
use crate::state::{cache_sway_container_id, cached_sway_container_id};
use crate::util::sanitize_group_id;

const NOTIFICATION_BODY_MAX_CHARS: usize = 120;

#[derive(Debug, Deserialize)]
struct AgentListEnvelope {
    result: Option<AgentListResult>,
}

#[derive(Debug, Deserialize)]
struct AgentListResult {
    agents: Vec<AgentInfo>,
}

#[derive(Debug, Deserialize)]
struct AgentInfo {
    focused: bool,
    agent: Option<String>,
    pane_id: Option<String>,
    cwd: Option<String>,
    workspace_id: Option<String>,
    terminal_title: Option<String>,
    terminal_title_stripped: Option<String>,
    agent_session: Option<AgentSession>,
}

#[derive(Debug, Deserialize)]
struct AgentSession {
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceGetEnvelope {
    result: Option<WorkspaceGetResult>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceGetResult {
    workspace: Option<WorkspaceInfo>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceInfo {
    label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationDecision {
    Skip,
    Send,
    SendWithVisibilityMonitor,
}

pub(crate) fn test_notification(herdr_bin: &str) -> FocusNotification {
    let pane_id = focused_pane_id(herdr_bin).unwrap_or_else(|| "test-pane".to_string());
    FocusNotification {
        pane_id: pane_id.clone(),
        workspace_id: None,
        status: "blocked".to_string(),
        title: "Herdr Focus Notify test".to_string(),
        body: format!("Click to run: {herdr_bin} agent focus {pane_id}"),
        group: format!("herdr-{}", sanitize_group_id(&pane_id)),
        app_icon: None,
    }
}

pub(crate) fn notification_decision(pane_id: &str, herdr_bin: &str) -> NotificationDecision {
    if is_linux_notifier() {
        return notification_decision_for_linux(
            pane_is_focused(pane_id, herdr_bin),
            cached_sway_container_id(pane_id),
            current_sway_container_id(),
        );
    }

    notification_decision_from_focus_and_bundles(
        pane_is_focused(pane_id, herdr_bin),
        herdr_bundle_id(),
        frontmost_bundle_id(),
    )
}

pub(crate) fn should_clear_notification_on_focus() -> bool {
    if is_linux_notifier() {
        return true;
    }

    configured_app_is_frontmost(herdr_bundle_id(), frontmost_bundle_id())
}

pub(crate) fn cache_current_sway_container_for_pane(pane_id: &str) -> Result<(), String> {
    if !is_linux_notifier() {
        return Ok(());
    }

    let Some(con_id) = current_sway_container_id() else {
        return Ok(());
    };

    cache_sway_container_id(pane_id, &con_id)
        .map_err(|err| format!("failed to cache sway container id: {err}"))
}

/// Prefer `{workspace_name} · {preview}` from Herdr agent/workspace metadata.
///
/// Workspace name comes from `herdr workspace get` label after stripping a
/// navigator-style `token:\s` prefix; cwd basename is the fallback.
/// Preview is the first line of the latest Cursor assistant turn when a
/// transcript exists; otherwise the terminal/session title.
pub(crate) fn enrich_notification_body(notification: &mut FocusNotification, herdr_bin: &str) {
    if let Some(body) = agent_context_body(&notification.pane_id, herdr_bin) {
        notification.body = body;
    }
}

fn agent_context_body(pane_id: &str, herdr_bin: &str) -> Option<String> {
    let output = Command::new(herdr_bin)
        .arg("agent")
        .arg("list")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json = String::from_utf8(output.stdout).ok()?;
    let envelope: AgentListEnvelope = serde_json::from_str(&json).ok()?;
    let agents = envelope.result?.agents;
    let agent = agents.into_iter().find(|agent| {
        agent
            .pane_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|id| id == pane_id)
    })?;

    let workspace_label = agent
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|workspace_id| fetch_workspace_label(herdr_bin, workspace_id));

    agent_context_body_from_agent(&agent, workspace_label.as_deref())
}

fn fetch_workspace_label(herdr_bin: &str, workspace_id: &str) -> Option<String> {
    let output = Command::new(herdr_bin)
        .arg("workspace")
        .arg("get")
        .arg(workspace_id)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json = String::from_utf8(output.stdout).ok()?;
    workspace_label_from_workspace_get_json(&json)
}

fn workspace_label_from_workspace_get_json(json: &str) -> Option<String> {
    let envelope: WorkspaceGetEnvelope = serde_json::from_str(json).ok()?;
    first_non_empty([envelope.result?.workspace?.label.as_deref()]).map(str::to_string)
}

fn agent_context_body_from_agent(
    agent: &AgentInfo,
    workspace_label: Option<&str>,
) -> Option<String> {
    let cwd_name = agent
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(cwd_basename);

    let workspace_name = workspace_label.and_then(workspace_display_name);
    let name = first_non_empty([workspace_name.as_deref(), cwd_name.as_deref()]);

    let session_title = first_non_empty([
        agent.terminal_title_stripped.as_deref(),
        agent.terminal_title.as_deref(),
    ])
    .map(str::to_string);

    let answer_preview = latest_answer_first_line(
        agent.agent.as_deref(),
        agent
            .agent_session
            .as_ref()
            .and_then(|session| session.value.as_deref()),
    );

    compose_notification_body(name, answer_preview.as_deref(), session_title.as_deref())
}

#[cfg(test)]
fn agent_context_body_from_agent_list_json(
    pane_id: &str,
    json: &str,
    workspace_label: Option<&str>,
) -> Option<String> {
    let envelope: AgentListEnvelope = serde_json::from_str(json).ok()?;
    let agents = envelope.result?.agents;
    let agent = agents.into_iter().find(|agent| {
        agent
            .pane_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|id| id == pane_id)
    })?;

    agent_context_body_from_agent(&agent, workspace_label)
}

fn cwd_basename(cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .filter(|name| !name.is_empty())
}

/// Navigator-style labels look like `project: ree-drive` / `dir: foo`.
/// Take the text after the first `[^\s]+:\s`; otherwise use the whole label.
fn workspace_display_name(label: &str) -> Option<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some((prefix, after_colon)) = trimmed.split_once(':') {
        let prefix_ok = !prefix.is_empty() && !prefix.chars().any(char::is_whitespace);
        let has_space_after_colon = after_colon
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        if prefix_ok && has_space_after_colon {
            let name = after_colon.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    Some(trimmed.to_string())
}

fn compose_notification_body(
    project: Option<&str>,
    answer_preview: Option<&str>,
    session_title: Option<&str>,
) -> Option<String> {
    let preview = first_non_empty([answer_preview, session_title]);

    match (project, preview) {
        (Some(project), Some(preview)) => Some(truncate(
            &format!("{project} · {preview}"),
            NOTIFICATION_BODY_MAX_CHARS,
        )),
        (Some(project), None) => Some(truncate(project, NOTIFICATION_BODY_MAX_CHARS)),
        (None, Some(preview)) => Some(truncate(preview, NOTIFICATION_BODY_MAX_CHARS)),
        (None, None) => None,
    }
}

fn first_non_empty<const N: usize>(values: [Option<&str>; N]) -> Option<&str> {
    values
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
}

fn truncate(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }

    let mut output: String = trimmed.chars().take(max_chars.saturating_sub(3)).collect();
    output.push_str("...");
    output
}

fn pane_is_focused(pane_id: &str, herdr_bin: &str) -> bool {
    focused_pane_id(herdr_bin)
        .map(|focused| focused == pane_id)
        .unwrap_or(false)
}

fn notification_decision_for_linux(
    pane_is_focused: bool,
    cached_sway_container_id: Option<String>,
    current_sway_container_id: Option<String>,
) -> NotificationDecision {
    if !pane_is_focused {
        return NotificationDecision::Send;
    }

    match (cached_sway_container_id, current_sway_container_id) {
        (Some(cached), Some(current)) if cached == current => NotificationDecision::Skip,
        _ => NotificationDecision::Send,
    }
}

fn herdr_bundle_id() -> Option<String> {
    bundle_id_from_app(activate_app().as_deref())
}

fn frontmost_bundle_id() -> Option<String> {
    frontmost_bundle_id_via_applescript()
}

fn frontmost_bundle_id_via_applescript() -> Option<String> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg("tell application \"System Events\" to return bundle identifier of first application process whose frontmost is true")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn bundle_id_from_app(app: Option<&str>) -> Option<String> {
    let app = app?;
    let escaped = app.replace('"', "\\\"");
    let script = format!("id of app \"{escaped}\"");

    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn focused_pane_id(herdr_bin: &str) -> Option<String> {
    let output = Command::new(herdr_bin)
        .arg("agent")
        .arg("list")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json = String::from_utf8(output.stdout).ok()?;
    focused_pane_id_from_agent_list_json(&json).ok().flatten()
}

fn focused_pane_id_from_agent_list_json(json: &str) -> Result<Option<String>, String> {
    let envelope: AgentListEnvelope =
        serde_json::from_str(json).map_err(|err| format!("invalid agent list json: {err}"))?;

    Ok(envelope.result.and_then(|result| {
        result.agents.into_iter().find_map(|agent| {
            agent
                .focused
                .then_some(agent.pane_id)
                .flatten()
                .map(|pane_id| pane_id.trim().to_string())
                .filter(|pane_id| !pane_id.is_empty())
        })
    }))
}

fn current_sway_container_id() -> Option<String> {
    let output = host_command("swaymsg")
        .arg("-t")
        .arg("get_tree")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    focused_sway_container_id_from_value(&json)
}

fn focused_sway_container_id_from_value(value: &serde_json::Value) -> Option<String> {
    if value
        .get("focused")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return value.get("id").and_then(|id| match id {
            serde_json::Value::Number(number) => Some(number.to_string()),
            serde_json::Value::String(string) => Some(string.clone()),
            _ => None,
        });
    }

    for key in ["nodes", "floating_nodes"] {
        let Some(nodes) = value.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };

        for node in nodes {
            if let Some(id) = focused_sway_container_id_from_value(node) {
                return Some(id);
            }
        }
    }

    None
}

fn configured_app_is_frontmost(
    herdr_bundle_id: Option<String>,
    frontmost_bundle_id: Option<String>,
) -> bool {
    matches!((herdr_bundle_id, frontmost_bundle_id), (Some(herdr), Some(frontmost)) if herdr == frontmost)
}

fn notification_decision_from_focus_and_bundles(
    pane_is_focused: bool,
    herdr_bundle_id: Option<String>,
    frontmost_bundle_id: Option<String>,
) -> NotificationDecision {
    if !pane_is_focused {
        return NotificationDecision::Send;
    }

    match (herdr_bundle_id, frontmost_bundle_id) {
        (Some(herdr), Some(frontmost)) if herdr == frontmost => NotificationDecision::Skip,
        (Some(_), Some(_)) => NotificationDecision::SendWithVisibilityMonitor,
        _ => NotificationDecision::Send,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_focused_pane_from_agent_list_json() {
        let json = r#"{
            "id": "cli:agent:list",
            "result": {
                "agents": [
                    {"agent": "codex", "focused": false, "pane_id": "w1:p1"},
                    {"agent": "kimi", "focused": true, "pane_id": "w1:p2"}
                ]
            }
        }"#;

        assert_eq!(
            focused_pane_id_from_agent_list_json(json).unwrap(),
            Some("w1:p2".to_string())
        );
    }

    #[test]
    fn decides_linux_notifications_from_sway_container_visibility() {
        assert_eq!(
            notification_decision_for_linux(false, Some("11".to_string()), Some("11".to_string())),
            NotificationDecision::Send
        );
        assert_eq!(
            notification_decision_for_linux(true, Some("11".to_string()), Some("11".to_string())),
            NotificationDecision::Skip
        );
        assert_eq!(
            notification_decision_for_linux(true, Some("11".to_string()), Some("22".to_string())),
            NotificationDecision::Send
        );
        assert_eq!(
            notification_decision_for_linux(true, Some("11".to_string()), None),
            NotificationDecision::Send
        );
    }

    #[test]
    fn decides_when_to_skip_or_monitor_notifications() {
        assert_eq!(
            notification_decision_from_focus_and_bundles(
                true,
                Some("com.example.Herdr".to_string()),
                Some("com.example.Herdr".to_string())
            ),
            NotificationDecision::Skip
        );
        assert_eq!(
            notification_decision_from_focus_and_bundles(
                true,
                Some("com.example.Herdr".to_string()),
                Some("com.apple.Terminal".to_string())
            ),
            NotificationDecision::SendWithVisibilityMonitor
        );
        assert_eq!(
            notification_decision_from_focus_and_bundles(
                true,
                None,
                Some("com.example.Herdr".to_string())
            ),
            NotificationDecision::Send
        );
        assert_eq!(
            notification_decision_from_focus_and_bundles(
                true,
                Some("com.example.Herdr".to_string()),
                None
            ),
            NotificationDecision::Send
        );
        assert_eq!(
            notification_decision_from_focus_and_bundles(
                false,
                Some("com.example.Herdr".to_string()),
                Some("com.example.Herdr".to_string())
            ),
            NotificationDecision::Send
        );
    }

    #[test]
    fn confirms_configured_app_is_frontmost_only_for_matching_bundle_ids() {
        assert!(configured_app_is_frontmost(
            Some("com.example.Herdr".to_string()),
            Some("com.example.Herdr".to_string())
        ));
        assert!(!configured_app_is_frontmost(
            Some("com.example.Herdr".to_string()),
            Some("com.apple.Terminal".to_string())
        ));
        assert!(!configured_app_is_frontmost(
            None,
            Some("com.example.Herdr".to_string())
        ));
    }

    #[test]
    fn finds_focused_sway_container_id_from_tree() {
        let json = serde_json::json!({
            "id": 1,
            "focused": false,
            "nodes": [
                {
                    "id": 2,
                    "focused": false,
                    "nodes": [
                        {"id": 42, "focused": true, "nodes": []}
                    ]
                }
            ],
            "floating_nodes": []
        });

        assert_eq!(
            focused_sway_container_id_from_value(&json),
            Some("42".to_string())
        );
    }

    #[test]
    fn builds_context_body_from_cwd_and_terminal_title() {
        let json = r#"{
            "result": {
                "agents": [
                    {
                        "agent": "cursor",
                        "focused": false,
                        "pane_id": "w13:p1",
                        "cwd": "/home/sungsik/projects/real-fake-peripheral",
                        "terminal_title": "Hello There",
                        "terminal_title_stripped": "Hello There"
                    }
                ]
            }
        }"#;

        assert_eq!(
            agent_context_body_from_agent_list_json("w13:p1", json, None),
            Some("real-fake-peripheral · Hello There".to_string())
        );
    }

    #[test]
    fn prefers_workspace_label_over_cwd_basename() {
        let json = r#"{
            "result": {
                "agents": [
                    {
                        "agent": "cursor",
                        "focused": false,
                        "pane_id": "w1K:p2",
                        "cwd": "/home/sungsik/.config",
                        "workspace_id": "w1K",
                        "terminal_title": "Hello",
                        "terminal_title_stripped": "Hello"
                    }
                ]
            }
        }"#;

        assert_eq!(
            agent_context_body_from_agent_list_json("w1K:p2", json, Some("project: dotfiles")),
            Some("dotfiles · Hello".to_string())
        );
    }

    #[test]
    fn strips_navigator_style_workspace_labels() {
        assert_eq!(
            workspace_display_name("project: ree-drive").as_deref(),
            Some("ree-drive")
        );
        assert_eq!(workspace_display_name("dir: vifm").as_deref(), Some("vifm"));
        assert_eq!(workspace_display_name("~").as_deref(), Some("~"));
        assert_eq!(
            workspace_display_name("project:ree-drive").as_deref(),
            Some("project:ree-drive")
        );
    }

    #[test]
    fn parses_workspace_label_from_workspace_get_json() {
        let json = r#"{
            "id": "cli:workspace:get",
            "result": {
                "type": "workspace_info",
                "workspace": {
                    "label": "project: ask",
                    "workspace_id": "w1J"
                }
            }
        }"#;

        assert_eq!(
            workspace_label_from_workspace_get_json(json).as_deref(),
            Some("project: ask")
        );
    }

    #[test]
    fn prefers_answer_preview_over_session_title() {
        assert_eq!(
            compose_notification_body(
                Some("dotfiles"),
                Some("Latest answer first line."),
                Some("Session From First Question")
            ),
            Some("dotfiles · Latest answer first line.".to_string())
        );
    }

    #[test]
    fn falls_back_to_session_title_without_answer_preview() {
        assert_eq!(
            compose_notification_body(Some("dotfiles"), None, Some("Session From First Question")),
            Some("dotfiles · Session From First Question".to_string())
        );
    }

    #[test]
    fn truncates_notification_body_to_120_chars() {
        let long = "x".repeat(200);
        let body = compose_notification_body(Some("proj"), Some(&long), None).unwrap();
        assert_eq!(body.chars().count(), 120);
        assert!(body.ends_with("..."));
        assert!(body.starts_with("proj · "));
    }

    #[test]
    fn builds_context_body_from_cwd_only() {
        let json = r#"{
            "result": {
                "agents": [
                    {
                        "focused": false,
                        "pane_id": "w1:p1",
                        "cwd": "/tmp/project"
                    }
                ]
            }
        }"#;

        assert_eq!(
            agent_context_body_from_agent_list_json("w1:p1", json, None),
            Some("project".to_string())
        );
    }

    #[test]
    fn returns_none_when_pane_missing_from_agent_list() {
        let json = r#"{
            "result": {
                "agents": [
                    {"focused": false, "pane_id": "w1:p1", "cwd": "/tmp/project"}
                ]
            }
        }"#;

        assert_eq!(
            agent_context_body_from_agent_list_json("w1:p9", json, None),
            None
        );
    }
}
