# nixos-package-search
**100% AI GENERATED USE AT OWN RISK**

A search-as-you-type popup for nixpkgs. Type a package name, see its
description/license/homepage/etc. as you hover or move through the results,
and hit Enter to run it from a throwaway `nix-shell` — no need to have it
installed. Ctrl+Enter instead drops you into a terminal sitting in that
shell, for CLI tools you want a prompt with. It opens on your recent
launches, so re-running something you reached for yesterday is Enter.

```
Running google-chrome would normally mean installing it first. This tool
does the equivalent of:

    NIXPKGS_ALLOW_UNFREE=1 nix-shell -p google-chrome \
      --run "exec google-chrome-stable"            # Enter

    ghostty -e nix-shell -p ripgrep \
      --run "exec zsh"                           # Ctrl+Enter

for whatever you selected, spawned detached so it survives the popup closing.
```

## Building & running

Everything needed to build (Rust toolchain, pkg-config, Wayland/X11/GL dev
libs) comes from the flake — you don't need anything installed globally
besides Nix with flakes enabled.

```console
$ nix run .                 # build + launch
$ nix build .                # build only -> ./result/bin/nixos-package-search
$ nix develop -c cargo run   # dev loop (fast incremental rebuilds)
```

## Usage

- **It opens on your launch history** — the packages you last ran, newest
  first, drawn as the same rows a search draws, each with its application
  icon and a pill saying whether it was last launched normally (`▶ run`) or
  in a terminal (`⌨ terminal`). That's only a record of what you did last time, not a
  constraint: Enter and Ctrl+Enter both work on a history row exactly as
  they do on a search result. Clearing the search box brings the list back.
  History isn't searchable — typing searches nixpkgs, as it always did.
- Start typing — results update ~150ms after you stop (debounced).
- **Hover** a result (or use ↑/↓) to see its full description, license,
  maintainers, homepage, provided binaries, and nixpkgs source position in
  the right-hand panel.
