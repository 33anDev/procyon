
The workspace is set up so future crates slot in cleanly without restructuring.

## Roadmap

- [x] **v0.1** — live process monitor (list, search, sort, kill)
- [ ] **Per-process module view** — see the DLLs each process has loaded
      *(this is the actual "Process Hacker" superpower, and the natural next step)*
- [ ] **Richer detail** — threads, handles, integrity level, command line
- [ ] **A nicer UI pass** — optionally a Tauri front-end for a fully custom look
- [ ] **DLL injector** — a separate, Windows-only crate, paired with a harmless
      test DLL (just a `MessageBox`) so injection can be verified without any
      sketchy payload

## Scope & intent

Procyon is for understanding your own system: debugging, learning how Windows
internals work, and inspecting what's running on a machine you control. The
injector on the roadmap is a classic systems-programming exercise with plenty of
legitimate uses (debuggers, overlays, accessibility tools, game mods). It is
**not** for tampering with software you don't own, defeating anti-cheat, or
anything you'd need to hide. Keep it honest.

## License

MIT — see [LICENSE](LICENSE).