# Windows notes

- **Run cargo from PowerShell or cmd, not Git Bash.** Git Bash puts its own `link` ahead of the MSVC linker on `PATH` and linking fails.
- Setup: `powershell -ExecutionPolicy Bypass -File .\setup.ps1` (policy applies to that process only). Agent scripts, one command each: `.\scripts\agents.ps1 sync`, `.\scripts\agents.ps1 check`, `.\scripts\agents.ps1 install-hooks`.
- Agent CLIs install as `claude.cmd` / `codex.cmd`. LYRA resolves them with `which` and spawns through `cmd /C` (`proc::resolve_program`). Code that spawns programs must go through `proc`, not `std::process::Command`.
- Process trees are killed through a Job Object with kill-on-close (`windows-sys`). Don't add a spawn path that bypasses it.
- Paths: always normalize model- or git-supplied paths with `guards::norm_path` before comparing (`\` vs `/`). Use `dunce::canonicalize` instead of `std::fs::canonicalize` to avoid `\\?\` prefixes.
- Line endings: fixtures and guards must tolerate CRLF. Don't rely on `\n` in parsed output.
- Tests that need a helper executable follow the `#[cfg(windows)]` / `#[cfg(not(windows))]` pattern (`.cmd` vs `.sh`, no `chmod`).
- Temp cleanup can fail while a handle is open; verification cleanup uses a fresh cancellation token and must stay best-effort-visible, not silent.
- Git hooks under `scripts/git-hooks` run through Git for Windows' `sh`; keep them POSIX `sh` and LF line endings.
