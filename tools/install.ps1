# Installs BETTERBS to D:\Apps\BETTERBS with Start menu / desktop shortcuts "BETTERBS".
# Close BETTERBS first. Usage: powershell -ExecutionPolicy Bypass -File tools\install.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$dest = 'D:\Apps\BETTERBS'
$running = Get-Process -Name BETTERBS, EBS -ErrorAction SilentlyContinue | Where-Object { $_.Path -like 'D:\Apps\*' }
if ($running) { throw "$($running[0].Path) is running. Close it first." }
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item "$root\target\release\BETTERBS.exe" $dest -Force
Copy-Item "$root\crates\app\assets\ebs.ico" "$dest\betterbs.ico" -Force
Get-ChildItem 'D:\ArloCapture\third_party\ffmpeg\bin\*.dll' | Copy-Item -Destination $dest -Force
$programs = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs"
$desktop = [Environment]::GetFolderPath('Desktop')
# Earlier names of this app ("EBS", "EBS (Rust)") are replaced by this install.
foreach ($old in @("$programs\EBS.lnk", "$desktop\EBS.lnk", "$programs\EBS (Rust).lnk", "$desktop\EBS (Rust).lnk")) { if (Test-Path $old) { Remove-Item $old } }
$sh = New-Object -ComObject WScript.Shell
foreach ($lnk in @("$programs\BETTERBS.lnk", "$desktop\BETTERBS.lnk")) {
    $s = $sh.CreateShortcut($lnk)
    $s.TargetPath = "$dest\BETTERBS.exe"
    $s.WorkingDirectory = $dest
    $s.IconLocation = "$dest\betterbs.ico,0"
    $s.Description = 'BETTERBS gameplay recorder'
    $s.Save()
}
"Installed to $dest"
