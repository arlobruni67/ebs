# Installs EBS to D:\Apps\EBS with Start menu / desktop shortcuts "EBS".
# Close EBS first. Usage: powershell -ExecutionPolicy Bypass -File tools\install.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$dest = 'D:\Apps\EBS'
$running = Get-Process -Name EBS, BETTERBS -ErrorAction SilentlyContinue | Where-Object { $_.Path -like 'D:\Apps\*' }
if ($running) { throw "$($running[0].Path) is running. Close it first." }
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item "$root\target\release\EBS.exe" $dest -Force
Copy-Item "$root\crates\app\assets\ebs.ico" $dest -Force
Get-ChildItem 'D:\ArloCapture\third_party\ffmpeg\bin\*.dll' | Copy-Item -Destination $dest -Force
$programs = "$env:APPDATA\Microsoft\Windows\Start Menu\Programs"
$desktop = [Environment]::GetFolderPath('Desktop')
# Shortcuts from earlier names of this app ("BETTERBS", "EBS (Rust)") are replaced by this install.
foreach ($old in @("$programs\BETTERBS.lnk", "$desktop\BETTERBS.lnk", "$programs\EBS (Rust).lnk", "$desktop\EBS (Rust).lnk")) { if (Test-Path $old) { Remove-Item $old } }
$sh = New-Object -ComObject WScript.Shell
foreach ($lnk in @("$programs\EBS.lnk", "$desktop\EBS.lnk")) {
    $s = $sh.CreateShortcut($lnk)
    $s.TargetPath = "$dest\EBS.exe"
    $s.WorkingDirectory = $dest
    $s.IconLocation = "$dest\ebs.ico,0"
    $s.Description = 'EBS gameplay recorder'
    $s.Save()
}
"Installed to $dest"
