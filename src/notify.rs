use std::process::{Command, Stdio};

/// Posts a desktop notification, if `notify-send` is installed and something
/// is listening for one. Fire and forget: a missing notification daemon,
/// a missing `notify-send`, a D-Bus that doesn't answer — none of it is
/// worth reporting to someone who just pressed a key and expected a window
/// to go away.
///
/// This exists because the popup closes immediately after a copy, which
/// leaves nowhere in the app itself to say what was copied. `setsid` for the
/// same reason as the clipboard helper: the notification is sent over D-Bus
/// by a process that has to be able to finish the call after we're gone.
pub fn send(summary: &str) {
    let Some(path) = crate::launch::resolve_binary("notify-send") else {
        return;
    };
    let _ = Command::new("setsid")
        .arg(path)
        .args([
            "--app-name=nixos-package-search",
            "--icon=edit-copy",
            "--expire-time=2000",
            // Ends option parsing, so a summary is never read as a flag.
            "--",
        ])
        .arg(summary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
