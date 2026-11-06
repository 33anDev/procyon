//! The eframe application: header, process table, context menu, status bar,
//! confirmation dialog.
//!
//! Data flow per frame:
//!   1. On a timer, refresh `ProcessMonitor` and mark the cached rows dirty.
//!   2. Rebuild the sorted/filtered `rows` only if dirty or the view changed.
//!   3. Draw. Widgets never act directly; they record a [`RowAction`] which is
//!      applied after drawing, keeping borrows simple and side effects in one
//!      place ([`ProcyonApp::apply_action`]).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Button, Color32, FontId, Frame, Layout, Margin, Mesh, Rect, RichText, Sense, Shape,
    Stroke, Ui,
};
use egui_extras::{Column, TableBuilder};
use humansize::{format_size, BINARY};

use crate::actions::{self, Priority};
use crate::icons::IconCache;
use crate::process::{ProcInfo, ProcessMonitor, SortKey};
use crate::theme;

/// How often the process list is re-read from the OS.
const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
/// Height of one table row, in points.
const ROW_HEIGHT: f32 = 26.0;
/// Size of the per-process icon, in points.
const ICON_SIZE: f32 = 16.0;
/// Number of global-CPU samples kept for the header sparkline.
const HISTORY_LEN: usize = 48;
/// Shown on hover over a greyed-out menu item.
const DENIED_HINT: &str = "Access denied: protected system process, or Procyon needs to run as administrator";

/// Something the user asked for via the table or its context menu.
enum RowAction {
    Select(u32),
    SetPriority { pid: u32, name: String, priority: Priority },
    Suspend { pid: u32, name: String },
    Resume { pid: u32, name: String },
    /// Opens the confirmation dialog; nothing is killed until confirmed.
    RequestKill(KillRequest),
    Reveal(PathBuf),
    Copy { text: String, what: &'static str },
}

#[derive(Clone)]
struct KillRequest {
    pid: u32,
    name: String,
    /// `true` = also end every descendant process.
    tree: bool,
}

/// One line of feedback in the status bar.
struct Status {
    text: String,
    ok: bool,
}

pub struct ProcyonApp {
    monitor: ProcessMonitor,
    last_refresh: Instant,
    icons: IconCache,

    /// Cached, sorted, filtered rows. Rebuilt only when the data refreshes
    /// or the view (query/sort) changes, not every frame.
    rows: Vec<ProcInfo>,
    rows_dirty: bool,
    rows_view: Option<(String, SortKey, bool)>,

    /// Recent global CPU% samples for the header sparkline.
    cpu_history: VecDeque<f32>,

    // View state.
    query: String,
    sort_key: SortKey,
    sort_desc: bool,
    selected: Option<u32>,

    /// A row's context menu is open. While true the row list is frozen, so
    /// rows don't re-sort under the user's cursor mid-menu.
    menu_open: bool,

    /// Pending destructive action, awaiting confirmation.
    confirm_kill: Option<KillRequest>,
    status: Option<Status>,
}

impl ProcyonApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);

        let mut monitor = ProcessMonitor::new();
        monitor.refresh(); // prime once so the first frame isn't empty

        Self {
            monitor,
            last_refresh: Instant::now(),
            icons: IconCache::new(&cc.egui_ctx),
            rows: Vec::new(),
            rows_dirty: true,
            rows_view: None,
            cpu_history: VecDeque::with_capacity(HISTORY_LEN),
            query: String::new(),
            sort_key: SortKey::Cpu,
            sort_desc: true,
            selected: None,
            menu_open: false,
            confirm_kill: None,
            status: None,
        }
    }

    /// Click a column header: toggle direction if it's already active,
    /// otherwise switch to it with a sensible default direction.
    fn sort_by(&mut self, key: SortKey) {
        if self.sort_key == key {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort_key = key;
            // Text ascending, numbers descending — the usual expectation.
            self.sort_desc = !matches!(key, SortKey::Name);
        }
    }

    fn set_status(&mut self, ok: bool, text: impl Into<String>) {
        self.status = Some(Status { text: text.into(), ok });
    }

    /// Re-read the OS on the refresh timer and rebuild cached rows if needed.
    fn tick(&mut self) {
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.monitor.refresh();
            self.last_refresh = Instant::now();
            self.rows_dirty = true;

            if self.cpu_history.len() == HISTORY_LEN {
                self.cpu_history.pop_front();
            }
            self.cpu_history.push_back(self.monitor.stats().cpu);
        }

        // Don't reshuffle rows while a context menu is open.
        if self.menu_open {
            return;
        }
        let view = (self.query.clone(), self.sort_key, self.sort_desc);
        if self.rows_dirty || self.rows_view.as_ref() != Some(&view) {
            self.rows = self.monitor.snapshot(&view.0, view.1, view.2);
            self.rows_view = Some(view);
            self.rows_dirty = false;
        }
    }
}

