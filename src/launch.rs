use crate::types::PackageHit;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Launching goes through legacy `nix-shell -p <attr>` against whatever
/// `<nixpkgs>` the system's `NIX_PATH` points at, rather than
/// `nix shell github:NixOS/nixpkgs/<branch>#<attr>`. The flake form has to
/// fetch (and evaluate, and lock) a whole nixpkgs tarball for the channel
/// before it can even look at the package, which costs seconds on every
/// single launch; the system's nixpkgs is already on disk and evaluates in
/// well under a second. The trade-off is that the channel picker now only
/// affects *search results* — launches always use the local nixpkgs, which
/// in practice is the one whose packages are already in the store anyway.
const NIX_SHELL: &str = "nix-shell";

/// So packages like google-chrome (license = unfree) don't get refused
/// outright — matches what people expect from "search and launch anything",
/// not just free-licensed packages. Unlike the `nix shell` flake form, plain
/// `nix-shell` honors this without also needing `--impure`.
const UNFREE: (&str, &str) = ("NIXPKGS_ALLOW_UNFREE", "1");

/// The exact command that will be run to launch `hit` in the background.
/// Shown in the UI so the user can see what's about to happen first.
pub fn command_preview(hit: &PackageHit) -> String {
    format!(
        "{}=1 {NIX_SHELL} -p {} --run \"{}\"",
        UNFREE.0,
        hit.attr_name,
        run_arg(hit)
    )
}

/// The equivalent preview for terminal mode, or `None` when no terminal
/// emulator could be found (in which case the UI says so instead of
/// offering a command that can't run).
pub fn terminal_command_preview(hit: &PackageHit, autorun: bool) -> Option<String> {
    let term = find_terminal()?;
    let shell = terminal_shell();
    let inner = if autorun {
        format!("{}; exec {shell}", hit.runnable())
    } else {
        format!("exec {shell}")
    };
    let mut parts = vec![term.display_name()];
    parts.extend(term.exec_prefix.iter().map(|a| a.to_string()));
    parts.extend([
        NIX_SHELL.to_string(),
        "-p".to_string(),
        hit.attr_name.clone(),
        "--run".to_string(),
        format!("\"{inner}\""),
    ]);
    Some(parts.join(" "))
}

/// A launch in progress: the (detached) child process, plus the log file its
/// stdout/stderr were redirected to so the UI can tail nix's build/fetch
/// progress while showing the loading indicator.
pub struct Launching {
    pub child: Child,
    pub log_path: PathBuf,
}

/// Launches `hit`'s main program in a throwaway `nix-shell` and detaches it
/// from this process (via `setsid`) so the package keeps running after the
/// popup exits.
///
/// stdout/stderr are redirected to a plain file rather than a pipe: a pipe's
/// read end would close the moment our process exits, and the launched
/// program writing to it afterwards would get killed by SIGPIPE. A file has
/// no such reader to lose — the child keeps its own handle to it — and we
/// can still tail its contents for progress while we're around to watch.
pub fn launch(hit: &PackageHit) -> io::Result<Launching> {
    let log_path =
        std::env::temp_dir().join(format!("nixos-package-search-{}.log", std::process::id()));
    let stdout_log = File::create(&log_path)?;
    let stderr_log = stdout_log.try_clone()?;

    let child = Command::new("setsid")
        .args([NIX_SHELL, "-p", &hit.attr_name, "--run", &run_arg(hit)])
        .env(UNFREE.0, UNFREE.1)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_log))
        .stderr(Stdio::from(stderr_log))
        .spawn()?;

    Ok(Launching { child, log_path })
}

