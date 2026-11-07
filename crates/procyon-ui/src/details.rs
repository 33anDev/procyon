//! In-depth data for the per-process detail panel.
//!
//! The table's [`ProcInfo`](crate::process::ProcInfo) only carries what is
//! cheap to read for *every* process. The detail panel shows one process in
//! depth: command line, token integrity and elevation, owning account,
//! thread/handle counts, suspended state and start time. Those take several
//! Win32/NT calls, and the account lookup can stall for seconds on a
//! domain-joined machine, so they're fetched on a background worker:
//!
//! - The UI calls [`DetailFetcher::request`] when the panel opens, when it
//!   switches to another process, and on each periodic refresh. It never
//!   requests every frame.
//! - The worker answers with a [`ProcDetails`], which the UI picks up with
//!   [`DetailFetcher::poll`] and keeps until the next answer arrives.
//!
//! Every value is a [`Field`], so one value the OS refuses (protected
//! process, or Procyon not elevated) shows as "Access denied" instead of
//! failing the whole panel. On non-Windows platforms every field is
//! [`Missing::Unavailable`].

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::SystemTime;

use crate::actions::Priority;

/// Why a value couldn't be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// The OS refused: protected process, or Procyon needs to run as admin.
    Denied,
    /// Not available on this platform, not applicable to this process (e.g.
    /// kernel processes have no command line), or the query failed.
    Unavailable,
}

/// One value in the detail panel.
pub type Field<T> = Result<T, Missing>;

/// Mandatory integrity level of a process token, lowest to highest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Integrity {
    Untrusted,
    Low,
    Medium,
    MediumPlus,
    High,
    System,
    Protected,
}

impl Integrity {
    /// Map the last sub-authority (RID) of the token's mandatory-label SID
    /// to a level. Uses ranges, as Windows does, so in-between values round
    /// down to the nearest named level.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn from_rid(rid: u32) -> Self {
        match rid {
            0..=0x0FFF => Integrity::Untrusted,
            0x1000..=0x1FFF => Integrity::Low,
            0x2000..=0x20FF => Integrity::Medium,
            0x2100..=0x2FFF => Integrity::MediumPlus,
            0x3000..=0x3FFF => Integrity::High,
            0x4000..=0x4FFF => Integrity::System,
            _ => Integrity::Protected,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Integrity::Untrusted => "Untrusted",
            Integrity::Low => "Low",
            Integrity::Medium => "Medium",
            Integrity::MediumPlus => "Medium+",
            Integrity::High => "High",
            Integrity::System => "System",
            Integrity::Protected => "Protected",
        }
    }
}

/// When a process started.
#[derive(Clone, Debug)]
pub struct Started {
    /// For computing uptime live in the UI.
    pub at: SystemTime,
    /// Pre-formatted local wall-clock time, e.g. `2026-11-07 09:14:03`.
    pub local: Option<String>,
}

/// Everything the detail panel shows that isn't already in `ProcInfo`.
#[derive(Clone, Debug)]
pub struct ProcDetails {
    /// The process these details belong to. Answers for a process the panel
    /// has since moved away from are discarded by this.
    pub pid: u32,
    /// `true` when every thread is suspended.
    pub suspended: Field<bool>,
    pub threads: Field<u32>,
    pub handles: Field<u32>,
    pub priority: Field<Priority>,
    pub integrity: Field<Integrity>,
    pub elevated: Field<bool>,
    pub started: Field<Started>,
    pub command_line: Field<String>,
    /// Owning account as `DOMAIN\user`.
    pub account: Field<String>,
}

impl ProcDetails {
    /// Details with every field missing for the same reason.
    fn missing(pid: u32, why: Missing) -> Self {
        Self {
            pid,
            suspended: Err(why),
            threads: Err(why),
            handles: Err(why),
            priority: Err(why),
            integrity: Err(why),
            elevated: Err(why),
            started: Err(why),
            command_line: Err(why),
            account: Err(why),
        }
    }
}

/// SID bytes -> resolved account name. Account lookups are the slowest part
/// of a fetch and a SID's name doesn't change, so each is resolved once.
type AccountCache = HashMap<Vec<u8>, Field<String>>;