impl eframe::App for ProcyonApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick();
        self.icons.poll(ctx);
        // Keep the UI live even when idle so CPU%/memory tick along.
        ctx.request_repaint_after(REFRESH_INTERVAL);

        paint_backdrop(ctx);
        self.header(ctx);
        self.status_bar(ctx);
        if let Some(action) = self.table(ctx) {
            self.apply_action(ctx, action);
        }
        self.confirm_dialog(ctx);
    }
}

// --- Layout -------------------------------------------------------------------

impl ProcyonApp {
    fn header(&mut self, ctx: &egui::Context) {
        let frame = Frame::none().inner_margin(Margin { left: 18.0, right: 18.0, top: 14.0, bottom: 10.0 });
        egui::TopBottomPanel::top("header")
            .frame(frame)
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    logo(ui);

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Right-to-left: first added is rightmost.
                        ui.add(
                            egui::TextEdit::singleline(&mut self.query)
                                .hint_text("🔎  Filter by name or PID")
                                .desired_width(220.0)
                                .margin(egui::vec2(10.0, 6.0)),
                        );
                        ui.add_space(6.0);

                        let stats = self.monitor.stats();
                        stat_chip(ui, "PROCS", &self.monitor.count().to_string(), |_| {});
                        stat_chip(
                            ui,
                            "MEMORY",
                            &format!(
                                "{} / {}",
                                format_size(stats.used_memory, BINARY),
                                format_size(stats.total_memory, BINARY)
                            ),
                            |_| {},
                        );
                        stat_chip(ui, "CPU", &format!("{:>4.1}%", stats.cpu), |ui| {
                            sparkline(ui, &self.cpu_history);
                        });
                    });
                });
            });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        let frame = Frame::none().inner_margin(Margin { left: 18.0, right: 18.0, top: 6.0, bottom: 8.0 });
        egui::TopBottomPanel::bottom("status")
            .frame(frame)
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    match &self.status {
                        Some(s) => {
                            let color = if s.ok { theme::OK } else { theme::DANGER };
                            status_dot(ui, color);
                            ui.label(RichText::new(&s.text).color(theme::TEXT));
                        }
                        None => {
                            status_dot(ui, theme::ACCENT);
                            ui.label(
                                RichText::new("Right-click a process for actions").color(theme::dim()),
                            );
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let Some(pid) = self.selected else { return };
                        let end = Button::new(RichText::new("End process").color(theme::DANGER));
                        if ui.add(end).clicked() {
                            let name = self
                                .rows
                                .iter()
                                .find(|r| r.pid == pid)
                                .map(|r| r.name.clone())
                                .unwrap_or_else(|| pid.to_string());
                            self.confirm_kill = Some(KillRequest { pid, name, tree: false });
                        }
                        ui.label(RichText::new(format!("PID {pid}")).monospace().color(theme::dim()));
                    });
                });
            });
    }

    /// Draw the process table. Returns at most one action the user took.
    fn table(&mut self, ctx: &egui::Context) -> Option<RowAction> {
        let mut action: Option<RowAction> = None;
        let mut sort_clicked: Option<SortKey> = None;

        // Split borrows: the row closure needs `icons` mutably and the rest
        // read-only.
        let rows = &self.rows;
        let icons = &mut self.icons;
        let mut menu_open = false;
        let selected = self.selected;
        let (sort_key, sort_desc) = (self.sort_key, self.sort_desc);

        let frame = Frame::none().inner_margin(Margin { left: 14.0, right: 14.0, top: 2.0, bottom: 0.0 });
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            theme::card().show(ui, |ui| {
                // Subtle row hover, local to the table (doesn't affect buttons
                // elsewhere).
                ui.visuals_mut().widgets.hovered.bg_fill = Color32::WHITE.gamma_multiply(0.04);

                let header = |ui: &mut Ui, label: &str, key: SortKey, clicked: &mut Option<SortKey>| {
                    if sort_header(ui, label, sort_key == key, sort_desc) {
                        *clicked = Some(key);
                    }
                };

                TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(Sense::click())
                    .cell_layout(Layout::left_to_right(Align::Center))
                    .column(Column::initial(280.0).at_least(140.0).clip(true)) // name
                    .column(Column::initial(72.0).at_least(56.0))               // pid
                    .column(Column::initial(72.0).at_least(56.0))               // ppid
                    .column(Column::initial(96.0).at_least(70.0))               // cpu
                    .column(Column::initial(104.0).at_least(80.0))              // memory
                    .column(Column::remainder().at_least(80.0).clip(true))      // user
                    .header(28.0, |mut h| {
                        h.col(|ui| header(ui, "NAME", SortKey::Name, &mut sort_clicked));
                        h.col(|ui| right(ui, |ui| header(ui, "PID", SortKey::Pid, &mut sort_clicked)));
                        h.col(|ui| right(ui, |ui| { ui.label(header_text("PPID", false)); }));
                        h.col(|ui| right(ui, |ui| header(ui, "CPU", SortKey::Cpu, &mut sort_clicked)));
                        h.col(|ui| right(ui, |ui| header(ui, "MEMORY", SortKey::Memory, &mut sort_clicked)));
                        h.col(|ui| { ui.label(header_text("USER", false)); });
                    })
                    .body(|body| {
                        // `rows` (virtualized): only visible rows are laid
                        // out, so hundreds of processes stay cheap.
                        body.rows(ROW_HEIGHT, rows.len(), |mut row| {
                            let r = &rows[row.index()];
                            let is_selected = selected == Some(r.pid);
                            row.set_selected(is_selected);

                            row.col(|ui| name_cell(ui, r, is_selected, icons));
                            row.col(|ui| right(ui, |ui| { ui.monospace(r.pid.to_string()); }));
                            row.col(|ui| {
                                right(ui, |ui| {
                                    let ppid = r.parent.map(|p| p.to_string()).unwrap_or_default();
                                    ui.label(RichText::new(ppid).monospace().color(theme::dim()));
                                });
                            });
                            row.col(|ui| cpu_cell(ui, r.cpu));
                            row.col(|ui| {
                                right(ui, |ui| { ui.monospace(format_size(r.memory, BINARY)); });
                            });
                            row.col(|ui| {
                                ui.label(RichText::new(r.user.as_deref().unwrap_or("")).color(theme::dim()));
                            });

                            let mut resp = row.response();
                            if resp.clicked() || resp.secondary_clicked() {
                                action = Some(RowAction::Select(r.pid));
                            }

                            // Key the context menu by PID, not by row.
                            // egui remembers an open menu by its widget id,
                            // and table cell ids aren't stable across frames
                            // (they depend on layout order). Left as-is, an
                            // open menu could re-attach to a *different*
                            // process. Keyed by PID, it stays bound to the
                            // process that was right-clicked.
                            resp.id = egui::Id::new(("procyon-row-menu", r.pid));
                            let shown = resp.context_menu(|ui| {
                                if let Some(a) = row_menu(ui, r) {
                                    action = Some(a);
                                    ui.close_menu();
                                }
                            });
                            menu_open |= shown.is_some();
                        });
                    });
            });
        });

        self.menu_open = menu_open;
        if let Some(k) = sort_clicked {
            self.sort_by(k);
        }
        action
    }

    /// Perform a user action and report the outcome in the status bar.
    fn apply_action(&mut self, ctx: &egui::Context, action: RowAction) {
        match action {
            RowAction::Select(pid) => self.selected = Some(pid),

            RowAction::SetPriority { pid, name, priority } => {
                match actions::set_priority(pid, priority) {
                    Ok(applied) if applied == priority => self.set_status(
                        true,
                        format!("Priority of {name} ({pid}) set to {}", applied.label()),
                    ),
                    // Windows downgrades Realtime to High without admin rights.
                    Ok(applied) => self.set_status(
                        false,
                        format!(
                            "Requested {} for {name} ({pid}); Windows applied {}. {} requires running as administrator",
                            priority.label(),
                            applied.label(),
                            priority.label()
                        ),
                    ),
                    Err(e) => self.set_status(false, format!("Could not set priority of {name} ({pid}): {e}")),
                }
            }

            RowAction::Suspend { pid, name } => match self.monitor.suspend(pid) {
                Ok(()) => self.set_status(true, format!("Suspended {name} ({pid})")),
                Err(e) => self.set_status(false, format!("Could not suspend {name} ({pid}): {e}")),
            },

            RowAction::Resume { pid, name } => match self.monitor.resume(pid) {
                Ok(()) => self.set_status(true, format!("Resumed {name} ({pid})")),
                Err(e) => self.set_status(false, format!("Could not resume {name} ({pid}): {e}")),
            },

            RowAction::RequestKill(req) => self.confirm_kill = Some(req),

            RowAction::Reveal(path) => match actions::reveal_in_file_manager(&path) {
                Ok(()) => self.set_status(true, format!("Opened {}", path.display())),
                Err(e) => self.set_status(false, e),
            },

            RowAction::Copy { text, what } => {
                self.set_status(true, format!("Copied {what}: {text}"));
                ctx.copy_text(text);
            }
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(req) = self.confirm_kill.clone() else { return };

        // The process may have exited while the dialog was open.
        if !self.monitor.exists(req.pid) {
            self.confirm_kill = None;
            self.set_status(false, format!("{} ({}) has already exited", req.name, req.pid));
            return;
        }

        // For a tree kill, show how many processes are affected.
        let tree = if req.tree { self.monitor.tree(req.pid) } else { vec![req.pid] };

        let mut open = true;
        let mut decision: Option<bool> = None;
        let title = if req.tree { "End process tree" } else { "End process" };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("Terminate \"{}\" (PID {})?", req.name, req.pid))
                        .color(theme::TEXT_BRIGHT),
                );
                if req.tree && tree.len() > 1 {
                    ui.label(format!(
                        "This also ends its {} descendant process(es).",
                        tree.len() - 1
                    ));
                }
                ui.label(RichText::new("Unsaved work in those processes will be lost.").color(theme::dim()));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let danger = Button::new(RichText::new(title).color(theme::TEXT_BRIGHT).strong())
                        .fill(theme::DANGER.gamma_multiply(0.6));
                    if ui.add(danger).clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        decision = Some(false);
                    }
                });
            });

        // Window close button == cancel.
        if !open {
            decision = Some(false);
        }
        let Some(go) = decision else { return };
        self.confirm_kill = None;
        if go {
            self.kill_all(&req, &tree);
        }
    }

    /// End `pids` in order (children before parents) and summarize.
    fn kill_all(&mut self, req: &KillRequest, pids: &[u32]) {
        let me = std::process::id();
        let mut ended = Vec::new();
        let mut first_err: Option<String> = None;
        let mut skipped_self = false;

        for &pid in pids {
            // Never end Procyon itself as part of a tree.
            if pid == me && req.tree {
                skipped_self = true;
                continue;
            }
            match self.monitor.kill(pid) {
                Ok(()) => ended.push(pid),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }

        // Drop ended rows right away instead of waiting for the next refresh.
        self.rows.retain(|r| !ended.contains(&r.pid));
        if self.selected.is_some_and(|s| ended.contains(&s)) {
            self.selected = None;
        }

        let total = pids.len() - usize::from(skipped_self);
        let note = if skipped_self { " (Procyon itself was skipped)" } else { "" };
        match (ended.len(), first_err) {
            (_, None) if !req.tree => {
                self.set_status(true, format!("Terminated {} ({})", req.name, req.pid))
            }
            (n, None) => self.set_status(true, format!("Ended {n} process(es) in the tree of {}{note}", req.name)),
            (0, Some(e)) => self.set_status(false, format!("Could not terminate {} ({}): {e}", req.name, req.pid)),
            (n, Some(e)) => self.set_status(false, format!("Ended {n} of {total} process(es); some failed: {e}{note}")),
        }
    }
}

