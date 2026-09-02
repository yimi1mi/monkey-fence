Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Fg {
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, System.Text.StringBuilder text, int count);
}
"@
$h = [Fg]::GetForegroundWindow()
$sb = New-Object System.Text.StringBuilder 256
[Fg]::GetWindowText($h, $sb, 256) | Out-Null
$title = $sb.ToString()
$p = Get-Process monkeyfence -ErrorAction SilentlyContinue
Write-Output "foreground: $title"
Write-Output "mf handle: $($p.MainWindowHandle) fg: $h match: $($h -eq $p.MainWindowHandle)"
