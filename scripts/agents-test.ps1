# Windows regression test for scripts/agents.ps1 (run in CI on windows-latest; also works with pwsh on macOS/Linux).
# Runs `sync` by absolute path from OUTSIDE the repository, then both checkers, on a scratch copy.
# Guards against .NET calls resolving paths against the caller's working directory.
$ErrorActionPreference = "Stop"
$repo = Split-Path -Parent $PSScriptRoot
$work = Join-Path ([IO.Path]::GetTempPath()) ("agents-test-" + [guid]::NewGuid())
$copy = Join-Path $work "repo"
$elsewhere = Join-Path $work "elsewhere"
New-Item -ItemType Directory -Path $copy, $elsewhere | Out-Null
$failures = 0
function Assert($ok, $label) { if ($ok) { Write-Host "ok   $label" } else { Write-Host "FAIL $label"; $script:failures++ } }

try {
    foreach ($d in ".agents", "scripts") { Copy-Item (Join-Path $repo $d) (Join-Path $copy $d) -Recurse }
    # An unrelated .claude/skills in the caller's directory must never be touched.
    New-Item -ItemType Directory -Path (Join-Path $elsewhere ".claude/skills") | Out-Null

    Push-Location $elsewhere
    try {
        & pwsh -NoProfile -File (Join-Path $copy "scripts/agents.ps1") sync
        Assert ($LASTEXITCODE -eq 0) "sync from outside the repo exits 0"
    } finally { Pop-Location }

    $marker = Join-Path $copy ".claude/skills/.generated"
    Assert (Test-Path $marker) "marker written inside the repo copy"
    Assert (-not (Test-Path (Join-Path $elsewhere ".claude/skills/.generated"))) "caller's directory untouched"
    $expected = "Generated from .agents/skills by scripts/agents.sh sync. Do not edit here.`n"
    Assert ([IO.File]::ReadAllText($marker) -eq $expected) "marker bytes identical to agents.sh"

    Push-Location $elsewhere
    try {
        & pwsh -NoProfile -File (Join-Path $copy "scripts/agents.ps1") check
        Assert ($LASTEXITCODE -eq 0) "agents.ps1 check from outside the repo passes"
    } finally { Pop-Location }

    if (Get-Command sh -ErrorAction SilentlyContinue) {
        & sh (Join-Path $copy "scripts/agents.sh") check
        Assert ($LASTEXITCODE -eq 0) "agents.sh check passes on the PowerShell-synced tree"
    } else { Write-Host "skip agents.sh check (no sh on PATH)" }
} finally {
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
if ($failures -gt 0) { Write-Host "$failures test(s) failed"; exit 1 }
Write-Host "all agents.ps1 tests passed"
