# S3 trial: app-server + two desktop clients on this machine. Client A shares a whole
# monitor (default: the secondary one, where both windows are), client B watches it.
# Runs while the user keeps working: windows open on the secondary monitor without
# activation (CreateProcess, see window-check.ps1), no mouse or keyboard input is sent,
# speakers play silence. Each client prints a `TURBO_PANCAKE_CALL ...` line per second
# with the share's sending stats (A) and the viewer's receive stats (B), including
# capture → decoded latency (both clients share one clock). Optional -StopAfter: A runs
# that many seconds and quits (B must see the share end). -SwitchTo: A's monitor index
# for a second run in the same room (monitor switch = a new share pipeline).
#
#   powershell -File benchmarks/scripts/screen-trial.ps1 [-Target D:/turbo-pancake-target/release]
#     [-Seconds 30] [-Monitor 1] [-Quality hd60] [-Media auto] [-Out benchmarks/results/screen-trial.log]
param(
  [string]$Target = "D:/turbo-pancake-target/release",
  [int]$Seconds = 30,
  [string]$Monitor = "1",
  [string]$Quality = "hd60",
  [string]$Media = "auto",
  [string]$Room = "s3-trial",
  [int]$StopAfter = 0,
  [string]$Out = ""
)
$ErrorActionPreference = "Stop"
$server = Join-Path $Target "turbo-pancake-app-server.exe"
$desktop = Join-Path $Target "turbo-pancake-desktop.exe"
$tmp = Join-Path ([IO.Path]::GetTempPath()) "tp-screen-trial"
New-Item -ItemType Directory -Force $tmp | Out-Null

$srv = Start-Process -FilePath $server -ArgumentList "--bind", "127.0.0.1:7878" -PassThru -WindowStyle Hidden `
  -RedirectStandardOutput "$tmp/server.out" -RedirectStandardError "$tmp/server.err"
Start-Sleep -Milliseconds 800
$common = @("--no-activate", "--room", $Room, "--call-log", "--remote-volume", "0", "--mute", "--media", $Media)
$aSeconds = if ($StopAfter -gt 0) { $StopAfter } else { $Seconds }
# Redirected output makes Start-Process use CreateProcess (no ShellExecute show mode).
$clients = @(
  @{ n = "b"; args = @("--smoke-exit-after-ms", "$($Seconds * 1000)", "--window-pos", "2560,300", "--name", "Viewer-B", "--watch") },
  @{ n = "a"; args = @("--smoke-exit-after-ms", "$($aSeconds * 1000)", "--window-pos", "1940,40", "--name", "Sharer-A",
      "--share", $Monitor, "--share-quality", $Quality) }
)
$procs = @()
foreach ($c in $clients) {
  $p = Start-Process -FilePath $desktop -PassThru -RedirectStandardOutput "$tmp/$($c.n).out" `
    -RedirectStandardError "$tmp/$($c.n).err" -ArgumentList ($common + $c.args)
  $null = $p.Handle  # keeps the handle, so ExitCode is available after exit
  $procs += $p
  Start-Sleep -Milliseconds 1000
}
foreach ($p in $procs) { $p.WaitForExit(($Seconds + 20) * 1000) | Out-Null }
foreach ($p in $procs) { if (-not $p.HasExited) { $p.Kill(); Write-Warning "client $($p.Id) did not exit" } }
Stop-Process -Id $srv.Id -ErrorAction SilentlyContinue

$report = @()
$report += "# screen-trial $(Get-Date -Format s) seconds=$Seconds monitor=$Monitor quality=$Quality media=$Media stop_after=$StopAfter room=$Room"
$report += "# exit codes (b a): " + (($procs | ForEach-Object { $_.ExitCode }) -join " ")
foreach ($c in $clients) {
  $report += "## client $($c.n)"
  # Device names identify the machine: the public log keeps only that a device was open.
  $report += Get-Content -Encoding UTF8 "$tmp/$($c.n).out" -ErrorAction SilentlyContinue |
    ForEach-Object { $_ -replace 'mic="[^"]*"', 'mic="<input>"' -replace 'out="[^"]*"', 'out="<output>"' }
}
if ($Out) { $report | Set-Content -Encoding utf8 $Out }
$report