- **Enter** or double-click runs the hovered/selected package's main
  program (there's also a **▶ Run** button in the detail panel). This
  shows a loading screen (with the live tail of `nix`'s own output as the
  status line) instead of closing immediately: launching keeps running in
  the background either way, but the loading screen sticks around long
  enough to catch a fast failure (typo'd attribute, no `mainProgram` guess,
  no network) and show it as an error instead of just silently vanishing.
  For an already-cached package it'll flash by quickly; for something being
  fetched/built for the first time it gives up watching (and closes) after
  a few seconds, trusting the detached process to keep going on its own.
  Esc during loading cancels the launch outright. The exact command it's
  about to run is shown at the bottom of the detail panel before you launch.
- **Ctrl+Enter** (or Shift+Enter, or the **⌨ Terminal** button) opens a
  terminal emulator sitting *inside* that package's `nix-shell` instead —
  which is what you want for anything with a CLI, where one fire-and-forget
  run of a single binary isn't the point: you get a prompt with the package
  (and everything it brings with it) on `$PATH`, for as long as you keep the
  terminal open. You land in **zsh** (falling back to `$SHELL`, then bash),
  not in `nix-shell`'s own bash, so your prompt, aliases and completions are
  there too. There's no loading screen for this one, because the
  terminal itself is showing you nix's output; the popup just gets out of
  the way. If the launch fails, the terminal stays open with the error on
  screen rather than blinking out of existence.

  Which terminal gets used: `$TERMINAL` if you've set it, otherwise the
  first one actually installed out of ghostty, kitty, foot, alacritty,
  wezterm, konsole, gnome-terminal, xfce4-terminal, terminator, urxvt, st,
  xterm. Set `$TERMINAL` if that guess picks the wrong one.
- **Tab** switches between *Packages* search (name + description) and
  *Programs* search (search by the binary a package provides — e.g. typing
  `rg` finds `ripgrep`).
- **Ctrl+Tab** switches the channel between `unstable` (default) and the
  current stable release. This affects *search results only* — launching
  always uses your system's own nixpkgs (see below), so a package you find
  on one channel but don't have on the other will report that when you try
  to run it.
- **Esc**, or clicking away from the window, closes it.

## How it works

Search queries hit the same Elasticsearch backend that powers
[search.nixos.org](https://search.nixos.org) — the site ships its (read-only,
intentionally public) query credentials to every browser tab, and
[`nix-search-cli`](https://github.com/peterldowns/nix-search-cli) talks to
the same endpoint. This gets instant, richly-indexed results (descriptions,
licenses, maintainers, `mainProgram`, ...) without needing to evaluate
nixpkgs locally.

Launching runs:

```sh
NIXPKGS_ALLOW_UNFREE=1 setsid nix-shell -p <attr> --run "exec <mainProgram>"
```

and terminal mode runs, detached the same way:

```sh
<terminal> -e /bin/sh -c '<script>' nixos-package-search <attr> <shell>
```

where `<script>` does `nix-shell -p "$1" --run true` first and only then
`exec nix-shell -p "$1" --run "exec $2"`. Entering the package environment
via `--run "exec zsh"` is what gets you a zsh instead of the bash
`nix-shell` would otherwise drop you in — same environment, different
shell, and `exec` so no bash is left sitting underneath it. The first pass
is what does the fetching/building,
so a failure there (unknown attribute, no network, a build error) is
something the script can *see* and pause on — otherwise the terminal window
would vanish a fraction of a second after appearing, taking the error with
it. Interactive `nix-shell` exits with the status of whatever you last ran
in it, so its own exit code can't be used for that. The second invocation
re-evaluates against a warm cache and costs about half a second. The script
is passed as a single argument with the attribute as `$1`, so no package
name is ever re-parsed by a shell, and every terminal's `-e`-ish convention
sees one tidy argv.

This deliberately uses **legacy `nix-shell -p` against your system's
`<nixpkgs>`**, not `nix shell github:NixOS/nixpkgs/nixos-<channel>#<attr>`.
The flake form has to fetch, lock and evaluate an entire nixpkgs tarball for
the channel before it can even look at the package, which costs seconds on
every single launch; the local nixpkgs is already on disk and evaluates in
well under a second. The trade-off is that the channel picker only selects
which search index gets queried — in practice the local nixpkgs is the one
whose packages are already in your store anyway.

`mainProgram` comes straight from the package's metadata (falling back to
the package name), so the right binary gets run even when it doesn't match
the attribute name — e.g. `google-chrome` → `google-chrome-stable`.
`setsid` detaches the process so it keeps running after the popup exits.
`NIXPKGS_ALLOW_UNFREE=1` is there so unfree packages (Chrome, Steam, ...)
actually launch instead of failing with a license-refusal error; unlike the
`nix shell` flake form, plain `nix-shell` honors it without also needing
`--impure`.
In app mode stdout/stderr are redirected to a temp file (not a pipe or
`/dev/null`) so the app can tail it for the loading screen's status line
without risking a `SIGPIPE` in the launched program if the popup exits
while it's still writing.

### Launch history

Every launch that actually got somewhere is recorded in
`$XDG_STATE_HOME/nixos-package-search/history.json` (so
`~/.local/state/nixos-package-search/history.json` by default) — state
rather than `~/.config` or `~/.cache`, which is exactly the case the XDG
base directory spec gives history as an example of. The file is written to
a temp file and renamed into place, since the popup is routinely killed
moments after a launch; a half-written or unreadable file just means an
empty history rather than a failure, as does a missing `$HOME`.

Each entry stores the *whole* search hit, not just the attribute name, so
the history list renders identical rows to a search (description, version,
license, outputs, homepage) and the detail panel works on them — all
without the one screen that must appear instantly depending on the network.
Entries are deduplicated by attribute name (most recent wins, and carries
the mode you last used), and the list is capped at 50.

### History icons

Since index schema 52 each package document carries
`package_desktop_icons` — a `{ icon name: "<md5>.<png|svg>" }` map of files
search.nixos.org serves under `/icons/`, which its own frontend renders the
same way. That map is the only reason icons are possible here at all: a
package's icon otherwise lives inside a store path that doesn't exist until
you've built the package.

History entries store the whole index document, so the icon reference comes
along for free. The file itself is fetched once and cached in
`$XDG_CACHE_HOME/nixos-package-search/icons/` (`~/.cache/...`) — cache
rather than state, since every one of them can be downloaded again. Fetching
and decoding happen on a worker thread, so the history list is on screen
before any of it starts; measured against the previous build, icons cost
nothing at startup (84 ms vs 89 ms median to window-on-screen).

Roughly 57% of the icons in the index are PNG and 43% SVG, so both are
decoded: PNG through `image` (already in the dependency tree via
eframe/arboard, with exactly the `png` feature, so it costs no new crates)
and SVG through `resvg` built **without** its text/font features — 11 new
crates instead of 30, on the grounds that an application icon is shapes
rather than text.

A package with no icon is the normal case for CLI tools, which have no
desktop entry: those get their initial in a faint chip, so the column stays
aligned instead of leaving a hole. History entries recorded before schema 52
have no icon reference either and look the same until you launch them again.

A launch is only recorded once it's got somewhere: in app mode that means
after the child has survived its first moments, so a typo'd attribute or a
missing `mainProgram` that fails on the spot doesn't end up at the top of
the list next time. Terminal mode records at spawn, since the popup hands
off to the terminal and stops watching.

### Search ranking: ported, not reconstructed

`src/search.rs` is a port of `packagesRanking` and `encodeRequestBody` from
search.nixos.org's own frontend — [`frontend/src/Search/Query.elm`] in
NixOS/nixos-search — clause for clause, field for field, boost for boost, in
the order that file builds them. Not a reconstruction from a captured
request: the site's ranking is, in its own source's words, "the champion of
an evolutionary search over the clause structure, the fields and the boosts",
so no single number in it can be reasoned about on its own, and the previous
hand-tuned approximation here drifted badly from the real results.

[`frontend/src/Search/Query.elm`]: https://github.com/NixOS/nixos-search/blob/main/frontend/src/Search/Query.elm

What that buys, concretely: the query is one `dis_max` over a phrase match on
the descriptions (so a search that names no package still matches), a fuzzy
pass on the *last* word, and each word as `*word*` against the attribute
name — plus `should` clauses for the words glued back into an attribute name
(`google chrome` → a `term` on `google-chrome` boosted ×127, which is what
puts it first), prefix matches on name and pname, a `cross_fields` pass over
everything *but* the last word, two `rank_feature` popularity reads of
`package_repology_repos`, a `package_dep_count` signal, and the query read
as a program path (`python.http.server`). A final rescore over the top 100
divides by attribute-name length.

Two things fall out of the port for free:

- **`-word` excludes.** `python -wrapper` drops anything whose attribute name
  contains "wrapper", in each of its dash/underscore spellings. That's the
  site's own syntax, peeled off before the ranking ever sees the words.
- **Dash/underscore spellings.** `home_manager` also searches `home-manager`.

`cargo test` checks the generated request against the real site's captured
request body byte for byte (`query_body_matches_the_real_site_request`), so
"tidying up" a boost fails the build rather than silently changing results.
The `--ignored` tests then check the *results*: "google chrome" returns
`google-chrome`, `python314Packages.google`, `python313Packages.google`,
`chromium` — the same four, in that order, that the real site returns.

The site's `aggs` block is deliberately not sent: it feeds the facet sidebar,
which this app doesn't have, and it changes neither the hits nor their order.

The index name carries a schema version (`latest-<version>-nixos-<channel>`,
currently **52**). When search.nixos.org bumps it, every search here starts
failing with `index_not_found_exception` — check the site's network tab for
the index it queries and update `SCHEMA_VERSION` in `src/search.rs`.

## Hyprland integration

This is a plain undecorated floating window (`app_id=nixos-package-search`),
not a wlr-layer-shell overlay, so getting proper "popup" placement/behavior
needs a couple of Hyprland rules in your config:

```ini
windowrulev2 = float, class:^(nixos-package-search)$
windowrulev2 = center, class:^(nixos-package-search)$
windowrulev2 = size 1100 620, class:^(nixos-package-search)$
windowrulev2 = stayfocused, class:^(nixos-package-search)$

bind = $mod, D, exec, nix run --refresh /path/to/nixos-package-search
```

(swap `$mod, D` for whatever's free — e.g. many setups use this slot for
`rofi`/`wofi`, so pick a different key if you keep those around too).

## Other ways to search nixpkgs

This tool trades a network dependency for speed and rich metadata. Depending
on what you need:

| Tool | Good for | Trade-off |
|---|---|---|
| **This tool / search.nixos.org / `nix-search-cli`** | Instant, rich results (description, license, maintainers) as you type | Needs network; the index is a snapshot of a channel, while launching uses your local nixpkgs |
| `nix search nixpkgs <regex>` | Searching *your* local nixpkgs flake input exactly | Slow on first run (builds an eval cache); no long description in output |
| `nix-env -qaP --description '<regex>'` | Pre-flakes, channel-based search | Legacy; requires nix-channels set up |
| `nix-locate` (from [nix-index](https://github.com/nix-community/nix-index)) | "Which package has file/binary X" from a local index | Needs a one-time local database build (~GBs) |
| [nixhub.io](https://www.nixhub.io) | Finding a *specific historical version* of a package across nixpkgs revisions | Web only; used by tools like devbox |
| search.nixos.org's *Flakes* tab / [FlakeHub](https://flakehub.com) | Searching community flakes, not nixpkgs packages | Different corpus entirely |
| [`comma`](https://github.com/nix-community/comma) (`, <cmd>`) | Running a one-off command without installing it, from the shell | Uses a local nix-index database, not live search |

## Project layout

- `src/search.rs` — Elasticsearch query building + HTTP client, channel list
- `src/types.rs` — response deserialization
- `src/history.rs` — launch history: XDG state file, load/record/save
- `src/icons.rs` — history icons: index icon lookup, XDG cache, PNG/SVG decode
- `src/launch.rs` — builds and spawns the `nix-shell` launch commands
  (app mode and terminal mode), plus terminal-emulator detection
- `src/app.rs` — the egui UI: search box, results list, detail pane, keybindings
- `src/main.rs` — window setup (borderless, fixed size, `app_id` for window rules)
- `flake.nix` — devShell + package build (`rustPlatform.buildRustPackage`)

Run the (network-hitting, hence `#[ignore]`d) search tests with:

```console
$ nix develop -c cargo test -- --ignored --nocapture
```
