<#
.SYNOPSIS
    ADK-Rust development environment setup for Windows.

.DESCRIPTION
    Windows counterpart to scripts/setup-dev.sh. The bash script and devenv both
    require a POSIX shell, so Windows hosts have no scripted path without this.

    Checks for - and optionally installs - the tools the workspace needs, then
    reports the environment variables the build expects. Safe to re-run: every
    step skips work that is already done.

    Tools that have no unattended installer (Visual Studio Build Tools, Git for
    Windows, Python) are reported with a download link rather than installed.

.PARAMETER Check
    Report what is installed without changing anything.

.EXAMPLE
    ./scripts/setup-dev.ps1
    Install recommended tools and register git hooks.

.EXAMPLE
    ./scripts/setup-dev.ps1 -Check
    Report status only.
#>
[CmdletBinding()]
param(
    [switch]$Check
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:LocalToolRoot = Join-Path $env:USERPROFILE '.local'
$script:PathAdditions = @()
$script:Missing = @()

function Write-Ok      { param([string]$Message) Write-Host "  [ok]   $Message" -ForegroundColor Green }
function Write-Warn    { param([string]$Message) Write-Host "  [warn] $Message" -ForegroundColor Yellow }
function Write-Missing { param([string]$Message) Write-Host "  [--]   $Message" -ForegroundColor Red }
function Write-Section { param([string]$Message) Write-Host ""; Write-Host "${Message}:" }

function Test-Tool {
    param([Parameter(Mandatory)][string]$Name)
    [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

# Resolves a tool that may only exist under the local tool root, so a re-run
# finds what a previous run installed even when PATH has not been reloaded.
function Find-LocalTool {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$Subdir
    )
    $root = Join-Path $script:LocalToolRoot $Subdir
    if (-not (Test-Path $root)) { return $null }
    Get-ChildItem -Path $root -Recurse -Filter $Name -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty FullName
}

function Get-ToolVersion {
    param(
        [Parameter(Mandatory)][string]$Command,
        [string[]]$Arguments = @('--version')
    )
    try {
        $raw = & $Command @Arguments 2>&1 | Select-Object -First 1
        if ($raw) { return ($raw -replace '\s+', ' ').Trim() }
    } catch {
        # Version probes are advisory: a tool that runs but cannot report a
        # version is still usable, so fall through to the generic label.
        Write-Verbose "version probe for '$Command' failed: $($_.Exception.Message)"
    }
    'installed'
}

function Install-ZipTool {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$Url,
        [Parameter(Mandatory)][string]$Subdir,
        [Parameter(Mandatory)][string]$ExeName,
        [ValidateSet('zip', 'tar')][string]$Format = 'zip'
    )

    $dest = Join-Path $script:LocalToolRoot $Subdir
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    $archive = Join-Path $env:TEMP "adk-$Subdir.$Format"

    Write-Host "  Downloading $Name..."
    try {
        Invoke-WebRequest -Uri $Url -OutFile $archive -UseBasicParsing
        if ($Format -eq 'zip') {
            Expand-Archive -Path $archive -DestinationPath $dest -Force
        } else {
            & tar -xzf $archive -C $dest
            if ($LASTEXITCODE -ne 0) { throw "tar exited with $LASTEXITCODE" }
        }
    } catch {
        Write-Warn "could not install $Name automatically: $($_.Exception.Message)"
        return $null
    } finally {
        Remove-Item $archive -Force -ErrorAction SilentlyContinue
    }

    $exe = Find-LocalTool -Name $ExeName -Subdir $Subdir
    if ($exe) {
        Write-Ok "$Name installed to $exe"
        $script:PathAdditions += (Split-Path $exe)
    } else {
        Write-Warn "$Name archive unpacked but $ExeName was not found under $dest"
    }
    return $exe
}

Write-Host "==================================="
Write-Host " ADK-Rust Dev Environment Setup"
Write-Host " OS: Windows  Arch: $env:PROCESSOR_ARCHITECTURE"
Write-Host "==================================="

# ---------------------------------------------------------------------------
# Core toolchain
# ---------------------------------------------------------------------------
Write-Section 'Core toolchain'

foreach ($tool in 'rustc', 'cargo') {
    if (Test-Tool $tool) {
        Write-Ok (Get-ToolVersion $tool)
    } else {
        Write-Missing "$tool - install from https://rustup.rs"
        $script:Missing += $tool
    }
}

# The MSVC linker is what .cargo/config.toml's rust-lld pins against, and
# aws-lc-sys needs cl.exe for its C sources.
if (Test-Tool 'cl') {
    Write-Ok 'cl.exe (MSVC C/C++ compiler)'
    # adk-sandbox forwards LIB to rustc when a Developer shell supplies it and
    # otherwise discovers the same SDK paths from the installed Build Tools.
    if ($env:LIB) {
        Write-Ok 'LIB set (MSVC library path available to the sandbox tests)'
    } else {
        Write-Ok 'LIB not set (adk-sandbox will discover the installed MSVC SDK paths)'
    }
} else {
    Write-Warn 'cl.exe not on PATH - install Visual Studio Build Tools with the'
    Write-Host '         "Desktop development with C++" workload, then run this from'
    Write-Host '         a Developer PowerShell: https://visualstudio.microsoft.com/downloads/'
    $script:Missing += 'Visual Studio Build Tools'
}

# ---------------------------------------------------------------------------
# Build acceleration
# ---------------------------------------------------------------------------
Write-Section 'Build acceleration (optional)'

if (Test-Tool 'sccache') {
    Write-Ok (Get-ToolVersion 'sccache')
} elseif ($Check) {
    Write-Missing 'sccache - shared compilation cache'
} else {
    Write-Missing 'sccache - shared compilation cache'
    Write-Host '  Installing sccache via cargo...'
    cargo install sccache --locked
    if ($LASTEXITCODE -ne 0) {
        Write-Warn 'could not install sccache - see https://github.com/mozilla/sccache'
    }
}

# ---------------------------------------------------------------------------
# Test runner
# ---------------------------------------------------------------------------
Write-Section 'Test runner (quality gate)'

if (Test-Tool 'cargo-nextest') {
    Write-Ok (Get-ToolVersion 'cargo-nextest')
} elseif ($Check) {
    Write-Missing 'cargo-nextest - required by the test gate'
} else {
    Write-Missing 'cargo-nextest - required by the test gate'
    # Prebuilt binary: building from source costs minutes for no benefit.
    Install-ZipTool -Name 'cargo-nextest' `
        -Url 'https://get.nexte.st/latest/windows-tar' `
        -Subdir 'nextest' -ExeName 'cargo-nextest.exe' -Format 'tar' | Out-Null
}

# ---------------------------------------------------------------------------
# Feature-gated build tools
# ---------------------------------------------------------------------------
Write-Section 'Feature-gated build tools'

# protoc - lance's build script requires it, so `adk-rag --features lancedb`
# cannot compile without it. CI installs it on every feature-coverage runner,
# which is why a missing local copy fails while CI stays green.
$protoc = if (Test-Tool 'protoc') { 'protoc' } else { Find-LocalTool -Name 'protoc.exe' -Subdir 'protoc' }
if ($protoc) {
    Write-Ok "protoc $((Get-ToolVersion $protoc) -replace '^libprotoc ', '')"
    if ($protoc -ne 'protoc') { $script:PathAdditions += (Split-Path $protoc) }
} elseif ($Check) {
    Write-Missing 'protoc - needed for adk-rag --features lancedb'
} else {
    Write-Missing 'protoc - needed for adk-rag --features lancedb'
    $protoc = Install-ZipTool -Name 'protoc' `
        -Url 'https://github.com/protocolbuffers/protobuf/releases/download/v29.3/protoc-29.3-win64.zip' `
        -Subdir 'protoc' -ExeName 'protoc.exe'
}

# NASM - aws-lc-sys assembles its own sources on MSVC, so every rustls-backed
# feature (adk-auth --features sso among them) fails without it.
$nasm = if (Test-Tool 'nasm') { 'nasm' } else { Find-LocalTool -Name 'nasm.exe' -Subdir 'nasm' }
if ($nasm) {
    Write-Ok (Get-ToolVersion $nasm '-v')
    if ($nasm -ne 'nasm') { $script:PathAdditions += (Split-Path $nasm) }
} elseif ($Check) {
    Write-Missing 'nasm - needed by aws-lc-sys on MSVC (any rustls feature)'
} else {
    Write-Missing 'nasm - needed by aws-lc-sys on MSVC (any rustls feature)'
    Install-ZipTool -Name 'nasm' `
        -Url 'https://www.nasm.us/pub/nasm/releasebuilds/2.16.03/win64/nasm-2.16.03-win64.zip' `
        -Subdir 'nasm' -ExeName 'nasm.exe' | Out-Null
}

if (Test-Tool 'cmake') {
    Write-Ok (Get-ToolVersion 'cmake')
} else {
    Write-Warn 'cmake - needed only for the openai-webrtc feature (audiopus)'
    Write-Host '         https://cmake.org/download/'
}

# ---------------------------------------------------------------------------
# POSIX tooling required by the test suite
# ---------------------------------------------------------------------------
Write-Section 'POSIX tooling (required by 9 tests)'

# These tests shell out directly. Without bash/sh they fail with
# "program not found" locally while passing on GitHub's windows-latest image.
if (Test-Tool 'bash') {
    Write-Ok "bash - $((Get-Command bash).Source)"
} else {
    # Git for Windows ships bash/sh in Git\bin but adds only Git\cmd to PATH,
    # so the shell is usually already installed and merely unreachable.
    $gitBin = 'C:\Program Files\Git\bin'
    if (Test-Path (Join-Path $gitBin 'bash.exe')) {
        Write-Warn "bash present but not on PATH: $gitBin"
        $script:PathAdditions += $gitBin
    } else {
        Write-Missing 'bash - 9 shell-tool tests fail without it'
        Write-Host '         https://git-scm.com/download/win'
        $script:Missing += 'bash (Git for Windows)'
    }
}

# adk-sandbox process tests invoke `python3`; a bare `python` does not satisfy
# them. The python.org installer does not create the python3 alias, so check it
# explicitly rather than inferring from `python`.
if (Test-Tool 'python3') {
    Write-Ok "python3 - $(Get-ToolVersion 'python3')"
} elseif (Test-Tool 'python') {
    # The python.org installer creates no python3.exe, but adk-sandbox's tests
    # invoke `python3` specifically. Copying the shim is cheaper and less
    # surprising than asking every contributor to do it by hand.
    $pyDir = Split-Path (Get-Command python).Source
    $shim = Join-Path $pyDir 'python3.exe'
    if ($Check) {
        Write-Warn 'python present but python3 does not resolve - run without -Check to create the alias'
        $script:Missing += 'python3 alias'
    } else {
        try {
            Copy-Item (Join-Path $pyDir 'python.exe') $shim -Force -ErrorAction Stop
            Write-Ok "python3 alias created at $shim"
        } catch {
            Write-Warn "could not create python3 alias: $($_.Exception.Message)"
            $script:Missing += 'python3 alias'
        }
    }
} else {
    Write-Missing 'python3 - adk-sandbox process-execution tests fail without it'
    Write-Host '         https://www.python.org/downloads/'
    $script:Missing += 'python3'
}

# ---------------------------------------------------------------------------
# Git hooks
# ---------------------------------------------------------------------------
Write-Section 'Git hooks (quality gates)'

if (Test-Tool 'lefthook') {
    Write-Ok (Get-ToolVersion 'lefthook' @('version'))
} elseif ($Check) {
    Write-Missing 'lefthook - git hook runner for the quality gates'
} else {
    Write-Missing 'lefthook - git hook runner for the quality gates'
    # Not on crates.io, so cargo install cannot provide it.
    if (Test-Tool 'npm') {
        Write-Host '  Installing lefthook via npm...'
        npm install -g lefthook 2>$null
    }
    if (-not (Test-Tool 'lefthook')) {
        Write-Warn 'install lefthook manually: https://github.com/evilmartians/lefthook/releases'
    }
}

if (-not $Check -and (Test-Tool 'lefthook')) {
    git rev-parse --git-dir *> $null
    if ($LASTEXITCODE -eq 0) {
        lefthook install *> $null
        if ($LASTEXITCODE -eq 0) {
            Write-Ok 'git hooks installed (pre-commit, pre-push)'
        } else {
            Write-Warn "run 'lefthook install' from the repo root to register hooks"
        }
    } else {
        Write-Warn "not a git repo - run 'lefthook install' from the repo root"
    }
}

# ---------------------------------------------------------------------------
# Environment variables
# ---------------------------------------------------------------------------
Write-Section 'Environment variables'

# setx writes to the user environment so the value survives new shells; the
# current session is updated separately because setx does not affect it.
function Set-UserEnv {
    [CmdletBinding(SupportsShouldProcess)]
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string]$Value
    )
    if ($Check) {
        Write-Warn "$Name not set - run without -Check to persist it ($Name=$Value)"
        return
    }
    if (-not $PSCmdlet.ShouldProcess("user environment variable $Name", "set to '$Value'")) {
        return
    }
    [Environment]::SetEnvironmentVariable($Name, $Value, 'User')
    Set-Item -Path "env:$Name" -Value $Value
    Write-Ok "$Name=$Value (persisted for this user)"
}

