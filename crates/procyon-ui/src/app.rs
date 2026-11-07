//! The eframe application: header, process table, context menu, detail
//! panel, status bar, confirmation dialog.
//!
//! Data flow per frame:
//!   1. On a timer, refresh `ProcessMonitor` and mark the cached rows dirty.
//!      If the detail panel is open, also ask the detail worker for a fresh
//!      read of its process.
//!   2. Rebuild the sorted/filtered `rows` only if dirty or the view changed.
//!   3. Draw. Widgets never act directly; they record a [`RowAction`] which is
//!      applied after drawing, keeping borrows simple and side effects in one
//!      place ([`ProcyonApp::apply_action`]).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Button, Color32, FontId, Frame, Layout, Margin, Mesh, Rect, RichText, Sense, Shape,
    Stroke, Ui,
};
use egui_extras::{Column, TableBuilder};
use humansize::{format_size, BINARY};

use crate::actions::{self, Priority};
use crate::details::{DetailFetcher, Field, Integrity, Missing, ProcDetails};
use crate::icons::IconCache;
use crate::process::{ProcInfo, ProcessMonitor, SortKey};
use crate::theme;

/// How often the process list is re-read from the OS.
const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
/// Height of one table row, in points. A touch taller than the text needs,
/// so rows are an easy click/right-click target.
const ROW_HEIGHT: f32 = 30.0;
/// Size of the per-process icon, in points.
const ICON_SIZE: f32 = 16.0;
/// Number of global-CPU samples kept for the header sparkline.
const HISTORY_LEN: usize = 48;
/// Shown on hover over a greyed-out menu item.
const DENIED_HINT: &str = "Access denied: protected system process, or Procyon needs to run as administrator";
/// Width of the key column in the detail panel.
const DETAIL_KEY_WIDTH: f32 = 104.0;

/// Something the user asked for via the table, its context menu, or the
/// detail panel.
enum RowAction {
    Select(u32),
    /// Open the detail panel on this process (double-click / "Details").
    ShowDetails(u32),
    CloseDetails,
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

/// State of the open detail panel.
struct DetailPanel {
    /// The process shown. Follows the table selection.
    pid: u32,
    /// Latest answer from the detail worker; `None` until the first fetch for
    /// this PID lands (a few ms after opening).
    data: Option<ProcDetails>,
    /// This process's CPU% at each refresh since the panel opened on it.
    cpu_history: VecDeque<f32>,
}

impl DetailPanel {
    fn new(pid: u32) -> Self {
        Self { pid, data: None, cpu_history: VecDeque::with_capacity(HISTORY_LEN) }
    }
}

pub struct ProcyonApp {
    monitor: ProcessMonitor,
    last_refresh: Instant,
    icons: IconCache,
    details: DetailFetcher,

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

    /// A row's context menu is open. While true the row order is frozen, so
    /// rows don't re-sort under the user's cursor mid-menu.
    menu_open: bool,
    /// The pointer was over the table last frame. While true the row order
    /// is frozen so the user can aim at a row without it sliding away;
    /// values still update in place. One frame of lag is imperceptible.
    table_hovered: bool,
    /// The header Pause toggle: freezes the row order on demand until the
    /// user turns it off. Values still update; only re-ranking stops.
    paused: bool,

