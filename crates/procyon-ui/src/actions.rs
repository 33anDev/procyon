//! OS-level process actions: priority, suspend/resume, terminate, reveal in
//! file manager.
//!
//! Everything that calls Win32 directly lives here, behind
//! `#[cfg(target_os = "windows")]`, so the unsafe surface is small and easy to
//! audit. Each function opens the target with the *minimum* access right it
//! needs, does one thing, and closes the handle (see [`OwnedHandle`]).
//!
//! On other platforms the Windows-only actions return a friendly error and
//! [`probe`] reports them as unavailable, so the UI greys them out.

use std::path::Path;

/// Windows priority classes, in the order Task Manager lists them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Priority {
    Realtime,
    High,
    AboveNormal,
    Normal,
    BelowNormal,
    Idle,
}

impl Priority {
    pub const ALL: [Priority; 6] = [
        Priority::Realtime,
        Priority::High,
        Priority::AboveNormal,
        Priority::Normal,
        Priority::BelowNormal,
        Priority::Idle,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Priority::Realtime => "Realtime",
            Priority::High => "High",
            Priority::AboveNormal => "Above normal",
            Priority::Normal => "Normal",
            Priority::BelowNormal => "Below normal",
            Priority::Idle => "Idle",
        }
    }
}

/// Which actions the current user is actually allowed to perform on a
/// process. Determined by trying to open it with each access right, so the
/// context menu can grey out what would fail (e.g. protected system
/// processes, or elevated processes when Procyon isn't elevated).
#[derive(Clone, Copy, Default)]
pub struct Access {
    pub set_priority: bool,
    pub suspend: bool,
    pub terminate: bool,
}

impl Access {
    pub fn any_denied(&self) -> bool {
        !(self.set_priority && self.suspend && self.terminate)
    }
}

