# S8 step 6: Windows regression suite. One command runs, in order:
#   checks  - cargo fmt --check, Clippy -D warnings, the headless test baseline
#             (real PostgreSQL; the real system-audio test is filtered out: it plays
#             sound on the default output device)
#   build   - release server, client and bench
#   load    - short forms of the S8 load scenarios (messenger, group-load, sfu,
#             sfu-screen with accounts incl. a high-budget run, login-burst); each
#             JSON report's verdict is checked
#   native  - background native trials on an isolated invisible desktop (no focus, no
#             input, simulated audio): tray, one-instance, layout, S6 chat/call,
#             reconnect; -Full adds the update, installer and end-to-end chains
# and writes one summary (JSON + Markdown) with PASS / FAIL / SKIP, duration and log
# file per item. A failed item does not stop the suite; the exit code is 1 if any
# item failed.
#
#   powershell -NoProfile -File benchmarks/scripts/regression.ps1 `
#       [-CargoWrapper <ps1 that runs its arguments with the toolchain environment>] `
#       [-PgBin <dir with initdb/pg_ctl/pg_dump>] [-Backend winit-software] `
#       [-Only checks,build,load,native] [-Full] [-Report benchmarks/results/<name>.md]
#
# The Markdown copy given with -Report holds no absolute paths or host names (the repo
# is public); logs stay in the ignored log directory.
param(
    [string]$CargoWrapper = '',
    [string]$PgBin = $env:TURBO_PANCAKE_PG_BIN,
    [string]$Backend = 'winit-software',
    [string[]]$Only = @('checks', 'build', 'load', 'native'),
    [switch]$Full,
    [string]$LogDir = '',
    [string]$Report = ''
)
$ErrorActionPreference = 'Stop'
$workspace = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
Set-Location $workspace
if (-not $CargoWrapper) { $CargoWrapper = Join-Path $workspace 'benchmarks/scripts/apm-env.ps1' }
if (-not $PgBin) { throw 'Set -PgBin or TURBO_PANCAKE_PG_BIN (PostgreSQL binaries for the tests and load runs)' }
$env:TURBO_PANCAKE_PG_BIN = $PgBin
# `powershell -File` passes "a,b" as one string: split the groups here.
$Only = @($Only | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim().ToLowerInvariant() } | Where-Object { $_ })
$unknown = @($Only | Where-Object { $_ -notin @('checks', 'build', 'load', 'native') })
if ($unknown.Count -gt 0) { throw "Unknown -Only group(s): $($unknown -join ', ') (checks, build, load, native)" }
$stamp = (Get-Date).ToUniversalTime().ToString('yyyyMMddTHHmmssZ')
if (-not $LogDir) { $LogDir = Join-Path $workspace ".local/logs/regression-$stamp" }
New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$reports = Join-Path $LogDir 'reports'
New-Item -ItemType Directory -Force -Path $reports | Out-Null
$meta = & cargo metadata --format-version 1 --no-deps | ConvertFrom-Json
$target = $meta.target_directory
$release = Join-Path $target 'release'
$server = Join-Path $release 'turbo-pancake-app-server.exe'
$bench = Join-Path $release 'turbo-pancake-bench.exe'
$items = New-Object System.Collections.ArrayList
$commit = (& git rev-parse --short HEAD).Trim()

function Quote([string]$a) {
    if ($a -match '[\s"]') { return '"' + ($a -replace '"', '\"') + '"' }
    return $a
}

# Runs one command with a timeout; stdout and stderr go to the item's log.
function Invoke-Logged([string]$name, [string]$exe, [string[]]$arguments, [int]$timeoutMin) {
    $log = Join-Path $LogDir "$name.log"
    $err = Join-Path $LogDir "$name.err.log"
    $argLine = ($arguments | ForEach-Object { Quote $_ }) -join ' '
    $p = Start-Process -FilePath $exe -ArgumentList $argLine -NoNewWindow -PassThru `
        -RedirectStandardOutput $log -RedirectStandardError $err
    $null = $p.Handle  # keeps the exit code available after the process ends
    $timedOut = -not $p.WaitForExit($timeoutMin * 60 * 1000)
    if ($timedOut) {
        & taskkill /T /F /PID $p.Id 2>&1 | Out-Null
        $code = -1
    } else {
        $p.WaitForExit()
        $code = $p.ExitCode
    }
    if ((Test-Path $err) -and (Get-Item $err).Length -gt 0) {
        Add-Content -Path $log -Value "`n--- stderr ---" -Encoding utf8
        Get-Content $err | Add-Content -Path $log -Encoding utf8
    }
    Remove-Item $err -ErrorAction SilentlyContinue
    return @{ Code = $code; Log = $log; TimedOut = $timedOut }
}

