# S7 step 5: signed update, atomic switch and rollback with real release binaries.
# Builds the client twice (0.1.0 and 0.1.1) with a freshly generated trial key pinned,
# lays out a per-user install (launcher + versions/0.1.0 + state.json), publishes
# signed releases on a real TLS server (--update-dir) and runs everything on an isolated
# invisible desktop (no focus, no input, simulated audio):
#   A  0.1.0 finds 0.1.1, downloads, verifies, restarts through the launcher; 0.1.1
#      confirms itself (current=0.1.1, previous=0.1.0).
#   B  a broken 0.1.2 (an executable that exits at once) is installed; the launcher's
#      trial fails, 0.1.2 is marked bad and 0.1.1 starts again; 0.1.2 is not re-offered.
#   C  available but not confirmed by the user: nothing installed (settings snapshot);
#      truncated file, tampered manifest and a signed downgrade change nothing.
# The trial key lives in the ignored work directory only.
param([string]$CargoWrapper = '', [string]$Backend = 'winit-software', [string]$Out = '',
      [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $CargoWrapper) { $CargoWrapper = Join-Path $PSScriptRoot 'apm-env.ps1' }
$release = Join-Path $workspacePath 't/release'
$keyedTarget = Join-Path $workspacePath 't/u'  # short: APM object paths stay below MAX_PATH
$trialId = [Guid]::NewGuid().ToString('N')
$work = Join-Path $release ('s7-update-' + $trialId)
New-Item -ItemType Directory -Path $work -Force | Out-Null
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
function Convert-Ppm($path) {
    Add-Type -AssemblyName System.Drawing
    $bytes = [IO.File]::ReadAllBytes($path)
    $header = [Text.Encoding]::ASCII.GetString($bytes, 0, [Math]::Min(40, $bytes.Length)) -split '\s+'
    $w = [int]$header[1]; $h = [int]$header[2]; $offset = ("P6`n$w $h`n255`n").Length
    $bmp = New-Object Drawing.Bitmap $w, $h
    for ($y = 0; $y -lt $h; $y++) { for ($x = 0; $x -lt $w; $x++) {
        $i = $offset + 3 * ($y * $w + $x)
        $bmp.SetPixel($x, $y, [Drawing.Color]::FromArgb($bytes[$i], $bytes[$i+1], $bytes[$i+2])) } }
    $bmp.Save([IO.Path]::ChangeExtension($path, '.png')); $bmp.Dispose()
}
Add-Type @'
using System; using System.IO; using System.Text; using System.Collections.Generic; using System.Runtime.InteropServices;
public sealed class S7uDesktop : IDisposable {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Startup {
        public uint cb; public string reserved,desktop,title; public uint x,y,w,h,xc,yc,fill,flags;
        public ushort show,reservedSize; public IntPtr reservedPtr,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] struct Info { public IntPtr process,thread; public uint pid,tid; }
    delegate bool EnumWindow(IntPtr window,IntPtr param);
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateDesktop(string name,IntPtr device,IntPtr mode,uint flags,uint access,IntPtr security);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("user32.dll")] static extern bool EnumDesktopWindows(IntPtr desktop,EnumWindow callback,IntPtr param);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window,out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string app,StringBuilder command,IntPtr ps,IntPtr ts,bool inherit,uint flags,IntPtr env,string dir,ref Startup start,out Info info);
    [DllImport("kernel32.dll")] static extern bool SetHandleInformation(IntPtr h,uint mask,uint flags);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    readonly string name = "tp-s7u-"+Guid.NewGuid().ToString("N");
    readonly IntPtr desktop; readonly List<FileStream> logs = new List<FileStream>();
    public S7uDesktop() { desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x1ff,IntPtr.Zero); if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception(); }
    public uint Start(string exe,string arguments,string dir,string log) {
        var f=new FileStream(log,FileMode.Create,FileAccess.Write,FileShare.ReadWrite); logs.Add(f);
        var h=f.SafeFileHandle.DangerousGetHandle(); SetHandleInformation(h,1,1);
        var startup=new Startup(); startup.cb=(uint)Marshal.SizeOf(startup); startup.desktop=name; startup.flags=0x100; startup.output=h; startup.error=h;
        Info info; if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+arguments),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref startup,out info)) throw new System.ComponentModel.Win32Exception();
        CloseHandle(info.thread); CloseHandle(info.process); return info.pid;
    }
    public static uint ForegroundPid() { uint pid; GetWindowThreadProcessId(GetForegroundWindow(),out pid); return pid; }
    public uint[] VisiblePids() {
        var list=new List<uint>();
        EnumDesktopWindows(desktop,delegate(IntPtr w,IntPtr unused) { uint pid; GetWindowThreadProcessId(w,out pid); if(IsWindowVisible(w)) list.Add(pid); return true; },IntPtr.Zero);
        return list.ToArray();
    }
    public void Dispose() { foreach(var f in logs) f.Dispose(); CloseDesktop(desktop); }
}
'@
$script:seen = @{}; $script:foreground = 0
function Sample {
    $fg = [S7uDesktop]::ForegroundPid()
    $p = Get-Process -Id $fg -ErrorAction SilentlyContinue
    if ($p -and $p.Path -and $p.Path.StartsWith($work)) { $script:foreground++; throw "A trial window took the foreground: $($p.Path)" }
    foreach ($id in $native.VisiblePids()) {
        $q = Get-Process -Id $id -ErrorAction SilentlyContinue
        if ($q -and $q.Path -and $q.Path.StartsWith($work)) { $script:seen[$q.Path] = $true }
    }
}
function Ours { Get-Process | Where-Object { $_.Path -and $_.Path.StartsWith($root) } }
function Wait-Until([scriptblock]$condition, [int]$ms, [string]$what) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($ms)
    while (-not (& $condition)) {
        Sample; Start-Sleep -Milliseconds 100
        if ([DateTime]::UtcNow -gt $deadline) { throw "Timed out: $what" }
    }
}
function State { Get-Content -Raw (Join-Path $root 'state.json') | ConvertFrom-Json }
$server = $null; $native = $null; $summary = @()
$oldLocal = $env:LOCALAPPDATA; $oldBackend = $env:SLINT_BACKEND
$oldKeys = $env:TURBO_PANCAKE_UPDATE_PUBKEYS; $oldVersion = $env:TURBO_PANCAKE_BUILD_VERSION
try {
    $tool = Join-Path $release 'turbo-pancake-update.exe'
    if (-not $SkipBuild) {
        Cargo @('build','--release','--locked','-p','turbo-pancake-launcher','-p','turbo-pancake-update-tool','-p','turbo-pancake-app-server') (Join-Path $work 'build-tools.log')
    }
    $key = Join-Path $work 'trial-update-key.pk8'
    $keyOut = & $tool keygen --id trial --out $key
    $line = ($keyOut | Where-Object { $_ -like 'TURBO_PANCAKE_UPDATE_PUBKEYS=*' }) -replace '^TURBO_PANCAKE_UPDATE_PUBKEYS=', ''
    if (-not $line) { throw 'keygen failed' }
    # The pinned key is per trial: the keyed builds always rebuild the client.
    $builds = @{}
    foreach ($v in @('0.1.0','0.1.1')) {
        $env:TURBO_PANCAKE_UPDATE_PUBKEYS = $line; $env:TURBO_PANCAKE_BUILD_VERSION = $v
        Cargo @('build','--release','--locked','-p','turbo-pancake-desktop','--target-dir',$keyedTarget) (Join-Path $work "build-$v.log")
        $dir = Join-Path $work "build-$v"; New-Item -ItemType Directory -Path $dir -Force | Out-Null
        Copy-Item (Join-Path $keyedTarget 'release/turbo-pancake-desktop.exe') (Join-Path $dir 'turbo-pancake-desktop.exe')
        $builds[$v] = $dir
    }
    $env:TURBO_PANCAKE_UPDATE_PUBKEYS = $oldKeys; $env:TURBO_PANCAKE_BUILD_VERSION = $oldVersion
    $exeSize = (Get-Item (Join-Path $builds['0.1.1'] 'turbo-pancake-desktop.exe')).Length
    # Install layout (what the installer of step 6 creates).
    $root = Join-Path $work 'install'
    New-Item -ItemType Directory -Path (Join-Path $root 'versions/0.1.0') -Force | Out-Null
    Copy-Item (Join-Path $release 'turbo-pancake.exe') (Join-Path $root 'turbo-pancake.exe')
    Copy-Item (Join-Path $builds['0.1.0'] 'turbo-pancake-desktop.exe') (Join-Path $root 'versions/0.1.0/')
    '{"current":"0.1.0"}' | Set-Content -Encoding ascii (Join-Path $root 'state.json')
    # Server with TLS and an update directory.
    $port = Free-Port; $upd = Join-Path $work 'updates'
    & (Join-Path $release 'turbo-pancake-app-server.exe') gen-cert --name 127.0.0.1 --out (Join-Path $work 'tls') *> (Join-Path $work 'cert.log')
    if ($LASTEXITCODE -ne 0) { throw 'certificate generation failed' }
    function Publish($version, $inputDir, $outDir) {
        & $tool sign --key $key --id trial --version $version --input $inputDir --out $outDir *> (Join-Path $work "sign-$version.log")
        if ($LASTEXITCODE -ne 0) { throw "signing $version failed" }
    }
    Publish '0.1.1' $builds['0.1.1'] $upd
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $release 'turbo-pancake-app-server.exe'
    $psi.Arguments = Quote-Args @('--bind',"127.0.0.1:$port",'--no-media-udp','--update-dir',$upd,'--tls-cert',(Join-Path $work 'tls/server.pem'),'--tls-key',(Join-Path $work 'tls/server.key'))
    $psi.UseShellExecute = $false; $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true
    $server = [Diagnostics.Process]::Start($psi)
    $serverOut = $server.StandardOutput.ReadToEndAsync(); $serverErr = $server.StandardError.ReadToEndAsync()
    Start-Sleep -Milliseconds 500
    $env:LOCALAPPDATA = Join-Path $work 'profiles'; New-Item -ItemType Directory -Path $env:LOCALAPPDATA -Force | Out-Null
    $env:SLINT_BACKEND = $Backend
    $native = New-Object S7uDesktop
    $common = @('--server',"wss://127.0.0.1:$port/v1/signal",'--server-ca',(Join-Path $work 'tls/ca.pem'),'--no-activate','--simulate-audio','--chat-log')
    # ---- A: update 0.1.0 -> 0.1.1 ----
    $logA = Join-Path $work 'a.log'
    $t0 = [DateTime]::UtcNow
    $native.Start((Join-Path $root 'turbo-pancake.exe'), (Quote-Args ($common + @('--trial-update','--smoke-exit-after-ms','15000'))), $root, $logA) | Out-Null
    Wait-Until { (Read-Text $logA) -match 'TURBO_PANCAKE_UPDATE status=ready version=0.1.1' } 30000 'A: staged'
    $stagedMs = [int]([DateTime]::UtcNow - $t0).TotalMilliseconds
    Wait-Until { (Read-Text $logA) -match 'TURBO_PANCAKE_LAUNCHER confirmed version=0.1.1' } 40000 'A: confirmed'
    $confirmedMs = [int]([DateTime]::UtcNow - $t0).TotalMilliseconds
    Wait-Until { -not (Ours) } 40000 'A: all processes exited'
    $text = Read-Text $logA; $s = State
    if ($s.current -ne '0.1.1' -or $s.previous -ne '0.1.0' -or $s.pending) { throw "A: wrong state $($s | ConvertTo-Json -Compress)" }
    foreach ($m in @('LAUNCHER plan=run version=0.1.0','status=available version=0.1.1','restart=true','LAUNCHER plan=trial version=0.1.1 fallback=0.1.0','confirmed=true version=0.1.1','status=up_to_date version=0.1.1')) {
        if (-not $text.Contains($m)) { throw "A: missing $m" }
    }
    if (([regex]::Matches($text, 'TURBO_PANCAKE_EXIT')).Count -ne 2) { throw 'A: both clients must exit cleanly' }
    if (-not $script:seen[(Join-Path $root 'versions/0.1.1/turbo-pancake-desktop.exe')]) { throw 'A: no native window of 0.1.1' }
    $summary += "A update 0.1.0 -> 0.1.1: PASS; download+verify ${stagedMs} ms after launch ($exeSize bytes); switch confirmed ${confirmedMs} ms after launch; state current=0.1.1 previous=0.1.0; both clients exit 0"
    # ---- B: broken 0.1.2 rolls back ----
    $broken = Join-Path $work 'build-0.1.2'; New-Item -ItemType Directory -Path $broken -Force | Out-Null
    # A real executable that exits at once with an error (the server rejects these args).
    Copy-Item (Join-Path $release 'turbo-pancake-app-server.exe') (Join-Path $broken 'turbo-pancake-desktop.exe')
    Publish '0.1.2' $broken $upd
    $logB = Join-Path $work 'b.log'
    $native.Start((Join-Path $root 'turbo-pancake.exe'), (Quote-Args ($common + @('--trial-update','--smoke-exit-after-ms','15000'))), $root, $logB) | Out-Null
    Wait-Until { (Read-Text $logB) -match 'LAUNCHER rollback version=0.1.2 to=0.1.1' } 40000 'B: rollback'
    Wait-Until { (Read-Text $logB) -match 'status=up_to_date version=0.1.1' } 30000 'B: 0.1.2 not offered again'
    Wait-Until { -not (Ours) } 40000 'B: all processes exited'
    $s = State
    if ($s.current -ne '0.1.1' -or $s.pending -or -not ($s.bad -contains '0.1.2')) { throw "B: wrong state $($s | ConvertTo-Json -Compress)" }
    if (([regex]::Matches((Read-Text $logB), 'TURBO_PANCAKE_EXIT')).Count -ne 2) { throw 'B: clients must exit cleanly' }
    $summary += 'B broken 0.1.2: PASS; trial exited before confirming, marked bad, 0.1.1 restarted and did not re-offer 0.1.2'
    # ---- C: nothing changes without a valid, newer, intact, user-accepted release ----
    $current = Join-Path $root 'versions/0.1.1/turbo-pancake-desktop.exe'
    $upd3 = Join-Path $work 'updates-0.1.3'
    Publish '0.1.3' $builds['0.1.1'] $upd3
    function Run-Client($label, [string[]]$extra) {
        $log = Join-Path $work "$label.log"
        $native.Start($current, (Quote-Args ($common + $extra + @('--smoke-exit-after-ms','8000'))), (Split-Path $current), $log) | Out-Null
        Wait-Until { (Read-Text $log) -match 'TURBO_PANCAKE_EXIT' } 30000 "$label exit"
        Wait-Until { -not (Ours) } 10000 "$label processes"
        Read-Text $log
    }
    function Use-Release($dir) {
        Remove-Item -Recurse -Force $upd
        Copy-Item -Recurse $dir $upd
    }
    $before = (Get-Content -Raw (Join-Path $root 'state.json'))
    function Unchanged($label) {
        if ((Get-Content -Raw (Join-Path $root 'state.json')) -ne $before) { throw "$label changed the install state" }
        if (Test-Path (Join-Path $root 'versions/0.1.3')) { throw "$label left a referenced version" }
    }
    Use-Release $upd3
    $c1 = Run-Client 'c1-available' @('--open-settings','--snapshot',(Join-Path $work 'c1-settings.ppm'),'--snapshot-after-ms','5000')
    if ($c1 -notmatch 'status=available version=0.1.3' -or $c1 -match 'status=downloading') { throw 'C1: available release must wait for the user' }
    Unchanged 'C1'
    Convert-Ppm (Join-Path $work 'c1-settings.ppm')
    # Truncated file on the server.
    $f = Join-Path $upd 'files/0.1.3/turbo-pancake-desktop.exe'
    $fs = [IO.File]::Open($f, 'Open', 'ReadWrite'); $fs.SetLength([long]($fs.Length / 2)); $fs.Dispose()
    $c2 = Run-Client 'c2-truncated' @('--trial-update')
    if ($c2 -notmatch 'status=failed' -or $c2 -match 'status=ready') { throw 'C2: truncated file accepted' }
    Unchanged 'C2'
    # Same size, one byte changed.
    Use-Release $upd3
    $f = Join-Path $upd 'files/0.1.3/turbo-pancake-desktop.exe'
    $bytes = [IO.File]::ReadAllBytes($f); $bytes[1000] = $bytes[1000] -bxor 1; [IO.File]::WriteAllBytes($f, $bytes)
    $c3 = Run-Client 'c3-tampered-file' @('--trial-update')
    if ($c3 -notmatch 'status=failed' -or $c3 -match 'status=ready') { throw 'C3: tampered file accepted' }
    Unchanged 'C3'
    # Manifest edited after signing (newer version claimed under the old signature).
    Use-Release $upd3
    $envelope = Get-Content -Raw (Join-Path $upd 'manifest.json') | ConvertFrom-Json
    $m = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($envelope.manifest)) -replace '"0\.1\.3"', '"0.1.4"'
    $envelope.manifest = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($m))
    $envelope | ConvertTo-Json | Set-Content -Encoding ascii (Join-Path $upd 'manifest.json')
    $c4 = Run-Client 'c4-tampered-manifest' @('--trial-update')
    if ($c4 -notmatch 'status=failed' -or $c4 -match 'status=(available|ready)') { throw 'C4: tampered manifest accepted' }
    Unchanged 'C4'
    # A correctly signed older release (downgrade/replay).
    $upd0 = Join-Path $work 'updates-0.1.0'; Publish '0.1.0' $builds['0.1.0'] $upd0; Use-Release $upd0
    $c5 = Run-Client 'c5-downgrade' @('--trial-update')
    if ($c5 -notmatch 'status=up_to_date version=0.1.1') { throw 'C5: downgrade offered' }
    Unchanged 'C5'
    $summary += 'C negatives: PASS; available-but-unconfirmed, truncated file, tampered file, tampered manifest, signed downgrade: install state unchanged, nothing referenced'
    $summary += "S7 update trial PASS; backend=$Backend; TLS; isolated invisible desktop; foreground_samples=$($script:foreground)"
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
    $env:LOCALAPPDATA = $oldLocal; $env:SLINT_BACKEND = $oldBackend
    $env:TURBO_PANCAKE_UPDATE_PUBKEYS = $oldKeys; $env:TURBO_PANCAKE_BUILD_VERSION = $oldVersion
}
