# S7 step 2: one client per profile. Release clients on one separate invisible desktop
# (no focus, no input, simulated audio):
#   A (profile P, guarded) closes its window to the tray;
#   B (profile P, guarded) starts later: must hand over to A and exit without a window,
#     and A's window must come back;
#   C (profile P, scripted smoke run without --single-instance) must start as before;
#   D (profile Q, guarded) must start: other profiles are not affected.
param([string]$Target = '', [string]$Backend = 'winit-software', [string]$Out = '')
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $Target) { $Target = Join-Path $workspacePath 't/release' }
$exe = Join-Path $Target 'turbo-pancake-desktop.exe'
$work = Join-Path $Target ('s7-instance-trial-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$oldLocal = $env:LOCALAPPDATA; $oldBackend = $env:SLINT_BACKEND
$env:LOCALAPPDATA = $work; $env:SLINT_BACKEND = $Backend
Add-Type @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Text;
using System.Threading;
using System.Runtime.InteropServices;
public class InstanceTrial : IDisposable {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Startup {
        public uint cb; public string reserved, desktop, title;
        public uint x,y,width,height,xChars,yChars,fill,flags;
        public ushort show,reservedSize; public IntPtr reservedPtr,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] struct ProcessInfo { public IntPtr process,thread; public uint pid,tid; }
    delegate bool EnumWindow(IntPtr window, IntPtr param);
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateDesktop(string name,IntPtr device,IntPtr mode,uint flags,uint access,IntPtr security);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("user32.dll")] static extern bool EnumDesktopWindows(IntPtr desktop,EnumWindow callback,IntPtr param);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window,out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll",CharSet=CharSet.Unicode)] static extern int GetClassName(IntPtr w,StringBuilder name,int max);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string app,StringBuilder command,IntPtr ps,IntPtr ts,bool inherit,uint flags,IntPtr env,string dir,ref Startup startup,out ProcessInfo process);
    [DllImport("kernel32.dll")] static extern bool SetHandleInformation(IntPtr handle,uint mask,uint flags);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr handle,out uint code);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr handle,uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    class Child { public ProcessInfo p; public FileStream log; public StringBuilder trace=new StringBuilder(); public uint exit=259; }
    readonly string name; readonly IntPtr desktop; readonly List<Child> children=new List<Child>();
    public int Foreground;
    public InstanceTrial() {
        name="tp-s7i-"+Guid.NewGuid().ToString("N");
        desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x01ff,IntPtr.Zero);
        if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception();
    }
    public int Start(string exe,string args,string dir,string log) {
        var c=new Child(); c.log=File.Create(log);
        var handle=c.log.SafeFileHandle.DangerousGetHandle(); SetHandleInformation(handle,1,1);
        var s=new Startup(); s.cb=(uint)Marshal.SizeOf(s); s.desktop=name; s.flags=0x100; s.output=handle; s.error=handle;
        if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+args),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref s,out c.p)) throw new System.ComponentModel.Win32Exception();
        children.Add(c); return children.Count-1;
    }
    // One 100 ms sample of every running child: V = an app window ("Window Class")
    // shown, h = none (winit's always-visible message window does not count).
    public void Sample(int ms) {
        var end=DateTime.UtcNow.AddMilliseconds(ms);
        while(DateTime.UtcNow<end) {
            var shown=new HashSet<uint>(); var fg=GetForegroundWindow(); var pids=new HashSet<uint>();
            foreach(var c in children) if(c.exit==259) pids.Add(c.p.pid);
            EnumDesktopWindows(desktop,delegate(IntPtr w,IntPtr p) {
                uint pid; GetWindowThreadProcessId(w,out pid);
                if(!pids.Contains(pid)) return true;
                if(w==fg) Foreground++;
                var cls=new StringBuilder(256); GetClassName(w,cls,256);
                if(IsWindowVisible(w) && cls.ToString()=="Window Class") shown.Add(pid);
                return true;
            },IntPtr.Zero);
            foreach(var c in children) {
                if(c.exit!=259) continue;
                GetExitCodeProcess(c.p.process,out c.exit);
                if(c.exit==259) c.trace.Append(shown.Contains(c.p.pid)?'V':'h');
            }
            Thread.Sleep(100);
        }
    }
    public bool Running(int i) { GetExitCodeProcess(children[i].p.process,out children[i].exit); return children[i].exit==259; }
    public void WaitAll(int ms) {
        var end=DateTime.UtcNow.AddMilliseconds(ms);
        while(DateTime.UtcNow<end) { bool any=false; for(int i=0;i<children.Count;i++) if(Running(i)) any=true; if(!any) break; Sample(100); }
        foreach(var c in children) if(c.exit==259) { TerminateProcess(c.p.process,124); WaitForSingleObject(c.p.process,5000); c.exit=124; }
    }
    public uint Exit(int i) { return children[i].exit; }
    public string Trace(int i) { return children[i].trace.ToString(); }
    public void Dispose() {
        foreach(var c in children) { c.log.Dispose(); CloseHandle(c.p.thread); CloseHandle(c.p.process); }
        CloseDesktop(desktop);
    }
}
'@
$common = '--no-activate --simulate-audio'
$trial = New-Object InstanceTrial
try {
    $a = $trial.Start($exe, "$common --profile s7-inst-p --single-instance --trial-close-after-ms 1500 --smoke-exit-after-ms 9000", $workspacePath, (Join-Path $work 'a.log'))
    $trial.Sample(3500)
    $b = $trial.Start($exe, "$common --profile s7-inst-p --single-instance --smoke-exit-after-ms 9000", $workspacePath, (Join-Path $work 'b.log'))
    $bStarted = [DateTime]::UtcNow
    while ($trial.Running($b) -and ([DateTime]::UtcNow - $bStarted).TotalSeconds -lt 10) { $trial.Sample(100) }
    $bSeconds = [math]::Round(([DateTime]::UtcNow - $bStarted).TotalSeconds, 1)
    $c = $trial.Start($exe, "$common --profile s7-inst-p --smoke-exit-after-ms 2500", $workspacePath, (Join-Path $work 'c.log'))
    $d = $trial.Start($exe, "$common --profile s7-inst-q --single-instance --smoke-exit-after-ms 2500", $workspacePath, (Join-Path $work 'd.log'))
    $trial.WaitAll(20000)
    $foreground = $trial.Foreground
    $exits = @($a, $b, $c, $d | ForEach-Object { $trial.Exit($_) })
    $traces = @($a, $b, $c, $d | ForEach-Object { $trial.Trace($_) })
} finally { $trial.Dispose(); $env:LOCALAPPDATA = $oldLocal; $env:SLINT_BACKEND = $oldBackend }
function Read-Shared($path) {
    $s = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
    try { (New-Object IO.StreamReader($s)).ReadToEnd() } finally { $s.Dispose() }
}
$logs = @("a", "b", "c", "d") | ForEach-Object { Read-Shared (Join-Path $work "$_.log") }
$checks = [ordered]@{
    all_exit_0 = -not ($exits | Where-Object { $_ -ne 0 })
    no_foreground = $foreground -eq 0
    a_hidden_then_restored = $traces[0] -match '^h*V+h+V+h*$' -and $logs[0] -match 'closed visible=false' -and $logs[0] -match 'restored visible=true'
    b_forwarded_without_window = $logs[1] -match 'TURBO_PANCAKE_SINGLE_INSTANCE forwarded=true' -and $traces[1] -notmatch 'V'
    c_scripted_run_unaffected = $traces[2] -match 'V' -and $logs[2] -match 'TURBO_PANCAKE_EXIT'
    d_other_profile_starts = $traces[3] -match 'V' -and $logs[3] -match 'TURBO_PANCAKE_EXIT'
}
$summary = "backend=$Backend exits=$($exits -join ',') foreground_samples=$foreground b_seconds=$bSeconds`n" +
    (($checks.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join "`n") +
    "`ntrace_a=$($traces[0])`ntrace_b=$($traces[1])`ntrace_c=$($traces[2])`ntrace_d=$($traces[3])"
if ($Out) { Set-Content -LiteralPath $Out -Value $summary -Encoding utf8 }
$summary
if ($checks.Values -contains $false) { throw 'S7 instance trial failed' }