    /// The right-docked detail panel, when open.
    detail: Option<DetailPanel>,

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
            details: DetailFetcher::new(&cc.egui_ctx),
            rows: Vec::new(),
            rows_dirty: true,
            rows_view: None,
            cpu_history: VecDeque::with_capacity(HISTORY_LEN),
            query: String::new(),
            // Default to Name ascending: a calm, alphabetical list that
            // doesn't reshuffle on startup. CPU sort is one header-click away.
            sort_key: SortKey::Name,
            sort_desc: false,
            selected: None,
            menu_open: false,
            table_hovered: false,
            paused: false,
            detail: None,
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
    /// Returns `true` if the OS was re-read this frame.
    fn tick(&mut self) -> bool {
        let refreshed = self.last_refresh.elapsed() >= REFRESH_INTERVAL;
        if refreshed {
            self.monitor.refresh();
            self.last_refresh = Instant::now();
            self.rows_dirty = true;
            push_sample(&mut self.cpu_history, self.monitor.stats().cpu);
        }

        // The row order is frozen while the user is pointing at the table
        // (hovering or mid-menu) or has hit Pause, so rows don't slide out
        // from under the cursor. Changing the filter or clicking a sort
        // header is an explicit reorder request and always applies.
        let view = (self.query.clone(), self.sort_key, self.sort_desc);
        let view_changed = self.rows_view.as_ref() != Some(&view);
        let frozen = self.paused || self.menu_open || self.table_hovered;

        if view_changed || (self.rows_dirty && !frozen) {
            // Full rebuild: re-filter, re-sort, fresh values.
            self.rows = self.monitor.snapshot(&view.0, view.1, view.2);
            self.rows_view = Some(view);
            self.rows_dirty = false;
        } else if self.rows_dirty {
            // Frozen: keep the current order and set of rows, just refresh
            // the live numbers in place.
            self.monitor.refresh_values(&mut self.rows);
            self.rows_dirty = false;
        }
        refreshed
    }

    /// Open the detail panel on `pid`, or switch the open panel to it.
    fn show_details(&mut self, pid: u32) {
        if self.detail.as_ref().is_some_and(|d| d.pid == pid) {
            return; // already showing it; keep its CPU history
        }
        self.detail = Some(DetailPanel::new(pid));
        self.details.request(pid);
    }

    /// Keep the detail panel in step with the selection and the refresh
    /// timer. The expensive Windows reads happen on the worker, and only
    /// when the panel opens, switches process, or the OS is re-read.
    fn sync_detail(&mut self, refreshed: bool) {
        let Some(shown) = self.detail.as_ref().map(|d| d.pid) else { return };
        // Follow the selection. (If the selected process ended, `selected`
        // is cleared and the panel stays on it to show that it has exited.)
        if let Some(sel) = self.selected.filter(|&s| s != shown) {
            self.show_details(sel);
        }
        let Some(panel) = &mut self.detail else { return };

        if let Some(fresh) = self.details.poll(panel.pid) {
            panel.data = Some(fresh);
        }
        if refreshed {
            if let Some(info) = self.monitor.info(panel.pid) {
                push_sample(&mut panel.cpu_history, info.cpu);
                self.details.request(panel.pid);
            }
        }
    }
}

impl eframe::App for ProcyonApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let refreshed = self.tick();
        self.sync_detail(refreshed);
        self.icons.poll(ctx);
        // Keep the UI live even when idle so CPU%/memory tick along.
        ctx.request_repaint_after(REFRESH_INTERVAL);