function Add-Item([string]$group, [string]$name, [string]$status, [double]$seconds, [string]$log, [string]$detail) {
    $null = $items.Add([pscustomobject]@{
        group = $group; name = $name; status = $status; seconds = [math]::Round($seconds, 1)
        log = if ($log) { Split-Path $log -Leaf } else { '' }; detail = $detail
    })
    Write-Host ("[{0}] {1}/{2} ({3:N0} s) {4}" -f $status, $group, $name, $seconds, $detail)
}

function Run-Cargo([string]$group, [string]$name, [string[]]$cargoArgs, [int]$timeoutMin, [scriptblock]$detailOf) {
    $t = [Diagnostics.Stopwatch]::StartNew()
    $r = Invoke-Logged $name 'powershell' (@('-NoProfile', '-File', $CargoWrapper, 'cargo') + $cargoArgs) $timeoutMin
    $detail = if ($detailOf) { & $detailOf $r.Log } else { '' }
    if ($r.TimedOut) { $detail = "timed out after $timeoutMin min; $detail" }
    $status = if ($r.Code -eq 0) { 'PASS' } else { 'FAIL' }
    Add-Item $group $name $status $t.Elapsed.TotalSeconds $r.Log $detail
    return ($r.Code -eq 0)
}

function Test-Summary([string]$name, [string]$text) {
    $m = [regex]::Match($text, 'report written to (.+)')
    if (-not $m.Success) { return @($false, 'no report written') }
    $path = $m.Groups[1].Value.Trim()
    $j = Get-Content $path -Raw | ConvertFrom-Json
    $s = $j.summary
    switch ($j.scenario) {
        'sfu' {
            $bad = @($s.phases | Where-Object { $_.delivered -lt 0.98 })
            $d = ($s.phases | ForEach-Object { "t$($_.talkers)=$([math]::Round($_.delivered, 3))" }) -join ' '
            return @(($bad.Count -eq 0), "delivered $d")
        }
        'sfu-screen' {
            $d = "main=$([math]::Round($s.delivered_main, 3)) preview=$([math]::Round($s.delivered_preview, 3)) voice=$([math]::Round($s.delivered_audio, 3)) high=$($s.viewers.high) low=$($s.viewers.low_reason_admission) cpu=$([math]::Round($s.server_cpu_pct_of_one_core))%"
            return @(($s.pass -eq $true), $d)
        }
        'messenger' {
            return @(($s.pass -eq $true), "presence missed=$($s.presence.missed_deliveries) fanout missed=$($s.group_fanout.missed_deliveries) stale=$($s.snapshot_presence.wrong)")
        }
        'group-load' {
            return @(($s.pass -eq $true), "sends=$($s.load.accepted_sends) missed=$($s.live.missed) while_removed=$($s.live.delivered_while_removed) storm_missed=$($s.reconnect_storm.missed_after_resync) gaps=$($s.history.gaps)")
        }
        'login-burst' {
            return @(($s.pass -eq $true), "ok=$($s.ok) 503=$($s.refused_503) peak_hashes=$($s.password_hashing.peak_running) mem_above_idle=$([math]::Round($s.server_private_mib.peak_above_idle))MiB")
        }
        default { return @($false, "unknown scenario $($j.scenario)") }
    }
}