/// Open the folder containing `exe` in the platform file manager, with the
/// file selected where the platform supports it.
pub fn reveal_in_file_manager(exe: &Path) -> Result<(), String> {
    use std::process::Command;

    #[cfg(target_os = "windows")]
    let result = {
        use std::os::windows::process::CommandExt;
        // `raw_arg` because explorer has its own argument parsing: it wants
        // `/select,"C:\path with spaces\app.exe"` exactly, which Rust's
        // standard quoting would mangle.
        Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", exe.display()))
            .spawn()
    };

    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg("-R").arg(exe).spawn();

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let result = match exe.parent() {
        Some(dir) => Command::new("xdg-open").arg(dir).spawn(),
        None => return Err("executable has no parent folder".into()),
    };

    result.map(|_| ()).map_err(|e| format!("could not open file manager: {e}"))
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
pub use win::{current_priority, probe, resume, set_priority, suspend, terminate};
/// Shared with `details.rs`, which reads (never modifies) process state.
#[cfg(target_os = "windows")]
pub(crate) use win::{open_process, OwnedHandle};

#[cfg(target_os = "windows")]
mod win {
    use super::{Access, Priority};

    use windows::core::Error;
    use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, E_INVALIDARG, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::Threading::{
        GetPriorityClass, OpenProcess, OpenThread, ResumeThread, SetPriorityClass,
        SuspendThread, TerminateProcess, ABOVE_NORMAL_PRIORITY_CLASS,
        BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS,
        NORMAL_PRIORITY_CLASS, PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME,
        PROCESS_TERMINATE, REALTIME_PRIORITY_CLASS, THREAD_SUSPEND_RESUME,
    };

    /// A Win32 handle that is closed when dropped, so no early-return path
    /// can leak it.
    pub(crate) struct OwnedHandle(pub(crate) HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: we own this handle and close it exactly once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub(crate) fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Result<OwnedHandle, Error> {
        // SAFETY: plain FFI call; the returned handle is wrapped immediately.
        unsafe { OpenProcess(access, false, pid).map(OwnedHandle) }
    }

    /// Turn a Win32 error into a short message for the status bar.
    fn describe(e: &Error) -> String {
        if e.code() == E_ACCESSDENIED {
            "access denied (protected process, or needs admin)".into()
        } else if e.code() == E_INVALIDARG {
            "process no longer exists".into()
        } else {
            e.message().trim().to_string()
        }
    }

    fn to_class(p: Priority) -> PROCESS_CREATION_FLAGS {
        match p {
            Priority::Realtime => REALTIME_PRIORITY_CLASS,
            Priority::High => HIGH_PRIORITY_CLASS,
            Priority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            Priority::Normal => NORMAL_PRIORITY_CLASS,
            Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            Priority::Idle => IDLE_PRIORITY_CLASS,
        }
    }

    fn from_class(class: u32) -> Option<Priority> {
        Priority::ALL.into_iter().find(|&p| to_class(p).0 == class)
    }

    /// Try each access right we need; cheap (a few syscalls) and only called
    /// while a context menu is open.
    pub fn probe(pid: u32) -> Access {
        let can = |right| open_process(pid, right).is_ok();
        Access {
            set_priority: can(PROCESS_SET_INFORMATION),
            suspend: can(PROCESS_SUSPEND_RESUME),
            terminate: can(PROCESS_TERMINATE),
        }
    }

    /// Read a process's current priority class, if we're allowed to.
    pub fn current_priority(pid: u32) -> Option<Priority> {
        let h = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
        // SAFETY: valid handle with query rights. Returns 0 on failure.
        let class = unsafe { GetPriorityClass(h.0) };
        from_class(class)
    }

    /// Set a process's priority class, then read it back to confirm what
    /// Windows actually applied.
    ///
    /// Returns the class now in effect. Note that Windows *silently* turns a
    /// Realtime request into High when the caller lacks the "increase
    /// scheduling priority" privilege (i.e. isn't running as admin), so the
    /// read-back matters.
    pub fn set_priority(pid: u32, p: Priority) -> Result<Priority, String> {
        let h = open_process(pid, PROCESS_SET_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION)
            .map_err(|e| describe(&e))?;
        // SAFETY: valid handle opened with PROCESS_SET_INFORMATION.
        unsafe { SetPriorityClass(h.0, to_class(p)) }.map_err(|e| describe(&e))?;
        // SAFETY: same handle, also opened with query rights.
        let applied = from_class(unsafe { GetPriorityClass(h.0) });
        applied.ok_or_else(|| "priority set, but could not read it back".into())
    }

    /// Terminate a process (exit code 1), like Task Manager's "End task".
    pub fn terminate(pid: u32) -> Result<(), String> {
        let h = open_process(pid, PROCESS_TERMINATE).map_err(|e| describe(&e))?;
        // SAFETY: valid handle opened with PROCESS_TERMINATE.
        unsafe { TerminateProcess(h.0, 1) }.map_err(|e| describe(&e))
    }

    /// Suspend every thread in the process.
    ///
    /// Uses only documented APIs: enumerate the process's threads with a
    /// ToolHelp snapshot and call `SuspendThread` on each. Windows keeps a
    /// per-thread suspend *count*, so suspending twice needs two resumes.
    /// A thread created between the snapshot and the suspend isn't caught;
    /// re-run Suspend if that matters.
    pub fn suspend(pid: u32) -> Result<(), String> {
        // SAFETY: valid thread handle with THREAD_SUSPEND_RESUME;
        // u32::MAX is SuspendThread's documented failure value.
        for_each_thread(pid, |t| unsafe { SuspendThread(t) } != u32::MAX)
    }

    /// Resume every thread in the process (decrements each suspend count once).
    pub fn resume(pid: u32) -> Result<(), String> {
        // SAFETY: as above.
        for_each_thread(pid, |t| unsafe { ResumeThread(t) } != u32::MAX)
    }

    /// Open each thread of `pid` with suspend/resume rights and run `op` on it.
    /// Succeeds if `op` succeeded on at least one thread.
    fn for_each_thread(pid: u32, op: impl Fn(HANDLE) -> bool) -> Result<(), String> {
        // Fail fast with a clear message if the process itself is off-limits.
        open_process(pid, PROCESS_SUSPEND_RESUME).map_err(|e| describe(&e))?;

        // SAFETY: snapshot of all threads system-wide; handle is wrapped.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }
            .map(OwnedHandle)
            .map_err(|e| describe(&e))?;

        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };

        let (mut ok, mut failed) = (0usize, 0usize);
        // SAFETY: `entry` is a properly sized THREADENTRY32 owned by us.
        let mut more = unsafe { Thread32First(snap.0, &mut entry) }.is_ok();
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: FFI call; the handle is wrapped immediately.
                match unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) } {
                    Ok(t) => {
                        let t = OwnedHandle(t);
                        if op(t.0) { ok += 1 } else { failed += 1 }
                    }
                    Err(_) => failed += 1,
                }
            }
            // SAFETY: as above.
            more = unsafe { Thread32Next(snap.0, &mut entry) }.is_ok();
        }

        match (ok, failed) {
            (0, 0) => Err("process has no threads (already exited?)".into()),
            (0, _) => Err("access denied on all threads".into()),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Non-Windows fallbacks
// ---------------------------------------------------------------------------

/// Priority classes are a Windows concept; elsewhere this is unavailable.
#[cfg(not(target_os = "windows"))]
pub fn set_priority(_pid: u32, _p: Priority) -> Result<Priority, String> {
    Err("setting priority is only supported on Windows".into())
}

#[cfg(not(target_os = "windows"))]
pub fn current_priority(_pid: u32) -> Option<Priority> {
    None
}

/// Without a cheap way to probe, assume suspend/terminate may work (failures
/// are still reported in the status bar) and disable priority.
#[cfg(not(target_os = "windows"))]
pub fn probe(_pid: u32) -> Access {
    Access { set_priority: false, suspend: true, terminate: true }
}

// ---------------------------------------------------------------------------
// Tests (Windows): exercise the real Win32 calls against a throwaway child
// process that we own, so no admin rights are needed.
// ---------------------------------------------------------------------------

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    /// A harmless long-running child: `ping` waiting ~30s. Killed on drop.
    struct TestChild(Child);

    impl TestChild {
        fn spawn() -> Self {
            let child = Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdout(Stdio::null())
                .spawn()
                .expect("spawn test process");
            TestChild(child)
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn set_priority_takes_effect() {
        let child = TestChild::spawn();
        assert_eq!(current_priority(child.pid()), Some(Priority::Normal));

        for p in [Priority::BelowNormal, Priority::Idle, Priority::AboveNormal, Priority::High] {
            let applied = set_priority(child.pid(), p).expect("set priority on own child");
            assert_eq!(applied, p);
            assert_eq!(current_priority(child.pid()), Some(p), "read-back mismatch for {p:?}");
        }
    }

    #[test]
    fn probe_own_child_has_full_access() {
        let child = TestChild::spawn();
        let a = probe(child.pid());
        assert!(a.set_priority && a.suspend && a.terminate);
    }

    #[test]
    fn protected_system_process_is_denied() {
        // PID 4 is the "System" process: never accessible from user mode.
        let a = probe(4);
        assert!(!a.set_priority && !a.terminate);
        assert!(set_priority(4, Priority::Normal).is_err());
    }

    #[test]
    fn suspend_resume_and_terminate() {
        let mut child = TestChild::spawn();
        suspend(child.pid()).expect("suspend");
        resume(child.pid()).expect("resume");
        terminate(child.pid()).expect("terminate");
        let status = child.0.wait().expect("wait");
        assert_eq!(status.code(), Some(1), "TerminateProcess exit code");
    }
}