/// Handle to the background detail worker.
pub struct DetailFetcher {
    /// `None` if the worker thread couldn't be started; the panel then shows
    /// only the live table data.
    requests: Option<Sender<u32>>,
    results: Option<Receiver<ProcDetails>>,
}

impl DetailFetcher {
    pub fn new(ctx: &egui::Context) -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<u32>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<ProcDetails>();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("procyon-details".into())
            .spawn(move || worker(req_rx, res_tx, ctx));
        match spawned {
            Ok(_) => Self { requests: Some(req_tx), results: Some(res_rx) },
            Err(_) => Self { requests: None, results: None },
        }
    }

    /// Ask for a fresh read of `pid`. Returns immediately.
    pub fn request(&self, pid: u32) {
        if let Some(tx) = &self.requests {
            let _ = tx.send(pid);
        }
    }

    /// The newest finished fetch for `pid`, if one arrived since the last
    /// poll. Answers for other PIDs (the panel moved on) are dropped.
    pub fn poll(&self, pid: u32) -> Option<ProcDetails> {
        self.results.as_ref()?.try_iter().filter(|d| d.pid == pid).last()
    }
}

/// Worker loop: fetch on request, send back, nudge the UI to repaint.
/// Ends when the `DetailFetcher` (and so the request sender) is dropped.
fn worker(requests: Receiver<u32>, results: Sender<ProcDetails>, ctx: egui::Context) {
    let mut accounts = AccountCache::new();
    while let Ok(mut pid) = requests.recv() {
        // If requests piled up (e.g. the user arrowed through rows), only
        // the newest one matters.
        while let Ok(newer) = requests.try_recv() {
            pid = newer;
        }
        if results.send(fetch(pid, &mut accounts)).is_err() {
            break;
        }
        ctx.request_repaint();
    }
}

#[cfg(target_os = "windows")]
use win::fetch;

