use crate::types::PackageHit;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// How many launches we remember. The list is only ever shown on an empty
/// query (and isn't searchable), so it's a "what did I run lately" list, not
/// an archive — a screenful or two is the useful size.
const MAX_ENTRIES: usize = 50;

/// Which of the three ways a package was launched. Recorded per entry so the
/// history list can show how you ran something last time; it does *not*
/// constrain how you run it again — every binding still works on a history
/// row, same as on a search result.
///
/// `rename_all` keeps these as plain lowercase words in the file; an entry
/// written before a variant existed still reads back, since an unknown one
/// just fails that entry's parse and an unparseable file means an empty
/// history rather than an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Run,
    Terminal,
    TerminalRun,
}

impl LaunchMode {
    pub fn label(&self) -> &'static str {
        match self {
            LaunchMode::Run => "▶ run",
            LaunchMode::Terminal => "⌨ terminal",
            LaunchMode::TerminalRun => "⌨▶ terminal run",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// The whole search hit, not just its name: the history list renders the
    /// exact same rows (description, version, license, outputs, homepage) as
    /// a search does, and the detail panel has to work on a history row too.
    /// Re-querying Elasticsearch for metadata we already had would make the
    /// one screen that shows up instantly on launch depend on the network.
    pub hit: PackageHit,
    pub mode: LaunchMode,
    /// Unix seconds. Not displayed; kept so the file says when things
    /// happened on its own, without relying on list order to carry it.
    #[serde(default)]
    pub at: u64,
}

/// `$XDG_STATE_HOME/nixos-package-search/history.json`, i.e.
/// `~/.local/state/nixos-package-search/history.json` by default.
///
/// State, not config (`~/.config`) and not cache (`~/.cache`), per the XDG
/// base directory spec: history is exactly its example of state — data that
/// should persist between restarts but isn't important enough for
/// `~/.local/share`, and that you'd be annoyed (not indifferent) to lose to
/// a cache clear.
fn path() -> Option<PathBuf> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state_home.join("nixos-package-search").join("history.json"))
}

/// Most-recent-first. Any problem reading it (missing file on a first run,
/// a half-written or hand-edited file, a schema that changed under us) just
/// means an empty history: this runs on the startup path of a popup whose
/// whole point is appearing instantly, so there's nothing here worth
/// failing, blocking or complaining to the user about.
pub fn load() -> Vec<Entry> {
    let Some(path) = path() else {
        return Vec::new();
    };
    let Ok(raw) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Records `hit` as the most recent launch, in memory and on disk.
///
/// Deduplicated by attribute name, so running the same thing repeatedly
/// keeps one (updated) row at the top rather than filling the list with it.
/// That also means an entry's mode is always the way you ran it *last*.
pub fn record(history: &mut Vec<Entry>, hit: &PackageHit, mode: LaunchMode) {
    history.retain(|e| e.hit.attr_name != hit.attr_name);
    history.insert(
        0,
        Entry {
            hit: hit.clone(),
            mode,
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        },
    );
    history.truncate(MAX_ENTRIES);
    save(history);
}

/// Written to a temporary file and renamed into place, so an interrupted
/// write (the popup is routinely killed moments after a launch) can't leave
/// a truncated file behind — the rename is atomic, so the next run sees
/// either the old history or the new one. Failures are ignored for the same
/// reason as in [`load`]: losing a history entry is not worth interrupting
/// a launch over.
fn save(history: &[Entry]) {
    let Some(path) = path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(history) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, json).is_ok() && fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}
