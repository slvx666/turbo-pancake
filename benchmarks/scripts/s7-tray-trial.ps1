# S7 step 1: window lifecycle trial. One release client on a separate invisible desktop
# (no focus, no input, simulated audio): close → hidden while running → restored through
# the tray window's message path → exit through the tray menu command, clean exit.
# The isolated desktop has no shell, so the tray icon itself is not shown there; the
# real icon is checked separately (`cargo test -p turbo-pancake-desktop --lib real_icon
# -- --ignored`).
param([string]$Target = '', [string]$Backend = 'winit-software', [string]$Out = '')
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $Target) { $Target = Join-Path $workspacePath 't/release' }
$exe = Join-Path $Target 'turbo-pancake-desktop.exe'
$work = Join-Path $Target ('s7-tray-trial-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$oldLocal = $env:LOCALAPPDATA; $oldBackend = $env:SLINT_BACKEND
$env:LOCALAPPDATA = $work; $env:SLINT_BACKEND = $Backend
Add-Type @'
using System;
using System.IO;
using System.Text;
using System.Runtime.InteropServices;
public static class TrayTrial {
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
    public static string Classes = "";
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string app,StringBuilder command,IntPtr ps,IntPtr ts,bool inherit,uint flags,IntPtr env,string dir,ref Startup startup,out ProcessInfo process);
    [DllImport("kernel32.dll")] static extern bool SetHandleInformation(IntPtr handle,uint mask,uint flags);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr handle,out uint code);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr handle,uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    // Visibility samples every 100 ms: V = a visible app window of the client (winit's
    // "Window Class"; its message window "Winit Thread Event Target" is always WS_VISIBLE
    // and does not count), h = none.
    public static string Run(string exe,string args,string dir,string log) {
        var name="tp-s7-"+Guid.NewGuid().ToString("N");
        var desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x01ff,IntPtr.Zero);
        if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception();
        var process=new ProcessInfo(); var trace=new StringBuilder(); uint exit=259; int foreground=0;
        try {
            using(var output=File.Create(log)) {
                var handle=output.SafeFileHandle.DangerousGetHandle();
                SetHandleInformation(handle,1,1);
                var startup=new Startup(); startup.cb=(uint)Marshal.SizeOf(startup);
                startup.desktop=name; startup.flags=0x100; startup.output=handle; startup.error=handle;
                if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+args),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref startup,out process)) throw new System.ComponentModel.Win32Exception();
                var deadline=DateTime.UtcNow.AddSeconds(30);
                while(WaitForSingleObject(process.process,100)==258 && DateTime.UtcNow<deadline) {
                    bool visible=false; IntPtr fg=GetForegroundWindow();
                    EnumDesktopWindows(desktop,delegate(IntPtr w,IntPtr p) {
                        uint pid; GetWindowThreadProcessId(w,out pid);
                        if(pid==process.pid && IsWindowVisible(w)) { var c=new StringBuilder(256); GetClassName(w,c,256); if(!Classes.Contains("["+c+"]")) Classes+="["+c+"]"; if(c.ToString()=="Window Class") visible=true; }
                        if(pid==process.pid && w==fg) foreground++;
                        return true;
                    },IntPtr.Zero);
                    trace.Append(visible?'V':'h');
                }
                GetExitCodeProcess(process.process,out exit);
                if(exit==259) { TerminateProcess(process.process,124); WaitForSingleObject(process.process,5000); exit=124; }
            }
            return "exit="+exit+" user_foreground_samples="+foreground+" visible_classes="+Classes+" trace="+trace;
        } finally {
            if(process.thread!=IntPtr.Zero) CloseHandle(process.thread);
            if(process.process!=IntPtr.Zero) CloseHandle(process.process);
            CloseDesktop(desktop);
        }
    }
}
'@
try {
    $log = Join-Path $work 'client.log'
    $result = [TrayTrial]::Run($exe, '--trial-tray --no-activate --simulate-audio --profile s7-tray-trial', $workspacePath, $log)
    $text = Get-Content -LiteralPath $log -Raw
    $trace = ($result -split 'trace=')[1]
    $checks = [ordered]@{
        clean_exit = $result -match '^exit=0 '
        not_foreground = $result -match 'user_foreground_samples=0 '
        shown_hidden_restored = $trace -match '^h*V+h+V+h*$'
        closed_hidden = $text -match 'TURBO_PANCAKE_LIFECYCLE closed visible=false'
        restored_visible = $text -match 'TURBO_PANCAKE_LIFECYCLE restored visible=true'
        exit_marker = $text -match 'TURBO_PANCAKE_EXIT'
    }
    $tray = ([regex]::Match($text, 'TURBO_PANCAKE_TRAY [^\r\n]*')).Value
    $summary = "backend=$Backend $tray`n$($result -replace ' trace=.*', '')`n" + (($checks.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join "`n")
    $summary = $summary + "`ntrace=" + $trace
    if ($Out) { Set-Content -LiteralPath $Out -Value $summary -Encoding utf8 }
    $summary
    if ($checks.Values -contains $false) { throw 'S7 tray trial failed' }
} finally {
    $env:LOCALAPPDATA = $oldLocal; $env:SLINT_BACKEND = $oldBackend
}
