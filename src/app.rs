use crate::launch;
use crate::search::{self, SearchMode, CHANNELS};
use crate::types::PackageHit;
use eframe::egui;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(150);
/// How long focus must stay lost before we auto-close. Guards against
/// transient focus flicker from the compositor (or from IME/virtual-keyboard
/// input) being mistaken for the user actually clicking away.
const UNFOCUS_CLOSE_DELAY: Duration = Duration::from_millis(250);

type SearchResult = Result<Vec<PackageHit>, String>;

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

    focused_once: bool,
    gained_focus: bool,
    unfocused_since: Option<Instant>,
    should_close: bool,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = channel();
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
            focused_once: false,
            gained_focus: false,
            unfocused_since: None,
            should_close: false,
        }
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
            self.results.clear();
            self.selected = 0;
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
        let Some(idx) = self.active_index() else {
            return;
        };
        let Some(hit) = self.results.get(idx).cloned() else {
            return;
        };
        match launch::launch(self.channel(), &hit) {
            Ok(()) => {
                self.should_close = true;
            }
            Err(e) => {
                self.error = Some(format!("failed to launch {}: {e}", hit.pname));
            }
        }
    }

    fn draw_top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("🔍").size(18.0));
            let edit = egui::TextEdit::singleline(&mut self.query)
                .hint_text("search nixpkgs...")
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Heading);
            let resp = ui.add(edit);

            if !self.focused_once {
                resp.request_focus();
                self.focused_once = true;
            }
            if resp.changed() {
                self.mark_dirty();
            }
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
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
                    ui.label(format!("{} results", self.results.len()));
                }
            });
        });
    }

    fn draw_results_list(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            if self.results.is_empty() {
                ui.add_space(12.0);
                let msg = if self.query.trim().is_empty() {
                    "Start typing to search nixpkgs..."
                } else if self.in_flight {
                    "Searching..."
                } else {
                    "No matches."
                };
                ui.weak(msg);
                return;
            }

            for idx in 0..self.results.len() {
                let is_selected = idx == self.selected;
                let hit = &self.results[idx];
                let mut label = egui::RichText::new(&hit.pname).strong();
                if is_selected {
                    label = label.color(ui.visuals().selection.stroke.color);
                }

                let row = ui.horizontal(|ui| {
                    ui.set_min_width(ui.available_width());
                    ui.vertical(|ui| {
                        ui.label(label);
                        if let Some(desc) = &hit.description {
                            ui.weak(truncate(desc, 60));
                        }
                    });
                });

                let resp = row.response.interact(egui::Sense::click());
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

                if resp.hovered() {
                    self.hovered = Some(idx);
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
                if is_selected {
                    ui.painter().rect_stroke(
                        resp.rect,
                        4.0,
                        egui::Stroke::new(1.0, ui.visuals().selection.bg_fill),
                        egui::StrokeKind::Inside,
                    );
                }
                ui.separator();
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

        ui.heading(&hit.pname);
        ui.label(format!("{}  ·  v{}", hit.attr_name, hit.version));
        ui.add_space(6.0);

        if let Some(desc) = &hit.description {
            ui.label(desc);
        }
        if let Some(long) = hit.long_description.as_ref().filter(|s| !s.is_empty()) {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(long).weak());
        }

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
                    ui.label(hit.licenses.join(", "));
                    ui.end_row();
                }
                if !hit.maintainers.is_empty() {
                    ui.label("maintainers");
                    ui.label(hit.maintainers.join(", "));
                    ui.end_row();
                }
                if !hit.homepage.is_empty() {
                    ui.label("homepage");
                    ui.label(hit.homepage.join(", "));
                    ui.end_row();
                }
                if !hit.programs.is_empty() {
                    ui.label("provides");
                    ui.label(hit.programs.join(", "));
                    ui.end_row();
                }
                if let Some(pos) = &hit.position {
                    ui.label("defined at");
                    ui.label(pos);
                    ui.end_row();
                }
                ui.label("platforms");
                ui.label(format!("{}", hit.platforms.len()));
                ui.end_row();
            });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label("will run:");
        ui.code(launch::command_preview(self.channel(), &hit));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        while let Ok((req_gen, result)) = self.rx.try_recv() {
            if req_gen == self.generation {
                self.in_flight = false;
                match result {
                    Ok(hits) => {
                        self.results = hits;
                        self.selected = 0;
                        self.hovered = None;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
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

        ctx.input(|i| {
            if i.key_pressed(egui::Key::Escape) {
                self.should_close = true;
            }
            if i.key_pressed(egui::Key::Tab) {
                self.cycle_mode();
            }
            if i.modifiers.ctrl && i.key_pressed(egui::Key::Tab) {
                self.cycle_channel();
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
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.launch_selected();
        }

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

        egui::Panel::top("top_bar")
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.add_space(6.0);
                self.draw_top_bar(ui);
                ui.add_space(6.0);
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

        egui::Panel::left("results_panel")
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=460.0)
            .show(ui, |ui| {
                self.draw_results_list(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.draw_detail_panel(ui);
            });
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                ui.weak("Enter launch · Tab mode · Ctrl+Tab channel · Esc close");
            });
        });
    }
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
