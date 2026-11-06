# S7 step 7: idle RAM and startup profiles of the signed-in client (spec §13).
# Real TLS server + PostgreSQL; two accounts with a short direct history. For each
# renderer the release client of the first account starts -Runs times with its stored
# session and the direct chat open, on an isolated invisible desktop (no focus, no
# input, simulated audio). Per run: spawn -> first painted frame, spawn -> chat
# history shown (interactive), then after -SettleMs of idle: working set, peak working
# set, private bytes, GPU dedicated/shared memory of the process (GPU performance
# counters, if the system exposes them) and CPU over -CpuMs. Run 1 of each renderer is
# the first start after the build (closest automated approximation of a cold start; a
# true cold start after a reboot or with a flushed file cache is a manual procedure).
param([string]$Target = '', [string]$PgBin = $env:TURBO_PANCAKE_PG_BIN,
      [string[]]$Backends = @('winit-software','winit-femtovg'), [int]$Runs = 5,
      [int]$SettleMs = 8000, [int]$CpuMs = 5000, [string]$Out = '', [string]$Json = '')
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $Target) { $Target = Join-Path $workspacePath 't/release' }
if (-not $PgBin) { throw 'Supply -PgBin (portable PostgreSQL binaries)' }
$trialId = [Guid]::NewGuid().ToString('N')
$profilePrefix = 's7p-' + $trialId.Substring(0,10)
$work = Join-Path $Target ('s7-profile-' + $trialId)
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
function Read-Log($path) {
    if (-not (Test-Path $path)) { return '' }
    $fs = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try { (New-Object IO.StreamReader($fs, [Text.Encoding]::UTF8)).ReadToEnd() } finally { $fs.Dispose() }
}
function Background($exe, $items) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $exe; $psi.Arguments = Quote-Args $items
    $psi.UseShellExecute = $false; $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($psi)
    $process | Add-Member -NotePropertyName OutputTask -NotePropertyValue $process.StandardOutput.ReadToEndAsync()
    $process | Add-Member -NotePropertyName ErrorTask -NotePropertyValue $process.StandardError.ReadToEndAsync()
    $process
}
Add-Type @'
using System; using System.IO; using System.Text; using System.Diagnostics; using System.Collections.Generic; using System.Runtime.InteropServices;
public sealed class S7pDesktop : IDisposable {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Startup {
        public uint cb; public string reserved,desktop,title; public uint x,y,w,h,xc,yc,fill,flags;
        public ushort show,reservedSize; public IntPtr reservedPtr,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] struct Info { public IntPtr process,thread; public uint pid,tid; }
    [StructLayout(LayoutKind.Sequential)] public struct Counters {
        public uint cb, PageFaultCount; public UIntPtr PeakWorkingSetSize, WorkingSetSize, QuotaPeakPagedPoolUsage, QuotaPagedPoolUsage,
        QuotaPeakNonPagedPoolUsage, QuotaNonPagedPoolUsage, PagefileUsage, PeakPagefileUsage, PrivateUsage;
    }
    public sealed class Child { public IntPtr Process, Thread; public uint Pid; public long StartTicks; internal FileStream Log; public bool WindowSeen; }
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
    [DllImport("kernel32.dll")] static extern bool GetProcessTimes(IntPtr h,out long create,out long exit,out long kernel,out long user);
    [DllImport("psapi.dll")] static extern bool GetProcessMemoryInfo(IntPtr h,out Counters c,uint size);
    readonly string name = "tp-s7p-"+Guid.NewGuid().ToString("N");
    readonly IntPtr desktop; readonly List<Child> children = new List<Child>();
    public int ForegroundSamples;
    public S7pDesktop() { desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x1ff,IntPtr.Zero); if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception(); }
    public Child Start(string exe,string arguments,string dir,string log) {
        var c=new Child(); c.Log=new FileStream(log,FileMode.Create,FileAccess.Write,FileShare.ReadWrite);
        var h=c.Log.SafeFileHandle.DangerousGetHandle(); SetHandleInformation(h,1,1);
        var startup=new Startup(); startup.cb=(uint)Marshal.SizeOf(startup); startup.desktop=name; startup.flags=0x100; startup.output=h; startup.error=h;
        Info info; c.StartTicks=Stopwatch.GetTimestamp();
        if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+arguments),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref startup,out info)) throw new System.ComponentModel.Win32Exception();
        c.Process=info.process; c.Thread=info.thread; c.Pid=info.pid; children.Add(c); return c;
    }
    public static double MsSince(long ticks) { return (Stopwatch.GetTimestamp()-ticks)*1000.0/Stopwatch.Frequency; }
    public void Sample() {
        uint fg; GetWindowThreadProcessId(GetForegroundWindow(),out fg);
        foreach(var c in children) if(fg==c.Pid) { ForegroundSamples++; throw new Exception("Test window took foreground"); }
        EnumDesktopWindows(desktop,delegate(IntPtr w,IntPtr unused) { uint pid; GetWindowThreadProcessId(w,out pid); foreach(var c in children) if(pid==c.Pid && IsWindowVisible(w)) c.WindowSeen=true; return true; },IntPtr.Zero);
    }
    public Counters Memory(Child c) { Counters m; m.cb=(uint)Marshal.SizeOf(typeof(Counters)); if(!GetProcessMemoryInfo(c.Process,out m,m.cb)) throw new System.ComponentModel.Win32Exception(); return m; }
    // 100 ns units of kernel + user time.
    public long CpuTime(Child c) { long a,b,k,u; GetProcessTimes(c.Process,out a,out b,out k,out u); return k+u; }
    public long Wait(Child c,int ms) {
        var until=DateTime.UtcNow.AddMilliseconds(ms);
        while(WaitForSingleObject(c.Process,100)==258) { Sample(); if(DateTime.UtcNow>until) { TerminateProcess(c.Process,124); return -1; } }
        uint e; GetExitCodeProcess(c.Process,out e); c.Log.Flush(); return e;
    }
    public void Dispose() {
        foreach(var c in children) { uint e; GetExitCodeProcess(c.Process,out e); if(e==259) { TerminateProcess(c.Process,124); WaitForSingleObject(c.Process,5000); }
            c.Log.Dispose(); CloseHandle(c.Thread); CloseHandle(c.Process); }
        CloseDesktop(desktop);
    }
}
'@
function Wait-Until($native, [scriptblock]$condition, [int]$ms, [string]$what) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($ms)
    while (-not (& $condition)) {
        $native.Sample(); Start-Sleep -Milliseconds 20
        if ([DateTime]::UtcNow -gt $deadline) { throw "Timed out: $what" }
    }
}
function Gpu-Memory([uint32]$processId) {
    # Not localized (unlike the PDH counter path); absent on systems without WDDM counters.
    try {
        $rows = @(Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUProcessMemory -ErrorAction Stop |
            Where-Object { $_.Name -like "pid_$($processId)_*" })
    } catch { return $null }
    if (-not $rows.Count) { return @{ dedicated = 0; shared = 0; instances = 0 } }
    @{ dedicated = ($rows | Measure-Object DedicatedUsage -Sum).Sum; shared = ($rows | Measure-Object SharedUsage -Sum).Sum; instances = $rows.Count }
}
function Stats($values) {
    $s = @($values | Sort-Object)
    if (-not $s.Count) { return @{} }
    @{ min = $s[0]; p50 = $s[[int][Math]::Floor(($s.Count - 1) / 2)]; max = $s[-1] }
}
function MiB($bytes) { [Math]::Round($bytes / 1MB, 1) }
$pgPort = Free-Port; $port = Free-Port
$pgDir = Join-Path $work 'pg'; $pgStarted = $false; $server = $null; $native = $null
$profiles = @(); $summary = @(); $records = @()
$oldLocal = $env:LOCALAPPDATA; $oldBackend = $env:SLINT_BACKEND; $oldPassword = $env:TURBO_PANCAKE_PASSWORD
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
    & "$PgBin/createdb.exe" -h 127.0.0.1 -p $pgPort -U postgres s7p
    if ($LASTEXITCODE -ne 0) { throw 'createdb failed' }
    & $serverExe gen-cert --name 127.0.0.1 --out (Join-Path $work 'tls') *> (Join-Path $work 'cert.log')
    if ($LASTEXITCODE -ne 0) { throw 'certificate generation failed' }
    $server = Background $serverExe @('--bind',"127.0.0.1:$port",'--database-url',"postgres://postgres@127.0.0.1:$pgPort/s7p",'--anonymous-calls','false','--no-media-udp',
        '--tls-cert',(Join-Path $work 'tls/server.pem'),'--tls-key',(Join-Path $work 'tls/server.key'))
    Start-Sleep -Milliseconds 800
    $env:LOCALAPPDATA = Join-Path $work 'profiles'; New-Item -ItemType Directory -Path $env:LOCALAPPDATA -Force | Out-Null
    $env:TURBO_PANCAKE_PASSWORD=[Guid]::NewGuid().ToString('N')
    $env:SLINT_BACKEND = 'winit-software'
    $native = New-Object S7pDesktop
    $common = @('--server',"wss://127.0.0.1:$port/v1/signal",'--server-ca',(Join-Path $work 'tls/ca.pem'),'--no-activate','--simulate-audio','--chat-log')
    $names = @('alice_s7p','bob_s7p')
    # ---- Accounts and a short history (two messages) ----
    foreach ($name in $names) {
        $p = "$profilePrefix-$name"; $profiles += $p
        $log = Join-Path $work "reg-$name.log"
        $c = $native.Start($desktopExe,(Quote-Args ($common + @('--profile',$p,'--register','--login',$name,'--smoke-exit-after-ms','5000'))),$workspacePath,$log)
        if ($native.Wait($c, 20000) -ne 0 -or (Read-Log $log) -notmatch 'signed_in user=') { throw "Registration failed: $name" }
    }
    foreach ($pair in @(@(0,'bob_s7p','S7 profile: hello from alice'), @(1,'alice_s7p','S7 profile: hello from bob'))) {
        $log = Join-Path $work "seed-$($pair[0]).log"
        $c = $native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[$pair[0]],'--chat-with',$pair[1],'--send',$pair[2],'--smoke-exit-after-ms','5000'))),$workspacePath,$log)
        if ($native.Wait($c, 20000) -ne 0 -or (Read-Log $log) -notmatch [regex]::Escape("last_body=""$($pair[2])""")) { throw "Seed message failed: $($pair[2])" }
    }
    # ---- Measured runs ----
    $exitAfter = 6000 + $SettleMs + $CpuMs + 8000
    foreach ($backend in $Backends) {
        $env:SLINT_BACKEND = $backend
        for ($i = 1; $i -le $Runs; $i++) {
            $log = Join-Path $work "run-$backend-$i.log"
            $c = $native.Start($desktopExe,(Quote-Args ($common + @('--profile',$profiles[0],'--chat-with','bob_s7p','--smoke-exit-after-ms',"$exitAfter"))),$workspacePath,$log)
            Wait-Until $native { (Read-Log $log) -match 'TURBO_PANCAKE_FIRST_FRAME' } 20000 "first frame $backend $i"
            $frameMs = [S7pDesktop]::MsSince($c.StartTicks)
            Wait-Until $native { (Read-Log $log) -match 'messages=2 last_body="S7 profile: hello from bob"' } 20000 "chat shown $backend $i"
            $chatMs = [S7pDesktop]::MsSince($c.StartTicks)
            $text = Read-Log $log
            $selfMs = [int]([regex]::Match($text,'TURBO_PANCAKE_FIRST_FRAME (\d+)').Groups[1].Value)
            $renderer = [regex]::Match($text,'TURBO_PANCAKE_FIRST_FRAME \d+ (\S+)').Groups[1].Value
            $until = [DateTime]::UtcNow.AddMilliseconds($SettleMs)
            while ([DateTime]::UtcNow -lt $until) { $native.Sample(); Start-Sleep -Milliseconds 200 }
            $cpu0 = $native.CpuTime($c); $t0 = [Diagnostics.Stopwatch]::GetTimestamp()
            $until = [DateTime]::UtcNow.AddMilliseconds($CpuMs)
            while ([DateTime]::UtcNow -lt $until) { $native.Sample(); Start-Sleep -Milliseconds 200 }
            $cpuPct = ($native.CpuTime($c) - $cpu0) / 10000.0 / [S7pDesktop]::MsSince($t0) * 100
            $m = $native.Memory($c)
            $gpu = Gpu-Memory $c.Pid
            $code = $native.Wait($c, $exitAfter + 15000)
            if ($code -ne 0 -or -not $c.WindowSeen -or (Read-Log $log) -notmatch 'TURBO_PANCAKE_EXIT') { throw "Run $backend $i failed (exit $code, window $($c.WindowSeen))" }
            $rec = [ordered]@{
                backend = $backend; renderer = $renderer; run = $i
                first_frame_ms = [Math]::Round($frameMs); first_frame_self_ms = $selfMs; chat_shown_ms = [Math]::Round($chatMs)
                working_set = [long]$m.WorkingSetSize.ToUInt64(); peak_working_set = [long]$m.PeakWorkingSetSize.ToUInt64(); private_bytes = [long]$m.PrivateUsage.ToUInt64()
                gpu_dedicated = if ($gpu) { [long]$gpu.dedicated } else { $null }; gpu_shared = if ($gpu) { [long]$gpu.shared } else { $null }
                cpu_percent_one_core = [Math]::Round($cpuPct, 2)
            }
            $records += $rec
            $summary += "$backend run ${i}: renderer=$renderer first_frame=$($rec.first_frame_ms) ms chat_shown=$($rec.chat_shown_ms) ms WS=$(MiB $rec.working_set) MiB peak=$(MiB $rec.peak_working_set) MiB private=$(MiB $rec.private_bytes) MiB GPU dedicated=$(if ($gpu) { MiB $gpu.dedicated } else { 'n/a' }) MiB shared=$(if ($gpu) { MiB $gpu.shared } else { 'n/a' }) MiB CPU=$($rec.cpu_percent_one_core) %"
        }
    }
    $agg = [ordered]@{}
    foreach ($backend in $Backends) {
        $r = @($records | Where-Object { $_.backend -eq $backend })
        $warm = @($r | Where-Object { $_.run -gt 1 })
        $agg[$backend] = [ordered]@{
            first_run_first_frame_ms = $r[0].first_frame_ms; first_run_chat_shown_ms = $r[0].chat_shown_ms
            warm_first_frame_ms = Stats ($warm | ForEach-Object { $_.first_frame_ms }); warm_chat_shown_ms = Stats ($warm | ForEach-Object { $_.chat_shown_ms })
            working_set_mib = Stats ($r | ForEach-Object { MiB $_.working_set }); private_mib = Stats ($r | ForEach-Object { MiB $_.private_bytes })
            peak_working_set_mib = Stats ($r | ForEach-Object { MiB $_.peak_working_set })
            gpu_dedicated_mib = Stats ($r | Where-Object { $_.gpu_dedicated -ne $null } | ForEach-Object { MiB $_.gpu_dedicated })
            gpu_shared_mib = Stats ($r | Where-Object { $_.gpu_shared -ne $null } | ForEach-Object { MiB $_.gpu_shared })
            cpu_percent = Stats ($r | ForEach-Object { $_.cpu_percent_one_core })
        }
        $a = $agg[$backend]
        $summary += "$backend p50: WS $($a.working_set_mib.p50) MiB, private $($a.private_mib.p50) MiB, peak WS $($a.peak_working_set_mib.p50) MiB, GPU dedicated $($a.gpu_dedicated_mib.p50) / shared $($a.gpu_shared_mib.p50) MiB, idle CPU $($a.cpu_percent.p50) %; first run: frame $($a.first_run_first_frame_ms) ms, chat $($a.first_run_chat_shown_ms) ms; warm p50: frame $($a.warm_first_frame_ms.p50) ms, chat $($a.warm_chat_shown_ms.p50) ms"
    }
    $os = Get-CimInstance Win32_OperatingSystem; $cpuInfo = Get-CimInstance Win32_Processor | Select-Object -First 1
    $gpus = @(Get-CimInstance Win32_VideoController | ForEach-Object { "$($_.Name) (driver $($_.DriverVersion))" })
    $commit = (& git -C $workspacePath rev-parse --short HEAD)
    $summary += "S7 profile trial PASS; runs=$Runs per renderer; settle_ms=$SettleMs; cpu_ms=$CpuMs; TLS/WSS + PostgreSQL; simulated audio; invisible desktop; foreground_samples=$($native.ForegroundSamples)"
    $summary += "Host: $($os.Caption) $($os.Version); $($cpuInfo.Name); RAM $([Math]::Round($os.TotalVisibleMemorySize/1MB,1)) GiB; GPU: $($gpus -join '; '); client exe $((Get-Item $desktopExe).Length) bytes; commit $commit"
    $summary | Set-Content -Encoding utf8 (Join-Path $work 'summary.log')
    if ($Out) { $summary | Set-Content -Encoding utf8 $Out }
    if ($Json) {
        # Public report: OS build only; the full host line stays in the local summary.
        [ordered]@{ scenario = 's7-profile'; commit = $commit; os = "Windows $($os.Version)"
            client_exe_bytes = (Get-Item $desktopExe).Length; params = @{ runs = $Runs; settle_ms = $SettleMs; cpu_ms = $CpuMs; history_messages = 2 }
            summary = $agg; samples = $records } | ConvertTo-Json -Depth 6 | Set-Content -Encoding utf8 $Json
    }
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
    foreach ($p in $profiles) { & cmdkey.exe "/delete:turbo-pancake/$p/https://127.0.0.1:$port" *> $null }
    $env:LOCALAPPDATA=$oldLocal; $env:SLINT_BACKEND=$oldBackend; $env:TURBO_PANCAKE_PASSWORD=$oldPassword
    $env:TEMP=$oldTemp; $env:TMP=$oldTmp
}
