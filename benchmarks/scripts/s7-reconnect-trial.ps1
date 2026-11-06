# S7 step 4: connection banner and unsent input across a server outage.
# Real TLS server + PostgreSQL, three native release clients on an isolated invisible
# desktop (no focus, no input, simulated audio). Alice keeps a direct chat open with an
# unsent text in the message and room fields; Bob and Cara are in a direct call. The
# server is killed, stays down for a while and is started again on the same port/data.
# Each client saves an own-window snapshot of every banner state.
param([string]$Target = '', [string]$PgBin = $env:TURBO_PANCAKE_PG_BIN,
      [string]$Backend = 'winit-software', [int]$OutageMs = 8000, [string]$Out = '')
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $Target) { $Target = Join-Path $workspacePath 't/release' }
if (-not $PgBin) { throw 'Supply -PgBin (portable PostgreSQL binaries)' }
$trialId = [Guid]::NewGuid().ToString('N')
$profilePrefix = 's7r-' + $trialId.Substring(0,10)
$work = Join-Path $Target ('s7-reconnect-' + $trialId)
New-Item -ItemType Directory -Path $work -Force | Out-Null
$serverExe = Join-Path $Target 'turbo-pancake-app-server.exe'
$desktopExe = Join-Path $Target 'turbo-pancake-desktop.exe'
$oldTemp=$env:TEMP; $oldTmp=$env:TMP
$env:TEMP=Join-Path $work 'tmp'; $env:TMP=$env:TEMP
New-Item -ItemType Directory -Path $env:TEMP -Force | Out-Null
function Free-Port {
    $listener = New-Object Net.Sockets.TcpListener ([Net.IPAddress]::Loopback), 0
    $listener.Start(); $port = $listener.LocalEndpoint.Port; $listener.Stop(); $port
}
function Quote-Args($items) { ($items | ForEach-Object { '"' + ($_ -replace '"', '\"') + '"' }) -join ' ' }
function Read-Log($name) {
    $path = Join-Path $work "$name.log"
    $fs = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try { (New-Object IO.StreamReader($fs, [Text.Encoding]::UTF8)).ReadToEnd() } finally { $fs.Dispose() }
}
function Background($exe, $items, $label) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $exe; $psi.Arguments = Quote-Args $items
    $psi.UseShellExecute = $false; $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($psi)
    $process | Add-Member -NotePropertyName OutputTask -NotePropertyValue $process.StandardOutput.ReadToEndAsync()
    $process | Add-Member -NotePropertyName ErrorTask -NotePropertyValue $process.StandardError.ReadToEndAsync()
    $process
}
function Convert-Ppm($path) {
    Add-Type -AssemblyName System.Drawing
    $bytes = [IO.File]::ReadAllBytes($path)
    $header = [Text.Encoding]::ASCII.GetString($bytes, 0, [Math]::Min(40, $bytes.Length)) -split '\s+'
    $w = [int]$header[1]; $h = [int]$header[2]
    $offset = ("P6`n$w $h`n255`n").Length
    $bmp = New-Object Drawing.Bitmap $w, $h
    for ($y = 0; $y -lt $h; $y++) { for ($x = 0; $x -lt $w; $x++) {
        $i = $offset + 3 * ($y * $w + $x)
        $bmp.SetPixel($x, $y, [Drawing.Color]::FromArgb($bytes[$i], $bytes[$i+1], $bytes[$i+2])) } }
    $bmp.Save([IO.Path]::ChangeExtension($path, '.png')); $bmp.Dispose()
}
Add-Type @'
using System; using System.IO; using System.Text; using System.Collections.Generic; using System.Runtime.InteropServices;
public sealed class S7rDesktop : IDisposable {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Startup {
        public uint cb; public string reserved,desktop,title; public uint x,y,w,h,xc,yc,fill,flags;
        public ushort show,reservedSize; public IntPtr reservedPtr,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] struct Info { public IntPtr process,thread; public uint pid,tid; }
    public sealed class Child { internal InfoData data; internal FileStream log; public bool WindowSeen; public uint Exit; }
    internal struct InfoData { public IntPtr process,thread; public uint pid; }
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
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr h,uint exit);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    readonly string name = "tp-s7r-"+Guid.NewGuid().ToString("N");
    readonly IntPtr desktop; readonly List<Child> children = new List<Child>();
    public int ForegroundSamples;
    public S7rDesktop() { desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x1ff,IntPtr.Zero); if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception(); }
    public Child Start(string exe,string arguments,string dir,string log) {
        var child = new Child(); child.log=new FileStream(log,FileMode.Create,FileAccess.Write,FileShare.ReadWrite);
        var h=child.log.SafeFileHandle.DangerousGetHandle(); SetHandleInformation(h,1,1);
        var startup=new Startup(); startup.cb=(uint)Marshal.SizeOf(startup); startup.desktop=name; startup.flags=0x100; startup.output=h; startup.error=h;
        Info info; if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+arguments),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref startup,out info)) throw new System.ComponentModel.Win32Exception();
        child.data=new InfoData { process=info.process,thread=info.thread,pid=info.pid }; children.Add(child); return child;
    }
    public void Sample() {
        uint foreground; GetWindowThreadProcessId(GetForegroundWindow(),out foreground);
        foreach(var c in children) { if(foreground==c.data.pid) { ForegroundSamples++; throw new Exception("Test window took foreground"); } }
        EnumDesktopWindows(desktop,delegate(IntPtr w,IntPtr unused) { uint pid; GetWindowThreadProcessId(w,out pid); foreach(var c in children) if(pid==c.data.pid && IsWindowVisible(w)) c.WindowSeen=true; return true; },IntPtr.Zero);
    }
    public void Wait(Child c,int milliseconds) {
        var until=DateTime.UtcNow.AddMilliseconds(milliseconds);
        while(WaitForSingleObject(c.data.process,100)==258) { Sample(); if(DateTime.UtcNow>until) throw new Exception("Native trial timeout"); }
        Sample(); GetExitCodeProcess(c.data.process,out c.Exit); c.log.Dispose();
        if(c.Exit!=0 || !c.WindowSeen) throw new Exception("Native trial exit/window failure");
    }
    public void Dispose() {
        foreach(var c in children) { uint exit; GetExitCodeProcess(c.data.process,out exit); if(exit==259) { TerminateProcess(c.data.process,124); WaitForSingleObject(c.data.process,5000); }
            c.log.Dispose(); CloseHandle(c.data.thread); CloseHandle(c.data.process); }
        CloseDesktop(desktop);
    }
}
'@
function Wait-Until($native, [scriptblock]$condition, [int]$ms, [string]$what) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($ms)
    while (-not (& $condition)) {
        $native.Sample(); Start-Sleep -Milliseconds 100
        if ([DateTime]::UtcNow -gt $deadline) { throw "Timed out: $what" }
    }
}
$pgPort = Free-Port; $port = Free-Port
$pgDir = Join-Path $work 'pg'; $pgStarted = $false; $server = $null; $native = $null
$profiles = @(); $summary = @()
$oldLocal = $env:LOCALAPPDATA; $oldBackend = $env:SLINT_BACKEND; $oldPassword = $env:TURBO_PANCAKE_PASSWORD
$serverArgs = @('--bind',"127.0.0.1:$port",'--database-url',"postgres://postgres@127.0.0.1:$pgPort/s7r",'--anonymous-calls','false','--no-media-udp',
    '--tls-cert',(Join-Path $work 'tls/server.pem'),'--tls-key',(Join-Path $work 'tls/server.key'))
