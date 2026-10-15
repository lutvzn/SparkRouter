<# Windows convenience helper. Environment files are parsed as data, never executed. #>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('start', 'run', 'stop', 'restart', 'rebuild', 'status', 'logs', 'health', 'build', 'check', 'test', 'keygen', 'help')]
    [string]$Command = 'help',
    [string]$EnvFile,
    [string]$StateDir,
    [switch]$DebugBuild,
    [string]$Url,
    [ValidateRange(1, 86400)] [int]$TimeoutSeconds = 30
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
if (-not $StateDir) { $StateDir = Join-Path $Root '.sparkrouter' }
$StateDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($StateDir)
$Profile = if ($DebugBuild) { 'debug' } else { 'release' }
$Binary = Join-Path $Root "target/$Profile/sparkrouter.exe"
$StateFile = Join-Path $StateDir 'process.json'
$OutLog = Join-Path $StateDir 'stdout.log'
$ErrLog = Join-Path $StateDir 'stderr.log'
$LockStream = $null
$Locked = $false
$OriginalLocation = Get-Location
$OriginalEnvironment = @{}

function Show-Help {
    @'
Usage: ./scripts/sparkrouter.ps1 COMMAND [options]
Commands: start, run, stop, restart, rebuild, status, logs, health, build, check, test, keygen, help
Options: -EnvFile FILE  -StateDir DIR  -DebugBuild  -Url URL  -TimeoutSeconds N
Defaults: release build, .sparkrouter state directory, 30-second stop timeout.
Loads .env from the caller working directory if present unless -EnvFile is specified.
Only KEY=value or quoted literal values are supported.
start builds if the binary is missing. run stays in the foreground (Ctrl+C to stop).
rebuild stops, builds, and starts again; a failed build leaves the server stopped.
On Windows, stop forcibly terminates the managed process; it cannot drain streams.
Use run and Ctrl+C when you need graceful shutdown.
'@
}
function Import-EnvironmentFile {
    param([string]$Path)
    $Allowed = @('ROUTER_MASTER_KEY','ROUTER_DATABASE_URL','ROUTER_API_KEY','ROUTER_BIND',
        'ROUTER_DASHBOARD_PASSWORD','ROUTER_DASHBOARD_SECURE_COOKIE','RUST_LOG')
    foreach ($Line in [IO.File]::ReadAllLines($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path))) {
        if ($Line -match '^\s*(#|$)') { continue }
        if ($Line -notmatch '^([A-Za-z_][A-Za-z0-9_]*)=(.*)$') {
            throw 'Invalid environment file line (expected KEY=value)'
        }
        $Name = $Matches[1]; $Value = $Matches[2]
        if ($Allowed -cnotcontains $Name) { throw 'Unsupported environment variable in file' }
        if ($Value.StartsWith('"') -or $Value.StartsWith("'")) {
            if ($Value.Length -lt 2 -or $Value[$Value.Length - 1] -ne $Value[0]) {
                throw 'Unmatched environment value quote'
            }
            $Value = $Value.Substring(1, $Value.Length - 2)
        }
        if ([string]::IsNullOrEmpty($Value)) { throw 'Invalid environment file: empty values are not supported; omit unset variables' }
        if (-not $OriginalEnvironment.ContainsKey($Name)) {
            $OriginalEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name, 'Process')
        }
        [Environment]::SetEnvironmentVariable($Name, $Value, 'Process')
    }
}
function Invoke-Cargo {
    param([string[]]$CargoArguments)
    & cargo @CargoArguments
    if ($LASTEXITCODE -ne 0) { throw "cargo failed (exit $LASTEXITCODE)" }
}
function Build-Binary {
    if ($DebugBuild) { Invoke-Cargo -CargoArguments @('build','--locked') }
    else { Invoke-Cargo -CargoArguments @('build','--release','--locked') }
}
function Ensure-Binary {
    if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) { Build-Binary }
}
function Lock-State {
    [IO.Directory]::CreateDirectory($StateDir) | Out-Null
    try {
        $script:LockStream = [IO.File]::Open((Join-Path $StateDir 'lock'), 'CreateNew', 'Write', 'None')
        $script:Locked = $true
    } catch {
        throw 'Another helper is running, or an abandoned lock exists; see BUILD.md'
    }
}
function Get-ManagedProcess {
    if (-not (Test-Path -LiteralPath $StateFile -PathType Leaf)) { return $null }
    try {
        $Saved = Get-Content -LiteralPath $StateFile -Raw | ConvertFrom-Json
        $ProcessId = 0
        if (-not [int]::TryParse([string]$Saved.pid, [ref]$ProcessId) -or $ProcessId -le 0 -or
            -not $Saved.binary -or -not $Saved.startedUtcTicks) { throw 'Invalid state' }
    } catch { throw 'Invalid process state; refusing to terminate a PID' }
    $Process = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($null -eq $Process) { return $null }
    try {
        # Match the executable and creation time, not just the potentially reused PID.
        if ($Process.StartTime.ToUniversalTime().Ticks.ToString() -ne [string]$Saved.startedUtcTicks -or
            -not [string]::Equals($Process.MainModule.FileName, [string]$Saved.binary, [StringComparison]::OrdinalIgnoreCase)) {
            $Process.Dispose(); return $null
        }
    } catch { $Process.Dispose(); throw 'Unable to verify process identity; refusing to terminate it' }
    return $Process
}
function Start-Router {
    $Existing = Get-ManagedProcess
    if ($null -ne $Existing) {
        Write-Host "Already running (PID $($Existing.Id))."; $Existing.Dispose(); return
    }
    if (-not $env:ROUTER_MASTER_KEY) {
        throw 'Set ROUTER_MASTER_KEY or pass -EnvFile FILE; use keygen once and save the key'
    }
    Ensure-Binary
    $Process = Start-Process -FilePath $Binary -WorkingDirectory $Root -PassThru `
        -RedirectStandardOutput $OutLog -RedirectStandardError $ErrLog -WindowStyle Hidden
    try {
        Start-Sleep -Seconds 1
        $Process.Refresh()
        if ($Process.HasExited) { throw "Startup failed; inspect $ErrLog (logs may contain configuration details)" }
        $Saved = @{ pid = $Process.Id; binary = $Binary; startedUtcTicks = $Process.StartTime.ToUniversalTime().Ticks.ToString() }
        $Saved | ConvertTo-Json | Set-Content -LiteralPath $StateFile -Encoding UTF8
        Write-Host "Started PID $($Process.Id). Logs: $StateDir"
        Write-Host 'Use health to check readiness; process startup alone does not prove readiness.'
    } finally { $Process.Dispose() }
}
function Stop-Router {
    $Process = Get-ManagedProcess
    if ($null -eq $Process) { Write-Host 'No matching managed process is running; nothing terminated.'; return }
    try {
        Write-Warning 'Windows background shutdown is forced; in-flight requests and streams will be interrupted.'
        # Kill this verified process object, never all processes sharing a name.
        $Process.Kill()
        if (-not $Process.WaitForExit($TimeoutSeconds * 1000)) { throw 'Shutdown timed out; process state retained' }
        Remove-Item -LiteralPath $StateFile
        Write-Host 'Stopped.'
    } finally { $Process.Dispose() }
}
try {
    if (-not $PSBoundParameters.ContainsKey('EnvFile') -and (Test-Path -LiteralPath '.env' -PathType Leaf)) {
        $EnvFile = '.env'
    }
    if ($EnvFile) { Import-EnvironmentFile $EnvFile }
    Set-Location -LiteralPath $Root
    if ($Command -in @('start','stop','restart','rebuild','run','status') -and $env:OS -ne 'Windows_NT') {
        throw 'Use scripts/sparkrouter.sh for Linux/macOS/WSL lifecycle management'
    }
    switch ($Command) {
        'help' { Show-Help }
        'keygen' {
            $Bytes = New-Object byte[] 32
            $Rng = [Security.Cryptography.RandomNumberGenerator]::Create()
            try { $Rng.GetBytes($Bytes); [Convert]::ToBase64String($Bytes) }
            finally { $Rng.Dispose(); [Array]::Clear($Bytes, 0, $Bytes.Length) }
            [Console]::Error.WriteLine('Save this master key securely and reuse it; this does not rotate stored credentials.')
        }
        'build' { Build-Binary }
        'check' {
            Invoke-Cargo -CargoArguments @('fmt','--check')
            Invoke-Cargo -CargoArguments @('check','--locked')
            Invoke-Cargo -CargoArguments @('clippy','--all-targets','--all-features','--locked','--','-D','warnings')
        }
        'test' { Invoke-Cargo -CargoArguments @('test','--locked') }
        'start' { Lock-State; Start-Router }
        'stop' { Lock-State; Stop-Router }
        'restart' { Lock-State; Stop-Router; Start-Router }
        'rebuild' { Lock-State; Stop-Router; Build-Binary; Start-Router }
        'run' {
            if (-not $env:ROUTER_MASTER_KEY) { throw 'Set ROUTER_MASTER_KEY or pass -EnvFile FILE' }
            Ensure-Binary
            & $Binary
            if ($LASTEXITCODE -ne 0) { throw "SparkRouter exited with code $LASTEXITCODE" }
        }
        'status' {
            $Process = Get-ManagedProcess
            if ($null -eq $Process) { Write-Host 'Not running (or process identity no longer matches).'; exit 1 }
            Write-Host "Running (PID $($Process.Id))."; $Process.Dispose()
        }
        'logs' {
            foreach ($File in @($OutLog,$ErrLog)) {
                if (Test-Path -LiteralPath $File -PathType Leaf) {
                    Write-Host "`n== $File =="; Get-Content -LiteralPath $File -Tail 100
                }
            }
        }
        'health' {
            if (-not $Url) {
                $BindAddress = if ($env:ROUTER_BIND) { $env:ROUTER_BIND } else { '127.0.0.1:14673' }
                $BindAddress = $BindAddress -replace '^0\.0\.0\.0:', '127.0.0.1:' -replace '^\[::\]:', '[::1]:'
                $Url = "http://$BindAddress"
            }
            $Response = Invoke-WebRequest -Uri ($Url.TrimEnd('/') + '/readyz') -TimeoutSec 5 -UseBasicParsing
            $Response.Content
        }
    }
} catch {
    # Do not echo exception details that could contain submitted configuration/secrets.
    # Our own fixed messages provide context; native command output remains unchanged.
    if ($_.Exception.Message -match '^(Invalid environment|Unsupported environment|Unmatched environment|Another helper|Invalid process state|Unable to verify|Set ROUTER_MASTER_KEY|Startup failed|Shutdown timed out|Use scripts/|cargo failed|SparkRouter exited)') {
        Write-Error $_.Exception.Message -ErrorAction Continue
    } else { Write-Error 'Helper operation failed; check file permissions, configuration, and command availability.' -ErrorAction Continue }
    exit 1
} finally {
    if ($null -ne $LockStream) { $LockStream.Dispose() }
    if ($Locked) { Remove-Item -LiteralPath (Join-Path $StateDir 'lock') -ErrorAction SilentlyContinue }
    foreach ($Name in $OriginalEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($Name, $OriginalEnvironment[$Name], 'Process')
    }
    Set-Location -LiteralPath $OriginalLocation.Path
}
