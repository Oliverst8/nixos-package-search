use crate::search::Channel;
use crate::types::PackageHit;
use std::io;
use std::process::{Command, Stdio};

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

/// Launches `hit` in a fresh, ephemeral `nix shell` and detaches it from this
/// process (via `setsid`) so the package keeps running after the popup exits.
pub fn launch(channel: Channel, hit: &PackageHit) -> io::Result<()> {
    let flake_ref = format!("{}#{}", channel.flake_ref(), hit.attr_name);
    let program = hit.runnable();

    Command::new("setsid")
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(())
}
