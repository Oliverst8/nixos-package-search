use crate::clipboard;
use crate::history::{self, LaunchMode};
use crate::icons;
use crate::launch;
use crate::notify;
use crate::search::{self, SearchMode, CHANNELS};
use crate::types::PackageHit;
use eframe::egui;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What a press of Enter means, given the modifiers held with it.
#[derive(Clone, Copy)]
enum EnterAction {
    /// Run the package's main program, detached, with no terminal.
    Run,
    /// Open a terminal sitting at a prompt inside the package's shell.
    Terminal,
    /// Both: a terminal with the program already running in it.
    TerminalRun,
}

/// How many icons are fetched at once. Enough to hide the latency of a
/// screenful, few enough to stay polite to search.nixos.org.
const ICON_WORKERS: usize = 4;

const DEBOUNCE: Duration = Duration::from_millis(150);
/// How long focus must stay lost before we auto-close. Guards against
/// transient focus flicker from the compositor (or from IME/virtual-keyboard
/// input) being mistaken for the user actually clicking away.
const UNFOCUS_CLOSE_DELAY: Duration = Duration::from_millis(250);
/// How long the loading screen stays up after hitting Enter before we give up
/// watching and close anyway, trusting the (detached) process to keep going
/// in the background. Long enough to catch a fast failure (bad attr name, no
/// network) and to show real progress on a package that needs fetching;
/// launching something already cached closes right away since `nix shell`
/// execs into it almost immediately.
const LAUNCH_GRACE_PERIOD: Duration = Duration::from_secs(3);

type SearchResult = Result<Vec<PackageHit>, String>;

struct LaunchState {
    /// The whole hit, not just its name: history is recorded from here once
    /// the launch has actually got somewhere (see `poll_launching`).
    hit: PackageHit,
    child: Child,
    log_path: PathBuf,
    log_offset: u64,
    status_line: String,
    started: Instant,
}

enum PollOutcome {
    Running,
    Finished(ExitStatus),
    Errored,
}

pub struct App {
    client: reqwest::blocking::Client,

    query: String,
    mode: SearchMode,
    channel_idx: usize,

    results: Vec<PackageHit>,
    selected: usize,
    hovered: Option<usize>,
    move_selection_into_view: bool,

    generation: u64,
    dirty_since: Option<Instant>,
    in_flight: bool,
    tx: Sender<(u64, SearchResult)>,
    rx: Receiver<(u64, SearchResult)>,

    status: Option<String>,
    error: Option<String>,

    gained_focus: bool,
    unfocused_since: Option<Instant>,
    should_close: bool,

    launching: Option<LaunchState>,

    /// Most-recent-first launch history, shown in place of results while the
    /// query is empty. `showing_history` says whether `results` currently
    /// holds those entries' hits (so a row can be drawn with the mode it was
    /// launched in) rather than search results.
    history: Vec<history::Entry>,
    showing_history: bool,

    /// Decoded icons by index file name, `None` once we've tried and found
    /// there's nothing to draw (a CLI tool with no desktop entry, a 404, an
    /// entry recorded before the index carried icons).
    ///
    /// `icon_queue` is the work still to do, shared with the workers that
    /// drain it, and `icon_requested` remembers every file ever put on it so
    /// a package that keeps reappearing as you type is only fetched once.
    /// Loading never happens on the way to a frame: results and history are
    /// on screen first, icons land in later frames.
    icon_textures: HashMap<String, Option<egui::TextureHandle>>,
    icon_queue: Arc<Mutex<VecDeque<String>>>,
    icon_requested: HashSet<String>,
    icon_workers: Arc<AtomicUsize>,
    /// Whether anything in `results` has an icon at all. A search like
    /// "firefox" matches nothing with a desktop entry, and a column of
    /// identical placeholder initials down the side of it is worse than no
    /// column: the space is only taken when there's something to put in it.
    rows_have_icons: bool,
    icon_tx: Sender<(String, Option<icons::Rgba>)>,
    icon_rx: Receiver<(String, Option<icons::Rgba>)>,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let (icon_tx, icon_rx) = channel();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("nixos-package-search/0.1")
            .build()
            .expect("failed to build http client");

