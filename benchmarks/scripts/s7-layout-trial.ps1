# S7 step 3: layout and scale checks. Release clients one after another on a separate
# invisible desktop (no focus, no input, simulated audio); each takes a snapshot of its
# own window (renderer pixels only, never the user's screen) at a given physical size
# and scale factor. Checks: the snapshot has the requested size, the scale factor in
# effect is the expected one (from SLINT_SCALE_FACTOR or from the profile's setting),
# clean exit. The PNGs are for visual review (stay in the ignored build directory).
param([string]$Target = '', [string]$Backend = 'winit-software', [string]$Out = '')
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
if (-not $Target) { $Target = Join-Path $workspacePath 't/release' }
$exe = Join-Path $Target 'turbo-pancake-desktop.exe'
$work = Join-Path $Target ('s7-layout-trial-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$saved = @{ LOCALAPPDATA = $env:LOCALAPPDATA; SLINT_BACKEND = $env:SLINT_BACKEND; SLINT_SCALE_FACTOR = $env:SLINT_SCALE_FACTOR }
$env:LOCALAPPDATA = $work; $env:SLINT_BACKEND = $Backend
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.IO;
using System.Text;
using System.Runtime.InteropServices;
public static class LayoutTrial {
    [StructLayout(LayoutKind.Sequential, CharSet=CharSet.Unicode)] struct Startup {
        public uint cb; public string reserved, desktop, title;
        public uint x,y,width,height,xChars,yChars,fill,flags;
        public ushort show,reservedSize; public IntPtr reservedPtr,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] struct ProcessInfo { public IntPtr process,thread; public uint pid,tid; }
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateDesktop(string name,IntPtr device,IntPtr mode,uint flags,uint access,IntPtr security);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcess(string app,StringBuilder command,IntPtr ps,IntPtr ts,bool inherit,uint flags,IntPtr env,string dir,ref Startup startup,out ProcessInfo process);
    [DllImport("kernel32.dll")] static extern bool SetHandleInformation(IntPtr handle,uint mask,uint flags);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr handle,uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr handle,out uint code);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr handle,uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    public static uint Run(string exe,string args,string dir,string log) {
        var name="tp-s7l-"+Guid.NewGuid().ToString("N");
        var desktop=CreateDesktop(name,IntPtr.Zero,IntPtr.Zero,0,0x01ff,IntPtr.Zero);
        if(desktop==IntPtr.Zero) throw new System.ComponentModel.Win32Exception();
        var p=new ProcessInfo(); uint exit=259;
        try {
            using(var output=File.Create(log)) {
                var h=output.SafeFileHandle.DangerousGetHandle(); SetHandleInformation(h,1,1);
                var s=new Startup(); s.cb=(uint)Marshal.SizeOf(s); s.desktop=name; s.flags=0x100; s.output=h; s.error=h;
                if(!CreateProcess(exe,new StringBuilder("\""+exe+"\" "+args),IntPtr.Zero,IntPtr.Zero,true,0x08000000,IntPtr.Zero,dir,ref s,out p)) throw new System.ComponentModel.Win32Exception();
                if(WaitForSingleObject(p.process,20000)==258) { TerminateProcess(p.process,124); WaitForSingleObject(p.process,5000); }
                GetExitCodeProcess(p.process,out exit);
            }
            return exit;
        } finally {
            if(p.thread!=IntPtr.Zero) CloseHandle(p.thread);
            if(p.process!=IntPtr.Zero) CloseHandle(p.process);
            CloseDesktop(desktop);
        }
    }
}
'@
function Convert-Ppm($ppm, $png) {
    $bytes = [IO.File]::ReadAllBytes($ppm)
    $header = [Text.Encoding]::ASCII.GetString($bytes, 0, 32) -split '\s+'
    $w = [int]$header[1]; $h = [int]$header[2]
    $offset = ("P6`n$w $h`n255`n").Length
    $bmp = New-Object Drawing.Bitmap $w, $h, ([Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $data = $bmp.LockBits((New-Object Drawing.Rectangle 0, 0, $w, $h), 'WriteOnly', $bmp.PixelFormat)
    $row = New-Object byte[] ($w * 3)
    for ($y = 0; $y -lt $h; $y++) {
        for ($x = 0; $x -lt $w; $x++) {
            $i = $offset + ($y * $w + $x) * 3
            $row[$x * 3] = $bytes[$i + 2]; $row[$x * 3 + 1] = $bytes[$i + 1]; $row[$x * 3 + 2] = $bytes[$i]
        }
        [Runtime.InteropServices.Marshal]::Copy($row, 0, [IntPtr]($data.Scan0.ToInt64() + $y * $data.Stride), $w * 3)
    }
    $bmp.UnlockBits($data); $bmp.Save($png, [Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    @($w, $h)
}
# name, physical size, SLINT_SCALE_FACTOR ('' = from the setting), view args, ui_scale setting, expected factor
$cases = @(
    @('wide-auth',       '1280x720', '1',   '',                        100, 1),
    @('narrow-auth',     '700x480',  '1',   '',                        100, 1),
    @('narrow-voice',    '700x480',  '1',   '--room layout-check',     100, 1),
    @('narrow-screen',   '700x480',  '1',   '--room layout-check --call-tab 1', 100, 1),
    @('narrow-sidebar',  '700x480',  '1',   '--sidebar-open',          100, 1),
    @('wide-voice',      '1280x720', '1',   '--room layout-check',     100, 1),
    @('hidpi150-voice',  '1920x1080','1.5', '--room layout-check',     100, 1.5),
    @('hidpi200-auth',   '1400x900', '2',   '',                        100, 2),
    @('hidpi200-settings','1400x900','2',   '--open-settings',         100, 2),
    @('setting125-settings','1100x700','',  '--open-settings',         125, 0)
)
$lines = @(); $failed = $false
try {
    foreach ($c in $cases) {
        $name, $size, $sf, $view, $setting, $expected = $c
        $profile = 's7-layout-' + $name
        if ($sf) { $env:SLINT_SCALE_FACTOR = $sf } else { Remove-Item Env:SLINT_SCALE_FACTOR -ErrorAction SilentlyContinue }
        $settingsDir = Join-Path $work 'turbo-pancake/settings'
        New-Item -ItemType Directory -Path $settingsDir -Force | Out-Null
        Set-Content -LiteralPath (Join-Path $settingsDir "$profile.conf") -Value "ui_scale=$setting" -Encoding ascii
        $ppm = Join-Path $work "$name.ppm"; $log = Join-Path $work "$name.log"
        $args = "--no-activate --simulate-audio --profile $profile --window-size $size --smoke-exit-after-ms 3500 --snapshot `"$ppm`" --snapshot-after-ms 2500 $view"
        $exit = [LayoutTrial]::Run($exe, $args, $workspacePath, $log)
        $text = Get-Content -LiteralPath $log -Raw
        $factor = [double](([regex]::Match($text, 'TURBO_PANCAKE_SCALE factor=([0-9.]+)')).Groups[1].Value)
        if ($expected -eq 0) { $expected = [math]::Round($factor, 3) }  # setting × system scale, reported below
        $dims = if (Test-Path $ppm) { Convert-Ppm $ppm (Join-Path $work "$name.png") } else { @(0, 0) }
        $want = $size -split 'x'
        $ok = $exit -eq 0 -and $text -match 'TURBO_PANCAKE_SNAPSHOT success=true' -and $text -match 'TURBO_PANCAKE_EXIT' -and
            $dims[0] -eq [int]$want[0] -and $dims[1] -eq [int]$want[1] -and [math]::Abs($factor - $expected) -lt 0.01
        # From the setting: 1.25 × the system scale (a multiple of 0.25 on Windows).
        if ($name -like "setting125*") { $sys = $factor / 1.25; $ok = $ok -and $sys -ge 1 -and [math]::Abs($sys * 4 - [math]::Round($sys * 4)) -lt 0.01 }
        if (-not $ok) { $failed = $true }
        $lines += "$name size=$($dims[0])x$($dims[1]) requested=$size factor=$factor exit=$exit pass=$ok"
    }
} finally {
    foreach ($k in $saved.Keys) { if ($null -eq $saved[$k]) { Remove-Item "Env:$k" -ErrorAction SilentlyContinue } else { Set-Item "Env:$k" $saved[$k] } }
}
$summary = "backend=$Backend work=$work`n" + ($lines -join "`n")
if ($Out) { Set-Content -LiteralPath $Out -Value $summary -Encoding utf8 }
$summary
if ($failed) { throw 'S7 layout trial failed' }