#[cfg(not(target_os = "windows"))]
fn fetch(pid: u32, _accounts: &mut AccountCache) -> ProcDetails {
    ProcDetails::missing(pid, Missing::Unavailable)
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod win {
    use super::{AccountCache, Field, Integrity, Missing, ProcDetails, Started};

    use std::time::{Duration, UNIX_EPOCH};

    use windows::core::{Error, PCWSTR, PWSTR};
    use windows::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemProcessInformation,
    };
    use windows::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessCommandLineInformation,
    };
    use windows::Win32::Foundation::{
        E_ACCESSDENIED, FILETIME, HANDLE, NTSTATUS, STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW,
        STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH, SYSTEMTIME, UNICODE_STRING,
    };
    use windows::Win32::Security::{
        GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
        LookupAccountSidW, TokenElevation, TokenIntegrityLevel, TokenUser, PSID, SID_NAME_USE,
        TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    use windows::Win32::System::WindowsProgramming::{
        SYSTEM_PROCESS_INFORMATION, SYSTEM_THREAD_INFORMATION,
    };

    use crate::actions::{open_process, OwnedHandle};

    /// `KTHREAD_STATE::Waiting` and `KWAIT_REASON::Suspended`: a thread in
    /// this state/reason pair is suspended.
    const THREAD_STATE_WAITING: u32 = 5;
    const WAIT_REASON_SUSPENDED: u32 = 5;

    /// Read everything the panel needs for `pid`. Never fails as a whole:
    /// each value reports its own [`Missing`] reason.
    pub fn fetch(pid: u32, accounts: &mut AccountCache) -> ProcDetails {
        // Everything except the thread summary needs a handle. Query-limited
        // is the weakest right and is granted for nearly every process,
        // including most protected ones.
        let mut d = match open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) {
            Ok(process) => with_handle(pid, &process, accounts),
            Err(e) => ProcDetails::missing(pid, missing(&e)),
        };

        // Thread/handle counts and suspended state come from the system-wide
        // process list, which needs no handle to the target, so they work
        // even for processes we can't open. If the PID isn't in the list,
        // the process has just exited.
        let summary = thread_summary(pid);
        d.threads = summary.as_ref().map(|s| s.threads).ok_or(Missing::Unavailable);
        d.handles = summary.as_ref().map(|s| s.handles).ok_or(Missing::Unavailable);
        d.suspended = summary.as_ref().map(|s| s.suspended).ok_or(Missing::Unavailable);
        d
    }

    /// The fields that need an open process handle.
    fn with_handle(pid: u32, process: &OwnedHandle, accounts: &mut AccountCache) -> ProcDetails {
        let mut d = ProcDetails::missing(pid, Missing::Unavailable);
        d.priority = crate::actions::current_priority(pid).ok_or(Missing::Unavailable);
        d.started = start_time(process);
        d.command_line = command_line(process);

        // The token is guarded separately from the process: protected
        // processes often allow query-limited access to the process but not
        // to the token.
        match open_token(process) {
            Ok(token) => {
                d.integrity = integrity(&token);
                d.elevated = elevated(&token);
                d.account = account(&token, accounts);
            }
            Err(why) => {
                d.integrity = Err(why);
                d.elevated = Err(why);
                d.account = Err(why);
            }
        }
        d
    }

    fn missing(e: &Error) -> Missing {
        if e.code() == E_ACCESSDENIED {
            Missing::Denied
        } else {
            Missing::Unavailable
        }
    }

    fn nt_missing(status: NTSTATUS) -> Missing {
        if status == STATUS_ACCESS_DENIED {
            Missing::Denied
        } else {
            Missing::Unavailable
        }
    }

    // --- Threads, handles, suspended ----------------------------------------

    struct ThreadSummary {
        threads: u32,
        handles: u32,
        suspended: bool,
    }

    /// Find `pid` in the kernel's process list and summarize its threads.
    fn thread_summary(pid: u32) -> Option<ThreadSummary> {
        let buf = system_process_list()?;
        let len = buf.len() * 8;
        let base = buf.as_ptr().cast::<u8>();
        let entry_size = std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>();
        let thread_size = std::mem::size_of::<SYSTEM_THREAD_INFORMATION>();

        let mut offset = 0usize;
        while offset + entry_size <= len {
            // SAFETY: bounds-checked above; the kernel 8-aligns every entry
            // and `buf` itself is 8-aligned (Vec<u64>).
            let entry = unsafe { &*base.add(offset).cast::<SYSTEM_PROCESS_INFORMATION>() };

            if entry.UniqueProcessId.0 as usize == pid as usize {
                let n = entry.NumberOfThreads as usize;
                // The entry's thread array follows it directly.
                if offset + entry_size + n * thread_size > len {
                    return None;
                }
                // SAFETY: bounds-checked just above.
                let threads = unsafe {
                    std::slice::from_raw_parts(
                        base.add(offset + entry_size).cast::<SYSTEM_THREAD_INFORMATION>(),
                        n,
                    )
                };
                let suspended = !threads.is_empty()
                    && threads.iter().all(|t| {
                        t.ThreadState == THREAD_STATE_WAITING && t.WaitReason == WAIT_REASON_SUSPENDED
                    });
                return Some(ThreadSummary {
                    threads: entry.NumberOfThreads,
                    handles: entry.HandleCount,
                    suspended,
                });
            }

            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
        None
    }

    /// One snapshot of every process and thread on the system, as filled by
    /// `NtQuerySystemInformation(SystemProcessInformation)`. Typically a few
    /// hundred KiB. Returned as `Vec<u64>` so the buffer is 8-aligned.
    fn system_process_list() -> Option<Vec<u64>> {
        let mut bytes: u32 = 512 * 1024;
        // Processes can start between the size query and the read, so retry
        // a few times with headroom.
        for _ in 0..5 {
            let mut buf = vec![0u64; (bytes as usize).div_ceil(8)];
            let mut needed = 0u32;
            // SAFETY: `buf` is writable for the length we pass.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                    &mut needed,
                )
            };
            if status.is_ok() {
                return Some(buf);
            }
            if status != STATUS_INFO_LENGTH_MISMATCH {
                return None;
            }
            bytes = needed.max(bytes) + 64 * 1024;
        }
        None
    }

    // --- Command line --------------------------------------------------------

    /// Read the full launch command line.
    ///
    /// Uses `ProcessCommandLineInformation` (Windows 8.1+), which returns a
    /// `UNICODE_STRING` followed by its characters. Unlike reading the
    /// target's PEB, it needs only query-limited rights and no
    /// `ReadProcessMemory`, and it works for 32-bit (WOW64) targets.
    fn command_line(process: &OwnedHandle) -> Field<String> {
        let mut buf = vec![0u64; 512]; // 4 KiB; most command lines fit
        for _ in 0..3 {
            let mut needed = 0u32;
            // SAFETY: `buf` is writable for the length we pass.
            let status = unsafe {
                NtQueryInformationProcess(
                    process.0,
                    ProcessCommandLineInformation,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                    &mut needed,
                )
            };
            if status.is_ok() {
                // SAFETY: on success the buffer starts with a UNICODE_STRING
                // whose `Buffer` points into the same allocation.
                let text = unsafe {
                    let s = &*buf.as_ptr().cast::<UNICODE_STRING>();
                    if s.Buffer.is_null() || s.Length == 0 {
                        return Ok(String::new());
                    }
                    std::slice::from_raw_parts(s.Buffer.0, s.Length as usize / 2)
                };
                return Ok(String::from_utf16_lossy(text));
            }
            let too_small = [STATUS_INFO_LENGTH_MISMATCH, STATUS_BUFFER_TOO_SMALL, STATUS_BUFFER_OVERFLOW];
            if !too_small.contains(&status) || needed == 0 {
                // Kernel processes (System, Registry, ...) have no command
                // line at all: that's "unavailable", not an error.
                return Err(nt_missing(status));
            }
            buf = vec![0u64; (needed as usize).div_ceil(8)];
        }
        Err(Missing::Unavailable)
    }

    // --- Start time ------------------------------------------------------------

    /// FILETIME ticks (100 ns since 1601-01-01 UTC) at the Unix epoch.
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

    fn start_time(process: &OwnedHandle) -> Field<Started> {
        let (mut created, mut exited, mut kernel, mut user) = Default::default();
        // SAFETY: valid handle with query rights; all four out-params are ours.
        unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) }
            .map_err(|e| missing(&e))?;

        let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        // The System Idle Process reports 0.
        if ticks <= UNIX_EPOCH_TICKS {
            return Err(Missing::Unavailable);
        }
        let at = UNIX_EPOCH + Duration::from_nanos((ticks - UNIX_EPOCH_TICKS) * 100);
        Ok(Started { at, local: local_time(&created) })
    }

    /// Format a FILETIME as local `YYYY-MM-DD HH:MM:SS`.
    fn local_time(ft: &FILETIME) -> Option<String> {
        let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
        // SAFETY: plain conversions between caller-owned structs.
        unsafe {
            FileTimeToSystemTime(ft, &mut utc).ok()?;
            SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
        }
        Some(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
        ))
    }

    // --- Token: integrity, elevation, account --------------------------------

    fn open_token(process: &OwnedHandle) -> Field<OwnedHandle> {
        let mut token = HANDLE::default();
        // SAFETY: valid process handle; the token handle is wrapped at once.
        unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) }.map_err(|e| missing(&e))?;
        Ok(OwnedHandle(token))
    }

    /// Read one token information class into an 8-aligned buffer.
    fn token_info(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> Field<Vec<u64>> {
        let mut len = 0u32;
        // First call only reports the size needed; it "fails" with
        // ERROR_INSUFFICIENT_BUFFER by design.
        // SAFETY: no buffer passed; only `len` is written.
        let _ = unsafe { GetTokenInformation(token.0, class, None, 0, &mut len) };
        if len == 0 {
            return Err(Missing::Unavailable);
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `len` bytes.
        unsafe { GetTokenInformation(token.0, class, Some(buf.as_mut_ptr().cast()), len, &mut len) }
            .map_err(|e| missing(&e))?;
        Ok(buf)
    }

    fn integrity(token: &OwnedHandle) -> Field<Integrity> {
        let buf = token_info(token, TokenIntegrityLevel)?;
        // SAFETY: the buffer holds a TOKEN_MANDATORY_LABEL whose SID points
        // into the same buffer, which outlives this block.
        let rid = unsafe {
            let sid = (*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid;
            let count = *GetSidSubAuthorityCount(sid);
            if count == 0 {
                return Err(Missing::Unavailable);
            }
            *GetSidSubAuthority(sid, u32::from(count) - 1)
        };
        Ok(Integrity::from_rid(rid))
    }

    fn elevated(token: &OwnedHandle) -> Field<bool> {
        let buf = token_info(token, TokenElevation)?;
        // SAFETY: the buffer holds a TOKEN_ELEVATION.
        let e = unsafe { *buf.as_ptr().cast::<TOKEN_ELEVATION>() };
        Ok(e.TokenIsElevated != 0)
    }

    /// The token's user as `DOMAIN\name`, resolved once per SID.
    fn account(token: &OwnedHandle, cache: &mut AccountCache) -> Field<String> {
        let buf = token_info(token, TokenUser)?;
        // SAFETY: the buffer holds a TOKEN_USER whose SID points into it.
        let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: `sid` is a valid SID of exactly GetLengthSid bytes.
        let key = unsafe { std::slice::from_raw_parts(sid.0.cast::<u8>(), GetLengthSid(sid) as usize) }
            .to_vec();

        cache.entry(key).or_insert_with(|| lookup_account(sid)).clone()
    }

    fn lookup_account(sid: PSID) -> Field<String> {
        // 256 is the documented maximum for account and domain names.
        let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
        let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
        let mut kind = SID_NAME_USE::default();
        // SAFETY: buffers and their lengths match; `sid` is valid.
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                PWSTR(name.as_mut_ptr()),
                &mut name_len,
                PWSTR(domain.as_mut_ptr()),
                &mut domain_len,
                &mut kind,
            )
        }
        .map_err(|_| Missing::Unavailable)?;

        let name = String::from_utf16_lossy(&name[..name_len as usize]);
        let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
        Ok(if domain.is_empty() { name } else { format!("{domain}\\{name}") })
    }
}

