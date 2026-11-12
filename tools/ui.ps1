# UI automation helpers for driving smoothyfiles in dev tests.
# Dot-source:  . tools/ui.ps1 ; $p = Start-RF "C:\some\dir" ; Send-Keys "^+n" ; Shot-Screen "x.png" ; Stop-RF
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class U {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
}
"@
[U]::SetProcessDPIAware() | Out-Null
$script:RF = $null
$script:RFExe = "$PSScriptRoot\..\target\debug\smoothyfiles.exe"
$env:SMOOTHYFILES_MULTI_INSTANCE = "1"   # tests must not forward into another running copy

function Start-RF([string]$Path = "") {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  if ($Path) { $script:RF = Start-Process $script:RFExe -ArgumentList "`"$Path`"" -PassThru }
  else { $script:RF = Start-Process $script:RFExe -PassThru }
  while ($script:RF.MainWindowHandle -eq 0 -and $sw.ElapsedMilliseconds -lt 15000) { Start-Sleep -Milliseconds 10; $script:RF.Refresh() }
  Start-Sleep -Milliseconds 700
  Focus-RF
  return $sw.ElapsedMilliseconds
}
function Focus-RF {
  # Alt tap lets a background process take foreground.
  [U]::keybd_event(0x12, 0, 0, [IntPtr]::Zero); [U]::keybd_event(0x12, 0, 2, [IntPtr]::Zero)
  [U]::SetForegroundWindow($script:RF.MainWindowHandle) | Out-Null
  Start-Sleep -Milliseconds 150
}
function Get-RFRect { $r = New-Object U+RECT; [U]::GetWindowRect($script:RF.MainWindowHandle, [ref]$r) | Out-Null; return $r }
function Send-Keys([string]$k, [int]$wait = 250) { [System.Windows.Forms.SendKeys]::SendWait($k); Start-Sleep -Milliseconds $wait }
# Click at window-relative physical pixel coordinates. button: left|right|middle
function Click-At([int]$x, [int]$y, [string]$button = "left", [int]$count = 1, [int]$wait = 300) {
  $r = Get-RFRect
  [U]::SetCursorPos($r.L + $x, $r.T + $y) | Out-Null
  Start-Sleep -Milliseconds 60
  $down, $up = switch ($button) { "right" { 0x8, 0x10 } "middle" { 0x20, 0x40 } default { 0x2, 0x4 } }
  for ($i = 0; $i -lt $count; $i++) {
    [U]::mouse_event($down, 0, 0, 0, [IntPtr]::Zero); [U]::mouse_event($up, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 40
  }
  Start-Sleep -Milliseconds $wait
}
# Move the cursor (window-relative) without clicking, e.g. to show tooltips.
function Move-To([int]$x, [int]$y, [int]$wait = 100) {
  $r = Get-RFRect; [U]::SetCursorPos($r.L + $x, $r.T + $y) | Out-Null; Start-Sleep -Milliseconds $wait
}
function Shot-Screen([string]$out) {
  $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $r = Get-RFRect
  $x = [Math]::Max($r.L, $b.Left); $y = [Math]::Max($r.T, $b.Top)
  $w = [Math]::Min($r.R, $b.Right) - $x; $h = [Math]::Min($r.B, $b.Bottom) - $y
  $bmp = New-Object System.Drawing.Bitmap $w, $h
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($x, $y, 0, 0, $bmp.Size)
  $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png); $g.Dispose(); $bmp.Dispose()
}
function Mem-RF { $script:RF.Refresh(); "WS {0:N1} MB, Private {1:N1} MB" -f ($script:RF.WorkingSet64/1MB), ($script:RF.PrivateMemorySize64/1MB) }
function Stop-RF { if ($script:RF) { Stop-Process -Id $script:RF.Id -ErrorAction SilentlyContinue } }
