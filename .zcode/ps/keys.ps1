param(
    [string]$Chord = ""   # e.g. "ctrl+shift+w" ; single key e.g. "escape" "return" "tab"
)
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Kb32 {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
}
"@
$p = Get-Process monkeyfence -ErrorAction Stop
[Kb32]::SetForegroundWindow($p.MainWindowHandle) | Out-Null
Start-Sleep -Milliseconds 250

$map = @{
    "ctrl"=0x11; "shift"=0x10; "alt"=0x12; "win"=0x5B
    "escape"=0x1B; "return"=0x0D; "enter"=0x0D; "tab"=0x09; "space"=0x20
    "left"=0x25; "up"=0x26; "right"=0x27; "down"=0x28; "backspace"=0x08; "delete"=0x2E
    "f5"=0x74; "f11"=0x7A
}
$parts = $Chord.ToLower() -split "\+" | Where-Object { $_ -ne "" }
$mods = @()
$key = $null
foreach ($part in $parts) {
    if ($map.ContainsKey($part)) {
        if ($part -in @("ctrl","shift","alt","win")) { $mods += $map[$part] } else { $key = $map[$part] }
    } elseif ($part.Length -eq 1) {
        $c = [char]$part
        $key = [byte][int][char]::ToUpper($c)   # 'a'..'z','0'..'9' == VK codes
    }
}
if ($null -eq $key) { Write-Output "no key in chord"; exit 1 }
foreach ($m in $mods) { [Kb32]::keybd_event($m, 0, 0, [UIntPtr]::Zero) }
Start-Sleep -Milliseconds 60
[Kb32]::keybd_event($key, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 40
[Kb32]::keybd_event($key, 0, 2, [UIntPtr]::Zero)
foreach ($m in $mods) { [Kb32]::keybd_event($m, 0, 2, [UIntPtr]::Zero) }
Write-Output "sent $Chord"
