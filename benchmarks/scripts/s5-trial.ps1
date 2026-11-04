# S5 trial: a real app-server with PostgreSQL + two release desktop clients (two users)
# on this machine. Runs while the user keeps working: windows on the secondary monitor,
# created without activation, no mouse or keyboard input; every check comes from the
# clients' `TURBO_PANCAKE_CHAT` lines (--chat-log).
#
#   powershell -File benchmarks/scripts/s5-trial.ps1 [-Target D:/turbo-pancake-target/release]
#     [-PgBin <dir with initdb/pg_ctl/createdb>] [-Out benchmarks/results/s5-trial.log]
#     [-Screenshots <dir>]   (own windows only; do not commit if other apps show)
#
# Phases: 1 register both, A writes to B (exchange, receipts); 2 B blocks A, A's send
# fails with the reason; 3 A restarts from the stored session (Credential Manager) and
# its cache: same history, no duplicates; 4 the server restarts while A runs: A
# reconnects (queued sends while offline are covered headless in messenger-client tests).
param(
  [string]$Target = "D:/turbo-pancake-target/release",
  [string]$PgBin = $env:TURBO_PANCAKE_PG_BIN,
  [string]$Out = "",
  [string]$Screenshots = "",
  [int]$Port = 7979,
  [int]$PgPort = 55432,
  [string]$X1 = "1940", [string]$X2 = "2900", [string]$Y = "100"
)
$ErrorActionPreference = "Stop"
$server = Join-Path $Target "turbo-pancake-app-server.exe"
$desktop = Join-Path $Target "turbo-pancake-desktop.exe"
$work = Join-Path $Target "s5-trial"
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $work | Out-Null
$log = @()
function Note($s) { $script:log += $s; Write-Host $s }

# ---- PostgreSQL (throw-away cluster in the build directory) ----
if (-not $PgBin) { throw "set -PgBin or TURBO_PANCAKE_PG_BIN" }
& "$PgBin/initdb.exe" -D "$work/pg" -U postgres -A trust -E UTF8 --no-locale --no-sync --no-instructions *> "$work/initdb.log"
# pg_ctl through CreateProcess without redirection: the server inherits pg_ctl's handles,
# so a redirected (piped) output would never end and PowerShell would wait forever.
$pgc = New-Object System.Diagnostics.ProcessStartInfo
$pgc.FileName = "$PgBin/pg_ctl.exe"
$pgc.Arguments = "start -D `"$work/pg`" -l `"$work/pg.log`" -w -o `"-h 127.0.0.1 -p $PgPort -c fsync=off`""
$pgc.UseShellExecute = $false
$pgc.CreateNoWindow = $true
$pgp = [System.Diagnostics.Process]::Start($pgc)
if (-not $pgp.WaitForExit(60000) -or $pgp.ExitCode -ne 0) { throw "PostgreSQL did not start, see $work/pg.log" }
& "$PgBin/createdb.exe" -h 127.0.0.1 -p $PgPort -U postgres tp
$db = "postgres://postgres@127.0.0.1:$PgPort/tp"

