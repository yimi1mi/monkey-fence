param(
    [int]$X,
    [int]$Y,
    [switch]$DoubleClick
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Mouse32 {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
}
"@
$p = Get-Process monkeyfence -ErrorAction Stop
[Mouse32]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
Start-Sleep -Milliseconds 200
[Mouse32]::SetCursorPos($X, $Y) | Out-Null
Start-Sleep -Milliseconds 120
[Mouse32]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero)  # LEFTDOWN
Start-Sleep -Milliseconds 40
[Mouse32]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)  # LEFTUP
if ($DoubleClick) {
    Start-Sleep -Milliseconds 60
    [Mouse32]::mouse_event(2, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 40
    [Mouse32]::mouse_event(4, 0, 0, 0, [UIntPtr]::Zero)
}
Write-Output "clicked $X,$Y"