        Self {
            client,
            query: String::new(),
            mode: SearchMode::Packages,
            channel_idx: 0, // unstable by default
            results: Vec::new(),
            selected: 0,
            hovered: None,
            move_selection_into_view: false,
            generation: 0,
            dirty_since: None,
            in_flight: false,
            tx,
            rx,
            status: None,
            error: None,
            gained_focus: false,
            unfocused_since: None,
            should_close: false,

            launching: None,

            // Populated below: an empty query shows history, and the query
            // starts empty, so this is what's on screen the instant the
            // popup appears.
            history: Vec::new(),
            showing_history: false,
            icon_textures: HashMap::new(),
            icon_queue: Arc::new(Mutex::new(VecDeque::new())),
            icon_requested: HashSet::new(),
            icon_workers: Arc::new(AtomicUsize::new(0)),
            rows_have_icons: false,
            icon_tx,
            icon_rx,
        }
        .with_history(history::load())
    }

    fn with_history(mut self, history: Vec<history::Entry>) -> Self {
        self.history = history;
        self.show_history();
        self
    }

    /// Puts the history's hits into `results` so that everything downstream —
    /// row rendering, hover, ↑/↓, the detail panel, Enter and Ctrl+Enter —
    /// works on history rows exactly as it does on search results, with no
    /// parallel code path to keep in step. `showing_history` keeps the two
    /// distinguishable for the bits that should differ (the mode pill, the
    /// list header, the result count).
    fn show_history(&mut self) {
        self.results = self.history.iter().map(|e| e.hit.clone()).collect();
        self.showing_history = !self.results.is_empty();
        self.selected = 0;
        self.hovered = None;

        self.queue_row_icons();
    }

    /// Queues the icons for whatever is in `results` now — history or search
    /// hits, they're the same rows.
    ///
    /// Anything still pending from the previous result set is dropped first
    /// (and un-requested, so it can come back if those rows do): with the
    /// workers taking the queue in order, a stale set sitting in front would
    /// delay the icons for the rows actually on screen by its whole length.
    fn queue_row_icons(&mut self) {
        // Known from the index data, not from what has loaded, so the column
        // doesn't appear or vanish as icons arrive.
        self.rows_have_icons = self
            .results
            .iter()
            .any(|hit| icons::icon_file(hit).is_some());

        let mut queue = self.icon_queue.lock().expect("icon queue poisoned");
        for stale in queue.drain(..) {
            self.icon_requested.remove(&stale);
        }
        for hit in &self.results {
            if let Some(file) = icons::icon_file(hit) {
                if !self.icon_textures.contains_key(file)
                    && self.icon_requested.insert(file.to_string())
                {
                    queue.push_back(file.to_string());
                }
            }
        }
    }

    /// Tops the worker pool up to [`ICON_WORKERS`] while there's work.
    ///
    /// A few workers rather than one: an icon is a small HTTP GET that is
    /// almost entirely waiting, so a cold screenful fetched one after the
    /// other is seconds of icons visibly trickling in (measured 2.6s for ten
    /// of them, against 0.6s at four at a time). A pool rather than threads
    /// per search: typing fires a search per pause, and each one would
    /// otherwise start its own batch.
    fn spawn_icon_loader(&mut self, ctx: &egui::Context) {
        loop {
            if self
                .icon_queue
                .lock()
                .expect("icon queue poisoned")
                .is_empty()
            {
                return;
            }
            // Claim a slot before spawning, so two frames in quick
            // succession can't both decide there's room for the same one.
            let claimed =
                self.icon_workers
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |active| {
                        (active < ICON_WORKERS).then_some(active + 1)
                    });
            if claimed.is_err() {
                return;
            }

            let queue = Arc::clone(&self.icon_queue);
            let workers = Arc::clone(&self.icon_workers);
            let client = self.client.clone();
            let tx = self.icon_tx.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                loop {
                    let next = queue.lock().expect("icon queue poisoned").pop_front();
                    let Some(file) = next else {
                        workers.fetch_sub(1, Ordering::SeqCst);
                        return;
                    };
                    let loaded = icons::load(&client, &file);
                    if tx.send((file, loaded)).is_err() {
                        workers.fetch_sub(1, Ordering::SeqCst);
                        return;
                    }
                    // The popup is idle while this runs; without this the
                    // icon would only appear on the next keystroke or mouse
                    // move.
                    ctx.request_repaint();
                }
            });
        }
    }

    /// Uploads decoded icons as textures. Keyed by file name, so the same
    /// icon shared by two packages is uploaded once.
    fn take_loaded_icons(&mut self, ctx: &egui::Context) {
        while let Ok((file, loaded)) = self.icon_rx.try_recv() {
            let texture = loaded.map(|rgba| {
                ctx.load_texture(
                    format!("icon:{file}"),
                    egui::ColorImage::from_rgba_unmultiplied(rgba.size, &rgba.pixels),
                    egui::TextureOptions::LINEAR,
                )
            });
            self.icon_textures.insert(file, texture);
        }
    }

    /// The icon square at the start of a history row. Always takes up the
    /// same space whether or not there's an icon to put in it, so the text
    /// beside it lines up down the list; a package with no icon (every CLI
    /// tool, since an icon comes from a desktop entry) gets its initial in a
    /// faint chip instead of a hole in the layout.
    fn draw_icon_slot(&self, ui: &mut egui::Ui, hit: &PackageHit) {
        const SLOT: f32 = 40.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(SLOT, SLOT), egui::Sense::hover());

        let texture = icons::icon_file(hit)
            .and_then(|file| self.icon_textures.get(file))
            .and_then(|texture| texture.as_ref());

        if let Some(texture) = texture {
            // Fit inside the square rather than filling it: these are a mix
            // of square PNGs and whatever aspect the SVG happened to have.
            let size = texture.size_vec2();
            let scale = (SLOT / size.x).min(SLOT / size.y).min(1.0);
            let fitted = egui::Rect::from_center_size(rect.center(), size * scale);
            egui::Image::new(texture).paint_at(ui, fitted);
        } else {
            let initial = hit
                .pname
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            ui.painter().rect_filled(
                rect,
                10.0,
                elevated(ui.visuals(), ui.visuals().panel_fill, 18),
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                initial,
                egui::FontId::proportional(18.0),
                ui.visuals().weak_text_color(),
            );
        }
        ui.add_space(10.0);
    }

    /// The mode a row was last launched in, for history rows only. Indices
    /// line up because `results` is built from `history` in order, and both
    /// are only ever rebuilt together in `show_history`.
    fn history_mode(&self, idx: usize) -> Option<LaunchMode> {
        if !self.showing_history {
            return None;
        }
        self.history.get(idx).map(|e| e.mode)
    }

    fn channel(&self) -> search::Channel {
        CHANNELS[self.channel_idx]
    }

    fn mark_dirty(&mut self) {
        self.dirty_since = Some(Instant::now());
    }

    fn cycle_channel(&mut self) {
        self.channel_idx = (self.channel_idx + 1) % CHANNELS.len();
        self.mark_dirty();
    }

    fn cycle_mode(&mut self) {
        self.mode = match self.mode {
            SearchMode::Packages => SearchMode::Programs,
            SearchMode::Programs => SearchMode::Packages,
        };
        self.mark_dirty();
    }

    fn spawn_search(&mut self) {
        if self.query.trim().is_empty() {
            // Clearing the search box goes back to where you started rather
            // than to an empty list.
            self.show_history();
            self.in_flight = false;
            return;
        }
        self.generation += 1;
        let req_gen = self.generation;
        let client = self.client.clone();
        let channel = self.channel();
        let mode = self.mode;
        let query = self.query.clone();
        let tx = self.tx.clone();
        self.in_flight = true;
        self.error = None;
        thread::spawn(move || {
            let result = search::run_search(&client, channel, mode, &query);
            let _ = tx.send((req_gen, result));
        });
    }

    fn active_index(&self) -> Option<usize> {
        self.hovered.or_else(|| {
            if self.results.is_empty() {
                None
            } else {
                Some(self.selected)
            }
        })
    }

    fn launch_selected(&mut self) {
        if self.launching.is_some() {
            return;
        }
        let Some(idx) = self.active_index() else {
            return;
        };
        let Some(hit) = self.results.get(idx).cloned() else {
            return;
        };
        self.error = None;
        match launch::launch(&hit) {
            Ok(l) => {
                self.launching = Some(LaunchState {
                    hit,
                    child: l.child,
                    log_path: l.log_path,
                    log_offset: 0,
                    status_line: String::new(),
                    started: Instant::now(),
                });
            }
            Err(e) => {
                self.error = Some(format!("failed to launch {}: {e}", hit.pname));
            }
        }
    }

    /// Copies the attribute name of whatever row is under the cursor (or
    /// selected) — `ripgrep`, `python313Packages.requests` — which is the
    /// string you would put in a configuration, not the pname or the binary.
    ///
    /// Copying closes the popup, like launching does: the name is on the
    /// clipboard and the next thing you do with it is in another window.
    /// What was copied is said in a desktop notification rather than in the
    /// app, since by then there's no app left to say it in.
    ///
    /// Nothing is waited for on the way out: handing the text to the
    /// clipboard helper costs 0.7 ms and the helper finishes on its own
    /// afterwards, so the window goes as soon as the frame does.
    fn copy_selected_name(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.active_index() else {
            return;
        };
        let Some(hit) = self.results.get(idx) else {
            return;
        };
        let name = hit.attr_name.clone();
        notify::send(&format!("Copied {name}"));
        clipboard::copy(ctx, &name);
        self.should_close = true;
    }

    /// Terminal mode: hands the package to a terminal emulator sitting in its
    /// `nix-shell`, for CLI tools you want a prompt with rather than a single
    /// run of one binary. Unlike [`Self::launch_selected`] there's no loading
    /// screen to keep around — the terminal window is itself the progress
    /// display, and it reports its own failures (see `launch::launch_in_terminal`)
    /// — so the popup gets out of the way immediately.
    fn open_terminal_selected(&mut self, autorun: bool) {
        if self.launching.is_some() {
            return;
        }
        let Some(idx) = self.active_index() else {
            return;
        };
        let Some(hit) = self.results.get(idx).cloned() else {
            return;
        };
        self.error = None;
        match launch::launch_in_terminal(&hit, autorun) {
            Ok(_) => {
                let mode = if autorun {
                    LaunchMode::TerminalRun
                } else {
                    LaunchMode::Terminal
                };
                history::record(&mut self.history, &hit, mode);
                self.should_close = true;
            }
            Err(e) => {
                self.error = Some(format!("failed to open a terminal for {}: {e}", hit.pname));
            }
        }
    }

    /// Cancels an in-progress launch: kills the (not-yet-detached-in-spirit)
    /// child and drops the loading screen. Only reachable while `launching`
    /// is `Some`, i.e. before we've let go of it after the grace period.
    fn cancel_launch(&mut self) {
        if let Some(mut state) = self.launching.take() {
            let _ = state.child.kill();
            let _ = std::fs::remove_file(&state.log_path);
        }
    }

    /// Tails the launch log and checks on the child. Returns whether a
    /// launch is still in progress and should keep the loading screen up.
    fn poll_launching(&mut self, ctx: &egui::Context) -> bool {
        let Some(state) = self.launching.as_mut() else {
            return false;
        };

        if let Ok(mut f) = File::open(&state.log_path) {
            if f.seek(SeekFrom::Start(state.log_offset)).is_ok() {
                let mut buf = String::new();
                if f.read_to_string(&mut buf).unwrap_or(0) > 0 {
                    state.log_offset += buf.len() as u64;
                    if let Some(last) = buf.lines().rev().find(|l| !l.trim().is_empty()) {
                        state.status_line = last.trim().to_string();
                    }
                }
            }
        }

        let outcome = match state.child.try_wait() {
            Ok(Some(status)) => PollOutcome::Finished(status),
            Ok(None) => PollOutcome::Running,
            Err(_) => PollOutcome::Errored,
        };

        // History is recorded here rather than at spawn time so that a
        // launch that fails on the spot — typo'd attribute, no network,
        // nothing with that `mainProgram` — doesn't end up at the top of
        // the list you see next time. Every branch that closes the popup is
        // one where the package either ran or got far enough that we handed
        // it off to the background.
        let launched = match outcome {
            PollOutcome::Running => {
                if state.started.elapsed() >= LAUNCH_GRACE_PERIOD {
                    let hit = state.hit.clone();
                    let _ = std::fs::remove_file(&state.log_path);
                    self.launching = None;
                    self.should_close = true;
                    Some(hit)
                } else {
                    ctx.request_repaint_after(Duration::from_millis(50));
                    None
                }
            }
            PollOutcome::Finished(status) if status.success() => {
                let hit = state.hit.clone();
                let _ = std::fs::remove_file(&state.log_path);
                self.launching = None;
                self.should_close = true;
                Some(hit)
            }
            PollOutcome::Finished(status) => {
                let pname = state.hit.pname.clone();
                let tail = state.status_line.clone();
                let _ = std::fs::remove_file(&state.log_path);
                self.error = Some(if tail.is_empty() {
                    format!("{pname} exited ({status})")
                } else {
                    format!("{pname} exited ({status}): {tail}")
                });
                self.launching = None;
                None
            }
            PollOutcome::Errored => {
                let hit = state.hit.clone();
                let _ = std::fs::remove_file(&state.log_path);
                self.launching = None;
                self.should_close = true;
                Some(hit)
            }
        };
        if let Some(hit) = launched {
            history::record(&mut self.history, &hit, LaunchMode::Run);
        }

        self.launching.is_some()
    }

    fn draw_launching_overlay(&self, ui: &mut egui::Ui) {
        let Some(state) = &self.launching else {
            return;
        };
        ui.vertical_centered(|ui| {
            ui.add_space(150.0);
            ui.heading(format!("Launching {}...", state.hit.pname));
            ui.add_space(14.0);
            ui.add(
                egui::ProgressBar::new(1.0)
                    .animate(true)
                    .desired_width(420.0)
                    .desired_height(10.0),
            );
            ui.add_space(10.0);
            ui.weak(if state.status_line.is_empty() {
                "starting nix shell..."
            } else {
                state.status_line.as_str()
            });
            ui.add_space(24.0);
            ui.weak("Esc to cancel");
        });
    }

    fn draw_top_bar(&mut self, ui: &mut egui::Ui) {
        let available_width = ui.available_width();
        let search_frame = egui::Frame::default()
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(14.0)
            .inner_margin(egui::Margin::symmetric(16, 12));

        search_frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("🔍")
                        .size(20.0)
                        .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(8.0);
                let edit = egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Search nixpkgs...")
                    .desired_width(f32::INFINITY)
                    .font(egui::FontId::proportional(22.0))
                    .frame(egui::Frame::NONE);
                let resp = ui.add(edit);

                // Keep the field focused at all times (like Spotlight/Alfred)
                // so typing is never interrupted by results appearing or the
                // mouse drifting over a row — arrow-key list navigation
                // already works independently via global key polling below,
                // it doesn't need to steal focus to do its job.
                if !resp.has_focus() {
                    resp.request_focus();
                }
                if resp.changed() {
                    self.mark_dirty();
                }

                if !self.query.is_empty()
                    && ui
                        .add(egui::Button::new(egui::RichText::new("✕").weak()).frame(false))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                {
                    self.query.clear();
                    self.mark_dirty();
                }
            });
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            // A plain (non-wrapping) `horizontal` layout doesn't cap itself
            // to the panel's real width — nesting a `right_to_left` region
            // in it (for the result count below) then right-aligns against
            // that uncapped width instead of the actual visible edge, and
            // the count runs under the detail panel. Pin it to the width we
            // know is correct, measured before any of this row's own
            // content could throw it off.
            ui.set_max_width(available_width);
            for mode in [SearchMode::Packages, SearchMode::Programs] {
                if ui
                    .selectable_label(self.mode == mode, mode.label())
                    .clicked()
                {
                    self.mode = mode;
                    self.mark_dirty();
                }
            }

            ui.separator();

            for (idx, ch) in CHANNELS.iter().enumerate() {
                if ui
                    .selectable_label(self.channel_idx == idx, ch.label)
                    .clicked()
                {
                    self.channel_idx = idx;
                    self.mark_dirty();
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.in_flight {
                    ui.spinner();
                } else {
                    // "recent" vs "results" is the clearest signal that the
                    // list below isn't a search — the rows themselves look
                    // the same by design.
                    let count = if self.showing_history {
                        format!("{} recent", self.results.len())
                    } else {
                        format!("{} results", self.results.len())
                    };
                    ui.add(egui::Label::new(count).truncate());
                }
            });
        });
    }

    fn draw_results_list(&mut self, ui: &mut egui::Ui) {
        // A `ScrollArea` with only vertical scrolling enabled still sizes
        // its own width to whatever its content wants ("expand to fit"),
        // rather than clipping content to the space actually available —
        // so a single too-wide child can silently balloon the whole column
        // out past the results list and under the (always-visible) detail
        // panel next to it. Measuring the width *before* entering the
        // scroll area, then hard-capping to it inside, keeps every row
        // confined to the real available space no matter what it contains.
        let available_width = ui.available_width();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.set_max_width(available_width);
            if self.results.is_empty() {
                ui.add_space(12.0);
                let msg = if self.query.trim().is_empty() {
                    // Only reachable before anything has ever been launched;
                    // otherwise an empty query shows the history instead.
                    "Start typing to search nixpkgs..."
                } else if self.in_flight {
                    "Searching..."
                } else {
                    "No matches."
                };
                ui.weak(msg);
                return;
            }

            if self.showing_history {
                ui.add_space(2.0);
                ui.weak("Recently launched — start typing to search nixpkgs");
                ui.add_space(6.0);
            }

            let mut any_hovered = false;

            for idx in 0..self.results.len() {
                let is_selected = idx == self.selected;
                let is_hovered = self.hovered == Some(idx);
                // Resolved before the row's closures borrow anything else,
                // and `Some` only for history rows.
                let launched_as = self.history_mode(idx);
                let hit = &self.results[idx];

                let bg = if is_selected {
                    ui.visuals().selection.bg_fill.linear_multiply(0.55)
                } else if is_hovered {
                    elevated(ui.visuals(), ui.visuals().panel_fill, 16)
                } else {
                    egui::Color32::TRANSPARENT
                };

                let row_frame = egui::Frame::default()
                    .fill(bg)
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(10, 8));

                let row = row_frame.show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    // A `Frame`'s contents lay out top-down, so the icon
                    // needs an explicit left-to-right region to sit *beside*
                    // the text rather than above it.
                    ui.horizontal_top(|ui| {
                        if self.rows_have_icons {
                            self.draw_icon_slot(ui, &self.results[idx]);
                        }
                        ui.vertical(|ui| {
                            // Title is the nixpkgs attribute name (e.g.
                            // "firefox-esr-140-unwrapped"), same as
                            // search.nixos.org's result cards — not the pname,
                            // which is often shared by several attrs.
                            let mut title = egui::RichText::new(&hit.attr_name).size(16.0);
                            title = title.color(if is_selected {
                                ui.visuals().selection.stroke.color
                            } else {
                                ui.visuals().hyperlink_color
                            });
                            // `.truncate()` clips to however many pixels are
                            // actually available (ellipsis, single line) instead
                            // of wrapping — needed because a long attr name is
                            // one unbroken token with no space to wrap at, so
                            // without this it would just overflow past the row
                            // uncontained, same as the untruncated grid values
                            // did in the detail panel (see MAX_VALUE below).
                            ui.add(egui::Label::new(title).truncate());

                            if let Some(desc) = &hit.description {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(desc).size(13.0))
                                        .truncate(),
                                );
                            }

                            ui.add_space(3.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if let Some(mode) = launched_as {
                                    code_pill(ui, mode.label());
                                }
                                ui.weak("Name:");
                                code_pill(ui, &hit.pname);
                                ui.weak("Version:");
                                code_pill(
                                    ui,
                                    if hit.version.is_empty() {
                                        "-"
                                    } else {
                                        &hit.version
                                    },
                                );
                                if !hit.outputs.is_empty() {
                                    ui.weak("Outputs:");
                                    for o in &hit.outputs {
                                        code_pill(ui, o);
                                    }
                                }
                                if let Some(home) = hit.homepage.first() {
                                    ui.hyperlink_to("🌐 Homepage", home);
                                }
                            });

                            if !hit.licenses.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    ui.weak("License:");
                                    for l in &hit.licenses {
                                        code_pill(ui, l);
                                    }
                                });
                            }
                        });
                    });
                });

                let resp = row.response.interact(egui::Sense::click());
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

                if resp.hovered() {
                    self.hovered = Some(idx);
                    any_hovered = true;
                }
                if resp.clicked() {
                    self.selected = idx;
                    self.hovered = Some(idx);
                }
                if resp.double_clicked() {
                    self.selected = idx;
                    self.launch_selected();
                }
                if is_selected && self.move_selection_into_view {
                    resp.scroll_to_me(Some(egui::Align::Center));
                }

                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
            }
            if !any_hovered {
                self.hovered = None;
            }
            self.move_selection_into_view = false;
        });
    }

    fn draw_detail_panel(&mut self, ui: &mut egui::Ui) {
        let Some(idx) = self.active_index() else {
            ui.weak("Hover or select a package to see details here.");
            return;
        };
        let Some(hit) = self.results.get(idx).cloned() else {
            return;
        };
        // Which terminal (if any) we'd use is a `$PATH` lookup, so resolve
        // it once per frame up here rather than inside the button and the
        // preview separately.
        let terminal_cmd = launch::terminal_command_preview(&hit, false);
        let terminal_run_cmd = launch::terminal_command_preview(&hit, true);
        // The buttons below are drawn while `ui` holds a borrow of `self`'s
        // drawing closure; acting on them is deferred to the end of the
        // function, where calling back into `&mut self` is free of it.
        let mut run_clicked = false;
        let mut terminal_clicked = false;
        let mut terminal_run_clicked = false;

        ui.heading(&hit.pname);
        ui.label(format!("{}  ·  v{}", hit.attr_name, hit.version));
        ui.add_space(6.0);

        if let Some(desc) = &hit.description {
            ui.label(desc);
        }
        if let Some(long) = hit.long_description.as_ref().filter(|s| !s.is_empty()) {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(strip_html_tags(long)).weak());
        }

        // Grid columns auto-size to the widest value ever shown in them and
        // never shrink back down (state is cached across frames/selections
        // by the grid's id), and this panel's `ScrollArea` expands to fit
        // whatever width its content wants since only vertical scrolling is
        // enabled. Together, one long value (a source path, a long
        // maintainer list) would balloon the column — and therefore the
        // whole scroll area — well past the panel, visibly shifting/clipping
        // everything else in it. Truncating free-text values keeps the grid
        // (and the panel) at a sane, predictable width.
        const MAX_VALUE: usize = 42;

        ui.add_space(10.0);
        egui::Grid::new("detail_grid")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                if let Some(prog) = &hit.main_program {
                    ui.label("runs as");
                    ui.label(prog);
                    ui.end_row();
                }
                if !hit.licenses.is_empty() {
                    ui.label("license");
                    ui.label(truncate(&hit.licenses.join(", "), MAX_VALUE));
                    ui.end_row();
                }
                if !hit.outputs.is_empty() {
                    ui.label("outputs");
                    ui.label(truncate(&hit.outputs.join(", "), MAX_VALUE));
                    ui.end_row();
                }
                if !hit.maintainers.is_empty() {
                    ui.label("maintainers");
                    ui.label(truncate(&hit.maintainers.join(", "), MAX_VALUE));
                    ui.end_row();
                }
                if !hit.homepage.is_empty() {
                    ui.label("homepage");
                    ui.label(truncate(&hit.homepage.join(", "), MAX_VALUE));
                    ui.end_row();
                }
                if !hit.programs.is_empty() {
                    ui.label("provides");
                    ui.label(truncate(&hit.programs.join(", "), MAX_VALUE));
                    ui.end_row();
                }
                if let Some(pos) = &hit.position {
                    ui.label("defined at");
                    ui.label(truncate(pos, MAX_VALUE));
                    ui.end_row();
                }
                ui.label("platforms");
                ui.label(format!("{}", hit.platforms.len()));
                ui.end_row();
            });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);

        // Wrapped: three buttons don't fit the panel's width on one line.
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("▶  Run")
                .on_hover_text("Enter — run the package's main program")
                .clicked()
            {
                run_clicked = true;
            }
            if ui
                .add_enabled(terminal_cmd.is_some(), egui::Button::new("⌨  Terminal"))
                .on_hover_text("Ctrl+Enter — open a terminal inside this package's nix-shell")
                .clicked()
            {
                terminal_clicked = true;
            }
            if ui
                .add_enabled(
                    terminal_run_cmd.is_some(),
                    egui::Button::new("⌨▶  Terminal + run"),
                )
                .on_hover_text("Ctrl+Alt+Enter — a terminal with the program already running in it")
                .clicked()
            {
                terminal_run_clicked = true;
            }
        });

        ui.add_space(8.0);
        ui.weak("Enter runs:");
        command_preview(ui, &launch::command_preview(&hit));

        ui.add_space(6.0);
        ui.weak("Ctrl+Enter opens:");
        match &terminal_cmd {
            Some(cmd) => command_preview(ui, cmd),
            // Only reachable with no terminal emulator installed at all;
            // say so here rather than letting the button silently do
            // nothing (it's disabled above for the same reason).
            None => {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    "no terminal emulator found — set $TERMINAL",
                );
            }
        }

        if let Some(cmd) = &terminal_run_cmd {
            ui.add_space(6.0);
            ui.weak("Ctrl+Alt+Enter opens:");
            command_preview(ui, cmd);
        }

        if run_clicked {
            self.launch_selected();
        }
        if terminal_clicked {
            self.open_terminal_selected(false);
        }
        if terminal_run_clicked {
            self.open_terminal_selected(true);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.take_loaded_icons(ctx);
        self.spawn_icon_loader(ctx);
        while let Ok((req_gen, result)) = self.rx.try_recv() {
            if req_gen == self.generation {
                self.in_flight = false;
                match result {
                    Ok(hits) => {
                        self.results = hits;
                        self.showing_history = false;
                        self.selected = 0;
                        self.hovered = None;
                        self.queue_row_icons();
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        }

        if self.poll_launching(ctx) {
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.cancel_launch();
            }
            if self.should_close {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            egui::CentralPanel::default().show(ui, |ui| {
                self.draw_launching_overlay(ui);
            });
            return;
        }

        if let Some(since) = self.dirty_since {
            if since.elapsed() >= DEBOUNCE {
                self.dirty_since = None;
                self.spawn_search();
            } else {
                ctx.request_repaint_after(Duration::from_millis(30));
            }
        }
        if self.in_flight {
            ctx.request_repaint_after(Duration::from_millis(30));
        }

        // Ctrl+C doesn't arrive as a key at all: egui-winit turns the
        // platform's copy shortcut into a semantic `Event::Copy` before egui
        // ever sees a `Key::C`, so matching on the key finds nothing. The
        // event is taken out of the queue rather than merely observed,
        // because the search box holds focus at all times and would
        // otherwise handle the same event as "copy the selected query
        // text", overwriting the name we just put on the clipboard.
        let copy_pressed = ctx.input_mut(|i| {
            let found = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            i.events.retain(|e| !matches!(e, egui::Event::Copy));
            found
        });
        if copy_pressed {
            self.copy_selected_name(ctx);
        }

        ctx.input(|i| {
            if i.key_pressed(egui::Key::Escape) {
                self.should_close = true;
            }
            // Ctrl+Tab is its own binding, not "Tab, with Ctrl held": without
            // the guard, one Ctrl+Tab press cycles the channel *and* the
            // search mode, since `key_pressed` ignores modifiers entirely.
            if i.key_pressed(egui::Key::Tab) {
                if i.modifiers.ctrl {
                    self.cycle_channel();
                } else {
                    self.cycle_mode();
                }
            }
            if i.key_pressed(egui::Key::ArrowDown) && !self.results.is_empty() {
                self.selected = (self.selected + 1).min(self.results.len() - 1);
                self.hovered = None;
                self.move_selection_into_view = true;
            }
            if i.key_pressed(egui::Key::ArrowUp) && !self.results.is_empty() {
                self.selected = self.selected.saturating_sub(1);
                self.hovered = None;
                self.move_selection_into_view = true;
            }
        });
        // Three things to do with the same hit, picked by modifier: run its
        // program, open a terminal in its shell, or both — the terminal with
        // the program already running in it. Resolved in one place so a
        // modified Enter never also triggers an unmodified one, for the same
        // reason as Ctrl+Tab above.
        let enter = ctx.input(|i| {
            i.key_pressed(egui::Key::Enter).then(|| {
                let terminal = i.modifiers.ctrl || i.modifiers.shift;
                match (terminal, i.modifiers.alt) {
                    (true, true) => EnterAction::TerminalRun,
                    (true, false) => EnterAction::Terminal,
                    (false, _) => EnterAction::Run,
                }
            })
        });
        match enter {
            Some(EnterAction::Run) => self.launch_selected(),
            Some(EnterAction::Terminal) => self.open_terminal_selected(false),
            Some(EnterAction::TerminalRun) => self.open_terminal_selected(true),
            None => {}
        }

        // The copy flash is the one thing that closes the popup on a timer
        // rather than on an event, so it needs to keep asking for frames
        // while it's up — nothing else is happening to produce them.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused {
            self.gained_focus = true;
            self.unfocused_since = None;
        } else if self.gained_focus {
            let since = self.unfocused_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= UNFOCUS_CLOSE_DELAY {
                self.should_close = true;
            } else {
                ctx.request_repaint_after(Duration::from_millis(30));
            }
        }

        if self.should_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // Detail card sits permanently on the right (showing a placeholder
        // until something is hovered/selected) rather than appearing and
        // disappearing. It's an *additional* panel, not a replacement for
        // the results column — the window is sized wide enough (see
        // main.rs) that it never squeezes the results list — and keeping it
        // always present means the results column's width never changes, so
        // nothing reflows or gets covered when you hover a result.
        let detail_frame = egui::Frame::default()
            .fill(elevated(ui.visuals(), ui.visuals().panel_fill, 9))
            .inner_margin(egui::Margin::symmetric(18, 16));
        egui::Panel::right("detail_panel")
            .resizable(false)
            .exact_size(380.0)
            .show_separator_line(true)
            .frame(detail_frame)
            .show(ui, |ui| {
                let available_width = ui.available_width();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.set_max_width(available_width);
                    self.draw_detail_panel(ui);
                });
            });

        if let Some(err) = self.error.clone() {
            egui::Panel::bottom("error_bar").show(ui, |ui| {
                ui.colored_label(egui::Color32::LIGHT_RED, &err);
            });
        }
        if let Some(status) = self.status.clone() {
            egui::Panel::bottom("status_bar").show(ui, |ui| {
                ui.weak(&status);
            });
        }

        // Its own panel rather than a `bottom_up` layout inside the central
        // one: that drew the line *over* the bottom of the results list
        // instead of reserving room for it, which was survivable while the
        // line was short and isn't now.
        egui::Panel::bottom("key_hints").show(ui, |ui| {
            ui.add_space(2.0);
            ui.weak(
                "Enter run · Ctrl+Enter terminal · Ctrl+Alt+Enter terminal+run · \
                 Ctrl+C copy name · Tab mode · Ctrl+Tab channel · Esc close",
            );
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(6.0);
            self.draw_top_bar(ui);
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            self.draw_results_list(ui);
        });
    }
}

