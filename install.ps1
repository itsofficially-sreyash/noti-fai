# Registers ballpop for the current user (no admin needed):
#   - the browser bridge for Chrome and Edge (native messaging host)
#   - `ballpop watch` at sign-in, for desktop app notifications
# Run again after moving the folder. `-Uninstall` removes everything.
param([switch]$Uninstall)

$ErrorActionPreference = 'Stop'
$name = 'com.ballpop.host'
$extensionId = 'jfolfoeeoohihembfjoghamkchchgmlc'
$exe = Join-Path $PSScriptRoot 'dist\ballpop.exe'
$manifest = Join-Path $PSScriptRoot "dist\$name.json"
$browsers = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts', 'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts'
$shortcut = Join-Path ([Environment]::GetFolderPath('Startup')) 'ballpop watch.lnk'

Get-Process ballpop -ErrorAction SilentlyContinue | Stop-Process -Force

if ($Uninstall) {
    foreach ($b in $browsers) { Remove-Item "$b\$name" -ErrorAction SilentlyContinue }
    Remove-Item $shortcut, $manifest -ErrorAction SilentlyContinue
    'ballpop removed.'
    return
}

if (-not (Test-Path $exe)) { throw "Build first: $exe not found (cargo build --release, then copy to dist\)" }

[ordered]@{
    name            = $name
    description     = 'ballpop bouncing-ball notifications'
    path            = $exe
    type            = 'stdio'
    allowed_origins = @("chrome-extension://$extensionId/")
} | ConvertTo-Json | Set-Content -Encoding utf8 $manifest

foreach ($b in $browsers) {
    New-Item "$b\$name" -Force | Out-Null
    Set-Item "$b\$name" -Value $manifest
}

$link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
$link.TargetPath = $exe
$link.Arguments = 'watch'
$link.Save()
Start-Process $exe -ArgumentList 'watch'

"ballpop installed. Watcher running and set to start at sign-in."
"Now load the extension: chrome://extensions > Developer mode > Load unpacked > $(Join-Path $PSScriptRoot 'extension')"
