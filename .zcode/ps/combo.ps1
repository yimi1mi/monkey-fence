param(
    [int]$ClickX = 0,
    [int]$ClickY = 0,
    [string]$Text = "",
    [string]$Chord = "",
    [string]$Out = ""
)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Combo {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern short VkKeyScan(char ch);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
$p = Get-Process monkeyfence -ErrorAction Stop
$h = $p.MainWindowHandle
[Combo]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 300
$fg1 = [Combo]::GetForegroundWindow()
Write-Output "fg after setfg: $($fg1 -eq $h)"

if ($ClickX -gt 0 -and $ClickY -gt 0) {
    [Combo]::SetCursorPos($ClickX, $ClickY) | Out-Null
    Start-Sleep -Milliseconds 120
    [Combo]::mouse_event(2,0,0,0,[UIntPtr]::Zero)
    Start-Sleep -Milliseconds 50
    [Combo]::mouse_event(4,0,0,0,[UIntPtr]::Zero)
    Start-Sleep -Milliseconds 250
    $fg2 = [Combo]::GetForegroundWindow()
    Write-Output "fg after click: $($fg2 -eq $h)"
}

$map = @{ "ctrl"=0x11; "shift"=0x10; "alt"=0x12; "escape"=0x1B; "return"=0x0D; "enter"=0x0D; "tab"=0x09 }
if ($Chord -ne "") {
    $parts = $Chord.ToLower() -split "\+" | Where-Object { $_ -ne "" }
    $mods = @(); $key = 0
    foreach ($part in $parts) {
        if ($map.ContainsKey($part)) {
            if ($part -in @("ctrl","shift","alt")) { $mods += $map[$part] } else { $key = $map[$part] }
        } elseif ($part.Length -eq 1) { $key = [byte][int][char]::ToUpper([char]$part) }
    }
    if ($key -gt 0) {
        foreach ($m in $mods) { [Combo]::keybd_event($m, 0, 0, [UIntPtr]::Zero) }
        Start-Sleep -Milliseconds 60
        [Combo]::keybd_event($key, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 40
        [Combo]::keybd_event($key, 0, 2, [UIntPtr]::Zero)
        foreach ($m in $mods) { [Combo]::keybd_event($m, 0, 2, [UIntPtr]::Zero) }
        $fg3 = [Combo]::GetForegroundWindow()
        Write-Output "fg after chord: $($fg3 -eq $h)"
    }
}

if ($Text -ne "") {
    foreach ($ch in $Text.ToCharArray()) {
        $vk = [Combo]::VkKeyScan($ch)
        $code = [byte]($vk -band 0xFF)
        $shift = (($vk -shr 8) -band 1) -eq 1
        if ($shift) { [Combo]::keybd_event(0x10, 0, 0, [UIntPtr]::Zero) }
        [Combo]::keybd_event($code, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 20
        [Combo]::keybd_event($code, 0, 2, [UIntPtr]::Zero)
        if ($shift) { [Combo]::keybd_event(0x10, 0, 2, [UIntPtr]::Zero) }
        Start-Sleep -Milliseconds 20
    }
    $fg4 = [Combo]::GetForegroundWindow()
    Write-Output "fg after text: $($fg4 -eq $h)"
}

if ($Out -ne "") {
    Start-Sleep -Milliseconds 400
    $rect = New-Object Combo+RECT
    [Combo]::GetWindowRect($h, [ref]$rect) | Out-Null
    $w = $rect.Right - $rect.Left; $ht = $rect.Bottom - $rect.Top
    $bmp = New-Object System.Drawing.Bitmap($w, $ht)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size($w, $ht)))
    $g.Dispose()
    $bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Output "saved $Out"
}
