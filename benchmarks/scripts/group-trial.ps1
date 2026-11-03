# S2 trial: app-server + three desktop clients on this machine in one room, with real
# audio devices. Runs while the user keeps working: every window opens on the secondary
# monitor without activation (launched through CreateProcess, see window-check.ps1); no
# mouse or keyboard input is sent. Each client prints a `TURBO_PANCAKE_CALL ...` line per
# second (--call-log, with per-member `members=name:played:concealed%:speaking`) and quits
# cleanly after -Seconds. Client C uses voice activation (-Gate dBFS).
#
#   powershell -File benchmarks/scripts/group-trial.ps1 [-Target D:/turbo-pancake-target/release]
#     [-Seconds 30] [-Gate -45] [-Out benchmarks/results/group-trial.log]
param(
  [string]$Target = "D:/turbo-pancake-target/release",
  [int]$Seconds = 30,
  [double]$Gate = -45,
  [string]$Room = "s2-trial",
  # Call volume in percent. Default 0: the full pipeline runs but the speakers play
  # silence (the owner keeps working during trials).
  [int]$Volume = 0,
  [string]$Out = ""
)
$ErrorActionPreference = "Stop"
$server = Join-Path $Target "turbo-pancake-app-server.exe"
$desktop = Join-Path $Target "turbo-pancake-desktop.exe"
$tmp = Join-Path ([IO.Path]::GetTempPath()) "tp-group-trial"
New-Item -ItemType Directory -Force $tmp | Out-Null

$srv = Start-Process -FilePath $server -ArgumentList "--bind", "127.0.0.1:7878" -PassThru -WindowStyle Hidden `
  -RedirectStandardOutput "$tmp/server.out" -RedirectStandardError "$tmp/server.err"
Start-Sleep -Milliseconds 800
$ms = $Seconds * 1000
$common = @("--smoke-exit-after-ms", "$ms", "--no-activate", "--room", $Room, "--call-log", "--remote-volume", "$Volume")
# Redirected output makes Start-Process use CreateProcess (no ShellExecute show mode).
$clients = @(
  @{ n = "a"; args = @("--window-pos", "1940,60", "--name", "Client-A") },
  @{ n = "b"; args = @("--window-pos", "2340,160", "--name", "Client-B") },
  @{ n = "c"; args = @("--window-pos", "2740,260", "--name", "Client-C", "--voice-gate", "$Gate") }
)
$procs = @()
foreach ($c in $clients) {
  $p = Start-Process -FilePath $desktop -PassThru -RedirectStandardOutput "$tmp/$($c.n).out" `
    -RedirectStandardError "$tmp/$($c.n).err" -ArgumentList ($common + $c.args)
  $null = $p.Handle  # keeps the handle, so ExitCode is available after exit
  $procs += $p
  Start-Sleep -Milliseconds 700
}
foreach ($p in $procs) { $p.WaitForExit(($Seconds + 20) * 1000) | Out-Null }
foreach ($p in $procs) { if (-not $p.HasExited) { $p.Kill(); Write-Warning "client $($p.Id) did not exit" } }
Stop-Process -Id $srv.Id -ErrorAction SilentlyContinue

$report = @()
$report += "# group-trial $(Get-Date -Format s) seconds=$Seconds gate_c=$Gate volume=$Volume room=$Room"
$report += "# exit codes: " + (($procs | ForEach-Object { $_.ExitCode }) -join " ")
foreach ($c in $clients) {
  $report += "## client $($c.n)"
  # Device names identify the machine: the public log keeps only that a device was open.
  $report += Get-Content -Encoding UTF8 "$tmp/$($c.n).out" -ErrorAction SilentlyContinue |
    ForEach-Object { $_ -replace 'mic="[^"]*"', 'mic="<input>"' -replace 'out="[^"]*"', 'out="<output>"' }
}
if ($Out) { $report | Set-Content -Encoding utf8 $Out }
$report