/// A panel-fill color nudged lighter (dark mode) or darker (light mode), for
/// opaque "elevated surface" panels/highlights. `visuals().faint_bg_color` is
/// alpha-0 additive-blend-only and turns invisible/see-through on a plain
/// `Frame` fill, so card and hover backgrounds use this instead.
fn elevated(visuals: &egui::Visuals, base: egui::Color32, delta: u8) -> egui::Color32 {
    let adj = |c: u8| -> u8 {
        if visuals.dark_mode {
            c.saturating_add(delta)
        } else {
            c.saturating_sub(delta)
        }
    };
    egui::Color32::from_rgb(adj(base.r()), adj(base.g()), adj(base.b()))
}

/// Renders a shell command as wrapped `ui.code` tokens. Same overflow
/// hazard as everything else in the detail panel: a command line is one
/// long unbroken string, and handing it to the panel's scroll area as a
/// single wide Label would balloon the panel's width instead of wrapping.
fn command_preview(ui: &mut egui::Ui, cmd: &str) {
    ui.horizontal_wrapped(|ui| {
        for token in cmd.split(' ').filter(|t| !t.is_empty()) {
            ui.code(token);
        }
    });
}

/// A `ui.code(text)` pill that truncates (ellipsis) instead of overflowing.
/// `horizontal_wrapped` only wraps *between* widgets — a single value long
/// enough to exceed the row's own width on its own (an unusually long
/// `pname`, license string, or output name) has nothing to wrap onto its own
/// line for, and would otherwise run past the results column, under the
/// always-visible detail panel next to it.
fn code_pill(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).code()).truncate());
}

/// Some packages' `longDescription` is raw HTML (nixpkgs source markup like
/// `<rendered-html><p>...</p></rendered-html>`) rather than plain text — this
/// strips tags and decodes the handful of entities that show up in practice,
/// so the detail panel doesn't display literal `<p>`/`</rendered-html>` tags.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut tag = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' => {
                in_tag = false;
                let tag = tag.to_ascii_lowercase();
                if tag.starts_with("/p") || tag.starts_with("br") {
                    out.push('\n');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