/// Opens a terminal emulator sitting inside `hit`'s `nix-shell` — the point
/// being CLI packages, where you want a prompt with the tool on `$PATH`
/// (and its `--help`, its subcommands, repeated invocations) rather than one
/// fire-and-forget run of a single binary.
///
/// With `autorun`, the package's own program runs first and the prompt is
/// what you're left with when it exits — so `htop` starts straight away, and
/// quitting it drops you into the shell it was running in rather than
/// closing the window. That's the difference between the two terminal
/// bindings: both end at a prompt, one of them has already run the thing.
///
/// Detached like [`launch`], so the terminal outlives the popup. Nothing is
/// tailed here: the terminal is already showing nix's output, and the script
/// below keeps a failed launch on screen instead of letting the window blink
/// out of existence with the error in it.
pub fn launch_in_terminal(hit: &PackageHit, autorun: bool) -> io::Result<Child> {
    let term = find_terminal().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no terminal emulator found on $PATH — set $TERMINAL to the one you use",
        )
    })?;

    let mut cmd = Command::new("setsid");
    cmd.arg(&term.path)
        .args(term.exec_prefix)
        // The script is passed as a single argument and takes the attribute
        // as "$1", so nothing about the package name is ever re-parsed by a
        // shell, and every terminal's "-e"-ish convention sees one tidy
        // argv regardless of how it splits (or doesn't split) its arguments.
        .args(["/bin/sh", "-c", TERMINAL_SCRIPT, "nixos-package-search"])
        .arg(&hit.attr_name)
        .arg(sh_quote(&terminal_shell()))
        // `$3`: the program to run on the way in, empty for a bare prompt.
        .arg(if autorun {
            sh_quote(&hit.runnable())
        } else {
            String::new()
        })
        .env(UNFREE.0, UNFREE.1)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // A popup bound to a compositor keybind inherits whatever working
    // directory the compositor happens to have (often `/`), which is a
    // confusing place to drop someone's shell. Home is the sane default.
    if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }

    cmd.spawn()
}

/// Run inside the spawned terminal, with the nixpkgs attribute as `$1`, the
/// shell to land in as `$2`, and optionally a program to run first as `$3`.
/// `$2` and `$3` are already shell-quoted, so `--run` can interpolate them
/// straight into the command it hands to bash.
///
/// The build/fetch is done by an explicit `--run true` pass first, rather
/// than just dropping straight into `nix-shell -p "$1"`: that way a failure
/// (unknown attribute, no network, a build error) is a *distinguishable*
/// event we can pause on, instead of the terminal window vanishing a
/// fraction of a second after it appeared with the error scrolled off with
/// it. Interactive `nix-shell` exits with the status of whatever the user
/// last ran, so its exit code can't tell us the same thing. The second
/// invocation then re-evaluates against a warm cache and costs ~0.5s.
const TERMINAL_SCRIPT: &str = r#"
nix-shell -p "$1" --run true || {
    st=$?
    printf '\nnix-shell -p %s failed (exit %s). Press Enter to close.' "$1" "$st"
    read -r _
    exit "$st"
}
if [ -n "$3" ]; then
    exec nix-shell -p "$1" --run "$3; exec $2"
fi
exec nix-shell -p "$1" --run "exec $2"
"#;

/// The shell terminal mode lands you in. `nix-shell`'s own interactive shell
/// is always bash, which isn't much use if your prompt, aliases, completions
/// and history all live in zsh — so the package environment is entered with
/// `--run "exec zsh"` instead, which keeps the environment and swaps the
/// shell. Falls back to `$SHELL`, then to leaving bash alone, on a machine
/// without zsh.
fn terminal_shell() -> String {
    if resolve_binary("zsh").is_some() {
        return "zsh".to_string();
    }
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "bash".to_string())
}

/// What `--run` gets: the package's best-guess binary. `exec` so the wrapping
/// bash doesn't hang around as a parent of the actual program, which also
/// makes the child's exit status the program's own.
fn run_arg(hit: &PackageHit) -> String {
    format!("exec {}", sh_quote(&hit.runnable()))
}

