//! Process data layer.
//!
//! Thin wrapper around `sysinfo` that produces an immutable snapshot the UI can
//! sort and filter without holding a lock on the live system handle. Keeping the
//! data layer separate from the UI means the Windows-specific additions later
//! (per-process module/DLL list, privileges, integrity level) have an obvious
//! home and don't tangle up the rendering code.

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

/// One row in the process table. Owned, cheap to clone, independent of the
/// live `System` so the UI can sort/filter freely.
#[derive(Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    /// CPU usage in percent, summed across cores (can exceed 100).
    pub cpu: f32,
    /// Resident memory in bytes.
    pub memory: u64,
    pub user: Option<String>,
}

/// Which column the table is sorted by.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Pid,
    Cpu,
    Memory,
}

/// Owns the live `sysinfo::System` and hands out snapshots on demand.
pub struct ProcessMonitor {
    system: System,
    /// CPU% is only meaningful after two refreshes spaced in time, so we skip
    /// reporting it on the very first pass to avoid showing bogus spikes.
    primed: bool,
}

impl ProcessMonitor {
    pub fn new() -> Self {
        // Only ask the OS for what we actually render. Cheaper refreshes.
        let refresh = RefreshKind::new().with_processes(
            ProcessRefreshKind::new()
                .with_cpu()
                .with_memory()
                .with_user(UpdateKind::OnlyIfNotSet),
        );
        let system = System::new_with_specifics(refresh);
        Self { system, primed: false }
    }

    /// Re-read the process list from the OS. Call on a timer.
    pub fn refresh(&mut self) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true, // remove processes that have since exited
            ProcessRefreshKind::new()
                .with_cpu()
                .with_memory()
                .with_user(UpdateKind::OnlyIfNotSet),
        );
        self.primed = true;
    }

    /// Build a sorted, filtered snapshot for the UI.
    ///
    /// `query` is matched case-insensitively against the process name and pid.
    pub fn snapshot(&self, query: &str, key: SortKey, descending: bool) -> Vec<ProcInfo> {
        let q = query.trim().to_ascii_lowercase();

        let mut rows: Vec<ProcInfo> = self
            .system
            .processes()
            .values()
            .map(|p| ProcInfo {
                pid: p.pid().as_u32(),
                parent: p.parent().map(|pp| pp.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                cpu: if self.primed { p.cpu_usage() } else { 0.0 },
                memory: p.memory(),
                user: p
                    .user_id()
                    .map(|uid| uid.to_string()),
            })
            .filter(|r| {
                q.is_empty()
                    || r.name.to_ascii_lowercase().contains(&q)
                    || r.pid.to_string().contains(&q)
            })
            .collect();

        rows.sort_by(|a, b| {
            let ord = match key {
                SortKey::Name => a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()),
                SortKey::Pid => a.pid.cmp(&b.pid),
                SortKey::Cpu => a.cpu.partial_cmp(&b.cpu).unwrap_or(std::cmp::Ordering::Equal),
                SortKey::Memory => a.memory.cmp(&b.memory),
            };
            if descending { ord.reverse() } else { ord }
        });

        rows
    }

    /// Total number of processes currently tracked (pre-filter).
    pub fn count(&self) -> usize {
        self.system.processes().len()
    }

    /// Ask the OS to terminate a process.
    ///
    /// This is the one genuinely privileged action in the app. It is the normal,
    /// legitimate Task-Manager "End task" operation: it only ever targets a
    /// process the user explicitly selected, and it never touches another
    /// process's memory. Returns `false` if the pid is gone or the OS refused
    /// (e.g. insufficient privileges for a system process).
    pub fn kill(&self, pid: u32) -> bool {
        match self.system.process(Pid::from_u32(pid)) {
            Some(p) => p.kill(),
            None => false,
        }
    }
}

impl Default for ProcessMonitor {
    fn default() -> Self {
        Self::new()
    }
}