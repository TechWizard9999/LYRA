# Manage the agent context in .agents/ (PowerShell twin of scripts/agents.sh).
#   sync           regenerate .claude/skills from .agents/skills (Claude Code only reads .claude/skills)
#   check          fail if a skill is malformed or .claude/skills drifted from .agents/skills
#   install-hooks  point this clone's git hooks at scripts/git-hooks
param([Parameter(Position = 0)][string]$Command = "")

$ErrorActionPreference = "Stop"
# Absolute repo root. .NET APIs resolve relative paths against the process working directory,
# which Set-Location does not change, so anything passed to [IO.File] must be built from $root.
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$src = ".agents/skills"
$dst = ".claude/skills"
$marker = ".generated"

function Test-Skills {
    $ok = $true
    foreach ($dir in Get-ChildItem $src -Directory) {
        $name = $dir.Name
        $file = Join-Path $dir.FullName "SKILL.md"
        if (-not (Test-Path $file)) { Write-Host "!! ${name}: missing SKILL.md"; $ok = $false; continue }
        $lines = Get-Content $file
        if ($lines[0] -ne "---") { Write-Host "!! ${name}: SKILL.md must start with --- frontmatter"; $ok = $false }
        $end = [array]::IndexOf($lines, "---", 1)
        if ($lines[0] -eq "---" -and $end -lt 1) { Write-Host "!! ${name}: SKILL.md frontmatter is not closed by a second ---"; $ok = $false; continue }
        $front = if ($end -gt 0) { $lines[1..($end - 1)] } else { @() }
        $fmName = ($front | Where-Object { $_ -match '^name:\s*(.+)$' } | ForEach-Object { $Matches[1].Trim() } | Select-Object -First 1)
        if ($fmName -ne $name) { Write-Host "!! ${name}: frontmatter name is '$fmName', expected '$name'"; $ok = $false }
        if (-not ($front | Where-Object { $_ -match '^description:\s*\S' })) {
            Write-Host "!! ${name}: frontmatter needs a non-empty description"; $ok = $false
        }
    }
    if (-not $ok) { exit 1 }
}

function Get-Tree($root) {
    $rootFull = (Resolve-Path $root).Path
    Get-ChildItem $root -Recurse -File | Where-Object { $_.Name -ne $marker } | ForEach-Object {
        $rel = $_.FullName.Substring($rootFull.Length).TrimStart('\', '/').Replace('\', '/')
        # Normalise line endings so a CRLF checkout does not look like drift.
        $text = [IO.File]::ReadAllText($_.FullName).Replace("`r`n", "`n")
        $bytes = [Text.Encoding]::UTF8.GetBytes($text)
        $hash = [BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash($bytes))
        "$rel $hash"
    } | Sort-Object
}

switch ($Command) {
    "sync" {
        Test-Skills
        if (Test-Path $dst) { Remove-Item $dst -Recurse -Force }
        New-Item -ItemType Directory -Path $dst | Out-Null
        Copy-Item "$src/*" $dst -Recurse
        # Same bytes as scripts/agents.sh writes, so syncing on either OS leaves no diff.
        [IO.File]::WriteAllText((Join-Path (Join-Path $root $dst) $marker), "Generated from .agents/skills by scripts/agents.sh sync. Do not edit here.`n")
        Write-Host "synced $((Get-ChildItem $src -Directory).Count) skills to $dst"
    }
    "check" {
        Test-Skills
        if (-not (Test-Path $dst)) { Write-Host "!! $dst missing: run .\scripts\agents.ps1 sync"; exit 1 }
        $diff = Compare-Object (Get-Tree $src) (Get-Tree $dst)
        if ($diff) {
            Write-Host "!! $dst is out of date with ${src}: run .\scripts\agents.ps1 sync and commit the result"
            $diff | ForEach-Object { Write-Host "   $($_.InputObject)" }
            exit 1
        }
        Write-Host "ok: $dst matches $src"
    }
    "install-hooks" {
        git config core.hooksPath scripts/git-hooks
        Write-Host "git hooks now read from scripts/git-hooks (commit-msg, pre-commit)"
    }
    default { Write-Host "usage: scripts/agents.ps1 sync|check|install-hooks"; exit 2 }
}
