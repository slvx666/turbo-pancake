# S7 step 7 (ADR 0002 renderer decision): cost of watching a full-size share per renderer.
# Anonymous loopback server; A shares a generated picture (no screen capture) at a fixed
# quality, B watches the high layer in a -Window sized window. Both run on an isolated
# invisible desktop (no focus, no input, simulated audio). For B over the steady part of
# the run: shown fps (view_fps p50), decode p50, process CPU (% of one core), working set
# and private bytes at the end.
param([string]$Target = '', [string[]]$Backends = @('winit-software','winit-femtovg'),
      [string[]]$Qualities = @('qhd120','fhd60'), [string]$Window = '1600,900',
      [int]$Seconds = 20, [string]$Out = '')
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $Target) { $Target = Join-Path $workspacePath 't/release' }
$work = Join-Path $Target ('s7-renderer-video-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$serverExe = Join-Path $Target 'turbo-pancake-app-server.exe'
$desktopExe = Join-Path $Target 'turbo-pancake-desktop.exe'
function Quote-Args($items) { ($items | ForEach-Object { '"' + ($_ -replace '"', '\"') + '"' }) -join ' ' }
function Free-Port {
    $l = New-Object Net.Sockets.TcpListener ([Net.IPAddress]::Loopback), 0
    $l.Start(); $p = $l.LocalEndpoint.Port; $l.Stop(); $p
}
function Read-Log($path) {
    if (-not (Test-Path $path)) { return '' }
    $fs = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try { (New-Object IO.StreamReader($fs, [Text.Encoding]::UTF8)).ReadToEnd() } finally { $fs.Dispose() }
}
Add-Type @'
using System; using System.IO; using System.Text; using System.Collections.Generic; using System.Runtime.InteropServices;
public sealed class S7vDesktop : IDisposable {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Startup {
        public uint cb; public string reserved,desktop,title; public uint x,y,w,h,xc,yc,fill,flags;
        public ushort show,reservedSize; public IntPtr reservedPtr,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] struct Info { public IntPtr process,thread; public uint pid,tid; }
    [StructLayout(LayoutKind.Sequential)] public struct Counters {
        public uint cb, PageFaultCount; public UIntPtr PeakWorkingSetSize, WorkingSetSize, QuotaPeakPagedPoolUsage, QuotaPagedPoolUsage,
        QuotaPeakNonPagedPoolUsage, QuotaNonPagedPoolUsage, PagefileUsage, PeakPagefileUsage, PrivateUsage;
    }
    public sealed class Child { public IntPtr Process, Thread; public uint Pid; internal FileStream Log; }
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateDesktop(string name,IntPtr device,IntPtr mode,uint flags,uint access,IntPtr security);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window,out uint pid);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string app,StringBuilder command,IntPtr ps,IntPtr ts,bool inherit,uint flags,IntPtr env,string dir,ref Startup start,out Info info);
    [DllImport("kernel32.dll")] static extern bool SetHandleInformation(IntPtr h,uint mask,uint flags);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr h,uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr h,out uint exit);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr h,uint exit);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll")] static extern bool GetProcessTimes(IntPtr h,out long create,out long exit,out long kernel,out long user);
    [DllImport("psapi.dll")] static extern bool GetProcessMemoryInfo(IntPtr h,out Counters c,uint size);
    readonly string name = "tp-s7v-"+Guid.NewGuid().ToString("N");
    readonly IntPtr desktop; readonly List<Child> children = new List<Child>();
    public int ForegroundSamples;
    public S7vDesktop() { desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x1ff,IntPtr.Zero); if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception(); }
    public Child Start(string exe,string arguments,string dir,string log) {
        var c=new Child(); c.Log=new FileStream(log,FileMode.Create,FileAccess.Write,FileShare.ReadWrite);
        var h=c.Log.SafeFileHandle.DangerousGetHandle(); SetHandleInformation(h,1,1);
        var startup=new Startup(); startup.cb=(uint)Marshal.SizeOf(startup); startup.desktop=name; startup.flags=0x100; startup.output=h; startup.error=h;
        Info info; if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+arguments),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref startup,out info)) throw new System.ComponentModel.Win32Exception();
        c.Process=info.process; c.Thread=info.thread; c.Pid=info.pid; children.Add(c); return c;
    }
    public void Sample() { uint fg; GetWindowThreadProcessId(GetForegroundWindow(),out fg); foreach(var c in children) if(fg==c.Pid) { ForegroundSamples++; throw new Exception("Test window took foreground"); } }
    public Counters Memory(Child c) { Counters m; m.cb=(uint)Marshal.SizeOf(typeof(Counters)); GetProcessMemoryInfo(c.Process,out m,m.cb); return m; }
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
function P50($v) { $s = @($v | Sort-Object); if (-not $s.Count) { return $null }; $s[[int][Math]::Floor(($s.Count - 1) / 2)] }
$server = $null; $native = $null; $summary = @()
$oldBackend = $env:SLINT_BACKEND; $oldLocal = $env:LOCALAPPDATA
try {
    $port = Free-Port
    $server = Start-Process -FilePath $serverExe -ArgumentList @('--bind',"127.0.0.1:$port") -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $work 'server.out') -RedirectStandardError (Join-Path $work 'server.err')
    Start-Sleep -Milliseconds 800
    $env:LOCALAPPDATA = Join-Path $work 'profiles'; New-Item -ItemType Directory -Path $env:LOCALAPPDATA -Force | Out-Null
    $native = New-Object S7vDesktop
    $ms = $Seconds * 1000
    foreach ($q in $Qualities) { foreach ($backend in $Backends) {
        $env:SLINT_BACKEND = $backend
        $room = "s7v-$q-$backend"
        $common = @('--server',"ws://127.0.0.1:$port/v1/signal",'--room',$room,'--no-activate','--simulate-audio','--mute','--remote-volume','0','--share-no-audio','--call-log','--multi-instance')
        $la = Join-Path $work "a-$q-$backend.log"; $lb = Join-Path $work "b-$q-$backend.log"
        $b = $native.Start($desktopExe,(Quote-Args ($common + @('--profile','s7v-b','--name','Viewer-B','--watch','--watch-layer','high','--window-size',$Window,'--smoke-exit-after-ms',"$($ms + 4000)"))),$workspacePath,$lb)
        Start-Sleep -Milliseconds 1000
        $a = $native.Start($desktopExe,(Quote-Args ($common + @('--profile','s7v-a','--name','Sharer-A','--share','synthetic','--share-quality',$q,'--fixed-quality','--smoke-exit-after-ms',"$($ms + 2000)"))),$workspacePath,$la)
        # Steady part: from 6 s after the share starts for 10 s.
        $until = [DateTime]::UtcNow.AddSeconds(6); while ([DateTime]::UtcNow -lt $until) { $native.Sample(); Start-Sleep -Milliseconds 200 }
        $cpu0 = $native.CpuTime($b); $t0 = [DateTime]::UtcNow
        $until = $t0.AddSeconds(10); while ([DateTime]::UtcNow -lt $until) { $native.Sample(); Start-Sleep -Milliseconds 200 }
        $cpu = ($native.CpuTime($b) - $cpu0) / 10000.0 / ([DateTime]::UtcNow - $t0).TotalMilliseconds * 100
        $m = $native.Memory($b)
        $ea = $native.Wait($a, $ms + 20000); $eb = $native.Wait($b, $ms + 20000)
        if ($ea -ne 0 -or $eb -ne 0) { throw "$q/$backend exit codes a=$ea b=$eb" }
        $lines = [regex]::Matches((Read-Log $lb),'TURBO_PANCAKE_CALL t_ms=(\d+)[^\r\n]*watching=(\S+) view=(\S+) view_fps=([\d.]+)[^\r\n]*decode_ms_p50=([\d.]+)')
        $steady = @($lines | Where-Object { $_.Groups[2].Value -ne '-' } | Select-Object -Skip 5)
        if ($steady.Count -lt 5) { throw "$q/$backend : viewer never showed the share" }
        $fps = P50 ($steady | ForEach-Object { [double]$_.Groups[4].Value })
        $dec = P50 ($steady | ForEach-Object { [double]$_.Groups[5].Value })
        $view = $steady[-1].Groups[3].Value
        $renderer = [regex]::Match((Read-Log $lb),'TURBO_PANCAKE_FIRST_FRAME \d+ (\S+)').Groups[1].Value
        $summary += "$q $backend ($renderer): view=$view shown_fps_p50=$fps decode_ms_p50=$dec viewer_cpu=$([Math]::Round($cpu,1)) % of one core WS=$([Math]::Round($m.WorkingSetSize.ToUInt64()/1MB,1)) MiB private=$([Math]::Round($m.PrivateUsage.ToUInt64()/1MB,1)) MiB samples=$($steady.Count)"
    } }
    $summary += "S7 renderer video trial PASS; window=$Window; generated picture, fixed quality, high layer; anonymous loopback; invisible desktop; foreground_samples=$($native.ForegroundSamples)"
    if ($Out) { $summary | Set-Content -Encoding utf8 $Out }
    $summary
} finally {
    if ($native) { $native.Dispose() }
    if ($server -and -not $server.HasExited) { $server.Kill(); $server.WaitForExit() }
    $env:SLINT_BACKEND = $oldBackend; $env:LOCALAPPDATA = $oldLocal
}
