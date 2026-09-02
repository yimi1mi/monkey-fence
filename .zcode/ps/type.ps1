param(
    [string]$Text = ""
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Kb33 {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern short VkKeyScan(char ch);
}
"@
$p = Get-Process monkeyfence -ErrorAction Stop
[Kb33]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
Start-Sleep -Milliseconds 250
foreach ($ch in $Text.ToCharArray()) {
    $vk = [Kb33]::VkKeyScan($ch)
    $code = [byte]($vk -band 0xFF)
    $shift = (($vk -shr 8) -band 1) -eq 1
    if ($shift) { [Kb33]::keybd_event(0x10, 0, 0, [UIntPtr]::Zero) }
    [Kb33]::keybd_event($code, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 15
    [Kb33]::keybd_event($code, 0, 2, [UIntPtr]::Zero)
    if ($shift) { [Kb33]::keybd_event(0x10, 0, 2, [UIntPtr]::Zero) }
    Start-Sleep -Milliseconds 15
}
Write-Output "typed $Text"
