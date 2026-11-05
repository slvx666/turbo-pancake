# S6 native acceptance: real TLS server/PostgreSQL and three native desktop clients.
# All windows stay on a separate invisible desktop. No focus/input/real audio/capture.
param([string]$Target = '', [string]$PgBin = $env:TURBO_PANCAKE_PG_BIN,
      [string]$Backend = 'winit-software', [string]$Out = '')
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $Target) { $Target = Join-Path $workspacePath 't/release' }
if (-not $PgBin) { throw 'Supply -PgBin (portable PostgreSQL binaries)' }
$trialId = [Guid]::NewGuid().ToString('N')
$profilePrefix = 's6-' + $trialId.Substring(0,12)
$work = Join-Path $Target ('s6-trial-' + $trialId)
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
function Background($exe, $items, $label) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $exe; $psi.Arguments = Quote-Args $items
    $psi.UseShellExecute = $false; $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($psi)
    $process | Add-Member -NotePropertyName OutputTask -NotePropertyValue $process.StandardOutput.ReadToEndAsync()
    $process | Add-Member -NotePropertyName ErrorTask -NotePropertyValue $process.StandardError.ReadToEndAsync()
    $process | Add-Member -NotePropertyName Label -NotePropertyValue $label
    $process
}
Add-Type @'
using System; using System.IO; using System.Text; using System.Collections.Generic; using System.Runtime.InteropServices;
public sealed class S6Desktop : IDisposable {
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
    readonly string name = "tp-s6-"+Guid.NewGuid().ToString("N");
    readonly IntPtr desktop; readonly List<Child> children = new List<Child>();
    public S6Desktop() { desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x1ff,IntPtr.Zero); if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception(); }
    public Child Start(string exe,string arguments,string dir,string log) {
        var child = new Child(); child.log=new FileStream(log,FileMode.Create,FileAccess.Write,FileShare.ReadWrite);
        var h=child.log.SafeFileHandle.DangerousGetHandle(); SetHandleInformation(h,1,1);
        var startup=new Startup(); startup.cb=(uint)Marshal.SizeOf(startup); startup.desktop=name; startup.flags=0x100; startup.output=h; startup.error=h;
        Info info; if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+arguments),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref startup,out info)) throw new System.ComponentModel.Win32Exception();
        child.data=new InfoData { process=info.process,thread=info.thread,pid=info.pid }; children.Add(child); return child;
    }
    public void Sample() {
        uint foreground; GetWindowThreadProcessId(GetForegroundWindow(),out foreground);
        foreach(var c in children) { if(foreground==c.data.pid) throw new Exception("Test window took foreground"); }
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
$pgPort = Free-Port; $port = Free-Port
$pgDir = Join-Path $work 'pg'; $pgStarted = $false; $server = $null; $native = $null
$profiles = @(); $summary = @()
$oldLocal = $env:LOCALAPPDATA; $oldBackend = $env:SLINT_BACKEND; $oldPassword = $env:TURBO_PANCAKE_PASSWORD
try {
    & "$PgBin/initdb.exe" -D $pgDir -U postgres -A trust -E UTF8 --no-locale --no-sync --no-instructions *> (Join-Path $work 'initdb.log')
    if ($LASTEXITCODE -ne 0) { throw 'initdb failed' }
    # pg_ctl must not inherit redirected pipe handles (see TESTING).
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = Join-Path $PgBin 'pg_ctl.exe'
    $psi.Arguments = Quote-Args @('start','-D',$pgDir,'-l',(Join-Path $work 'pg.log'),'-w','-o',"-h 127.0.0.1 -p $pgPort -c fsync=off")
    $psi.UseShellExecute=$false; $psi.CreateNoWindow=$true
    $pg = [Diagnostics.Process]::Start($psi)
    if (-not $pg.WaitForExit(30000) -or $pg.ExitCode -ne 0) { throw 'PostgreSQL start failed' }
    $pgStarted=$true
    & "$PgBin/createdb.exe" -h 127.0.0.1 -p $pgPort -U postgres s6
    if ($LASTEXITCODE -ne 0) { throw 'createdb failed' }
    & $serverExe gen-cert --name 127.0.0.1 --out (Join-Path $work 'tls') *> (Join-Path $work 'cert.log')
    if ($LASTEXITCODE -ne 0) { throw 'certificate generation failed' }
    $server = Background $serverExe @('--bind',"127.0.0.1:$port",'--database-url',"postgres://postgres@127.0.0.1:$pgPort/s6",'--anonymous-calls','false','--no-media-udp',
        '--tls-cert',(Join-Path $work 'tls/server.pem'),'--tls-key',(Join-Path $work 'tls/server.key')) 'server'
    $env:LOCALAPPDATA = Join-Path $work 'profiles'; New-Item -ItemType Directory -Path $env:LOCALAPPDATA -Force | Out-Null
    $env:SLINT_BACKEND=$Backend; $env:TURBO_PANCAKE_PASSWORD=[Guid]::NewGuid().ToString('N')
    $native = New-Object S6Desktop
    $common = @('--server',"wss://127.0.0.1:$port/v1/signal",'--server-ca',(Join-Path $work 'tls/ca.pem'), '--no-activate','--simulate-audio','--media','tcp','--share-no-audio','--call-log','--chat-log')
    $clients=@{}
    foreach ($name in @('bob_s6','cara_s6')) {
        $profile="$profilePrefix-$name"; $profiles += $profile
        $clients[$name]=$native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profile,'--register','--login',$name,'--auto-join-call','--leave-call-after-ms','9000','--trial-chat-switch','--smoke-exit-after-ms','30000','--snapshot',(Join-Path $work "$name.ppm"),'--snapshot-after-ms','18000'))),$workspacePath,(Join-Path $work "$name.log"))
    }
    $deadline=[DateTime]::UtcNow.AddSeconds(20)
    do {
        $native.Sample(); Start-Sleep -Milliseconds 100
        $ready = @('bob_s6','cara_s6') | Where-Object { (Get-Content (Join-Path $work "$_.log") -Raw) -match 'signed_in user=' }
        if ([DateTime]::UtcNow -gt $deadline) { throw 'Native participant sign-in timed out' }
    } while ($ready.Count -ne 2)
    $profile="$profilePrefix-alice_s6"; $profiles += $profile
    $clients['alice_s6']=$native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profile,'--register','--login','alice_s6','--group-title','S6 native group','--group-members','bob_s6,cara_s6','--call','--send','S6 message one','--send','S6 message two','--leave-call-after-ms','14000','--trial-chat-switch','--smoke-exit-after-ms','22000','--snapshot',(Join-Path $work 'alice_s6.ppm'),'--snapshot-after-ms','18000'))),$workspacePath,(Join-Path $work 'alice_s6.log'))
    foreach ($name in @('alice_s6','bob_s6','cara_s6')) {
        $native.Wait($clients[$name],35000)
        $text=Get-Content (Join-Path $work "$name.log") -Raw
        $others=([regex]::Matches($text,'others=(\d+)') | ForEach-Object { [int]$_.Groups[1].Value } | Measure-Object -Maximum).Maximum
        $played=([regex]::Matches($text,' played=(\d+)') | ForEach-Object { [int]$_.Groups[1].Value } | Measure-Object -Maximum).Maximum
        if ($others -lt 2 -or $played -lt 200) { throw "Incomplete three-member media flow: $name" }
        foreach ($marker in @('TURBO_PANCAKE_FIRST_FRAME','TURBO_PANCAKE_EXIT','TURBO_PANCAKE_S6_SWITCH draft_preserved=true','TURBO_PANCAKE_SNAPSHOT success=true')) {
            if (-not $text.Contains($marker)) { throw "Missing native marker: $name / $marker" }
        }
        if ($text -match 'decode_errors=[1-9]' -or $text -match 'draft_preserved=false') { throw "Native media/draft failure: $name" }
        if ($text -notmatch '(?s)joined=true.*joined=false others=0 media=none' -or $text -notmatch 'messages=2') { throw "Leave/history not observed: $name" }
        $summary += "$name : native_window=PASS; exit=0; peers=$others; played=$played; draft_switch=PASS; snapshot=PASS; history=PASS"
    }
    # Direct invitation -> accept -> leave, through the same native callbacks.
    $directB=$native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[0],'--auto-join-call','--leave-call-after-ms','8000','--trial-chat-switch','--smoke-exit-after-ms','18000','--snapshot',(Join-Path $work 'direct-bob.ppm'),'--snapshot-after-ms','13000'))),$workspacePath,(Join-Path $work 'direct-bob.log'))
    Start-Sleep -Milliseconds 1500
    $directA=$native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[2],'--chat-with','bob_s6','--call','--send','direct S6 history','--leave-call-after-ms','10000','--trial-chat-switch','--smoke-exit-after-ms','15000','--snapshot',(Join-Path $work 'direct-alice.ppm'),'--snapshot-after-ms','13000'))),$workspacePath,(Join-Path $work 'direct-alice.log'))
    $native.Wait($directA,25000); $native.Wait($directB,10000)
    foreach ($label in @('direct-alice','direct-bob')) {
        $text=Get-Content (Join-Path $work "$label.log") -Raw
        $played=([regex]::Matches($text,' played=(\d+)') | ForEach-Object { [int]$_.Groups[1].Value } | Measure-Object -Maximum).Maximum
        if ($played -lt 200 -or $text -notmatch '(?s)joined=true.*joined=false others=0 media=none' -or $text -notmatch 'messages=1' -or $text -notmatch 'draft_preserved=true' -or $text -match 'draft_preserved=false|decode_errors=[1-9]' -or $text -notmatch 'SNAPSHOT success=true') { throw "Direct native call/history failed: $label" }
        $summary += "$label : native_window=PASS; exit=0; played=$played; accept_leave=PASS; history=PASS; draft_switch=PASS"
    }
    # A new invitation is explicitly declined by the native recipient.
    $declineB=$native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[0],'--auto-decline-call','--smoke-exit-after-ms','12000'))),$workspacePath,(Join-Path $work 'decline-bob.log'))
    Start-Sleep -Milliseconds 1000
    $declineA=$native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[2],'--chat-with','bob_s6','--call','--smoke-exit-after-ms','9000'))),$workspacePath,(Join-Path $work 'decline-alice.log'))
    $native.Wait($declineA,15000); $native.Wait($declineB,7000)
    if ((Get-Content (Join-Path $work 'decline-alice.log') -Raw) -notmatch 'reason=declined') { throw 'Native direct-call decline not observed' }
    $summary += 'Direct decline: native_window=PASS; exit=0; ended_reason=declined'
    $summary += "S6 native trial PASS; backend=$Backend; TLS/WSS; simulated audio; isolated invisible desktop; no foreground/input/capture"
    $summary | Set-Content -Encoding utf8 (Join-Path $work 'summary.log')
    if ($Out) { $summary | Set-Content -Encoding utf8 $Out }
    $summary
    Write-Output "Local artifacts: $work"
} finally {
    if ($native) { $native.Dispose() }
    if ($server) {
        if (-not $server.HasExited) { $server.Kill(); $server.WaitForExit() }
        $server.OutputTask.Result | Set-Content -Encoding utf8 (Join-Path $work 'server.out')
        $server.ErrorTask.Result | Set-Content -Encoding utf8 (Join-Path $work 'server.err')
    }
    if ($pgStarted) {
        $psi.Arguments=Quote-Args @('stop','-D',$pgDir,'-m','fast','-w')
        [Diagnostics.Process]::Start($psi).WaitForExit(30000) | Out-Null
    }
    foreach ($profile in $profiles) { & cmdkey.exe "/delete:turbo-pancake/$profile/https://127.0.0.1:$port" *> $null }
    $env:LOCALAPPDATA=$oldLocal; $env:SLINT_BACKEND=$oldBackend; $env:TURBO_PANCAKE_PASSWORD=$oldPassword
    $env:TEMP=$oldTemp; $env:TMP=$oldTmp
}
