# S1 trial: app-server + two desktop clients on this machine, joined to one room, with
# real audio devices. Designed to run while the user keeps working: both windows open
# on the secondary monitor without activation; no mouse or keyboard input is sent.
# Each client prints a `TURBO_PANCAKE_CALL key=value ...` line per second (--call-log)
# and quits cleanly (leave room, release devices) after -Seconds.
#
#   powershell -File benchmarks/scripts/voice-trial.ps1 [-Target D:/turbo-pancake-target/release]
#     [-Seconds 30] [-Loss 0.05] [-Out benchmarks/results/voice-trial.log]
#
# Both clients use the default microphone and speakers, so on one machine they hear each
# other through the room (the acoustic loop the echo canceller must break). -Loss drops
# that share of client A's outgoing media (test impairment).
param(
  [string]$Target = "D:/turbo-pancake-target/release",
  [int]$Seconds = 30,
  [double]$Loss = 0.0,
  [string]$Room = "s1-trial",
  [string]$Out = ""
)
$ErrorActionPreference = "Stop"
$server = Join-Path $Target "turbo-pancake-app-server.exe"
$desktop = Join-Path $Target "turbo-pancake-desktop.exe"
$tmp = Join-Path ([IO.Path]::GetTempPath()) "tp-voice-trial"
New-Item -ItemType Directory -Force $tmp | Out-Null

$srv = Start-Process -FilePath $server -ArgumentList "--bind", "127.0.0.1:7878" -PassThru -WindowStyle Hidden `
  -RedirectStandardOutput "$tmp/server.out" -RedirectStandardError "$tmp/server.err"
Start-Sleep -Milliseconds 800
$ms = $Seconds * 1000
$common = @("--smoke-exit-after-ms", "$ms", "--no-activate", "--room", $Room, "--call-log")
# B waits in the room first, A joins and calls.
$b = Start-Process -FilePath $desktop -PassThru -RedirectStandardOutput "$tmp/b.out" -RedirectStandardError "$tmp/b.err" `
  -ArgumentList ($common + @("--window-pos", "2900,100", "--name", "Client-B"))
Start-Sleep -Milliseconds 1500
$a = Start-Process -FilePath $desktop -PassThru -RedirectStandardOutput "$tmp/a.out" -RedirectStandardError "$tmp/a.err" `
  -ArgumentList ($common + @("--window-pos", "1940,100", "--name", "Client-A", "--test-send-loss", "$Loss"))
$a.WaitForExit(($Seconds + 20) * 1000) | Out-Null
$b.WaitForExit(($Seconds + 20) * 1000) | Out-Null
foreach ($p in @($a, $b)) { if (-not $p.HasExited) { $p.Kill(); Write-Warning "client $($p.Id) did not exit" } }
Stop-Process -Id $srv.Id -ErrorAction SilentlyContinue

$report = @()
$report += "# voice-trial $(Get-Date -Format s) seconds=$Seconds loss_a=$Loss room=$Room"
$report += "# exit codes: a=$($a.ExitCode) b=$($b.ExitCode)"
foreach ($n in @("a", "b")) {
  $report += "## client $n"
  $report += Get-Content "$tmp/$n.out" -ErrorAction SilentlyContinue
}
if ($Out) { $report | Set-Content -Encoding utf8 $Out }
$report