/// Terminals differ in how a command is handed to them: some take it as the
/// trailing arguments directly (kitty, foot), most want `-e` first, and a
/// couple have their own spelling (`wezterm start --`, `gnome-terminal --`).
struct TerminalSpec {
    bin: &'static str,
    exec_prefix: &'static [&'static str],
}

/// Ordered by preference, and only as a fallback: `$TERMINAL` wins when set.
/// The order only ever matters on a machine with several installed, and
/// guessing wrong there is invisible until a terminal you don't use pops
/// up — so `$TERMINAL` is the real answer and this is a best effort.
const TERMINALS: &[TerminalSpec] = &[
    TerminalSpec {
        bin: "ghostty",
        exec_prefix: &["-e"],
    },
    TerminalSpec {
        bin: "kitty",
        exec_prefix: &[],
    },
    TerminalSpec {
        bin: "foot",
        exec_prefix: &[],
    },
    TerminalSpec {
        bin: "alacritty",
        exec_prefix: &["-e"],
    },
    TerminalSpec {
        bin: "wezterm",
        exec_prefix: &["start", "--"],
    },
    TerminalSpec {
        bin: "konsole",
        exec_prefix: &["-e"],
    },
    TerminalSpec {
        bin: "gnome-terminal",
        exec_prefix: &["--"],
    },
    TerminalSpec {
        bin: "xfce4-terminal",
        // `-e` takes a single string it then splits itself; `-x` takes the
        // rest of argv as-is, which is what we're handing it.
        exec_prefix: &["-x"],
    },
    TerminalSpec {
        bin: "terminator",
        exec_prefix: &["-x"],
    },
    TerminalSpec {
        bin: "urxvt",
        exec_prefix: &["-e"],
    },
    TerminalSpec {
        bin: "st",
        exec_prefix: &["-e"],
    },
    TerminalSpec {
        bin: "xterm",
        exec_prefix: &["-e"],
    },
];

pub struct FoundTerminal {
    path: PathBuf,
    exec_prefix: &'static [&'static str],
}

impl FoundTerminal {
    /// The bare binary name for the command preview — the resolved path is a
    /// store path a mile long, which says nothing useful in the UI.
    fn display_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

/// `$TERMINAL` first (the conventional way to say "this is my terminal"),
/// falling back to the first of [`TERMINALS`] that's actually installed. A
/// `$TERMINAL` we recognize gets its own argument convention; anything else
/// gets `-e`, which is what the overwhelming majority accept.
pub fn find_terminal() -> Option<FoundTerminal> {
    if let Some(configured) = std::env::var_os("TERMINAL") {
        let configured = configured.to_string_lossy().into_owned();
        if !configured.trim().is_empty() {
            if let Some(path) = resolve_binary(&configured) {
                let known = TERMINALS
                    .iter()
                    .find(|t| path.file_name().is_some_and(|n| n == t.bin));
                return Some(FoundTerminal {
                    path,
                    exec_prefix: known.map_or(&["-e"], |t| t.exec_prefix),
                });
            }
        }
    }

    TERMINALS.iter().find_map(|t| {
        resolve_binary(t.bin).map(|path| FoundTerminal {
            path,
            exec_prefix: t.exec_prefix,
        })
    })
}

/// `which`, basically: an explicit path is taken as-is, a bare name is
/// looked up in `$PATH`. Checked for the executable bit so a same-named
/// data file or directory on `$PATH` doesn't shadow a real terminal.
fn resolve_binary(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let executable = |p: &PathBuf| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };

    if name.contains('/') {
        let path = PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(executable)
}

/// Quotes a value for the one place a shell does the parsing: the `--run`
/// string, which `nix-shell` hands to bash. Package metadata is tame in
/// practice, but a `mainProgram` is still free-form text from nixpkgs, and
/// quoting it is cheaper than trusting it. Plain binary names (i.e. all of
/// them, nearly always) are left alone so the command preview in the UI
/// reads like something you'd actually type.
fn sh_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.+/=:@%".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}
