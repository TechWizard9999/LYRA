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
    # In Windows PowerShell 5.1 redirected native stderr is an ErrorRecord.
    # Authentication failures are a diagnostic, not a terminating script error.
    $savedPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        gh auth status *> $null
        $authExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedPreference
    }
    if ($authExitCode -eq 0) { Ok 'gh: authenticated' } else { Bad 'gh is installed but not authenticated: run `gh auth login`' }
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

Write-Host "`n== Building (cargo build --release --locked)"
cargo build --release --locked
if ($LASTEXITCODE -ne 0) {
    Write-Host "`nBuild failed. If the error mentions link.exe, install Visual Studio Build Tools or use the GNU toolchain (see SETUP.md)." -ForegroundColor Red
    exit 1
}

$lyraHome = if ($env:LYRA_HOME -and $env:LYRA_HOME.Trim()) { $env:LYRA_HOME } else { Join-Path $HOME '.lyra' }
$configPath = Join-Path $lyraHome 'config.toml'

Write-Host "`n== lyra doctor"
$lyra = Join-Path $PSScriptRoot 'target\release\lyra.exe'
# The binary owns the default config and creates it only when missing.
& $lyra doctor
$doctorOk = ($LASTEXITCODE -eq 0)
$lyraCommand = "'" + [System.Management.Automation.Language.CodeGeneration]::EscapeSingleQuotedStringContent($lyra) + "'"

Write-Host "`n== Next steps"
if ($problems.Count -gt 0 -or -not $doctorOk) {
    Write-Host "Fix the items marked !! above (details in SETUP.md), then run .\setup.ps1 again."
}
Write-Host "  1. Edit $configPath (runtime, auth, model) if needed."
Write-Host "  2. Start the dashboard:  & $lyraCommand serve"
Write-Host "  3. In the browser, enter the path of an already-cloned local repository with a supported profile and a GitHub issue, then press Run."
Write-Host "     Or from a terminal:   & $lyraCommand run --repo C:\path\to\repo --issue https://github.com/owner/repo/issues/123"