        paint_backdrop(ctx);
        self.header(ctx);
        self.status_bar(ctx);
        // Side panels must be laid out before the central table.
        let mut action = self.detail_panel(ctx);
        if let Some(a) = self.table(ctx) {
            action = Some(a);
        }
        if let Some(a) = action {
            self.apply_action(ctx, a);
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
                        pause_toggle(ui, &mut self.paused);
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
                            sparkline(ui, &self.cpu_history, 64.0);
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
                                RichText::new("Right-click a process for actions · double-click for details")
                                    .color(theme::dim()),
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

        let mut hovering = false;
        let frame = Frame::none().inner_margin(Margin { left: 14.0, right: 14.0, top: 2.0, bottom: 0.0 });
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let card = theme::card().show(ui, |ui| {
                // Clear row hover, local to the table (doesn't affect buttons
                // elsewhere). A visible highlight makes rows easy to aim at.
                ui.visuals_mut().widgets.hovered.bg_fill = Color32::WHITE.gamma_multiply(0.07);

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
                            // The second click of a double-click is also a
                            // click, so this must come after (and win).
                            if resp.double_clicked() {
                                action = Some(RowAction::ShowDetails(r.pid));
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
            // Freeze re-ranking while the pointer is anywhere over the table
            // card. Read next frame by `tick`; one frame of lag is invisible.
            hovering = card.response.contains_pointer();
        });

        self.menu_open = menu_open;
        self.table_hovered = hovering;
        if let Some(k) = sort_clicked {
            self.sort_by(k);
        }
        action
    }

    /// Perform a user action and report the outcome in the status bar.
    fn apply_action(&mut self, ctx: &egui::Context, action: RowAction) {
        match action {
            RowAction::Select(pid) => self.selected = Some(pid),

            RowAction::ShowDetails(pid) => {
                self.selected = Some(pid);
                self.show_details(pid);
            }

            RowAction::CloseDetails => self.detail = None,

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
                // Command lines can be thousands of characters; keep the
                // status bar to one readable line.
                const MAX: usize = 80;
                let shown = if text.chars().count() > MAX {
                    format!("{}…", text.chars().take(MAX).collect::<String>())
                } else {
                    text.clone()
                };
                self.set_status(true, format!("Copied {what}: {shown}"));
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

// --- Detail panel -------------------------------------------------------------

impl ProcyonApp {
    /// Right-docked panel with everything known about one process. Returns
    /// at most one action (copy, reveal, jump to parent, close).
    ///
    /// Drawing only reads already-fetched data: live numbers come from the
    /// monitor (refreshed on the timer), the rest from the worker's last
    /// answer. Nothing here calls into the OS.
    fn detail_panel(&mut self, ctx: &egui::Context) -> Option<RowAction> {
        let panel = self.detail.as_ref()?;

        // Esc closes the panel, unless a dialog or menu should get it.
        if self.confirm_kill.is_none()
            && !self.menu_open
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
        {
            return Some(RowAction::CloseDetails);
        }

        let info = self.monitor.info(panel.pid);
        let parent_name = self.monitor.parent_name(panel.pid);
        let icons = &mut self.icons;
        let mut action = None;

        let frame = Frame::none().inner_margin(Margin { left: 0.0, right: 14.0, top: 2.0, bottom: 0.0 });
        egui::SidePanel::right("details")
            .frame(frame)
            .show_separator_line(false)
            .resizable(true)
            .default_width(380.0)
            .width_range(320.0..=640.0)
            .show(ctx, |ui| {
                theme::card().show(ui, |ui| {
                    ui.set_min_size(ui.available_size());
                    if panel_title(ui) {
                        action = Some(RowAction::CloseDetails);
                    }
                    match &info {
                        Some(r) => {
                            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                                if let Some(a) = detail_body(ui, r, parent_name.as_deref(), panel, icons) {
                                    action = Some(a);
                                }
                            });
                        }
                        None => exited_notice(ui, panel.pid),
                    }
                });
            });
        action
    }
}

/// "◈ PROCESS DETAILS" caption with a close button. Returns `true` if the
/// close button was clicked.
fn panel_title(ui: &mut Ui) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("◈").color(theme::VIOLET));
        ui.label(spaced_caps("PROCESS DETAILS", theme::dim()));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            close = ui
                .add(Button::new(RichText::new("✖").size(12.0).color(theme::dim())).frame(false))
                .on_hover_text("Close (Esc)")
                .clicked();
        });
    });
    close
}

