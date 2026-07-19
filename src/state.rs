use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use crate::util::notification_group_id;

pub(crate) fn plugin_state_dir() -> PathBuf {
    env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("herdr-focus-notify"))
}

pub(crate) fn mark_notification_cleared(pane_id: &str) -> io::Result<()> {
    let state_dir = plugin_state_dir();
    fs::create_dir_all(&state_dir)?;
    fs::write(cleared_notification_marker_path(pane_id), [])
}

pub(crate) fn reset_notification_clearance(pane_id: &str) -> io::Result<()> {
    match fs::remove_file(cleared_notification_marker_path(pane_id)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub(crate) fn cleared_notification_marker_path(pane_id: &str) -> PathBuf {
    plugin_state_dir().join(format!("{}.cleared", notification_group_id(pane_id)))
}

pub(crate) fn cache_sway_container_id(pane_id: &str, con_id: &str) -> io::Result<()> {
    write_pane_state_file(sway_container_path(pane_id), con_id)
}

pub(crate) fn cached_sway_container_id(pane_id: &str) -> Option<String> {
    read_pane_state_file(sway_container_path(pane_id))
}

pub(crate) fn notification_id_path(pane_id: &str) -> PathBuf {
    plugin_state_dir()
        .join("linux-notifications")
        .join(notification_group_id(pane_id))
}

pub(crate) fn cached_notification_id(pane_id: &str) -> Option<String> {
    read_pane_state_file(notification_id_path(pane_id))
}

fn write_pane_state_file(path: PathBuf, value: &str) -> io::Result<()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, value)
}

fn sway_container_path(pane_id: &str) -> PathBuf {
    plugin_state_dir()
        .join("sway-containers")
        .join(notification_group_id(pane_id))
}

fn read_pane_state_file(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
