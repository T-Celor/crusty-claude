# Installs the prebuilt `crusty.exe` from the GitHub releases of T-Celor/crusty-claude.
#
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1            # latest release
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -Version v0.2.0
#
# The archive's SHA-256 is checked against the published checksum before anything is installed.
param(
    [string]$Version = "latest",
    [string]$Destination = (Join-Path $env:LOCALAPPDATA "Programs\crusty")
)
$ErrorActionPreference = "Stop"

$repo = "T-Celor/crusty-claude"
$target = "x86_64-pc-windows-msvc"
if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    Write-Error "No prebuilt binary for $env:PROCESSOR_ARCHITECTURE; build from source with: cargo install --path crates/crusty-cli"
}

if ($Version -eq "latest") {
    $Version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}
$name = "crusty-$Version-$target"
$base = "https://github.com/$repo/releases/download/$Version"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    Write-Host "Downloading $name.zip"
    Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
    Invoke-WebRequest "$base/$name.zip.sha256" -OutFile "$tmp\$name.zip.sha256"
    $expected = ((Get-Content "$tmp\$name.zip.sha256" -Raw).Trim() -split "\s+")[0].ToLower()
    $actual = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { Write-Error "checksum mismatch: expected $expected, got $actual" }

    Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force $Destination | Out-Null
    Copy-Item "$tmp\$name\crusty.exe" $Destination -Force
    Write-Host "Installed $Destination\crusty.exe ($Version)"

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not ($userPath -split ";" | Where-Object { $_ -eq $Destination })) {
        Write-Host "Add $Destination to your PATH to run 'crusty' (Settings > Environment variables)."
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
