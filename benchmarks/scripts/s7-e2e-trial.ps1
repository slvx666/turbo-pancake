# S7 step 7: one Windows end-to-end chain with real release binaries.
#   install (per-user installer, ADR 0017) -> register / sign in (TLS server +
#   PostgreSQL) -> direct chat between two live clients -> direct call (simulated
#   audio, media over TCP) -> signed update 0.1.0 -> 0.1.1 through the server (ADR 0016),
#   stored session and history kept -> broken 0.1.2 rolled back -> uninstall --purge
#   (install, shortcut, uninstall entry, profile data and stored sessions removed).
# The client is built twice with a fresh trial update key pinned; the installer embeds
# the 0.1.0 package. User folders (LOCALAPPDATA, APPDATA, TEMP) point into the work
# directory and the trial id suffixes the HKCU key and prefixes the profiles, so the
# real profile and Credential Manager entries are untouched. Every client runs on an
# isolated invisible desktop (no focus, no input).
param([string]$CargoWrapper = '', [string]$PgBin = $env:TURBO_PANCAKE_PG_BIN,
      [string]$Backend = 'winit-software', [string]$Out = '', [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $CargoWrapper) { $CargoWrapper = Join-Path $PSScriptRoot 'apm-env.ps1' }
if (-not $PgBin) { throw 'Supply -PgBin (portable PostgreSQL binaries)' }
$release = Join-Path $workspacePath 't/release'
$keyedTarget = Join-Path $workspacePath 't/u'  # short: APM object paths stay below MAX_PATH
$trialId = 's7e' + [Guid]::NewGuid().ToString('N').Substring(0, 10)
$work = Join-Path $release ('s7-e2e-' + $trialId)
New-Item -ItemType Directory -Path $work -Force | Out-Null
$regKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\turbo-pancake-$trialId"
function Quote-Args($items) { ($items | ForEach-Object { '"' + ($_ -replace '"', '\"') + '"' }) -join ' ' }
function Cargo([string[]]$cargoArgs, [string]$log) {
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    & powershell -NoProfile -File $CargoWrapper cargo @cargoArgs *> $log
    $code = $LASTEXITCODE; $ErrorActionPreference = $old
    if ($code -ne 0) { throw "cargo failed ($code), see $log" }
}
function Read-Text($path) {
    if (-not (Test-Path $path)) { return '' }
    $fs = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try { (New-Object IO.StreamReader($fs, [Text.Encoding]::UTF8)).ReadToEnd() } finally { $fs.Dispose() }
}
function Free-Port {
    $listener = New-Object Net.Sockets.TcpListener ([Net.IPAddress]::Loopback), 0
    $listener.Start(); $port = $listener.LocalEndpoint.Port; $listener.Stop(); $port
}
Add-Type @'
using System; using System.IO; using System.Text; using System.Collections.Generic; using System.Runtime.InteropServices;
public sealed class S7eDesktop : IDisposable {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Startup {
        public uint cb; public string reserved,desktop,title; public uint x,y,w,h,xc,yc,fill,flags;
        public ushort show,reservedSize; public IntPtr reservedPtr,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] struct Info { public IntPtr process,thread; public uint pid,tid; }
    public sealed class Child { public IntPtr Process; public uint Pid; internal FileStream Log; }
    delegate bool EnumWindow(IntPtr window,IntPtr param);
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateDesktop(string name,IntPtr device,IntPtr mode,uint flags,uint access,IntPtr security);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("user32.dll")] static extern bool EnumDesktopWindows(IntPtr desktop,EnumWindow callback,IntPtr param);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window,out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string app,StringBuilder command,IntPtr ps,IntPtr ts,bool inherit,uint flags,IntPtr env,string dir,ref Startup start,out Info info);
    [DllImport("kernel32.dll")] static extern bool SetHandleInformation(IntPtr h,uint mask,uint flags);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr h,uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr h,out uint exit);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    readonly string name = "tp-s7e-"+Guid.NewGuid().ToString("N");
    readonly IntPtr desktop; readonly List<Child> children = new List<Child>();
    public S7eDesktop() { desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x1ff,IntPtr.Zero); if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception(); }
    public Child Start(string exe,string arguments,string dir,string log) {
        var c=new Child(); c.Log=new FileStream(log,FileMode.Append,FileAccess.Write,FileShare.ReadWrite);
        var h=c.Log.SafeFileHandle.DangerousGetHandle(); SetHandleInformation(h,1,1);
        var startup=new Startup(); startup.cb=(uint)Marshal.SizeOf(startup); startup.desktop=name; startup.flags=0x100; startup.output=h; startup.error=h;
        Info info; if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+arguments),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref startup,out info)) throw new System.ComponentModel.Win32Exception();
        CloseHandle(info.thread); c.Process=info.process; c.Pid=info.pid; children.Add(c); return c;
    }
    // Exit code, or -1 on timeout.
    public long Wait(Child c,int ms) { if(WaitForSingleObject(c.Process,(uint)ms)!=0) return -1; uint e; GetExitCodeProcess(c.Process,out e); c.Log.Flush(); return e; }
    public static uint ForegroundPid() { uint pid; GetWindowThreadProcessId(GetForegroundWindow(),out pid); return pid; }
    public uint[] VisiblePids() {
        var list=new List<uint>();
        EnumDesktopWindows(desktop,delegate(IntPtr w,IntPtr unused) { uint pid; GetWindowThreadProcessId(w,out pid); if(IsWindowVisible(w)) list.Add(pid); return true; },IntPtr.Zero);
        return list.ToArray();
    }
    public void Dispose() { foreach(var c in children) { c.Log.Dispose(); CloseHandle(c.Process); } CloseDesktop(desktop); }
}
'@
$script:seen = @{}; $script:foreground = 0
function Sample {
    $fg = [S7eDesktop]::ForegroundPid()
    $p = Get-Process -Id $fg -ErrorAction SilentlyContinue
    if ($p -and $p.Path -and $p.Path.StartsWith($work)) { $script:foreground++; throw "A trial window took the foreground: $($p.Path)" }
    foreach ($id in $native.VisiblePids()) {
        $q = Get-Process -Id $id -ErrorAction SilentlyContinue
        if ($q -and $q.Path -and $q.Path.StartsWith($work)) { $script:seen[$q.Path] = $true }
    }
}
function Wait-Until([scriptblock]$condition, [int]$ms, [string]$what) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($ms)
    while (-not (& $condition)) {
        Sample; Start-Sleep -Milliseconds 100
        if ([DateTime]::UtcNow -gt $deadline) { throw "Timed out: $what" }
    }
}
function Clients { Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith($root) } }
function Max-Played($text) { ([regex]::Matches($text,' played=(\d+)') | ForEach-Object { [int]$_.Groups[1].Value } | Measure-Object -Maximum).Maximum }
$native = $null; $server = $null; $pgStarted = $false; $summary = @(); $port = 0
$saved = @{}; foreach ($k in @('LOCALAPPDATA','APPDATA','TEMP','TMP','SLINT_BACKEND','TURBO_PANCAKE_SETUP_PAYLOAD','TURBO_PANCAKE_UPDATE_PUBKEYS','TURBO_PANCAKE_BUILD_VERSION','TURBO_PANCAKE_PASSWORD')) { $saved[$k] = [Environment]::GetEnvironmentVariable($k) }
try {
    # ---- 0 builds: tools, two keyed clients, installer with the 0.1.0 package ----
    if (-not $SkipBuild) {
        Cargo @('build','--release','--locked','-p','turbo-pancake-launcher','-p','turbo-pancake-update-tool','-p','turbo-pancake-app-server') (Join-Path $work 'build-tools.log')
    }
    $tool = Join-Path $release 'turbo-pancake-update.exe'
    $serverExe = Join-Path $release 'turbo-pancake-app-server.exe'
    $key = Join-Path $work 'trial-update-key.pk8'
    $keyOut = & $tool keygen --id trial --out $key
    $line = ($keyOut | Where-Object { $_ -like 'TURBO_PANCAKE_UPDATE_PUBKEYS=*' }) -replace '^TURBO_PANCAKE_UPDATE_PUBKEYS=', ''
    if (-not $line) { throw 'keygen failed' }
    $builds = @{}
    foreach ($v in @('0.1.0','0.1.1')) {
        $env:TURBO_PANCAKE_UPDATE_PUBKEYS = $line; $env:TURBO_PANCAKE_BUILD_VERSION = $v
        Cargo @('build','--release','--locked','-p','turbo-pancake-desktop','--target-dir',$keyedTarget) (Join-Path $work "build-$v.log")
        $dir = Join-Path $work "build-$v"; New-Item -ItemType Directory -Path $dir -Force | Out-Null
        Copy-Item (Join-Path $keyedTarget 'release/turbo-pancake-desktop.exe') (Join-Path $dir 'turbo-pancake-desktop.exe')
        $builds[$v] = $dir
    }
    $env:TURBO_PANCAKE_UPDATE_PUBKEYS = $saved['TURBO_PANCAKE_UPDATE_PUBKEYS']; $env:TURBO_PANCAKE_BUILD_VERSION = $saved['TURBO_PANCAKE_BUILD_VERSION']
    $stage = Join-Path $work 'stage'; New-Item -ItemType Directory -Path $stage -Force | Out-Null
    Copy-Item (Join-Path $release 'turbo-pancake.exe'), (Join-Path $builds['0.1.0'] 'turbo-pancake-desktop.exe') $stage
    $pkg = Join-Path $work 'package-0.1.0.tppk'
    & $tool pack --version 0.1.0 --input $stage --out $pkg | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'pack failed' }
    $env:TURBO_PANCAKE_SETUP_PAYLOAD = $pkg
    Cargo @('build','--release','--locked','-p','turbo-pancake-installer') (Join-Path $work 'build-setup.log')
    $env:TURBO_PANCAKE_SETUP_PAYLOAD = $saved['TURBO_PANCAKE_SETUP_PAYLOAD']
    $setup = Join-Path $work 'turbo-pancake-setup-0.1.0.exe'
    Copy-Item (Join-Path $release 'turbo-pancake-setup.exe') $setup
    $setupSize = (Get-Item $setup).Length
    if ($setupSize -gt 30MB) { throw "Installer is $setupSize bytes (> 30 MB)" }
    # ---- server: PostgreSQL + TLS + update directory (empty until a release is published) ----
    $pgPort = Free-Port; $port = Free-Port; $pgDir = Join-Path $work 'pg'
    & "$PgBin/initdb.exe" -D $pgDir -U postgres -A trust -E UTF8 --no-locale --no-sync --no-instructions *> (Join-Path $work 'initdb.log')
    if ($LASTEXITCODE -ne 0) { throw 'initdb failed' }
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $PgBin 'pg_ctl.exe'
    $psi.Arguments = Quote-Args @('start','-D',$pgDir,'-l',(Join-Path $work 'pg.log'),'-w','-o',"-h 127.0.0.1 -p $pgPort -c fsync=off")
    $psi.UseShellExecute=$false; $psi.CreateNoWindow=$true
    $pg = [Diagnostics.Process]::Start($psi)
    if (-not $pg.WaitForExit(30000) -or $pg.ExitCode -ne 0) { throw 'PostgreSQL start failed' }
    $pgStarted = $true
    & "$PgBin/createdb.exe" -h 127.0.0.1 -p $pgPort -U postgres s7e
    if ($LASTEXITCODE -ne 0) { throw 'createdb failed' }
    & $serverExe gen-cert --name 127.0.0.1 --out (Join-Path $work 'tls') *> (Join-Path $work 'cert.log')
    if ($LASTEXITCODE -ne 0) { throw 'certificate generation failed' }
    $upd = Join-Path $work 'updates'; New-Item -ItemType Directory -Path $upd -Force | Out-Null
    $spsi = New-Object Diagnostics.ProcessStartInfo
    $spsi.FileName = $serverExe
    $spsi.Arguments = Quote-Args @('--bind',"127.0.0.1:$port",'--database-url',"postgres://postgres@127.0.0.1:$pgPort/s7e",'--anonymous-calls','false','--no-media-udp',
        '--update-dir',$upd,'--tls-cert',(Join-Path $work 'tls/server.pem'),'--tls-key',(Join-Path $work 'tls/server.key'))
    $spsi.UseShellExecute = $false; $spsi.CreateNoWindow = $true
    $spsi.RedirectStandardOutput = $true; $spsi.RedirectStandardError = $true
    $server = [Diagnostics.Process]::Start($spsi)
    $serverOut = $server.StandardOutput.ReadToEndAsync(); $serverErr = $server.StandardError.ReadToEndAsync()
    Start-Sleep -Milliseconds 800
    function Publish($version, $inputDir) {
        & $tool sign --key $key --id trial --version $version --input $inputDir --out $upd *> (Join-Path $work "sign-$version.log")
        if ($LASTEXITCODE -ne 0) { throw "signing $version failed" }
    }
    # ---- isolated user folders ----
    foreach ($k in @('LOCALAPPDATA','APPDATA','TEMP')) { $d = Join-Path $work "env-$k"; New-Item -ItemType Directory -Path $d -Force | Out-Null; [Environment]::SetEnvironmentVariable($k, $d) }
    $env:TMP = $env:TEMP; $env:SLINT_BACKEND = $Backend
    $env:TURBO_PANCAKE_PASSWORD = [Guid]::NewGuid().ToString('N')
    $root = Join-Path $env:LOCALAPPDATA 'Programs\turbo-pancake'
    $launcher = Join-Path $root 'turbo-pancake.exe'
    $lnk = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\turbo-pancake ($trialId).lnk"
    $profileDir = Join-Path $env:LOCALAPPDATA 'turbo-pancake'
    $native = New-Object S7eDesktop
    $common = @('--server',"wss://127.0.0.1:$port/v1/signal",'--server-ca',(Join-Path $work 'tls/ca.pem'),'--no-activate','--simulate-audio','--media','tcp','--chat-log','--call-log')
    $alice = "$trialId-alice"; $bob = "$trialId-bob"
    function Client($label, [string[]]$extra) {
        $log = Join-Path $work "$label.log"
        $null = $native.Start($launcher, (Quote-Args ($common + $extra)), $root, $log)
        $log
    }
    function Wait-Exits([string[]]$logs, [int]$ms, [string]$what) {
        Wait-Until { -not (Clients) } $ms $what
        foreach ($l in $logs) {
            $t = Read-Text $l
            if ($t -notmatch 'TURBO_PANCAKE_FIRST_FRAME' -or $t -notmatch 'TURBO_PANCAKE_EXIT') { throw "$what : missing first-frame/exit marker in $(Split-Path -Leaf $l)" }
        }
    }
    # ---- 1 install ----
    $t0 = [DateTime]::UtcNow
    $c = $native.Start($setup, (Quote-Args @('--quiet','--trial-id',$trialId,'--no-launch')), $work, (Join-Path $work 'setup.log'))
    if ($native.Wait($c, 60000) -ne 0) { throw 'install failed' }
    $installMs = [int]([DateTime]::UtcNow - $t0).TotalMilliseconds
    $state = Get-Content -Raw (Join-Path $root 'state.json') | ConvertFrom-Json
    if ($state.current -ne '0.1.0' -or -not (Test-Path $regKey) -or -not (Test-Path $lnk)) { throw 'install: state, uninstall entry or shortcut missing' }
    $installed = (Get-ChildItem -Recurse -File $root | Measure-Object Length -Sum).Sum
    $summary += "1 install 0.1.0: PASS; installer $setupSize bytes ($([Math]::Round($setupSize/1MB,1)) MiB), ${installMs} ms, installed $installed bytes; HKCU entry + Start-menu shortcut"
    # ---- 2 register and sign in through the installed launcher ----
    $regLogs = @()
    foreach ($p in @($alice, $bob)) {
        $regLogs += Client "reg-$p" @('--profile',$p,'--register','--login',($p.Substring($trialId.Length + 1) + '_' + $trialId.Substring(3,6)),'--smoke-exit-after-ms','6000')
    }
    Wait-Exits $regLogs 30000 'registration'
    foreach ($l in $regLogs) { if ((Read-Text $l) -notmatch 'signed_in user=') { throw "registration failed: $l" } }
    $aliceUser = [regex]::Match((Read-Text $regLogs[0]),'signed_in user=(\S+)').Groups[1].Value
    $bobUser = [regex]::Match((Read-Text $regLogs[1]),'signed_in user=(\S+)').Groups[1].Value
    $summary += "2 register + sign in (TLS/WSS, PostgreSQL) via the installed launcher: PASS ($aliceUser, $bobUser); sessions stored per profile"
    # ---- 3 chat between two live clients (stored sessions) ----
    $msg = 'S7 e2e: message from alice'
    $bobChat = Client 'chat-bob' @('--profile',$bob,'--chat-with',$aliceUser,'--smoke-exit-after-ms','14000')
    Wait-Until { (Read-Text $bobChat) -match 'signed_in user=' } 20000 'bob signed in'
    Start-Sleep -Milliseconds 1500
    $aliceChat = Client 'chat-alice' @('--profile',$alice,'--chat-with',$bobUser,'--send',$msg,'--smoke-exit-after-ms','8000')
    Wait-Until { (Read-Text $bobChat) -match [regex]::Escape("messages=1 last_body=""$msg""") } 20000 'bob received the message'
    Wait-Exits @($bobChat, $aliceChat) 30000 'chat run'
    $sent = [regex]::Match((Read-Text $aliceChat), [regex]::Escape("messages=1 last_body=""$msg""") + ' last_state="([^"]+)" failed=0')
    if (-not $sent.Success) { throw 'chat: alice did not show her message as sent without failures' }
    $summary += "3 direct chat: PASS; alice (stored session) sent (state ""$($sent.Groups[1].Value)"", failed=0), bob live received messages=1 with the same body"
    # ---- 4 direct call ----
    $bobCall = Client 'call-bob' @('--profile',$bob,'--auto-join-call','--smoke-exit-after-ms','25000')
    Start-Sleep -Milliseconds 1500
    $aliceCall = Client 'call-alice' @('--profile',$alice,'--chat-with',$bobUser,'--call','--smoke-exit-after-ms','22000')
    Wait-Until { (Max-Played (Read-Text $aliceCall)) -ge 100 -and (Max-Played (Read-Text $bobCall)) -ge 100 } 25000 'call media both ways'
    Wait-Exits @($bobCall, $aliceCall) 40000 'call run'
    $summary += "4 direct call: PASS; media both ways (played alice=$(Max-Played (Read-Text $aliceCall)) bob=$(Max-Played (Read-Text $bobCall)) frames), clean exits"
    # ---- 5 signed update 0.1.0 -> 0.1.1; session and history kept ----
    Publish '0.1.1' $builds['0.1.1']
    $t0 = [DateTime]::UtcNow
    $upLog = Client 'update-alice' @('--profile',$alice,'--chat-with',$bobUser,'--trial-update','--smoke-exit-after-ms','15000')
    Wait-Until { (Read-Text $upLog) -match 'TURBO_PANCAKE_LAUNCHER confirmed version=0.1.1' } 60000 'update confirmed'
    $confirmedMs = [int]([DateTime]::UtcNow - $t0).TotalMilliseconds
    Wait-Until { -not (Clients) } 40000 'update run exit'
    $text = Read-Text $upLog
    $state = Get-Content -Raw (Join-Path $root 'state.json') | ConvertFrom-Json
    if ($state.current -ne '0.1.1' -or $state.previous -ne '0.1.0') { throw "update: wrong state $($state | ConvertTo-Json -Compress)" }
    if (([regex]::Matches($text, 'TURBO_PANCAKE_EXIT')).Count -ne 2) { throw 'update: both clients must exit cleanly' }
    $after = $text.Substring($text.IndexOf('plan=trial version=0.1.1'))
    if ($after -notmatch "signed_in user=$aliceUser" -or $after -notmatch [regex]::Escape("messages=1 last_body=""$msg""")) { throw 'update: 0.1.1 lost the stored session or the history' }
    if (-not $script:seen[(Join-Path $root 'versions\0.1.1\turbo-pancake-desktop.exe')]) { throw 'update: no native window of 0.1.1' }
    $summary += "5 signed update 0.1.0 -> 0.1.1: PASS; downloaded, verified, restarted, confirmed ${confirmedMs} ms after launch; 0.1.1 signed in with the stored session and shows the same history"
    # ---- 6 broken 0.1.2 is rolled back ----
    $broken = Join-Path $work 'build-0.1.2'; New-Item -ItemType Directory -Path $broken -Force | Out-Null
    Copy-Item $serverExe (Join-Path $broken 'turbo-pancake-desktop.exe')  # a real exe that exits at once with these args
    Publish '0.1.2' $broken
    $rbLog = Client 'rollback-alice' @('--profile',$alice,'--chat-with',$bobUser,'--trial-update','--smoke-exit-after-ms','15000')
    Wait-Until { (Read-Text $rbLog) -match 'LAUNCHER rollback version=0.1.2 to=0.1.1' } 60000 'rollback'
    Wait-Until { (Read-Text $rbLog) -match 'status=up_to_date version=0.1.1' } 30000 '0.1.2 not offered again'
    Wait-Until { -not (Clients) } 40000 'rollback run exit'
    $state = Get-Content -Raw (Join-Path $root 'state.json') | ConvertFrom-Json
    if ($state.current -ne '0.1.1' -or -not ($state.bad -contains '0.1.2')) { throw "rollback: wrong state $($state | ConvertTo-Json -Compress)" }
    $text = Read-Text $rbLog; $after = $text.Substring($text.IndexOf('LAUNCHER rollback'))
    if ($after -notmatch [regex]::Escape("messages=1 last_body=""$msg""")) { throw 'rollback: history not shown by the restored 0.1.1' }
    $summary += '6 broken 0.1.2: PASS; exited before confirming, marked bad, 0.1.1 restarted with the session and history, 0.1.2 not offered again'
    # ---- 7 uninstall --purge ----
    $creds = (& cmdkey.exe /list) -match "turbo-pancake/$trialId"
    if (-not $creds) { throw 'no stored sessions found before uninstall' }
    $reg = Get-ItemProperty $regKey
    $parts = [regex]::Matches($reg.QuietUninstallString, '"([^"]*)"') | ForEach-Object { $_.Groups[1].Value }
    $uLog = Join-Path $work 'uninstall.log'
    $c = $native.Start($parts[0], (Quote-Args (@($parts | Select-Object -Skip 1) + @('--purge'))), $work, $uLog)
    if ($native.Wait($c, 20000) -ne 0) { throw 'uninstall launcher failed' }
    Wait-Until { (Read-Text $uLog) -match 'TURBO_PANCAKE_UNINSTALL' } 30000 'uninstall finished'
    if ((Read-Text $uLog) -match 'UNINSTALL done=false') { throw 'uninstall reported a failure' }
    Wait-Until { -not (Get-ChildItem $env:TEMP -Filter 'turbo-pancake-uninstall-*.exe') } 15000 'temporary uninstaller removed'
    if ((Test-Path $root) -or (Test-Path $lnk) -or (Test-Path $regKey) -or (Test-Path $profileDir)) { throw 'uninstall left files, shortcut, registry entry or profile data' }
    if ((& cmdkey.exe /list) -match "turbo-pancake/$trialId") { throw 'uninstall --purge left stored sessions' }
    $summary += "7 uninstall --purge: PASS; install dir, shortcut, HKCU entry, profile data and $(@($creds).Count) stored sessions removed"
    $summary += "S7 E2E trial PASS; backend=$Backend; trial id $trialId; isolated user folders; invisible desktop; simulated audio; foreground_samples=$($script:foreground)"
    $summary | Set-Content -Encoding utf8 (Join-Path $work 'summary.log')
    if ($Out) { $summary | Set-Content -Encoding utf8 $Out }
    $summary
    Write-Output "Local artifacts: $work"
} finally {
    Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith($work) } | Stop-Process -Force -ErrorAction SilentlyContinue
    if ($native) { $native.Dispose() }
    if ($server) {
        if (-not $server.HasExited) { $server.Kill(); $server.WaitForExit() }
        $serverOut.Result | Set-Content -Encoding utf8 (Join-Path $work 'server.out')
        $serverErr.Result | Set-Content -Encoding utf8 (Join-Path $work 'server.err')
    }
    if ($pgStarted) {
        $psi.Arguments = Quote-Args @('stop','-D',$pgDir,'-m','fast','-w')
        [Diagnostics.Process]::Start($psi).WaitForExit(30000) | Out-Null
    }
    if (Test-Path $regKey) { Remove-Item -Recurse -Force $regKey }
    if ($port) { foreach ($p in @("$trialId-alice","$trialId-bob")) { & cmdkey.exe "/delete:turbo-pancake/$p/https://127.0.0.1:$port" *> $null } }
    foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
}