function Run-Bench([string]$name, [string[]]$benchArgs, [int]$timeoutMin) {
    $t = [Diagnostics.Stopwatch]::StartNew()
    $r = Invoke-Logged $name $bench (@('--out-dir', $reports) + $benchArgs) $timeoutMin
    $text = Get-Content $r.Log -Raw
    if ($r.Code -ne 0) {
        $why = if ($r.TimedOut) { "timed out after $timeoutMin min" } else { "exit $($r.Code)" }
        Add-Item 'load' $name 'FAIL' $t.Elapsed.TotalSeconds $r.Log $why
        return
    }
    $v = Test-Summary $name $text
    $status = if ($v[0]) { 'PASS' } else { 'FAIL' }
    Add-Item 'load' $name $status $t.Elapsed.TotalSeconds $r.Log $v[1]
}

function Run-Trial([string]$name, [string]$script, [string[]]$trialArgs, [int]$timeoutMin) {
    $t = [Diagnostics.Stopwatch]::StartNew()
    $out = Join-Path $LogDir "$name.trial.log"
    $r = Invoke-Logged $name 'powershell' (@('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $workspace "benchmarks/scripts/$script")) + $trialArgs + @('-Out', $out)) $timeoutMin
    # The trial's own verdict line, else a count of its true/false check lines.
    $last = ''
    if (Test-Path $out) {
        $lines = Get-Content $out
        $last = ($lines | Where-Object { $_ -match 'trial (PASS|FAIL)' } | Select-Object -Last 1)
        if (-not $last) {
            # PowerShell booleans of the trial's checks (lowercase values are markers).
            $yes = @($lines | Where-Object { $_ -cmatch '=True$' }).Count
            $no = @($lines | Where-Object { $_ -cmatch '=False$' }).Count
            $last = "checks: $yes true, $no false"
        }
    }
    if (-not $last) { $last = (Get-Content $r.Log | Select-Object -Last 1) }
    if ($r.TimedOut) { $last = "timed out after $timeoutMin min" }
    $status = if ($r.Code -eq 0) { 'PASS' } else { 'FAIL' }
    if ($last.Length -gt 160) { $last = $last.Substring(0, 160) + '...' }
    Add-Item 'native' $name $status $t.Elapsed.TotalSeconds $r.Log $last
}

$suite = [Diagnostics.Stopwatch]::StartNew()
$built = $true

if ($Only -contains 'checks') {
    $null = Run-Cargo 'checks' 'fmt' @('fmt', '--all', '--', '--check') 10 $null
    $null = Run-Cargo 'checks' 'clippy' @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings') 60 $null
    $null = Run-Cargo 'checks' 'tests' @('test', '--workspace', '--locked', '--no-fail-fast', '--', '--skip', 'own_sound_is_excluded') 90 {
        param($log)
        $p = 0; $f = 0; $i = 0; $fl = 0; $n = 0
        foreach ($m in (Select-String -Path $log -Pattern '(\d+) passed; (\d+) failed; (\d+) ignored; \d+ measured; (\d+) filtered')) {
            $g = $m.Matches[0].Groups
            $p += [int]$g[1].Value; $f += [int]$g[2].Value; $i += [int]$g[3].Value; $fl += [int]$g[4].Value; $n++
        }
        "$p passed, $f failed, $i ignored, $fl filtered, $n binaries"
    }
}

if ($Only -contains 'build') {
    $built = Run-Cargo 'build' 'release' @('build', '--release', '--locked', '-p', 'turbo-pancake-app-server', '-p', 'turbo-pancake-desktop', '-p', 'turbo-pancake-bench') 60 $null
}

if ($Only -contains 'load') {
    if (-not $built -or -not (Test-Path $bench)) {
        Add-Item 'load' 'all' 'SKIP' 0 '' 'release build missing or failed'
    } else {
        Run-Bench 'messenger-100u' @('messenger', '--server-exe', $server, '--users', '100', '--churn', '5', '--messages', '5', '--hold-seconds', '5', '--silent', '2') 10
        Run-Bench 'group-load-1x100' @('group-load', '--server-exe', $server, '--groups', '1', '--messages-per-sender', '20', '--churn-rounds', '2', '--storm-messages', '20') 10
        Run-Bench 'sfu-10m-accounts' @('sfu', '--server-exe', $server, '--members', '10', '--talkers', '1,10', '--seconds', '3', '--accounts') 10
        Run-Bench 'screen-publishers-10' @('sfu-screen', '--server-exe', $server, '--scenario', 'publishers', '--n', '10', '--seconds', '4', '--talkers', '2', '--accounts') 10
        Run-Bench 'screen-grid-10' @('sfu-screen', '--server-exe', $server, '--scenario', 'grid', '--n', '10', '--seconds', '4', '--accounts') 10
        Run-Bench 'screen-fanout-10-budget' @('sfu-screen', '--server-exe', $server, '--scenario', 'fanout', '--n', '10', '--seconds', '4', '--talkers', '2', '--accounts', '--max-high-egress-mbps', '8') 10
        Run-Bench 'login-burst-100' @('login-burst', '--server-exe', $server, '--logins', '100') 10
    }
}

if ($Only -contains 'native') {
    if (-not $built) {
        Add-Item 'native' 'all' 'SKIP' 0 '' 'release build failed'
    } else {
        $common = @('-Target', $release, '-Backend', $Backend)
        Run-Trial 'tray' 's7-tray-trial.ps1' $common 10
        Run-Trial 'instance' 's7-instance-trial.ps1' $common 10
        Run-Trial 'layout' 's7-layout-trial.ps1' $common 15
        Run-Trial 's6-chat-call' 's6-trial.ps1' ($common + @('-PgBin', $PgBin)) 20
        Run-Trial 'reconnect' 's7-reconnect-trial.ps1' ($common + @('-PgBin', $PgBin)) 15
        if ($Full) {
            $wrap = @('-CargoWrapper', $CargoWrapper, '-Backend', $Backend)
            Run-Trial 'update' 's7-update-trial.ps1' $wrap 60
            Run-Trial 'installer' 's7-install-trial.ps1' $wrap 60
            Run-Trial 'e2e' 's7-e2e-trial.ps1' ($wrap + @('-PgBin', $PgBin)) 60
        }
    }
}

$failed = @($items | Where-Object { $_.status -eq 'FAIL' }).Count
$passed = @($items | Where-Object { $_.status -eq 'PASS' }).Count
$skipped = @($items | Where-Object { $_.status -eq 'SKIP' }).Count
$verdict = if ($failed -eq 0 -and $passed -gt 0) { 'PASS' } else { 'FAIL' }
$total = [math]::Round($suite.Elapsed.TotalMinutes, 1)
$summary = [pscustomobject]@{
    suite = 'regression'; verdict = $verdict; commit = $commit; utc = $stamp
    backend = $Backend; full = [bool]$Full; minutes = $total
    passed = $passed; failed = $failed; skipped = $skipped; items = $items
}
# UTF-8 without BOM (Out-File in PowerShell 5.1 writes one).
$utf8 = New-Object System.Text.UTF8Encoding($false)
[IO.File]::WriteAllText((Join-Path $LogDir 'summary.json'), ($summary | ConvertTo-Json -Depth 4), $utf8)

$cpus = [Environment]::ProcessorCount
$md = @("# Regression suite ($verdict)", '',
    "Commit ``$commit`` (plus uncommitted changes, if any), $stamp, Windows, $cpus logical CPUs, renderer ``$Backend``$(if ($Full) { ', full' }). Total $total min: $passed PASS, $failed FAIL, $skipped SKIP.", '',
    '| Group | Item | Result | Time | Details |', '|---|---|---|---|---|')
foreach ($it in $items) {
    $md += "| $($it.group) | $($it.name) | **$($it.status)** | $($it.seconds) s | $($it.detail -replace '\|', '/') |"
}
$md += @('', 'Logs: one file per item in the local log directory (not committed).')
[IO.File]::WriteAllLines((Join-Path $LogDir 'summary.md'), [string[]]$md, $utf8)
if ($Report) { [IO.File]::WriteAllLines([IO.Path]::GetFullPath((Join-Path $workspace $Report)), [string[]]$md, $utf8) }
Write-Host "regression $verdict ($passed pass, $failed fail, $skipped skip, $total min); summary: $(Join-Path $LogDir 'summary.md')"
if ($verdict -ne 'PASS') { exit 1 }