$serverRuns = 0
function Stop-Server {
    if ($script:server -and -not $script:server.HasExited) { $script:server.Kill(); $script:server.WaitForExit() }
    if ($script:server) {
        $script:server.OutputTask.Result | Set-Content -Encoding utf8 (Join-Path $work "server-$script:serverRuns.out")
        $script:server.ErrorTask.Result | Set-Content -Encoding utf8 (Join-Path $work "server-$script:serverRuns.err")
        $script:server = $null
    }
}
function Start-Server { $script:serverRuns++; $script:server = Background $serverExe $serverArgs 'server' }
try {
    & "$PgBin/initdb.exe" -D $pgDir -U postgres -A trust -E UTF8 --no-locale --no-sync --no-instructions *> (Join-Path $work 'initdb.log')
    if ($LASTEXITCODE -ne 0) { throw 'initdb failed' }
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $PgBin 'pg_ctl.exe'
    $psi.Arguments = Quote-Args @('start','-D',$pgDir,'-l',(Join-Path $work 'pg.log'),'-w','-o',"-h 127.0.0.1 -p $pgPort -c fsync=off")
    $psi.UseShellExecute=$false; $psi.CreateNoWindow=$true
    $pg = [Diagnostics.Process]::Start($psi)
    if (-not $pg.WaitForExit(30000) -or $pg.ExitCode -ne 0) { throw 'PostgreSQL start failed' }
    $pgStarted=$true
    & "$PgBin/createdb.exe" -h 127.0.0.1 -p $pgPort -U postgres s7r
    if ($LASTEXITCODE -ne 0) { throw 'createdb failed' }
    & $serverExe gen-cert --name 127.0.0.1 --out (Join-Path $work 'tls') *> (Join-Path $work 'cert.log')
    if ($LASTEXITCODE -ne 0) { throw 'certificate generation failed' }
    Start-Server
    $env:LOCALAPPDATA = Join-Path $work 'profiles'; New-Item -ItemType Directory -Path $env:LOCALAPPDATA -Force | Out-Null
    $env:SLINT_BACKEND=$Backend; $env:TURBO_PANCAKE_PASSWORD=[Guid]::NewGuid().ToString('N')
    $native = New-Object S7rDesktop
    $common = @('--server',"wss://127.0.0.1:$port/v1/signal",'--server-ca',(Join-Path $work 'tls/ca.pem'),'--no-activate','--simulate-audio','--media','tcp','--share-no-audio','--call-log','--chat-log')
    $names = @('alice_s7r','bob_s7r','cara_s7r')
    # ---- Accounts (registration run, stored sessions for the trial run) ----
    $reg = @{}
    foreach ($name in $names) {
        $profile = "$profilePrefix-$name"; $profiles += $profile
        $reg[$name] = $native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profile,'--register','--login',$name,'--smoke-exit-after-ms','6000'))),$workspacePath,(Join-Path $work "reg-$name.log"))
    }
    foreach ($name in $names) {
        $native.Wait($reg[$name], 20000)
        if ((Read-Log "reg-$name") -notmatch 'signed_in user=') { throw "Registration failed: $name" }
    }
    # ---- Trial run ----
    $exitMs = '60000'; $c = @{}
    $c['cara_s7r'] = $native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[2],'--auto-join-call','--snapshot-banner-dir',(Join-Path $work 'cara'),'--smoke-exit-after-ms',$exitMs))),$workspacePath,(Join-Path $work 'cara_s7r.log'))
    Start-Sleep -Milliseconds 1500
    $c['bob_s7r'] = $native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[1],'--chat-with','cara_s7r','--call','--snapshot-banner-dir',(Join-Path $work 'bob'),'--smoke-exit-after-ms',$exitMs))),$workspacePath,(Join-Path $work 'bob_s7r.log'))
    $c['alice_s7r'] = $native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[0],'--chat-with','bob_s7r','--send','S7 message before the outage','--trial-input','S7 unsent text kept across the outage','--snapshot-banner-dir',(Join-Path $work 'alice'),'--smoke-exit-after-ms',$exitMs))),$workspacePath,(Join-Path $work 'alice_s7r.log'))
    Wait-Until $native { (Read-Log 'alice_s7r') -match 'TURBO_PANCAKE_TRIAL_INPUT set' -and (Read-Log 'alice_s7r') -match 'messages=1 last_body="S7 message before the outage"' } 25000 'alice chat ready'
    foreach ($n in @('bob_s7r','cara_s7r')) {
        Wait-Until $native { ([regex]::Matches((Read-Log $n),' played=(\d+)') | ForEach-Object { [int]$_.Groups[1].Value } | Measure-Object -Maximum).Maximum -ge 100 } 25000 "$n call media"
    }
    $summary += 'Before outage: alice chat open with unsent text; bob/cara direct call with media flowing'
    # ---- Outage ----
    $outageStart = [DateTime]::UtcNow
    Stop-Server
    foreach ($n in $names) { Wait-Until $native { (Read-Log $n) -match 'BANNER kind=offline' } 15000 "$n offline banner" }
    $remaining = $OutageMs - ([DateTime]::UtcNow - $outageStart).TotalMilliseconds
    $deadline = [DateTime]::UtcNow.AddMilliseconds([Math]::Max(0, $remaining))
    while ([DateTime]::UtcNow -lt $deadline) { $native.Sample(); Start-Sleep -Milliseconds 100 }
    Start-Server
    $restartAt = [DateTime]::UtcNow
    foreach ($n in $names) { Wait-Until $native { (Read-Log $n) -match '(?s)BANNER kind=offline.*BANNER kind=restored' } 30000 "$n restored banner" }
    $recoverMs = [int]([DateTime]::UtcNow - $restartAt).TotalMilliseconds
    foreach ($n in $names) { $native.Wait($c[$n], 70000) }
    # ---- Checks ----
    foreach ($n in $names) {
        $text = Read-Log $n
        foreach ($marker in @('TURBO_PANCAKE_FIRST_FRAME','TURBO_PANCAKE_EXIT','BANNER_SNAPSHOT kind=offline success=true','BANNER_SNAPSHOT kind=restored success=true')) {
            if (-not $text.Contains($marker)) { throw "Missing marker: $n / $marker" }
        }
        if ($text -match 'BANNER_SNAPSHOT kind=\w+ success=false') { throw "Snapshot failure: $n" }
        $banners = [regex]::Matches($text,'TURBO_PANCAKE_BANNER kind=(\w+) messenger=(\w+) call=(\w+)') | ForEach-Object { "$($_.Groups[1].Value)/$($_.Groups[2].Value)/$($_.Groups[3].Value)" }
        $summary += "$n : native_window=PASS; exit=0; banners=" + ($banners -join ' > ')
    }
    $alice = Read-Log 'alice_s7r'
    $keptLines = [regex]::Matches($alice,'TURBO_PANCAKE_BANNER kind=(offline|reconnecting|restored)[^\r\n]*input_kept=(\w+)')
    if ($keptLines.Count -lt 2 -or ($keptLines | Where-Object { $_.Groups[2].Value -ne 'true' })) { throw 'Unsent input not kept across the outage' }
    $maxMessages = ([regex]::Matches($alice,'messages=(\d+)') | ForEach-Object { [int]$_.Groups[1].Value } | Measure-Object -Maximum).Maximum
    if ($maxMessages -ne 1) { throw "History changed across the outage (messages=$maxMessages)" }
    $summary += "alice : input_kept=true on $($keptLines.Count) banner states; history messages=1 (no duplicates)"
    foreach ($n in @('bob_s7r','cara_s7r')) {
        $text = Read-Log $n
        if ($text -notmatch 'BANNER kind=\w+ messenger=offline call=reconnecting') { throw "Call reconnecting not shown: $n" }
        # Joining a call is not a loss: no banner before the outage.
        $before = $text.Substring(0, [regex]::Match($text, 'TURBO_PANCAKE_BANNER kind=\w+ messenger=offline').Index)
        if ($before -match 'BANNER kind=(reconnecting|offline|restored)') { throw "False banner before the outage: $n" }
    }
    $summary += 'bob/cara : call link reconnecting shown during the outage (the restart ends live calls, ADR 0015)'
    foreach ($dir in @('alice','bob','cara')) { Get-ChildItem (Join-Path $work $dir) -Filter *.ppm | ForEach-Object { Convert-Ppm $_.FullName } }
    $summary += "S7 reconnect trial PASS; backend=$Backend; outage_ms=$OutageMs; restored_after_restart_ms=$recoverMs; TLS/WSS; simulated audio; invisible desktop; foreground_samples=$($native.ForegroundSamples)"
    $summary | Set-Content -Encoding utf8 (Join-Path $work 'summary.log')
    if ($Out) { $summary | Set-Content -Encoding utf8 $Out }
    $summary
    Write-Output "Local artifacts: $work"
} finally {
    if ($native) { $native.Dispose() }
    Stop-Server
    if ($pgStarted) {
        $psi.Arguments=Quote-Args @('stop','-D',$pgDir,'-m','fast','-w')
        [Diagnostics.Process]::Start($psi).WaitForExit(30000) | Out-Null
    }
    foreach ($profile in $profiles) { & cmdkey.exe "/delete:turbo-pancake/$profile/https://127.0.0.1:$port" *> $null }
    $env:LOCALAPPDATA=$oldLocal; $env:SLINT_BACKEND=$oldBackend; $env:TURBO_PANCAKE_PASSWORD=$oldPassword
    $env:TEMP=$oldTemp; $env:TMP=$oldTmp
}
