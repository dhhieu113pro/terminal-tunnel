# Creates a Start Menu shortcut (and optionally a "start with Windows" shortcut) for the tray app.
#
#   pwsh assets\install-shortcut.ps1                       # Start Menu entry
#   pwsh assets\install-shortcut.ps1 -Startup              # ...and start it when you sign in to Windows
#   pwsh assets\install-shortcut.ps1 -Root D:/Development  # pass --root (otherwise the root from the settings file is used)
#   pwsh assets\install-shortcut.ps1 -Remove               # delete both shortcuts again
#
# Run it from the repo (it points at dist\terminal-tunnel-tray.exe, or target\release\ if dist\ is missing).
param(
    [string]$Exe,
    [string]$Root,
    [switch]$Startup,
    [switch]$Remove
)

$programs = [Environment]::GetFolderPath('Programs')
$startup = [Environment]::GetFolderPath('Startup')
$name = 'terminal-tunnel.lnk'

if ($Remove) {
    foreach ($p in (Join-Path $programs $name), (Join-Path $startup $name)) {
        if (Test-Path $p) { Remove-Item $p; "removed $p" }
    }
    return
}

if (-not $Exe) {
    $repo = Split-Path $PSScriptRoot -Parent
    $Exe = @("$repo\dist\terminal-tunnel-tray.exe", "$repo\target\release\terminal-tunnel-tray.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Exe -or -not (Test-Path $Exe)) { throw 'terminal-tunnel-tray.exe not found: build it first (cargo build --release) or pass -Exe' }
$Exe = (Resolve-Path $Exe).Path

function New-Shortcut([string]$path) {
    $sh = New-Object -ComObject WScript.Shell
    $lnk = $sh.CreateShortcut($path)
    $lnk.TargetPath = $Exe
    $lnk.Arguments = $(if ($Root) { "--root `"$Root`"" } else { '' })
    $lnk.WorkingDirectory = Split-Path $Exe -Parent
    $lnk.IconLocation = "$Exe,0"
    $lnk.Description = 'terminal-tunnel: phone terminal in the system tray'
    $lnk.Save()
    "created $path"
}

New-Shortcut (Join-Path $programs $name)
if ($Startup) { New-Shortcut (Join-Path $startup $name) }
