# LYRA setup for Windows (PowerShell 5.1+ or 7+). Non-destructive and safe to run again:
# checks tools, builds the release binary, writes ~/.lyra/config.toml only if it is missing,
# and runs `lyra doctor`.
$ErrorActionPreference = 'Stop'
Set-Location -Path $PSScriptRoot

$problems = @()
function Test-Tool([string]$name) { return [bool](Get-Command $name -ErrorAction SilentlyContinue) }
function Ok([string]$msg) { Write-Host "ok  $msg" -ForegroundColor Green }
function Bad([string]$msg) { Write-Host "!!  $msg" -ForegroundColor Yellow; $script:problems += $msg }

Write-Host "== Checking tools"
if (Test-Tool 'cargo') { Ok ("cargo: " + (cargo --version)) } else { Bad 'cargo not found: install Rust from https://rustup.rs' }
if (Test-Tool 'git') { Ok ("git: " + (git --version)) } else { Bad 'git not found: install Git for Windows' }
if (Test-Tool 'gh') {
    gh auth status *> $null
    if ($LASTEXITCODE -eq 0) { Ok 'gh: authenticated' } else { Bad 'gh is installed but not authenticated: run `gh auth login`' }
} else {
    Bad 'gh (GitHub CLI) not found: install from https://cli.github.com and run `gh auth login`'
}
$runtimes = @()
if (Test-Tool 'claude') { $runtimes += 'claude' }
if (Test-Tool 'codex') { $runtimes += 'codex' }
if ($runtimes.Count -gt 0) { Ok ("runtime(s) on PATH: " + ($runtimes -join ', ')) }
else { Bad 'neither claude nor codex on PATH: `npm i -g @anthropic-ai/claude-code` and/or `npm i -g @openai/codex`' }

if (-not (Test-Tool 'cargo')) {
    Write-Host "`nCannot build without cargo. Fix the items above and run .\setup.ps1 again." -ForegroundColor Red
    exit 1
}

Write-Host "`n== Building (cargo build --release)"
cargo build --release
if ($LASTEXITCODE -ne 0) {
    Write-Host "`nBuild failed. If the error mentions link.exe, install Visual Studio Build Tools or use the GNU toolchain (see SETUP.md)." -ForegroundColor Red
    exit 1
}

Write-Host "`n== Configuration"
$lyraHome = if ($env:LYRA_HOME -and $env:LYRA_HOME.Trim()) { $env:LYRA_HOME } else { Join-Path $HOME '.lyra' }
$configPath = Join-Path $lyraHome 'config.toml'
if (Test-Path $configPath) {
    Ok "keeping existing $configPath"
} else {
    New-Item -ItemType Directory -Force -Path $lyraHome | Out-Null
    # Same text as DEFAULT_CONFIG_TOML in src/config.rs.
    $defaultConfig = @'
# LYRA configuration (see docs/PLAN.md {SECT}7)

[run]
provider = "claude"            # "claude" | "codex"
timeout_minutes = 30           # hang guard per agent call / verify command

[server]
bind = "127.0.0.1:7700"
open_browser = true

[providers.claude]
kind = "claude"
auth = "api_key"               # "api_key" (needs ANTHROPIC_API_KEY) | "login" (uses `claude` login)
model = "opus"

[providers.codex]
kind = "codex"
# model = "gpt-5.5-codex"

[verify]                       # <repo>/lyra.toml can override these
build = "cargo build --workspace"
test  = "cargo test --workspace"
fmt   = "cargo fmt --all -- --check"
'@
    $defaultConfig = $defaultConfig.Replace('{SECT}', [string][char]0x00A7)  # keeps this file ASCII-only for PowerShell 5.1
    [System.IO.File]::WriteAllText($configPath, ($defaultConfig -replace "`r`n", "`n") + "`n", (New-Object System.Text.UTF8Encoding $false))
    Ok "wrote $configPath"
}

Write-Host "`n== lyra doctor"
$lyra = Join-Path $PSScriptRoot 'target\release\lyra.exe'
& $lyra doctor
$doctorOk = ($LASTEXITCODE -eq 0)

Write-Host "`n== Next steps"
if ($problems.Count -gt 0 -or -not $doctorOk) {
    Write-Host "Fix the items marked !! above (details in SETUP.md), then run .\setup.ps1 again."
}
Write-Host "  1. Edit $configPath (runtime, auth, model) if needed."
Write-Host "  2. Start the dashboard:  $lyra serve"
Write-Host "  3. In the browser, enter the path of a cloned Rust repo and a GitHub issue, then press Run."
Write-Host "     Or from a terminal:   $lyra run --repo C:\path\to\repo --issue https://github.com/owner/repo/issues/123"
