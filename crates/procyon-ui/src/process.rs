//! Process data layer.
//!
//! Thin wrapper around `sysinfo` that produces an immutable snapshot the UI can
//! sort and filter without holding a lock on the live system handle. OS-level
//! *actions* (priority, suspend, terminate) live in `actions.rs`; this module
//! only reads, plus dispatching kills to the right platform backend.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

/// One row in the process table. Owned, cheap to clone, independent of the
/// live `System` so the UI can sort/filter freely.
#[derive(Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    /// Full path of the executable. `None` for protected/system processes
    /// whose image path we're not allowed to read.
    pub exe: Option<PathBuf>,
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

/// Machine-wide numbers for the header.
#[derive(Clone, Copy, Default)]
pub struct SystemStats {
    /// Average CPU usage across all cores, 0..=100.
    pub cpu: f32,
    pub used_memory: u64,
    pub total_memory: u64,
}

/// Owns the live `sysinfo::System` and hands out snapshots on demand.
pub struct ProcessMonitor {
    system: System,
    /// CPU% is only meaningful after two refreshes spaced in time, so we skip
    /// reporting it on the very first pass to avoid showing bogus spikes.
    primed: bool,
}

/// What we ask sysinfo to read per process. Kept in one place so the initial
/// read and every refresh agree. Fields that never change for a process's
/// lifetime (user, exe path) are only read once.
fn process_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::new()
        .with_cpu()
        .with_memory()
        .with_user(UpdateKind::OnlyIfNotSet)
        .with_exe(UpdateKind::OnlyIfNotSet)
}

impl ProcessMonitor {
    pub fn new() -> Self {
        let system = System::new_with_specifics(
            RefreshKind::new().with_processes(process_refresh_kind()),
        );
        Self { system, primed: false }
    }

    /// Re-read the process list and global stats from the OS. Call on a timer.
    pub fn refresh(&mut self) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true, // remove processes that have since exited
            process_refresh_kind(),
        );
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
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
            .map(|p| self.to_info(p))
            .filter(|r| {
                q.is_empty()
                    || r.name.to_ascii_lowercase().contains(&q)
                    || r.pid.to_string().contains(&q)
            })
            .collect();

        rows.sort_by(|a, b| row_order(a, b, key, descending));

        rows
    }

    /// Refresh the values that change over a process's lifetime (CPU%,
    /// memory) on rows that already exist, matched by PID, **without**
    /// changing their order or which rows are present.
    ///
    /// Used while the table is frozen (pointer hovering, or Pause engaged):
    /// the numbers stay live but rows don't jump, so the user can aim at a
    /// row and click it. Rows whose process has since exited keep their last
    /// values until the table unfreezes and is rebuilt.
    pub fn refresh_values(&self, rows: &mut [ProcInfo]) {
        for r in rows.iter_mut() {
            if let Some(p) = self.system.process(Pid::from_u32(r.pid)) {
                if self.primed {
                    r.cpu = p.cpu_usage();
                }
                r.memory = p.memory();
            }
        }
    }

    /// Live data for one process, regardless of the current filter (the
    /// detail panel keeps showing a process even if it's filtered out).
    pub fn info(&self, pid: u32) -> Option<ProcInfo> {
        self.system.process(Pid::from_u32(pid)).map(|p| self.to_info(p))
    }

    /// Name of `pid`'s parent, if the parent is still alive and really is
    /// its parent (see [`is_true_parent`] on PID reuse).
    pub fn parent_name(&self, pid: u32) -> Option<String> {
        let procs = self.system.processes();
        let child = procs.get(&Pid::from_u32(pid))?;
        let parent = procs.get(&child.parent()?)?;
        is_true_parent(parent, child).then(|| parent.name().to_string_lossy().into_owned())
    }

    fn to_info(&self, p: &Process) -> ProcInfo {
        ProcInfo {
            pid: p.pid().as_u32(),
            parent: p.parent().map(|pp| pp.as_u32()),
            name: p.name().to_string_lossy().into_owned(),
            exe: p.exe().map(|e| e.to_path_buf()),
            cpu: if self.primed { p.cpu_usage() } else { 0.0 },
            memory: p.memory(),
            user: p.user_id().map(|uid| uid.to_string()),
        }
    }

    /// Total number of processes currently tracked (pre-filter).
    pub fn count(&self) -> usize {
        self.system.processes().len()
    }

    pub fn stats(&self) -> SystemStats {
        SystemStats {
            cpu: if self.primed { self.system.global_cpu_usage() } else { 0.0 },
            used_memory: self.system.used_memory(),
            total_memory: self.system.total_memory(),
        }
    }

    /// `root` plus all of its descendants, ordered children-before-parents so
    /// that killing in this order never orphans a still-running subtree.
    ///
    /// Only trustworthy parent links are followed (see [`is_true_parent`]).
    pub fn tree(&self, root: u32) -> Vec<u32> {
        let procs = self.system.processes();

        // parent pid -> child pids, using only trustworthy links.
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in procs.values() {
            if let Some(parent_pid) = p.parent() {
                let parent_ok = procs.get(&parent_pid).is_some_and(|parent| is_true_parent(parent, p));
                if parent_ok {
                    children.entry(parent_pid.as_u32()).or_default().push(p.pid().as_u32());
                }
            }
        }

        // Iterative post-order walk; `seen` guards against any cycle.
        let mut order = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![(root, false)];
        while let Some((pid, expanded)) = stack.pop() {
            if expanded {
                order.push(pid);
            } else if seen.insert(pid) {
                stack.push((pid, true));
                for &c in children.get(&pid).into_iter().flatten() {
                    stack.push((c, false));
                }
            }
        }
        order
    }

    /// Ask the OS to terminate a process.
    ///
    /// This is the normal Task-Manager "End task" operation: it only ever
    /// targets a process the user explicitly selected and confirmed, and it
    /// never touches another process's memory.
    pub fn kill(&self, pid: u32) -> Result<(), String> {
        #[cfg(target_os = "windows")]
        {
            crate::actions::terminate(pid)
        }
        #[cfg(not(target_os = "windows"))]
        {
            match self.system.process(Pid::from_u32(pid)) {
                Some(p) if p.kill() => Ok(()),
                Some(_) => Err("access denied".into()),
                None => Err("process no longer exists".into()),
            }
        }
    }

    /// Suspend every thread of a process. On non-Windows this sends SIGSTOP.
    pub fn suspend(&self, pid: u32) -> Result<(), String> {
        guard_self(pid)?;
        #[cfg(target_os = "windows")]
        {
            crate::actions::suspend(pid)
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.signal(pid, sysinfo::Signal::Stop)
        }
    }

    /// Resume a suspended process. On non-Windows this sends SIGCONT.
    pub fn resume(&self, pid: u32) -> Result<(), String> {
        #[cfg(target_os = "windows")]
        {
            crate::actions::resume(pid)
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.signal(pid, sysinfo::Signal::Continue)
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn signal(&self, pid: u32, sig: sysinfo::Signal) -> Result<(), String> {
        match self.system.process(Pid::from_u32(pid)) {
            Some(p) => match p.kill_with(sig) {
                Some(true) => Ok(()),
                Some(false) => Err("access denied".into()),
                None => Err("not supported on this platform".into()),
            },
            None => Err("process no longer exists".into()),
        }
    }

    /// Whether `pid` is still alive (used to validate pending dialogs).
    pub fn exists(&self, pid: u32) -> bool {
        self.system.process(Pid::from_u32(pid)).is_some()
    }
}

/// Windows recycles PIDs, so a process's recorded parent PID may now belong
/// to an unrelated, *newer* process. A parent link is only trusted if the
/// parent started no later than the child, which rules that out.
fn is_true_parent(parent: &Process, child: &Process) -> bool {
    parent.pid() != child.pid() && parent.start_time() <= child.start_time()
}

/// Order two rows by the active sort column and direction, then break ties
/// by PID (always ascending). The PID tie-break makes the order total and
/// deterministic, so rows with equal CPU/memory/name never swap places from
/// one refresh to the next.
fn row_order(a: &ProcInfo, b: &ProcInfo, key: SortKey, descending: bool) -> std::cmp::Ordering {
    let ord = match key {
        SortKey::Name => a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()),
        SortKey::Pid => a.pid.cmp(&b.pid),
        SortKey::Cpu => a.cpu.partial_cmp(&b.cpu).unwrap_or(std::cmp::Ordering::Equal),
        SortKey::Memory => a.memory.cmp(&b.memory),
    };
    let ord = if descending { ord.reverse() } else { ord };
    ord.then(a.pid.cmp(&b.pid))
}

