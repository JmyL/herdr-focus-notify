use serde::Deserialize;
use std::process::Command;

use crate::config::{activate_app, is_linux_notifier};
use crate::notification::FocusNotification;
use crate::notifier::host_command;
use crate::state::cache_sway_container_id;
use crate::util::sanitize_group_id;

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
    pane_id: Option<String>,
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
        return if pane_is_focused(pane_id, herdr_bin) {
            NotificationDecision::Skip
        } else {
            NotificationDecision::Send
        };
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

fn pane_is_focused(pane_id: &str, herdr_bin: &str) -> bool {
    focused_pane_id(herdr_bin)
        .map(|focused| focused == pane_id)
        .unwrap_or(false)
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
}