if ($env:RUSTC_WRAPPER) {
    Write-Ok "RUSTC_WRAPPER=$env:RUSTC_WRAPPER"
} elseif (Test-Tool 'sccache') {
    Set-UserEnv -Name 'RUSTC_WRAPPER' -Value 'sccache'
} else {
    Write-Warn 'RUSTC_WRAPPER not set (sccache not installed)'
}

if ($env:CMAKE_POLICY_VERSION_MINIMUM) {
    Write-Ok "CMAKE_POLICY_VERSION_MINIMUM=$env:CMAKE_POLICY_VERSION_MINIMUM"
} else {
    Set-UserEnv -Name 'CMAKE_POLICY_VERSION_MINIMUM' -Value '3.5'
}

# PROTOC only matters when protoc is not on PATH: lance's build script reads it
# to locate the compiler.
if ($env:PROTOC) {
    Write-Ok "PROTOC=$env:PROTOC"
} elseif ($protoc -and $protoc -ne 'protoc') {
    Set-UserEnv -Name 'PROTOC' -Value $protoc
} elseif ($protoc) {
    Write-Ok 'PROTOC not needed (protoc is on PATH)'
} else {
    Write-Warn 'PROTOC not set and protoc not found'
}

# ---------------------------------------------------------------------------
# PATH additions
# ---------------------------------------------------------------------------
$script:PathAdditions = $script:PathAdditions | Where-Object { $_ } | Sort-Object -Unique
if ($script:PathAdditions) {
    Write-Section 'PATH'
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    foreach ($dir in $script:PathAdditions) {
        if ($userPath -and ($userPath -split ';' | Where-Object { $_.TrimEnd('\') -ieq $dir.TrimEnd('\') })) {
            Write-Ok "already on PATH: $dir"
            continue
        }
        if ($Check) {
            Write-Warn "not on PATH: $dir"
            continue
        }
        $userPath = if ($userPath) { "$dir;$userPath" } else { $dir }
        [Environment]::SetEnvironmentVariable('Path', $userPath, 'User')
        $env:Path = "$dir;$env:Path"
        Write-Ok "added to PATH: $dir"
    }
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
Write-Host ""
if ($script:Missing) {
    Write-Host "Outstanding manual steps:" -ForegroundColor Yellow
    $script:Missing | Sort-Object -Unique | ForEach-Object { Write-Host "  - $_" }
    Write-Host ""
}

if ($Check) {
    Write-Host "Run without -Check to install missing tools."
} else {
    Write-Host "Done. Open a new shell so PATH and environment changes apply, then:"
    Write-Host "  cargo fmt --all -- --check"
    Write-Host "  cargo clippy --workspace --all-targets -- -D warnings"
    Write-Host "  cargo nextest run --workspace"
}