// ---------------------------------------------------------------------------
// Tests (Windows): read details of a throwaway child process we own, and of
// the protected System process.
// ---------------------------------------------------------------------------

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

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
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn own_child_has_full_details() {
        let child = TestChild::spawn();
        let d = fetch(child.0.id(), &mut AccountCache::new());

        assert!(d.command_line.as_ref().unwrap().contains("127.0.0.1"));
        assert!(*d.threads.as_ref().unwrap() >= 1);
        assert!(*d.handles.as_ref().unwrap() >= 1);
        assert_eq!(d.suspended, Ok(false));
        assert_eq!(d.priority, Ok(Priority::Normal));
        assert!(d.integrity.is_ok() && d.elevated.is_ok());
        assert!(d.account.as_ref().unwrap().contains('\\'));
        assert!(d.started.as_ref().unwrap().local.is_some());
    }

    /// Poll until `pid`'s suspended state reads `want`, for up to ~2 s.
    /// `SuspendThread` is asynchronous: a thread only parks in the
    /// "suspended" wait once it next passes through the kernel.
    fn wait_for_suspended(pid: u32, want: bool) -> bool {
        (0..40).any(|_| {
            let ok = fetch(pid, &mut AccountCache::new()).suspended == Ok(want);
            if !ok {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ok
        })
    }

    #[test]
    fn suspended_state_is_detected() {
        let child = TestChild::spawn();
        let pid = child.0.id();
        // Let the child finish starting up: `suspend` only reaches threads
        // that exist when it runs, and ping spawns worker threads at startup.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(wait_for_suspended(pid, false));

        crate::actions::suspend(pid).expect("suspend");
        assert!(wait_for_suspended(pid, true), "suspend not detected");
        crate::actions::resume(pid).expect("resume");
        assert!(wait_for_suspended(pid, false), "resume not detected");
    }

    #[test]
    fn system_process_degrades_gracefully() {
        // PID 4 ("System") has no command line and a protected token, but
        // its thread count is still readable from the system process list.
        let d = fetch(4, &mut AccountCache::new());
        assert!(d.command_line.is_err());
        assert!(*d.threads.as_ref().unwrap() > 1);
    }
}
