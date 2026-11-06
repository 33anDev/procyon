<div align="center">

# ◆ Procyon

**A transparent, open-source process monitor — built in Rust.**

*An open, auditable window into everything running on your machine.*

</div>

---

## Why

The process-inspection tools floating around the internet are often opaque
binaries you're expected to trust blindly — and a worrying number of them ship
with a backdoor or trojan bundled in. Procyon is the opposite: a small, clean,
fully open codebase where you can read exactly what it does before you run it.
Nothing hidden, nothing phoning home.

It's also just a fun systems-programming project.

## What it does today

- **Live process list** — name, PID, parent PID, CPU %, memory, user
- **Search** by name or PID
- **Sort** by any column (click the header to toggle direction)
- **End process** — the normal Task-Manager "End task", with a confirmation step
- A clean, modern dark UI that stays responsive while the data ticks

## Download

Grab the latest `procyon.exe` from the
[**Releases**](https://github.com/33anDev/procyon/releases) page, then just
double-click it — no installer, no setup. Windows may show a SmartScreen
warning for a new unsigned app; click **More info → Run anyway**.

Prefer to build it yourself? See [Build from source](#build-from-source) below.

## Usage

- **Find a process** — start typing in the search box (top right) to filter by
  name or PID.
- **Sort** — click any column header (Name, PID, CPU%, Memory) to sort; click
  again to flip the direction.
- **Select** — click a row to select it.
- **End a process** — select it and click *End process* (bottom right), or
  right-click the row. You'll get a confirmation first.

The list refreshes automatically, so CPU% and memory stay live.

## Build from source

Requires a [Rust toolchain](https://rustup.rs/) (stable).

```bash
git clone https://github.com/33anDev/procyon
cd procyon
cargo run --release
```

That's it — no extra setup. The compiled binary lands in
`target/release/procyon.exe`.

## Project layout

procyon/
├── Cargo.toml # workspace root
└── crates/
└── procyon-ui/ # the process monitor (this crate)
└── src/
├── main.rs # window + entry point
├── app.rs # UI: top bar, table, status bar, dialogs
├── process.rs # data layer (sysinfo wrapper, snapshots, kill)
└── theme.rs # dark palette & styling

The workspace is set up so future crates slot in cleanly without restructuring.

## Roadmap

- [x] **v0.1** — live process monitor (list, search, sort, kill)
- [ ] **Process actions** — right-click to set priority, and more
- [ ] **Per-process module view** — see the DLLs each process has loaded
- [ ] **Richer detail** — threads, handles, integrity level, command line
- [ ] **A nicer UI pass** — optionally a Tauri front-end for a fully custom look

## Scope & intent

Procyon is for understanding your own system: debugging, learning how the OS
works, and inspecting what's running on a machine you control. Keep it honest —
it's a tool for your own machine, not for tampering with software you don't own.

## License

MIT — see [LICENSE](LICENSE).