/// Suspending ourselves would freeze the UI with no way to resume.
fn guard_self(pid: u32) -> Result<(), String> {
    if pid == std::process::id() {
        Err("refusing to suspend Procyon itself".into())
    } else {
        Ok(())
    }
}

impl Default for ProcessMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, name: &str, cpu: f32, memory: u64) -> ProcInfo {
        ProcInfo { pid, parent: None, name: name.into(), exe: None, cpu, memory, user: None }
    }

    fn sorted(mut rows: Vec<ProcInfo>, key: SortKey, descending: bool) -> Vec<u32> {
        rows.sort_by(|a, b| row_order(a, b, key, descending));
        rows.iter().map(|r| r.pid).collect()
    }

    #[test]
    fn equal_values_break_ties_by_pid() {
        // Three processes with identical CPU: order must be by PID, the same
        // in both directions, so hovering never sees them swap.
        let rows = vec![row(30, "c", 5.0, 0), row(10, "a", 5.0, 0), row(20, "b", 5.0, 0)];
        assert_eq!(sorted(rows.clone(), SortKey::Cpu, true), [10, 20, 30]);
        assert_eq!(sorted(rows, SortKey::Cpu, false), [10, 20, 30]);
    }

    #[test]
    fn duplicate_names_order_by_pid() {
        // Four "svchost.exe"-style duplicates stay in PID order under a Name
        // sort, regardless of input order.
        let rows = vec![row(44, "svc", 0.0, 0), row(11, "svc", 0.0, 0), row(22, "svc", 0.0, 0)];
        assert_eq!(sorted(rows, SortKey::Name, false), [11, 22, 44]);
    }

    #[test]
    fn primary_key_still_wins() {
        let rows = vec![row(10, "a", 1.0, 0), row(20, "b", 9.0, 0)];
        assert_eq!(sorted(rows.clone(), SortKey::Cpu, true), [20, 10]);
        assert_eq!(sorted(rows, SortKey::Cpu, false), [10, 20]);
    }
}
