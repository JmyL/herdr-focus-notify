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
        return find_executable("notify-send", notify_send_candidate_paths())
            .or_else(|| host_command_available("notify-send").then_some("notify-send".to_string()))
            .ok_or_else(|| {
                "no notify-send notifier found; install libnotify or set HERDR_FOCUS_NOTIFY_NOTIFIER"
                    .to_string()
            });
    }

    find_executable("alerter", alerter_candidate_paths()).ok_or_else(|| {
        "no alerter notifier found; install alerter with `brew install vjeantet/tap/alerter` or set HERDR_FOCUS_NOTIFY_NOTIFIER".to_string()
    })
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

    let closer = find_executable("swaync-client", swaync_candidate_paths())
        .unwrap_or_else(|| "swaync-client".to_string());
    let mut command = host_command(&closer);
    command.arg("--close").arg(notification_id);

    match command.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(io::Error::other(format!(
            "notification removal exited with {status}"
        ))),
        Err(err) => Err(err),
    }
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

fn notify_send_candidate_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/bin/notify-send"),
        PathBuf::from("/usr/local/bin/notify-send"),
    ]
}

fn swaync_candidate_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/bin/swaync-client"),
        PathBuf::from("/usr/local/bin/swaync-client"),
    ]
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
}