// --- Context menu -------------------------------------------------------------

/// Build the right-click menu for one row. Returns the chosen action, if any.
///
/// Access rights are probed when the menu is drawn, so actions the OS would
/// refuse (protected/elevated processes) are greyed out up front.
fn row_menu(ui: &mut Ui, r: &ProcInfo) -> Option<RowAction> {
    ui.set_min_width(220.0);
    let access = actions::probe(r.pid);
    let is_self = r.pid == std::process::id();
    let (pid, name) = (r.pid, r.name.clone());
    let mut chosen = None;

    // Title: which process this menu is about.
    ui.horizontal(|ui| {
        ui.label(RichText::new(&r.name).strong().color(theme::TEXT_BRIGHT));
        ui.label(RichText::new(format!("PID {pid}")).monospace().color(theme::dim()));
    });
    ui.separator();

    // Priority submenu, with the current class marked.
    ui.add_enabled_ui(access.set_priority, |ui| {
        ui.menu_button("⚡  Set priority", |ui| {
            let current = actions::current_priority(pid);
            for p in Priority::ALL {
                if ui.add(Button::new(p.label()).selected(current == Some(p))).clicked() {
                    chosen = Some(RowAction::SetPriority { pid, name: name.clone(), priority: p });
                }
            }
            ui.separator();
            ui.label(RichText::new("Realtime requires administrator").small().color(theme::dim()));
        })
        .response
        .on_disabled_hover_text(DENIED_HINT);
    });

    if menu_item(ui, access.suspend && !is_self, "⏸  Suspend", None) {
        chosen = Some(RowAction::Suspend { pid, name: name.clone() });
    }
    if menu_item(ui, access.suspend, "▶  Resume", None) {
        chosen = Some(RowAction::Resume { pid, name: name.clone() });
    }

    ui.separator();
    if menu_item(ui, access.terminate, "✖  End process…", Some(theme::DANGER)) {
        chosen = Some(RowAction::RequestKill(KillRequest { pid, name: name.clone(), tree: false }));
    }
    if menu_item(ui, access.terminate, "✖  End process tree…", Some(theme::DANGER)) {
        chosen = Some(RowAction::RequestKill(KillRequest { pid, name: name.clone(), tree: true }));
    }

    ui.separator();
    let has_path = r.exe.is_some();
    let path_hint = "Executable path not available for this process";
    if ui
        .add_enabled(has_path, Button::new("📂  Open file location"))
        .on_disabled_hover_text(path_hint)
        .clicked()
    {
        chosen = r.exe.clone().map(RowAction::Reveal);
    }
    if ui.button("📋  Copy PID").clicked() {
        chosen = Some(RowAction::Copy { text: pid.to_string(), what: "PID" });
    }
    if ui.button("📋  Copy name").clicked() {
        chosen = Some(RowAction::Copy { text: name.clone(), what: "name" });
    }
    if ui
        .add_enabled(has_path, Button::new("📋  Copy full path"))
        .on_disabled_hover_text(path_hint)
        .clicked()
    {
        chosen = r.exe.as_ref().map(|p| RowAction::Copy { text: p.display().to_string(), what: "path" });
    }

    if access.any_denied() {
        ui.separator();
        ui.label(
            RichText::new("🔒 Some actions need administrator rights")
                .small()
                .color(theme::dim()),
        );
    }

    chosen
}

