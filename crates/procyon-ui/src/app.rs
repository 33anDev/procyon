//! The eframe application: top bar, process table, status bar.

use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Layout, RichText};
use egui_extras::{Column, TableBuilder};
use humansize::{format_size, BINARY};

use crate::process::{ProcessMonitor, SortKey};
use crate::theme;

/// How often the process list is re-read from the OS.
const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

pub struct ProcyonApp {
    monitor: ProcessMonitor,
    last_refresh: Instant,

    // View state.
    query: String,
    sort_key: SortKey,
    sort_desc: bool,
    selected: Option<u32>,

    // Pending destructive action, awaiting confirmation: (pid, name).
    confirm_kill: Option<(u32, String)>,
    /// Transient status line message, e.g. the result of a kill.
    status: Option<String>,
}

impl ProcyonApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);

        let mut monitor = ProcessMonitor::new();
        monitor.refresh(); // prime once so the first frame isn't empty

        Self {
            monitor,
            last_refresh: Instant::now(),
            query: String::new(),
            sort_key: SortKey::Cpu,
            sort_desc: true,
            selected: None,
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

    fn header_label(&self, label: &str, key: SortKey) -> RichText {
        let arrow = if self.sort_key == key {
            if self.sort_desc { " ▾" } else { " ▴" }
        } else {
            ""
        };
        let mut t = RichText::new(format!("{label}{arrow}")).strong();
        if self.sort_key == key {
            t = t.color(theme::ACCENT);
        }
        t
    }
}

impl eframe::App for ProcyonApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Timed refresh, independent of user interaction.
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.monitor.refresh();
            self.last_refresh = Instant::now();
        }
        // Keep the UI live even when idle so CPU%/memory tick along.
        ctx.request_repaint_after(REFRESH_INTERVAL);

        self.top_bar(ctx);
        self.status_bar(ctx);
        self.table(ctx);
        self.confirm_dialog(ctx);
    }
}

