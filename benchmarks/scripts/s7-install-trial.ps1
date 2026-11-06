# S7 step 6: per-user installer, upgrade, repair, uninstall (ADR 0017).
# Builds the release launcher/client, packs them as 0.1.0 and 0.1.1, builds one
# installer per package and exercises them with LOCALAPPDATA/APPDATA/TEMP pointed into
# the work directory (default install paths, but isolated) and a trial id that
# suffixes the HKCU uninstall key. Every client window runs on an isolated invisible
# desktop (no focus, no input). The trial removes its registry key at the end.
param([string]$CargoWrapper = '', [string]$Backend = 'winit-software', [string]$Out = '',
      [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $CargoWrapper) { $CargoWrapper = Join-Path $PSScriptRoot 'apm-env.ps1' }
$release = Join-Path $workspacePath 't/release'
$trialId = 's7i-' + [Guid]::NewGuid().ToString('N').Substring(0, 10)
$work = Join-Path $release ('s7-install-' + $trialId)
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
Add-Type @'
using System; using System.IO; using System.Text; using System.Collections.Generic; using System.Runtime.InteropServices;
public sealed class S7iDesktop : IDisposable {
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
    readonly string name = "tp-s7i-"+Guid.NewGuid().ToString("N");
    readonly IntPtr desktop; readonly List<Child> children = new List<Child>();
    public S7iDesktop() { desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x1ff,IntPtr.Zero); if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception(); }
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
    $fg = [S7iDesktop]::ForegroundPid()
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
function Ours { Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith($work) -and $_.Path -notlike '*\setup-*' } }
$native = $null; $summary = @()
$saved = @{}; foreach ($k in @('LOCALAPPDATA','APPDATA','TEMP','TMP','SLINT_BACKEND','TURBO_PANCAKE_SETUP_PAYLOAD')) { $saved[$k] = [Environment]::GetEnvironmentVariable($k) }
try {
    if (-not $SkipBuild) {
        Cargo @('build','--release','--locked','-p','turbo-pancake-launcher','-p','turbo-pancake-update-tool','-p','turbo-pancake-desktop') (Join-Path $work 'build.log')
    }
    $tool = Join-Path $release 'turbo-pancake-update.exe'
    $stage = Join-Path $work 'stage'; New-Item -ItemType Directory -Path $stage -Force | Out-Null
    Copy-Item (Join-Path $release 'turbo-pancake.exe'), (Join-Path $release 'turbo-pancake-desktop.exe') $stage
    $raw = (Get-ChildItem $stage | Measure-Object Length -Sum).Sum
    $setups = @{}
    foreach ($v in @('0.1.0','0.1.1')) {
        $pkg = Join-Path $work "package-$v.tppk"
        $packOut = & $tool pack --version $v --input $stage --out $pkg
        if ($LASTEXITCODE -ne 0) { throw "pack $v failed" }
        $env:TURBO_PANCAKE_SETUP_PAYLOAD = $pkg
        Cargo @('build','--release','--locked','-p','turbo-pancake-installer') (Join-Path $work "build-setup-$v.log")
        $setups[$v] = Join-Path $work "setup-$v.exe"
        Copy-Item (Join-Path $release 'turbo-pancake-setup.exe') $setups[$v]
        $env:TURBO_PANCAKE_SETUP_PAYLOAD = $saved['TURBO_PANCAKE_SETUP_PAYLOAD']
    }
    $setupSize = (Get-Item $setups['0.1.1']).Length
    if ($setupSize -gt 30MB) { throw "Installer is $setupSize bytes (> 30 MB)" }
    # Isolated user folders: the default install paths, inside the work directory.
    foreach ($k in @('LOCALAPPDATA','APPDATA','TEMP')) { $d = Join-Path $work "env-$k"; New-Item -ItemType Directory -Path $d -Force | Out-Null; [Environment]::SetEnvironmentVariable($k, $d) }
    $env:TMP = $env:TEMP; $env:SLINT_BACKEND = $Backend
    $root = Join-Path $env:LOCALAPPDATA 'Programs\turbo-pancake'
    $lnk = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\turbo-pancake ($trialId).lnk"
    $native = New-Object S7iDesktop
    $log = Join-Path $work 'trial.log'
    function Setup($v, [string[]]$extra, [int]$expect = 0) {
        $c = $native.Start($setups[$v], (Quote-Args (@('--quiet','--trial-id',$trialId) + $extra)), $work, $log)
        $code = $native.Wait($c, 60000)
        if ($code -ne $expect) { throw "setup $v $extra exited with $code (expected $expect)" }
    }
    function State { Get-Content -Raw (Join-Path $root 'state.json') | ConvertFrom-Json }
    $smoke = @('--no-activate','--simulate-audio','--server','ws://127.0.0.1:9/v1/signal')
    # ---- 1 fresh install, the installer starts the client ----
    $t0 = [DateTime]::UtcNow
    Setup '0.1.0' @()
    $installMs = [int]([DateTime]::UtcNow - $t0).TotalMilliseconds
    $clientExe = Join-Path $root 'versions\0.1.0\turbo-pancake-desktop.exe'
    Wait-Until { $script:seen[$clientExe] } 20000 'client window after install'
    Get-Process | Where-Object { $_.Path -eq $clientExe } | Stop-Process -Force
    Wait-Until { -not (Ours) } 10000 'client stopped'
    $s = State
    if ($s.current -ne '0.1.0') { throw 'fresh: state' }
    $reg = Get-ItemProperty $regKey
    if ($reg.DisplayVersion -ne '0.1.0' -or $reg.InstallLocation -ne $root -or $reg.UninstallString -notlike "*turbo-pancake.exe*--uninstall*") { throw 'fresh: registry entry' }
    $sh = New-Object -ComObject WScript.Shell
    $target = $sh.CreateShortcut($lnk).TargetPath
    if ($target -ne (Join-Path $root 'turbo-pancake.exe')) { throw "fresh: shortcut target $target" }
    $installed = (Get-ChildItem -Recurse -File $root | Measure-Object Length -Sum).Sum
    $summary += "1 fresh install: PASS; ${installMs} ms; client window started by the installer; HKCU entry, Start-menu shortcut, state current=0.1.0; installed $installed bytes"
    # ---- 2 start through the shortcut target ----
    $c = $native.Start($target, (Quote-Args ($smoke + @('--smoke-exit-after-ms','6000'))), $root, $log)
    Wait-Until { (Read-Text $log) -match 'TURBO_PANCAKE_EXIT' } 30000 'shortcut start'
    Wait-Until { -not (Ours) } 10000 'shortcut client exit'
    if ((Read-Text $log) -notmatch 'TURBO_PANCAKE_FIRST_FRAME') { throw 'shortcut: no first frame' }
    $summary += '2 Start-menu shortcut target starts the client: PASS (first frame, clean exit)'
    # ---- 3 upgrade, repair, older installer ----
    Setup '0.1.1' @('--no-launch')
    $s = State
    if ($s.current -ne '0.1.1' -or $s.previous -ne '0.1.0' -or (Get-ItemProperty $regKey).DisplayVersion -ne '0.1.1') { throw 'upgrade' }
    Remove-Item (Join-Path $root 'versions\0.1.1\turbo-pancake-desktop.exe')
    Setup '0.1.1' @('--no-launch')
    if (-not (Test-Path (Join-Path $root 'versions\0.1.1\turbo-pancake-desktop.exe'))) { throw 'repair' }
    Setup '0.1.0' @('--no-launch')
    if ((State).current -ne '0.1.1' -or (Get-ItemProperty $regKey).DisplayVersion -ne '0.1.1') { throw 'older installer replaced the newer version' }
    $text = Read-Text $log
    foreach ($m in @('action=fresh version=0.1.0','action=upgrade version=0.1.1','action=repair version=0.1.1','action=kept_newer version=0.1.0 current=0.1.1')) {
        if (-not $text.Contains($m)) { throw "missing $m" }
    }
    $summary += '3 upgrade 0.1.0 -> 0.1.1, repair of a deleted client, older installer keeps 0.1.1: PASS'
    # ---- 4 files in use ----
    $before = Get-Content -Raw (Join-Path $root 'state.json')
    $c = $native.Start((Join-Path $root 'turbo-pancake.exe'), (Quote-Args ($smoke + @('--smoke-exit-after-ms','12000'))), $root, $log)
    Wait-Until { $script:seen[(Join-Path $root 'versions\0.1.1\turbo-pancake-desktop.exe')] } 20000 'running client'
    Setup '0.1.1' @('--no-launch') 1
    if ((Get-Content -Raw (Join-Path $root 'state.json')) -ne $before -or -not (Test-Path (Join-Path $root 'versions\0.1.1\turbo-pancake-desktop.exe'))) { throw 'in-use repair damaged the install' }
    Wait-Until { -not (Ours) } 30000 'running client exit'
    $summary += '4 repair while the client runs: refused (exit 1), install intact: PASS'
    # ---- 5 uninstall keeps profile data; 6 reinstall + uninstall --purge removes it ----
    $profileDir = Join-Path $env:LOCALAPPDATA 'turbo-pancake'
    # Profile data as a signed-in client leaves it (cache and settings of a profile).
    foreach ($sub in @('cache\default','settings')) { New-Item -ItemType Directory -Path (Join-Path $profileDir $sub) -Force | Out-Null }
    'x' | Set-Content (Join-Path $profileDir 'settings\default.conf')
    function Uninstall([string[]]$extra) {
        $reg = Get-ItemProperty $regKey
        $parts = [regex]::Matches($reg.QuietUninstallString, '"([^"]*)"') | ForEach-Object { $_.Groups[1].Value }
        $marks = ([regex]::Matches((Read-Text $log), 'TURBO_PANCAKE_UNINSTALL')).Count
        $c = $native.Start($parts[0], (Quote-Args (@($parts | Select-Object -Skip 1) + $extra)), $work, $log)
        if ($native.Wait($c, 20000) -ne 0) { throw 'uninstall launcher failed' }
        Wait-Until { ([regex]::Matches((Read-Text $log), 'TURBO_PANCAKE_UNINSTALL')).Count -gt $marks } 30000 'uninstall finished'
        if ((Read-Text $log) -match 'UNINSTALL done=false') { throw 'uninstall reported a failure' }
    }
    Uninstall @()
    if ((Test-Path $root) -or (Test-Path $lnk) -or (Test-Path $regKey)) { throw 'uninstall left the install, shortcut or registry entry' }
    if (-not (Test-Path (Join-Path $profileDir 'settings'))) { throw 'uninstall without purge removed profile data' }
    Wait-Until { -not (Get-ChildItem $env:TEMP -Filter 'turbo-pancake-uninstall-*.exe') } 15000 'temporary uninstaller removed'
    $summary += '5 uninstall: install dir, shortcut, HKCU entry removed; profile data kept; temporary uninstaller deleted itself: PASS'
    Setup '0.1.1' @('--no-launch')
    Uninstall @('--purge')
    if ((Test-Path $root) -or (Test-Path $profileDir) -or (Test-Path $regKey)) { throw 'purge left data' }
    $summary += '6 reinstall + uninstall --purge: profile data removed too: PASS'
    $summary += "Sizes: client+launcher $raw bytes; installer $setupSize bytes ($([Math]::Round($setupSize/1MB,1)) MiB, limit 30 MB, deflate level 10); installed $installed bytes"
    $summary += "S7 install trial PASS; backend=$Backend; isolated user folders; trial id $trialId; invisible desktop; foreground_samples=$($script:foreground)"
    $summary | Set-Content -Encoding utf8 (Join-Path $work 'summary.log')
    if ($Out) { $summary | Set-Content -Encoding utf8 $Out }
    $summary
    Write-Output "Local artifacts: $work"
} finally {
    Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith($work) } | Stop-Process -Force -ErrorAction SilentlyContinue
    if ($native) { $native.Dispose() }
    if (Test-Path $regKey) { Remove-Item -Recurse -Force $regKey }
    foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
}
