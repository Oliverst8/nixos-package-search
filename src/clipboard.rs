use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Puts `text` on the clipboard in a way that survives this process.
///
/// That caveat is the whole reason this isn't a one-liner. On Wayland the
/// clipboard holds no content of its own: it holds a *reference to the
/// client that offered it*, and the compositor asks that client for the
/// bytes when something pastes. A client that has exited can't answer, so
/// its clipboard entry disappears with it — and copying here is followed,
/// almost by definition, by switching to the window you want to paste into,
/// which makes this popup lose focus and close. Copied with egui's own
/// clipboard and nothing else, the name would be gone before you could use
/// it, unless a clipboard manager happened to be running to take ownership.
///
/// So the copy is handed to `wl-copy`, which forks a tiny process that
/// keeps serving the selection after we're gone — exactly what a clipboard
/// manager would otherwise do. `xclip`/`xsel` do the same on X11. Only if
/// none of them is available does this fall back to egui's own clipboard,
/// which lasts exactly as long as the window does.
///
/// It is deliberately one *or* the other, not both. Two clients can't own
/// the selection at once: whichever sets it second takes ownership, and
/// `wl-copy` exits as soon as it loses the selection, since its whole job
/// is to serve that one copy. Setting egui's clipboard as well looks
/// harmless and quietly undoes the thing it's there for —
/// `Context::copy_text` queues the copy until the end of the frame, so it
/// lands *after* the helper, kills it, and leaves nothing behind when the
/// popup closes a moment later. Measured, not reasoned about: with both,
/// the name was on the clipboard while the window was open and gone the
/// instant it closed.
/// Returns whether a helper took the text; `false` means egui's own
/// clipboard was used instead.
///
/// Fast enough to call on the way to closing the window: spawning the
/// helper and handing it the text costs **0.7 ms**, measured. What it
/// deliberately does *not* do is wait for the helper to finish taking the
/// selection, which costs another ~135 ms of pure waiting while it starts
/// up and talks to the compositor.
///
/// That wait turns out to be unnecessary, which is worth stating plainly
/// because the opposite seems obvious: the helper is `setsid`-detached with
/// the text already in its stdin buffer, so nothing it has left to do
/// depends on this process still existing. Tested directly — a parent that
/// exits immediately after writing still ends up with the right clipboard.
/// The only consequence is that for ~135 ms after the popup disappears the
/// clipboard still holds whatever it held before, which is far less time
/// than it takes to switch windows and paste.
pub fn copy(ctx: &egui::Context, text: &str) -> bool {
    match helper() {
        Some((path, args)) => {
            hand_off_to_helper(&path, args, text);
            true
        }
        // No helper installed, so fall back to egui's own clipboard, which
        // lives as long as the window does (and no longer — see above).
        // Decided here rather than after a failed handoff so that the
        // fallback is set during this frame, while there is still a frame
        // left before the window closes.
        None => {
            ctx.copy_text(text.to_string());
            false
        }
    }
}

/// The first helper that's actually installed.
fn helper() -> Option<(PathBuf, &'static [&'static str])> {
    HELPERS
        .iter()
        .find_map(|(program, args)| crate::launch::resolve_binary(program).map(|p| (p, *args)))
}

/// The helpers that can outlive us, in the order they're tried. Each takes
/// the text on stdin, which keeps a name that happens to start with `-` from
/// being read as an option.
const HELPERS: &[(&str, &[&str])] = &[
    // Wayland. `--trim-newline` because we write the text with no trailing
    // newline and wl-copy would otherwise add one.
    ("wl-copy", &["--trim-newline"]),
    // X11. Both of these fork and keep serving the selection too.
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

/// Spawns the helper and gives it the text, and that's all.
///
fn hand_off_to_helper(program: &Path, args: &[&str], text: &str) {
    // `setsid` so the helper isn't in our process group: it has to outlive
    // the popup, and a group-wide signal on the way out would take the
    // clipboard with it.
    let mut child = match Command::new("setsid")
        .arg(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };

    // Dropping the handle closes the pipe, which is the helper's cue that
    // the text is complete — so this has to happen before we return, and it
    // is the last thing this process has to do with the clipboard.
    let written = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(text.as_bytes()).is_ok())
        .unwrap_or(false);
    if !written {
        let _ = child.kill();
        let _ = child.wait();
        return;
    }

    // Not waited on, so it stays a zombie for the handful of milliseconds
    // until this process exits, at which point init inherits and reaps it.
    // Waiting here is what used to cost 135 ms.
}