/// A menu button that can be disabled (with an explanation on hover) and
/// optionally colored. Returns `true` if clicked.
fn menu_item(ui: &mut Ui, enabled: bool, label: &str, color: Option<Color32>) -> bool {
    let mut text = RichText::new(label);
    if let Some(c) = color.filter(|_| enabled) {
        text = text.color(c);
    }
    ui.add_enabled(enabled, Button::new(text))
        .on_disabled_hover_text(DENIED_HINT)
        .clicked()
}

// --- Cells & small widgets ------------------------------------------------------

/// Icon + name, with an accent bar on the selected row.
fn name_cell(ui: &mut Ui, r: &ProcInfo, selected: bool, icons: &mut IconCache) {
    if selected {
        let cell = ui.max_rect();
        let bar = Rect::from_center_size(
            egui::pos2(cell.left() + 1.5, cell.center().y),
            egui::vec2(3.0, cell.height() * 0.6),
        );
        ui.painter().rect_filled(bar, 2.0, theme::ACCENT);
    }
    ui.add_space(8.0);

    match icons.get(r.exe.as_deref()) {
        Some(tex) => {
            ui.add(egui::Image::new((tex, egui::vec2(ICON_SIZE, ICON_SIZE))));
        }
        None => placeholder_icon(ui),
    }
    ui.add_space(2.0);
    ui.label(&r.name);
}

