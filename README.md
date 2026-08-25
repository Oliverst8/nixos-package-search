# nixos-package-search

A search-as-you-type popup for nixpkgs. Type a package name, see its
description/license/homepage/etc. as you hover or move through the results,
hit Enter, and it launches the package in a throwaway `nix shell` — no need
to have it installed.

```
nix run github:NixOS/nixpkgs/nixos-unstable#google-chrome would normally
require installing google-chrome first. This tool does the equivalent of:

    nix shell github:NixOS/nixpkgs/nixos-unstable#google-chrome \
      --command google-chrome-stable

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

- Start typing — results update ~150ms after you stop (debounced).
- **Hover** a result (or use ↑/↓) to see its full description, license,
  maintainers, homepage, provided binaries, and nixpkgs source position in
  the right-hand panel.
- **Enter** or double-click launches the hovered/selected package. This
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
- **Tab** switches between *Packages* search (name + description) and
  *Programs* search (search by the binary a package provides — e.g. typing
  `rg` finds `ripgrep`).
- **Ctrl+Tab** switches the channel between `unstable` (default) and the
  current stable release. This affects both what shows up in results and
  which nixpkgs revision gets used to launch it.
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
setsid nix --extra-experimental-features "nix-command flakes" shell \
  github:NixOS/nixpkgs/nixos-<channel>#<attr> --command <mainProgram>
```

`mainProgram` comes straight from the package's metadata (falling back to
the package name), so the right binary gets run even when it doesn't match
the attribute name — e.g. `google-chrome` → `google-chrome-stable`.
`setsid` detaches the process so it keeps running after the popup exits.
stdout/stderr are redirected to a temp file (not a pipe or `/dev/null`) so
the app can tail it for the loading screen's status line without risking a
`SIGPIPE` in the launched program if the popup exits while it's still
writing.

The Elasticsearch index name is versioned
(`latest-<schema-version>-nixos-<channel>`); `SCHEMA_VERSION` in
`src/search.rs` may need bumping if search.nixos.org changes its schema and
searches start failing with `index_not_found_exception` — check the site's
page source for the current `elasticsearchMappingSchemaVersion`.

### A note on search ranking

`package_attr_name`/`package_pname` are indexed as ES `keyword` fields with
an edge-ngram sub-field, which is great for prefix search ("goog..." →
`google-chrome`) but structurally can't match a term in the *middle* of a
value ("chrome" alone won't ngram-match `google-chrome`, since the ngrams
only grow from the start of the string). `src/search.rs` adds explicit
heavily-boosted `wildcard` substring queries to cover that case, on top of
the prefix/exact/description matching. See the comments in `match_search`
for the full story, including why the query pins `"analyzer": "lowercase"`
on the prefix clause (without it, the field's own `edge_ngram` analyzer gets
reused to tokenize the *query* too, which shreds a word like "chrome" into
`ch`/`chr`/`chro`/... and floods results with noise).

## Hyprland integration

This is a plain undecorated floating window (`app_id=nixos-package-search`),
not a wlr-layer-shell overlay, so getting proper "popup" placement/behavior
needs a couple of Hyprland rules in your config:

```ini
windowrulev2 = float, class:^(nixos-package-search)$
windowrulev2 = center, class:^(nixos-package-search)$
windowrulev2 = size 780 480, class:^(nixos-package-search)$
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
| **This tool / search.nixos.org / `nix-search-cli`** | Instant, rich results (description, license, maintainers) as you type | Needs network; index is a snapshot, not your exact local nixpkgs checkout |
| `nix search nixpkgs <regex>` | Searching *your* local nixpkgs flake input exactly | Slow on first run (builds an eval cache); no long description in output |
| `nix-env -qaP --description '<regex>'` | Pre-flakes, channel-based search | Legacy; requires nix-channels set up |
| `nix-locate` (from [nix-index](https://github.com/nix-community/nix-index)) | "Which package has file/binary X" from a local index | Needs a one-time local database build (~GBs) |
| [nixhub.io](https://www.nixhub.io) | Finding a *specific historical version* of a package across nixpkgs revisions | Web only; used by tools like devbox |
| search.nixos.org's *Flakes* tab / [FlakeHub](https://flakehub.com) | Searching community flakes, not nixpkgs packages | Different corpus entirely |
| [`comma`](https://github.com/nix-community/comma) (`, <cmd>`) | Running a one-off command without installing it, from the shell | Uses a local nix-index database, not live search |

## Project layout

- `src/search.rs` — Elasticsearch query building + HTTP client, channel list
- `src/types.rs` — response deserialization
- `src/launch.rs` — builds and spawns the `nix shell` launch command
- `src/app.rs` — the egui UI: search box, results list, detail pane, keybindings
- `src/main.rs` — window setup (borderless, fixed size, `app_id` for window rules)
- `flake.nix` — devShell + package build (`rustPlatform.buildRustPackage`)

Run the (network-hitting, hence `#[ignore]`d) search tests with:

```console
$ nix develop -c cargo test -- --ignored --nocapture
```