impl ProcyonApp {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("◆ Procyon").size(18.0).strong().color(theme::ACCENT));
                ui.label(RichText::new("transparent process monitor").color(theme::dim()));

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // Search box pinned to the right.
                    let edit = egui::TextEdit::singleline(&mut self.query)
                        .hint_text("filter by name or pid…")
                        .desired_width(240.0);
                    ui.add(edit);
                    ui.label(RichText::new("🔎").color(theme::dim()));
                });
            });
            ui.add_space(6.0);
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} processes", self.monitor.count()))
                        .color(theme::dim()),
                );
                if let Some(msg) = &self.status {
                    ui.separator();
                    ui.label(RichText::new(msg).color(theme::dim()));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(pid) = self.selected {
                        let enabled = true;
                        if ui
                            .add_enabled(
                                enabled,
                                egui::Button::new(
                                    RichText::new("End process").color(theme::DANGER),
                                ),
                            )
                            .clicked()
                        {
                            // Look up the name for the confirmation prompt.
                            let name = self
                                .monitor
                                .snapshot("", SortKey::Pid, false)
                                .into_iter()
                                .find(|r| r.pid == pid)
                                .map(|r| r.name)
                                .unwrap_or_else(|| pid.to_string());
                            self.confirm_kill = Some((pid, name));
                        }
                        ui.label(RichText::new(format!("selected pid {pid}")).color(theme::dim()));
                    }
                });
            });
            ui.add_space(3.0);
        });
    }

    fn table(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let rows = self.monitor.snapshot(&self.query, self.sort_key, self.sort_desc);

            // Header click intents are collected and applied after the closure
            // to avoid borrowing `self` mutably while it's already borrowed.
            let mut clicked: Option<SortKey> = None;
            let mut new_selection: Option<Option<u32>> = None;
            let mut new_kill: Option<(u32, String)> = None;

            let text_height = egui::TextStyle::Body.resolve(ui.style()).size + 8.0;

            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::initial(260.0).at_least(120.0).clip(true)) // name
                .column(Column::initial(70.0).at_least(55.0))               // pid
                .column(Column::initial(70.0).at_least(55.0))               // ppid
                .column(Column::initial(80.0).at_least(60.0))               // cpu
                .column(Column::initial(110.0).at_least(80.0))              // memory
                .column(Column::remainder().at_least(80.0))                 // user
                .header(24.0, |mut header| {
                    header.col(|ui| {
                        if ui.button(self.header_label("Name", SortKey::Name)).clicked() {
                            clicked = Some(SortKey::Name);
                        }
                    });
                    header.col(|ui| {
                        if ui.button(self.header_label("PID", SortKey::Pid)).clicked() {
                            clicked = Some(SortKey::Pid);
                        }
                    });
                    header.col(|ui| {
                        ui.label(RichText::new("PPID").strong());
                    });
                    header.col(|ui| {
                        if ui.button(self.header_label("CPU%", SortKey::Cpu)).clicked() {
                            clicked = Some(SortKey::Cpu);
                        }
                    });
                    header.col(|ui| {
                        if ui.button(self.header_label("Memory", SortKey::Memory)).clicked() {
                            clicked = Some(SortKey::Memory);
                        }
                    });
                    header.col(|ui| {
                        ui.label(RichText::new("User").strong());
                    });
                })
                .body(|mut body| {
                    for r in &rows {
                        body.row(text_height, |mut row| {
                            row.set_selected(self.selected == Some(r.pid));

                            row.col(|ui| {
                                ui.label(&r.name);
                            });
                            row.col(|ui| {
                                ui.monospace(r.pid.to_string());
                            });
                            row.col(|ui| {
                                ui.label(
                                    RichText::new(
                                        r.parent.map(|p| p.to_string()).unwrap_or_default(),
                                    )
                                    .color(theme::dim()),
                                );
                            });
                            row.col(|ui| {
                                // Tint hot processes so they stand out.
                                let txt = format!("{:.1}", r.cpu);
                                let t = if r.cpu >= 20.0 {
                                    RichText::new(txt).color(theme::DANGER)
                                } else {
                                    RichText::new(txt)
                                };
                                ui.monospace(t);
                            });
                            row.col(|ui| {
                                ui.monospace(format_size(r.memory, BINARY));
                            });
                            row.col(|ui| {
                                ui.label(
                                    RichText::new(r.user.clone().unwrap_or_default())
                                        .color(theme::dim()),
                                );
                            });

                            // Row-wide interaction.
                            let resp = row.response();
                            if resp.clicked() {
                                new_selection = Some(Some(r.pid));
                            }
                            resp.context_menu(|ui| {
                                if ui.button("End process").clicked() {
                                    new_kill = Some((r.pid, r.name.clone()));
                                    ui.close_menu();
                                }
                            });
                        });
                    }
                });

            if let Some(k) = clicked {
                self.sort_by(k);
            }
            if let Some(sel) = new_selection {
                self.selected = sel;
            }
            if let Some(k) = new_kill {
                self.confirm_kill = Some(k);
            }
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some((pid, name)) = self.confirm_kill.clone() else {
            return;
        };

        let mut open = true;
        let mut decision: Option<bool> = None;

        egui::Window::new("End process")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(format!("Terminate \"{name}\" (pid {pid})?"));
                ui.label(
                    RichText::new("Unsaved work in that process will be lost.")
                        .color(theme::dim()),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new("End process").color(theme::DANGER))
                        .clicked()
                    {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });

        // Window close button == cancel.
        if !open {
            decision = Some(false);
        }

        if let Some(go) = decision {
            if go {
                let ok = self.monitor.kill(pid);
                self.status = Some(if ok {
                    format!("terminated \"{name}\" (pid {pid})")
                } else {
                    format!("could not terminate pid {pid} (gone or access denied)")
                });
                if self.selected == Some(pid) {
                    self.selected = None;
                }
            }
            self.confirm_kill = None;
        }
    }
}