/// Neutral stand-in when there's no icon (unknown path, extraction failed,
/// or still loading): a small outlined rounded square.
fn placeholder_icon(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE), Sense::hover());
    let r = rect.shrink(2.0);
    ui.painter().rect_stroke(r, 3.0, Stroke::new(1.2_f32, theme::TEXT_DIM.gamma_multiply(0.6)));
    ui.painter().rect_filled(
        Rect::from_min_size(r.min, egui::vec2(r.width(), 3.0)),
        egui::Rounding { nw: 3.0, ne: 3.0, sw: 0.0, se: 0.0 },
        theme::TEXT_DIM.gamma_multiply(0.45),
    );
}

/// CPU% with a heat-colored bar behind it. The bar is scaled so that one
/// fully busy core (100%) fills the cell; multi-core hogs simply max it out.
fn cpu_cell(ui: &mut Ui, cpu: f32) {
    let cell = ui.max_rect();
    let frac = (cpu / 100.0).clamp(0.0, 1.0);
    if cpu >= 0.05 {
        let bar_h = (cell.height() - 8.0).max(8.0);
        let w = (cell.width() * frac).max(2.0);
        let bar = Rect::from_min_size(
            egui::pos2(cell.left(), cell.center().y - bar_h / 2.0),
            egui::vec2(w, bar_h),
        );
        // Low alpha keeps the number on top fully readable.
        ui.painter().rect_filled(bar, 4.0, theme::heat(frac).gamma_multiply(0.38));
    }

    let color = if cpu >= 50.0 {
        theme::DANGER
    } else if cpu >= 5.0 {
        theme::TEXT_BRIGHT
    } else if cpu < 0.05 {
        theme::dim()
    } else {
        theme::TEXT
    };
    right(ui, |ui| {
        ui.label(RichText::new(format!("{cpu:.1}")).monospace().color(color));
    });
}

