# S4 trial: app-server + three desktop clients on this machine. A shares a whole monitor
# (default: the secondary one, where all windows are) with the chosen top quality and
# automatic adaptation, C shares a generated picture, B watches A (asking for -WatchLayer)
# and shows C as a preview. Same rules as screen-trial.ps1: windows on the secondary
# monitor without activation (CreateProcess), no input sent, silence on the speakers.
# Every client prints a `TURBO_PANCAKE_CALL ...` line per second (layers, previews,
# adaptation). Impairments: -ALoss drops A's outgoing media (uplink → adaptation),
# -BRecvLoss drops B's incoming media (downlink → the server moves B to the low layer).
# -Budget sets the server's high-stream budget in Mbit/s.
#
#   powershell -File benchmarks/scripts/s4-trial.ps1 [-Seconds 30] [-Quality qhd120]
#     [-WatchLayer auto] [-ALoss 0] [-BRecvLoss 0] [-Budget 0] [-ShareWindow [-Fullscreen]] [-Out log]
param(
  [string]$Target = "D:/turbo-pancake-target/release",
  [int]$Seconds = 30,
  [string]$Monitor = "1",
  [string]$Quality = "qhd120",
  [string]$WatchLayer = "auto",
  [double]$ALoss = 0,
  [double]$BRecvLoss = 0,
  [int]$Budget = 0,
  [switch]$ShareWindow,
  [switch]$Fullscreen,
  [string]$Room = "s4-trial",
  [string]$Out = ""
)
$ErrorActionPreference = "Stop"
$server = Join-Path $Target "turbo-pancake-app-server.exe"
$desktop = Join-Path $Target "turbo-pancake-desktop.exe"
$tmp = Join-Path ([IO.Path]::GetTempPath()) "tp-s4-trial"
New-Item -ItemType Directory -Force $tmp | Out-Null

$srvArgs = @("--bind", "127.0.0.1:7878")
if ($Budget -gt 0) { $srvArgs += @("--max-high-egress-mbps", "$Budget") }
$srv = Start-Process -FilePath $server -ArgumentList $srvArgs -PassThru -WindowStyle Hidden `
  -RedirectStandardOutput "$tmp/server.out" -RedirectStandardError "$tmp/server.err"
Start-Sleep -Milliseconds 800
$common = @("--no-activate", "--room", $Room, "--call-log", "--remote-volume", "0", "--mute",
  "--smoke-exit-after-ms", "$($Seconds * 1000)", "--share-no-audio")
$b = @("--window-pos", "2560,300", "--name", "Viewer-B", "--watch", "--watch-layer", $WatchLayer)
if ($BRecvLoss -gt 0) { $b += @("--test-recv-loss", "$BRecvLoss") }
if ($ShareWindow) { $b += @("--share-window") }
if ($Fullscreen) { $b += @("--fullscreen") }
$a = @("--window-pos", "1940,40", "--name", "Sharer-A", "--share", $Monitor, "--share-quality", $Quality)
if ($ALoss -gt 0) { $a += @("--test-send-loss", "$ALoss") }
$c = @("--window-pos", "2300,500", "--name", "Sharer-C", "--share", "synthetic", "--share-quality", "hd30")
# B joins first and watches the first share: A starts before C.
$clients = @(@{ n = "b"; args = $b }, @{ n = "a"; args = $a }, @{ n = "c"; args = $c })
$procs = @()
foreach ($x in $clients) {
  $p = Start-Process -FilePath $desktop -PassThru -RedirectStandardOutput "$tmp/$($x.n).out" `
    -RedirectStandardError "$tmp/$($x.n).err" -ArgumentList ($common + $x.args)
  $null = $p.Handle
  $procs += $p
  Start-Sleep -Milliseconds 1500
}
$windows = @()
if ($ShareWindow) {
  # Which top-level windows B has open (title, position, foreground?): no screenshots.
  Add-Type @"
using System; using System.Text; using System.Runtime.InteropServices; using System.Collections.Generic;
public static class TpWin {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  public static List<string> Of(uint pid) {
    var list = new List<string>(); var fg = GetForegroundWindow();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p);
      if (p == pid && IsWindowVisible(h)) { var sb = new StringBuilder(256); GetWindowText(h, sb, 256); RECT r; GetWindowRect(h, out r);
        list.Add(sb.ToString() + " @" + r.L + "," + r.T + " " + (r.R - r.L) + "x" + (r.B - r.T) + (h == fg ? " FOREGROUND" : "")); }
      return true; }, IntPtr.Zero);
    return list; } }
"@
  Start-Sleep -Seconds ([int]($Seconds / 2))
  $windows = [TpWin]::Of([uint32]$procs[0].Id)
}
foreach ($p in $procs) { $p.WaitForExit(($Seconds + 20) * 1000) | Out-Null }
foreach ($p in $procs) { if (-not $p.HasExited) { $p.Kill(); Write-Warning "client $($p.Id) did not exit" } }
Stop-Process -Id $srv.Id -ErrorAction SilentlyContinue

$report = @()
$report += "# s4-trial $(Get-Date -Format s) seconds=$Seconds monitor=$Monitor quality=$Quality watch_layer=$WatchLayer a_loss=$ALoss b_recv_loss=$BRecvLoss budget_mbps=$Budget share_window=$ShareWindow fullscreen=$Fullscreen"
$report += "# exit codes (b a c): " + (($procs | ForEach-Object { $_.ExitCode }) -join " ")
foreach ($w in $windows) { $report += "# window of b: $w" }
foreach ($x in $clients) {
  $report += "## client $($x.n)"
  $report += Get-Content -Encoding UTF8 "$tmp/$($x.n).out" -ErrorAction SilentlyContinue |
    ForEach-Object { $_ -replace 'mic="[^"]*"', 'mic="<input>"' -replace 'out="[^"]*"', 'out="<output>"' }
}
if ($Out) { $report | Set-Content -Encoding utf8 $Out }
$report