# CreateProcess without ShellExecute, so no window takes the foreground (window-check.ps1).
function Start-Hidden($exe, $argList, $name, [hashtable]$envs = @{}) {
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $exe
  $psi.Arguments = ($argList | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join " "
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
  foreach ($k in $envs.Keys) { $psi.EnvironmentVariables[$k] = $envs[$k] }
  $p = [System.Diagnostics.Process]::Start($psi)
  $outFile = "$work/$name.out"
  # Drain both streams asynchronously into files.
  $p | Add-Member -NotePropertyName OutTask -NotePropertyValue $p.StandardOutput.ReadToEndAsync()
  $p | Add-Member -NotePropertyName ErrTask -NotePropertyValue $p.StandardError.ReadToEndAsync()
  $p | Add-Member -NotePropertyName OutFile -NotePropertyValue $outFile
  $p
}
function Finish($p, [int]$timeoutMs = 60000) {
  if (-not $p.WaitForExit($timeoutMs)) { $p.Kill(); Note "WARN: process $($p.Id) killed" }
  Set-Content -Encoding utf8 $p.OutFile $p.OutTask.Result
  Add-Content -Encoding utf8 "$($p.OutFile).err" $p.ErrTask.Result
  Get-Content -Encoding utf8 $p.OutFile
}
function Start-Server {
  Start-Hidden $server @("--bind", "127.0.0.1:$Port", "--database-url", $db, "--no-media-udp") "server-$(Get-Random)"
}
function Shot($p, $file) {
  if (-not $Screenshots) { return }
  New-Item -ItemType Directory -Force $Screenshots | Out-Null
  Add-Type -AssemblyName System.Drawing
  Add-Type @"
using System; using System.Runtime.InteropServices;
public static class SW { public struct R { public int L, T, Rt, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r); }
"@ -ErrorAction SilentlyContinue
  $p.Refresh(); $h = $p.MainWindowHandle; if ($h -eq [IntPtr]::Zero) { return }
  $r = New-Object SW+R; [void][SW]::GetWindowRect($h, [ref]$r)
  $bmp = New-Object System.Drawing.Bitmap ($r.Rt - $r.L), ($r.B - $r.T)
  $g = [System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size)
  $bmp.Save((Join-Path $Screenshots $file), [System.Drawing.Imaging.ImageFormat]::Png); $g.Dispose(); $bmp.Dispose()
}

$pw = @{ "TURBO_PANCAKE_PASSWORD" = "trial password 1" }
$ws = "ws://127.0.0.1:$Port/v1/signal"
$common = @("--server", $ws, "--no-activate", "--chat-log")
$srv = Start-Server
Start-Sleep -Milliseconds 1500

# ---- Phase 1: exchange ----
$b = Start-Hidden $desktop ($common + @("--window-pos", "$X2,$Y", "--profile", "s5b", "--register", "--login", "bob_trial", "--smoke-exit-after-ms", "16000")) "p1-b" $pw
Start-Sleep -Milliseconds 3000
$a = Start-Hidden $desktop ($common + @("--window-pos", "$X1,$Y", "--profile", "s5a", "--register", "--login", "alice_trial",
  "--chat-with", "bob_trial", "--send", "привет, Боб 👋", "--send", "второе сообщение", "--smoke-exit-after-ms", "10000")) "p1-a" $pw
Start-Sleep -Milliseconds 7000
Shot $a "s5-a-phase1.png"; Shot $b "s5-b-phase1.png"
$outA = Finish $a; $outB = Finish $b
Note "## phase 1 — A"; $outA | ForEach-Object { Note $_ }
Note "## phase 1 — B"; $outB | ForEach-Object { Note $_ }

# ---- Phase 2: B blocks A; A's send fails ----
$b = Start-Hidden $desktop ($common + @("--window-pos", "$X2,$Y", "--profile", "s5b", "--chat-with", "alice_trial", "--block", "--smoke-exit-after-ms", "9000")) "p2-b"
Start-Sleep -Milliseconds 3500
$a = Start-Hidden $desktop ($common + @("--window-pos", "$X1,$Y", "--profile", "s5a", "--chat-with", "bob_trial", "--send", "после блокировки", "--smoke-exit-after-ms", "6000")) "p2-a"
Start-Sleep -Milliseconds 4500
Shot $a "s5-a-phase2.png"
$outA = Finish $a; $outB = Finish $b
Note "## phase 2 — A"; $outA | ForEach-Object { Note $_ }
Note "## phase 2 — B"; $outB | ForEach-Object { Note $_ }

# ---- Phase 3: A restarts from the stored session and its cache ----
$a = Start-Hidden $desktop ($common + @("--window-pos", "$X1,$Y", "--profile", "s5a", "--chat-with", "bob_trial", "--smoke-exit-after-ms", "5000")) "p3-a"
$outA = Finish $a
Note "## phase 3 — A (restart, resume)"; $outA | ForEach-Object { Note $_ }

# ---- Phase 4: server restart while A runs (B unblocks first) ----
$b = Start-Hidden $desktop ($common + @("--window-pos", "$X2,$Y", "--profile", "s5b", "--chat-with", "alice_trial", "--block", "--smoke-exit-after-ms", "5000")) "p4-b"
$outB = Finish $b
Note "## phase 4 — B (unblock)"; $outB | ForEach-Object { Note $_ }
$a = Start-Hidden $desktop ($common + @("--window-pos", "$X1,$Y", "--profile", "s5a", "--chat-with", "bob_trial", "--smoke-exit-after-ms", "16000")) "p4-a"
Start-Sleep -Milliseconds 3000
$srv.Kill(); $srv.WaitForExit()
Start-Sleep -Milliseconds 2500
$srv = Start-Server
$outA = Finish $a
Note "## phase 4 — A (server restart)"; $outA | ForEach-Object { Note $_ }

# ---- Clean up: server, database, the trial profiles' stored sessions and caches ----
$srv.Kill(); $srv.WaitForExit()
$pgc.Arguments = "stop -D `"$work/pg`" -m fast"
[System.Diagnostics.Process]::Start($pgc).WaitForExit(30000) | Out-Null
foreach ($p in @("s5a", "s5b")) {
  cmdkey /delete:"turbo-pancake/$p/http://127.0.0.1:$Port" *> $null
  Remove-Item -Recurse -Force (Join-Path $env:LOCALAPPDATA "turbo-pancake/cache/$p") -ErrorAction SilentlyContinue
}
if ($Out) { $log | Set-Content -Encoding utf8 $Out }
