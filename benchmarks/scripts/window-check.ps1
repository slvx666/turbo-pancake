# Windows smoke check for the desktop window, designed to run while the user keeps
# working: the window is placed on a secondary monitor and created without
# activation; the script never moves the mouse or sends input.
# Verifies: window appears at the requested position, never becomes the
# foreground window, exits cleanly. Optionally saves a screenshot of the window.
#
#   powershell -File benchmarks/scripts/window-check.ps1 -Exe D:/dsmini-target/release/dsmini-desktop.exe `
#     [-Backend winit-software] [-X 2020 -Y 100] [-Screenshot out.png]
param([string]$Exe, [string]$Backend = "", [int]$X = 2020, [int]$Y = 100, [string]$Screenshot = "")
Add-Type @"
using System; using System.Collections.Generic; using System.Runtime.InteropServices; using System.Text;
public static class W {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  public struct RECT { public int L, T, R, B; }
  public static string Title(IntPtr h) { var s = new StringBuilder(256); GetWindowText(h, s, 256); return s.ToString(); }
  public static uint Pid(IntPtr h) { uint p; GetWindowThreadProcessId(h, out p); return p; }
  public static List<IntPtr> Visible(uint pid) {
    var l = new List<IntPtr>();
    EnumWindows((h, x) => { if (Pid(h) == pid && IsWindowVisible(h)) l.Add(h); return true; }, IntPtr.Zero);
    return l;
  }
}
"@
if ($Backend) { $env:SLINT_BACKEND = $Backend } else { Remove-Item Env:SLINT_BACKEND -ErrorAction SilentlyContinue }
$proc = Start-Process -FilePath $Exe -ArgumentList "--smoke-exit-after-ms","4000","--window-pos","$X,$Y","--no-activate" -PassThru
$ourFg = 0; $samples = 0; $win = $null; $best = 0
$deadline = (Get-Date).AddMilliseconds(3500)
while ((Get-Date) -lt $deadline) {
  $samples++
  if ([W]::Pid([W]::GetForegroundWindow()) -eq $proc.Id) { $ourFg++ }
  foreach ($h in [W]::Visible([uint32]$proc.Id)) {
    $t = New-Object W+RECT; [void][W]::GetWindowRect($h, [ref]$t)
    $area = ($t.R - $t.L) * ($t.B - $t.T)
    if ($area -gt $best) { $best = $area; $win = $h }
  }
  Start-Sleep -Milliseconds 20
}
$r = New-Object W+RECT
if ($win) { [void][W]::GetWindowRect($win, [ref]$r) }
$title = if ($win) { [W]::Title($win) } else { "" }
if ($win -and $Screenshot) {
  Add-Type -AssemblyName System.Drawing
  $bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size)
  $bmp.Save($Screenshot, [System.Drawing.Imaging.ImageFormat]::Png); $g.Dispose(); $bmp.Dispose()
}
$proc.WaitForExit(10000) | Out-Null
"window_found=$([bool]$win) title='$title' rect=($($r.L),$($r.T))-($($r.R),$($r.B)) at_requested_pos=$($r.L -eq $X -and $r.T -eq $Y)"
"foreground_samples=$samples foreground_was_ours=$ourFg exit_code=$($proc.ExitCode)"
