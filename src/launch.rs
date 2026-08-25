use crate::search::Channel;
use crate::types::PackageHit;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// The exact command that will be run to launch `hit`. Shown in the UI so the
/// user can see what's about to happen before they hit Enter.
pub fn command_preview(channel: Channel, hit: &PackageHit) -> String {
    format!(
        "nix shell {}#{} --command {}",
        channel.flake_ref(),
        hit.attr_name,
        hit.runnable()
    )
}

/// A launch in progress: the (detached) child process, plus the log file its
/// stdout/stderr were redirected to so the UI can tail nix's build/fetch
/// progress while showing the loading indicator.
pub struct Launching {
    pub child: Child,
    pub log_path: PathBuf,
}

/// Launches `hit` in a fresh, ephemeral `nix shell` and detaches it from this
/// process (via `setsid`) so the package keeps running after the popup exits.
///
/// stdout/stderr are redirected to a plain file rather than a pipe: a pipe's
/// read end would close the moment our process exits, and the launched
/// program writing to it afterwards would get killed by SIGPIPE. A file has
/// no such reader to lose — the child keeps its own handle to it — and we
/// can still tail its contents for progress while we're around to watch.
pub fn launch(channel: Channel, hit: &PackageHit) -> io::Result<Launching> {
    let flake_ref = format!("{}#{}", channel.flake_ref(), hit.attr_name);
    let program = hit.runnable();

    let log_path =
        std::env::temp_dir().join(format!("nixos-package-search-{}.log", std::process::id()));
    let stdout_log = File::create(&log_path)?;
    let stderr_log = stdout_log.try_clone()?;

    let child = Command::new("setsid")
        .arg("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "shell",
            &flake_ref,
            "--command",
            &program,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()?;

    Ok(Launching { child, log_path })
}
