use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::is_linux_notifier;
use crate::executable::{executable_in_path, find_executable, home_dir, is_executable_file};
use crate::util::{notification_group_id, shell_quote};

pub(crate) fn resolve_notifier_bin() -> Result<String, String> {
    if let Some(configured) = crate::config::config_var("HERDR_FOCUS_NOTIFY_NOTIFIER") {
        if is_executable_file(Path::new(&configured))
            || executable_in_path(&configured).is_some()
            || (is_linux_notifier() && host_command_available(&configured))
        {
            return Ok(configured);
        }

        return Err(format!(
            "configured notifier is not executable: {configured}"
        ));
    }

    if is_linux_notifier() {
        // Prefer the D-Bus helper: libnotify's notify-send often advertises
        // "actions" support but still sends an empty actions array, so click
        // never invokes ActionInvoked under swaync.
        return resolve_linux_notify_wait_script()
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or_else(|| {
                "no scripts/linux-notify-wait.py found next to the plugin; reinstall the plugin or set HERDR_FOCUS_NOTIFY_NOTIFIER"
                    .to_string()
            });
    }

    find_executable("alerter", alerter_candidate_paths()).ok_or_else(|| {
        "no alerter notifier found; install alerter with `brew install vjeantet/tap/alerter` or set HERDR_FOCUS_NOTIFY_NOTIFIER".to_string()
    })
}

/// Path to the bundled Linux helper that sends actionable notifications over D-Bus.
pub(crate) fn resolve_linux_notify_wait_script() -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(root) = env::var("HERDR_PLUGIN_ROOT") {
        candidates.push(PathBuf::from(root).join("scripts/linux-notify-wait.py"));
    }

    if let Ok(exe) = env::current_exe() {
        // target/release/herdr-focus-notify -> plugin root
        if let Some(root) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            candidates.push(root.join("scripts/linux-notify-wait.py"));
        }
    }

    candidates.push(PathBuf::from("scripts/linux-notify-wait.py"));

    candidates.into_iter().find(|path| path.is_file())
}

/// True when `notifier_bin` is the bundled D-Bus helper (not notify-send/alerter).
pub(crate) fn is_linux_dbus_notify_helper(notifier_bin: &str) -> bool {
    Path::new(notifier_bin)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("linux-notify-wait.py")
}

pub(crate) fn send_notification(script_path: &Path, foreground: bool) -> io::Result<()> {
    if foreground {
        run_script_foreground(script_path)
    } else {
        spawn_detached_script(script_path)
    }
}

pub(crate) fn remove_notification(pane_id: &str, notifier_bin: &str) -> io::Result<()> {
    if is_linux_notifier() {
        return remove_linux_notification(pane_id);
    }

    let group = notification_group_id(pane_id);

    match Command::new(notifier_bin)
        .arg("--remove")
        .arg(group)
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(io::Error::other(format!(
            "notification removal exited with {status}"
        ))),
        Err(err) => Err(err),
    }
}

fn remove_linux_notification(pane_id: &str) -> io::Result<()> {
    let Some(notification_id) = crate::state::cached_notification_id(pane_id) else {
        return Ok(());
    };

    // Prefer swaync (Ubuntu), then mako (Fedora). Either may be absent.
    if let Some(closer) = find_executable("swaync-client", swaync_candidate_paths()) {
        let mut command = host_command(&closer);
        command.arg("--close").arg(&notification_id);
        return match command.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(io::Error::other(format!(
                "notification removal exited with {status}"
            ))),
            Err(err) => Err(err),
        };
    }

    if let Some(closer) = find_executable("makoctl", makoctl_candidate_paths()) {
        let mut command = host_command(&closer);
        command.arg("dismiss").arg("-n").arg(&notification_id);
        return match command.status() {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(io::Error::other(format!(
                "notification removal exited with {status}"
            ))),
            Err(err) => Err(err),
        };
    }

    Err(io::Error::other(
        "no notification closer found; install swaync-client or makoctl",
    ))
}

fn alerter_candidate_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/opt/homebrew/bin/alerter"),
        PathBuf::from("/usr/local/bin/alerter"),
    ];
    if let Some(home) = home_dir() {
        paths.push(home.join(".local/bin/alerter"));
    }
    paths
}

fn swaync_candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home_dir() {
        paths.push(home.join(".local/bin/swaync-client"));
    }
    paths.push(PathBuf::from("/usr/bin/swaync-client"));
    paths.push(PathBuf::from("/usr/local/bin/swaync-client"));
    paths
}

fn makoctl_candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home_dir() {
        paths.push(home.join(".local/bin/makoctl"));
    }
    paths.push(PathBuf::from("/usr/bin/makoctl"));
    paths.push(PathBuf::from("/usr/local/bin/makoctl"));
    paths
}

pub(crate) fn host_command(command: &str) -> Command {
    if cfg!(target_os = "linux") && executable_in_path(command).is_none() {
        let mut host = Command::new("flatpak-spawn");
        host.arg("--host").arg(command);
        host
    } else {
        Command::new(command)
    }
}

fn host_command_available(command: &str) -> bool {
    cfg!(target_os = "linux")
        && executable_in_path("flatpak-spawn").is_some()
        && Command::new("flatpak-spawn")
            .arg("--host")
            .arg("command")
            .arg("-v")
            .arg(command)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
}

fn run_script_foreground(script_path: &Path) -> io::Result<()> {
    match Command::new("sh").arg(script_path).status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(io::Error::other(format!(
            "notification script exited with {status}"
        ))),
        Err(err) => Err(err),
    }
}

fn spawn_detached_script(script_path: &Path) -> io::Result<()> {
    let script_str = script_path.to_string_lossy();
    let cmd = format!(
        "nohup sh {} >/dev/null 2>&1 &",
        shell_quote(script_str.as_ref())
    );

    Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alerter_candidates_include_common_homebrew_paths() {
        let paths = alerter_candidate_paths();

        assert!(paths.contains(&PathBuf::from("/opt/homebrew/bin/alerter")));
        assert!(paths.contains(&PathBuf::from("/usr/local/bin/alerter")));
    }

    #[test]
    fn linux_closer_candidates_include_swaync_and_mako() {
        let swaync = swaync_candidate_paths();
        let mako = makoctl_candidate_paths();

        assert!(swaync.contains(&PathBuf::from("/usr/bin/swaync-client")));
        assert!(mako.contains(&PathBuf::from("/usr/bin/makoctl")));
    }
}
