param(
    [string]$Text = "",
    [string]$Out = "",
    [int]$ClickX = 0,
    [int]$ClickY = 0
)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Uni {
    [DllImport("user32.dll", SetLastError=true)] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [DllImport("user32.dll", SetLastError=true)] public static extern uint SendInput(uint n, INPUT[] pInputs, int cbSize);

    [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public InputUnion u; }
    [StructLayout(LayoutKind.Explicit)] public struct InputUnion {
        [FieldOffset(0)] public MOUSEINPUT mi;
        [FieldOffset(0)] public KEYBDINPUT ki;
    }
    [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx, dy; public uint mouseData, dwFlags, time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk, wScan; public uint dwFlags, time; public IntPtr dwExtraInfo; }
}
"@
$p = Get-Process monkeyfence -ErrorAction Stop
$h = $p.MainWindowHandle
[Uni]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 300
Write-Output "fg: $([Uni]::GetForegroundWindow() -eq $h)"

if ($ClickX -gt 0) {
    [Uni]::SetCursorPos($ClickX, $ClickY) | Out-Null
    Start-Sleep -Milliseconds 120
    [Uni]::mouse_event(2,0,0,0,[UIntPtr]::Zero)
    Start-Sleep -Milliseconds 50
    [Uni]::mouse_event(4,0,0,0,[UIntPtr]::Zero)
    Start-Sleep -Milliseconds 300
}

foreach ($ch in $Text.ToCharArray()) {
    $scan = [ushort][int]$ch
    $down = New-Object Uni+INPUT
    $down.type = 1
    $down.u.ki.wScan = $scan
    $down.u.ki.dwFlags = 0x0004  # KEYEVENTF_UNICODE
    $up = New-Object Uni+INPUT
    $up.type = 1
    $up.u.ki.wScan = $scan
    $up.u.ki.dwFlags = 0x0004 -bor 0x0002  # + KEYEVENTF_KEYUP
    $sent = [Uni]::SendInput(2, @($down, $up), [System.Runtime.InteropServices.Marshal]::SizeOf([type][Uni+INPUT]))
    if ($sent -ne 2) { Write-Output "SendInput failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }
    Start-Sleep -Milliseconds 25
}
Write-Output "sent unicode: $Text"

if ($Out -ne "") {
    Start-Sleep -Milliseconds 500
    Add-Type -AssemblyName System.Drawing
    $bmp = New-Object System.Drawing.Bitmap(1296, 839)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen(632, 301, 0, 0, (New-Object System.Drawing.Size(1296, 839)))
    $g.Dispose()
    $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Output "saved $Out"
}