/// The panel's sections for a live process.
fn detail_body(
    ui: &mut Ui,
    r: &ProcInfo,
    parent_name: Option<&str>,
    panel: &DetailPanel,
    icons: &mut IconCache,
) -> Option<RowAction> {
    let d = panel.data.as_ref();
    let mut action = None;

    // --- Identity
    section(ui, "IDENTITY");
    ui.horizontal(|ui| {
        const HERO_ICON: f32 = 24.0;
        match icons.get(r.exe.as_deref()) {
            Some(tex) => {
                ui.add(egui::Image::new((tex, egui::vec2(HERO_ICON, HERO_ICON))));
            }
            None => placeholder_icon(ui, HERO_ICON),
        }
        ui.label(RichText::new(&r.name).size(17.0).strong().color(theme::TEXT_BRIGHT));
    });
    ui.add_space(2.0);

    kv(ui, "PID", |ui| {
        mono(ui, r.pid.to_string());
    });
    kv(ui, "Parent", |ui| match (r.parent, parent_name) {
        (Some(ppid), Some(name)) => {
            ui.horizontal(|ui| {
                let link = ui.link(RichText::new(ppid.to_string()).monospace());
                if link.on_hover_text("Inspect parent").clicked() {
                    action = Some(RowAction::Select(ppid));
                }
                ui.label(name);
            });
        }
        // PID recorded, but that process is gone (or the PID was reused).
        (Some(ppid), None) => {
            ui.horizontal(|ui| {
                mono(ui, ppid.to_string());
                ui.label(RichText::new("not running").color(theme::dim()));
            });
        }
        (None, _) => missing_label(ui, Missing::Unavailable),
    });
    kv(ui, "Path", |ui| {
        match &r.exe {
            Some(path) => {
                ui.add(egui::Label::new(RichText::new(path.display().to_string()).monospace()).wrap());
            }
            None => {
                ui.label(RichText::new("Not available").color(theme::dim()))
                    .on_hover_text("Protected processes don't expose their image path");
            }
        }
        ui.horizontal_wrapped(|ui| {
            let path = r.exe.as_ref();
            if ui.add_enabled(path.is_some(), Button::new("📋  Copy path")).clicked() {
                action = path.map(|p| RowAction::Copy { text: p.display().to_string(), what: "path" });
            }
            if ui.add_enabled(path.is_some(), Button::new("📂  Open location")).clicked() {
                action = path.cloned().map(RowAction::Reveal);
            }
        });
    });

    // --- Status
    section(ui, "STATUS");
    kv(ui, "State", |ui| {
        fetched(ui, d.map(|d| &d.suspended), |ui, &suspended| {
            if suspended {
                ui.add(pill("Suspended", theme::VIOLET));
            } else {
                ui.add(pill("Running", theme::OK));
            }
        });
    });
    kv(ui, "Priority", |ui| {
        fetched(ui, d.map(|d| &d.priority), |ui, p| {
            ui.label(RichText::new(p.label()).color(theme::TEXT_BRIGHT));
        });
    });
    kv(ui, "Integrity", |ui| {
        fetched(ui, d.map(|d| &d.integrity), |ui, &level| {
            ui.add(pill(level.label(), integrity_color(level)));
        });
    });
    kv(ui, "Elevated", |ui| {
        fetched(ui, d.map(|d| &d.elevated), |ui, &elevated| {
            let (text, color) = if elevated { ("Yes", theme::ACCENT) } else { ("No", theme::TEXT) };
            ui.label(RichText::new(text).color(color));
        });
    });

    // --- Resources
    section(ui, "RESOURCES");
    kv(ui, "CPU", |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{:.1} %", r.cpu)).monospace().color(cpu_color(r.cpu)));
            ui.add_space(4.0);
            sparkline(ui, &panel.cpu_history, 96.0);
        });
    });
    kv(ui, "Memory", |ui| {
        ui.horizontal(|ui| {
            mono(ui, format_size(r.memory, BINARY)).on_hover_text(format!("{} bytes", r.memory));
            ui.label(RichText::new("working set").small().color(theme::dim()));
        });
    });
    kv(ui, "Threads", |ui| {
        fetched(ui, d.map(|d| &d.threads), |ui, n| {
            mono(ui, n.to_string());
        });
    });
    kv(ui, "Handles", |ui| {
        fetched(ui, d.map(|d| &d.handles), |ui, n| {
            mono(ui, n.to_string());
        });
    });
    let started = d.map(|d| &d.started);
    kv(ui, "Started", |ui| {
        fetched(ui, started, |ui, s| match &s.local {
            Some(local) => {
                mono(ui, local.as_str());
            }
            None => missing_label(ui, Missing::Unavailable),
        });
    });
    kv(ui, "Uptime", |ui| {
        // Computed per frame from the stored start time, so it ticks along
        // without re-querying the OS.
        fetched(ui, started, |ui, s| {
            let up = SystemTime::now().duration_since(s.at).unwrap_or_default();
            mono(ui, format_uptime(up));
        });
    });

    // --- Command line
    section(ui, "COMMAND LINE");
    fetched(ui, d.map(|d| &d.command_line), |ui, cmd| {
        if cmd.is_empty() {
            ui.label(RichText::new("(empty)").color(theme::dim()));
            return;
        }
        // Read-only but selectable: a `&str` buffer can't be edited.
        egui::ScrollArea::vertical().id_salt("cmdline").max_height(140.0).show(ui, |ui| {
            let mut text = cmd.as_str();
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .font(egui::TextStyle::Monospace)
                    .desired_width(f32::INFINITY)
                    .desired_rows(1),
            );
        });
        if ui.button("📋  Copy command line").clicked() {
            action = Some(RowAction::Copy { text: cmd.clone(), what: "command line" });
        }
    });

    // --- User
    section(ui, "USER");
    kv(ui, "Account", |ui| match d.map(|d| &d.account) {
        Some(Ok(account)) => {
            ui.label(RichText::new(account).color(theme::TEXT_BRIGHT));
        }
        // The token was off-limits; sysinfo may still know the owner's SID.
        Some(Err(why)) => match &r.user {
            Some(user) => {
                mono(ui, user.as_str());
            }
            None => missing_label(ui, *why),
        },
        None => loading_label(ui),
    });
    ui.add_space(8.0);

    action
}