/// Run `add` right-aligned within the current cell.
fn right(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.with_layout(Layout::right_to_left(Align::Center), add);
}

fn header_text(label: &str, active: bool) -> RichText {
    let t = RichText::new(label).size(11.0).strong();
    if active { t.color(theme::ACCENT) } else { t.color(theme::dim()) }
}

/// A frameless, clickable column header with a sort arrow when active.
fn sort_header(ui: &mut Ui, label: &str, active: bool, desc: bool) -> bool {
    let arrow = match (active, desc) {
        (false, _) => "",
        (true, true) => " ▾",
        (true, false) => " ▴",
    };
    ui.add(Button::new(header_text(&format!("{label}{arrow}"), active)).frame(false))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// "◆ PROCYON" wordmark with a tagline.
fn logo(ui: &mut Ui) {
    ui.label(RichText::new("◆").size(22.0).color(theme::ACCENT));
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut job = LayoutJob::default();
        job.append(
            "PROCYON",
            0.0,
            TextFormat {
                font_id: FontId::proportional(16.0),
                color: theme::TEXT_BRIGHT,
                extra_letter_spacing: 3.5,
                ..Default::default()
            },
        );
        ui.label(job);
        ui.label(RichText::new("transparent process monitor").size(11.0).color(theme::dim()));
    });
}

/// A small header chip: dim caption, monospace value, optional extra widget.
fn stat_chip(ui: &mut Ui, caption: &str, value: &str, extra: impl FnOnce(&mut Ui)) {
    theme::chip().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(caption).size(10.5).strong().color(theme::dim()));
            ui.label(RichText::new(value).monospace().color(theme::TEXT_BRIGHT));
            extra(ui);
        });
    });
}

/// Tiny line chart of recent global CPU%.
fn sparkline(ui: &mut Ui, history: &VecDeque<f32>) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(64.0, 18.0), Sense::hover());
    if history.len() < 2 {
        return;
    }
    let step = rect.width() / (HISTORY_LEN - 1) as f32;
    let x0 = rect.right() - step * (history.len() - 1) as f32;
    let points: Vec<egui::Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let y = rect.bottom() - rect.height() * (v / 100.0).clamp(0.0, 1.0);
            egui::pos2(x0 + step * i as f32, y)
        })
        .collect();
    let last = *points.last().unwrap_or(&rect.right_bottom());
    ui.painter().add(Shape::line(points, Stroke::new(1.5_f32, theme::ACCENT)));
    ui.painter().circle_filled(last, 2.2, theme::ACCENT);
}

fn status_dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// Faint cyan/violet glow behind the translucent panels. One 4-vertex mesh,
/// so it costs essentially nothing per frame.
fn paint_backdrop(ctx: &egui::Context) {
    let rect = ctx.screen_rect();
    let painter = ctx.layer_painter(egui::LayerId::background());
    painter.rect_filled(rect, 0.0, theme::BG);

    let mut mesh = Mesh::default();
    let top_left = Color32::from_rgb(16, 12, 32); // violet hint
    let top_right = Color32::from_rgb(6, 22, 30); // cyan hint
    mesh.colored_vertex(rect.left_top(), top_left);
    mesh.colored_vertex(rect.right_top(), top_right);
    mesh.colored_vertex(rect.right_bottom(), theme::BG);
    mesh.colored_vertex(rect.left_bottom(), theme::BG);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(mesh));
}
