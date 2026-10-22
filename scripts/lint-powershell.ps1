<#
.SYNOPSIS
    Lints PowerShell scripts with PSScriptAnalyzer.

.DESCRIPTION
    Backs the `psscriptanalyzer` pre-commit hook - the PowerShell counterpart to
    the shellcheck gate. Lives in a file rather than inline in lefthook.yml
    because lefthook pipes commands through `sh` on Unix, which would mangle
    the `$` variables of an inline script.

    Exits 0 when PSScriptAnalyzer is unavailable: the module is not part of any
    setup script, and a missing optional linter should not block a commit. This
    mirrors shellcheck running at --severity=warning rather than failing on
    everything.

.PARAMETER Path
    Script paths to analyze. Lefthook passes the staged *.ps1 files.

.EXAMPLE
    ./scripts/lint-powershell.ps1 scripts/setup-dev.ps1
#>
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $Path) {
    Write-Host 'No PowerShell files to analyze.'
    exit 0
}

if (-not (Get-Module -ListAvailable PSScriptAnalyzer)) {
    Write-Host 'PSScriptAnalyzer not installed - skipping PowerShell lint.'
    Write-Host 'Install it with: Install-Module PSScriptAnalyzer -Scope CurrentUser'
    exit 0
}

Import-Module PSScriptAnalyzer

# @() keeps this an array for a single match: StrictMode rejects .Count on the
# scalar that Where-Object would otherwise return.
$existing = @($Path | Where-Object { Test-Path $_ })
if (-not $existing) {
    Write-Host 'No existing PowerShell files to analyze.'
    exit 0
}

# PSAvoidUsingWriteHost is excluded: these are interactive setup scripts whose
# whole job is colored console output for a human, which is what Write-Host is
# for. Their status lines are not pipeline data.
$excluded = @('PSAvoidUsingWriteHost')

# -Path binds a single string, so analyze one file per call rather than passing
# the whole staged set at once.
$findings = @(foreach ($file in $existing) {
    Invoke-ScriptAnalyzer -Path $file -Severity Warning, Error -ExcludeRule $excluded
})

if ($findings) {
    $findings | Format-Table -AutoSize RuleName, Severity, ScriptName, Line, Message
    Write-Host "PSScriptAnalyzer reported $($findings.Count) issue(s)."
    exit 1
}

Write-Host "PowerShell lint clean ($($existing.Count) file(s))."
exit 0