/// Shown in place of the sections once the process is gone.
fn exited_notice(ui: &mut Ui, pid: u32) {
    ui.add_space(32.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("◇").size(28.0).color(theme::dim()));
        ui.label(RichText::new(format!("Process {pid} has exited")).color(theme::TEXT_BRIGHT));
        ui.label(RichText::new("Select another process to inspect it.").color(theme::dim()));
    });
}

/// Section caption: spaced small caps in cyan, trailed by a fading hairline.
fn section(ui: &mut Ui, title: &str) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(spaced_caps(title, theme::ACCENT));
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), Sense::hover());
        let mut mesh = Mesh::default();
        let (from, to) = (theme::ACCENT.gamma_multiply(0.45), Color32::TRANSPARENT);
        mesh.colored_vertex(rect.left_top(), from);
        mesh.colored_vertex(rect.right_top(), to);
        mesh.colored_vertex(rect.right_bottom(), to);
        mesh.colored_vertex(rect.left_bottom(), from);
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        ui.painter().add(Shape::mesh(mesh));
    });
    ui.add_space(2.0);
}

/// Small letter-spaced caps, for captions.
fn spaced_caps(text: &str, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: FontId::proportional(10.5),
            color,
            extra_letter_spacing: 1.6,
            ..Default::default()
        },
    );
    job
}

/// One key/value row. The value side is its own vertical layout, so long
/// values (paths) wrap inside the panel instead of widening it.
fn kv(ui: &mut Ui, key: &str, value: impl FnOnce(&mut Ui)) {
    ui.horizontal_top(|ui| {
        let used = ui.label(RichText::new(key).color(theme::dim())).rect.width();
        ui.add_space((DETAIL_KEY_WIDTH - used).max(0.0));
        ui.vertical(value);
    });
}

/// Show one worker-fetched value: "…" until the first fetch lands, the
/// reason if it's missing, otherwise whatever `show` draws.
fn fetched<T>(ui: &mut Ui, field: Option<&Field<T>>, show: impl FnOnce(&mut Ui, &T)) {
    match field {
        None => loading_label(ui),
        Some(Ok(v)) => show(ui, v),
        Some(Err(why)) => missing_label(ui, *why),
    }
}

fn loading_label(ui: &mut Ui) {
    ui.label(RichText::new("…").color(theme::dim()));
}

fn missing_label(ui: &mut Ui, why: Missing) {
    match why {
        Missing::Denied => {
            ui.label(RichText::new("🔒 Access denied").color(theme::dim()))
                .on_hover_text(DENIED_HINT);
        }
        Missing::Unavailable => {
            ui.label(RichText::new("—").color(theme::dim()));
        }
    }
}

/// Bright monospace value (numbers, paths, times).
fn mono(ui: &mut Ui, text: impl Into<String>) -> egui::Response {
    ui.label(RichText::new(text.into()).monospace().color(theme::TEXT_BRIGHT))
}

/// A small tinted, rounded tag (state, integrity level).
fn pill(text: &str, color: Color32) -> impl egui::Widget + '_ {
    move |ui: &mut Ui| {
        theme::pill(color)
            .show(ui, |ui| ui.label(RichText::new(text).size(11.5).color(color)))
            .response
    }
}

fn integrity_color(level: Integrity) -> Color32 {
    match level {
        Integrity::System | Integrity::Protected => theme::VIOLET,
        Integrity::High => theme::ACCENT,
        Integrity::Medium | Integrity::MediumPlus => theme::TEXT_BRIGHT,
        Integrity::Low | Integrity::Untrusted => theme::dim(),
    }
}

/// `3d 04:12:09`, or `04:12:09` under a day.
fn format_uptime(d: Duration) -> String {
    let s = d.as_secs();
    let (days, h, m, s) = (s / 86_400, s / 3_600 % 24, s / 60 % 60, s % 60);
    if days > 0 {
        format!("{days}d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// Append to a bounded sample history, dropping the oldest when full.
fn push_sample(history: &mut VecDeque<f32>, v: f32) {
    if history.len() == HISTORY_LEN {
        history.pop_front();
    }
    history.push_back(v);
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

    if ui.button(RichText::new("◈  Details").color(theme::ACCENT)).clicked() {
        chosen = Some(RowAction::ShowDetails(pid));
    }
    ui.separator();

    // The common actions come first, each a single click. "Set priority" is
    // a submenu, so it's placed *below* all of them (near the bottom): egui
    // springs a submenu open on hover, so if it sat up here the pointer would
    // trip it every time on its way down to Suspend / End / Copy. With it at
    // the end, the path to the common actions never crosses it.
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

    // Priority submenu, tucked at the bottom so it stays out of the way (see
    // the note above). The current class keeps its checkmark, and the whole
    // control is greyed out when we can't change priority. Picking a value
    // closes the entire menu, not just the submenu, via `ui.close_menu()`.
    ui.separator();
    ui.add_enabled_ui(access.set_priority, |ui| {
        ui.menu_button("⚡  Set priority", |ui| {
            let current = actions::current_priority(pid);
            for p in Priority::ALL {
                if ui.add(Button::new(p.label()).selected(current == Some(p))).clicked() {
                    chosen = Some(RowAction::SetPriority { pid, name: name.clone(), priority: p });
                    ui.close_menu();
                }
            }
            ui.separator();
            ui.label(RichText::new("Realtime requires administrator").small().color(theme::dim()));
        })
        .response
        .on_disabled_hover_text(DENIED_HINT);
    });

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
        None => placeholder_icon(ui, ICON_SIZE),
    }
    ui.add_space(2.0);
    ui.label(&r.name);
}

/// Neutral stand-in when there's no icon (unknown path, extraction failed,
/// or still loading): a small outlined rounded square, `size` points wide.
fn placeholder_icon(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    let r = rect.shrink(size / 8.0);
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

    right(ui, |ui| {
        ui.label(RichText::new(format!("{cpu:.1}")).monospace().color(cpu_color(cpu)));
    });
}

/// Text color for a CPU% figure: dim when idle, red when hot.
fn cpu_color(cpu: f32) -> Color32 {
    if cpu >= 50.0 {
        theme::DANGER
    } else if cpu >= 5.0 {
        theme::TEXT_BRIGHT
    } else if cpu < 0.05 {
        theme::dim()
    } else {
        theme::TEXT
    }
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

/// Header toggle that freezes the table's row ordering on demand. Lit when
/// active. Values keep updating either way; only re-ranking stops.
///
/// Uses the ⏸ glyph (U+23F8) already proven to render via the app's symbol
/// font, rather than a bare `❚❚`, which the bundled fonts may not carry.
fn pause_toggle(ui: &mut Ui, paused: &mut bool) {
    let (color, hint) = if *paused {
        (theme::VIOLET, "Row order frozen — click to resume live sorting")
    } else {
        (theme::dim(), "Freeze the row order so it's easy to click (values keep updating)")
    };
    let label = if *paused { "⏸ PAUSED" } else { "⏸ PAUSE" };
    let frame = theme::chip().fill(if *paused { theme::VIOLET.gamma_multiply(0.14) } else { theme::ELEVATED });
    frame.show(ui, |ui| {
        let btn = Button::new(RichText::new(label).size(10.5).strong().color(color)).frame(false);
        if ui.add(btn).on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(hint).clicked() {
            *paused = !*paused;
        }
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

/// Tiny line chart of recent CPU% samples (global in the header, one
/// process in the detail panel). Values are clamped to 0..=100.
fn sparkline(ui: &mut Ui, history: &VecDeque<f32>, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 18.0), Sense::hover());
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
