# Sets up a Windows build environment for Crusty-Claude WITHOUT Visual Studio / MSVC:
# the Rust `stable-x86_64-pc-windows-gnullvm` toolchain plus the llvm-mingw linker and C compiler
# (needed by the tree-sitter grammars). Prebuilt binaries from the Releases page need none of this.
#
#   powershell -ExecutionPolicy Bypass -File scripts\setup-gnu-toolchain.ps1
#
# Then, in each new shell (or add both lines to your profile):
#   $env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnullvm"
#   $env:Path = "$HOME\.llvm-mingw\<folder printed below>\bin;$env:Path"
param(
    [string]$InstallRoot = (Join-Path $HOME ".llvm-mingw")
)
$ErrorActionPreference = "Stop"

if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
    Write-Error "rustup is required: install it from https://rustup.rs (choose 'proceed without installing Visual Studio')."
}
rustup toolchain install stable-x86_64-pc-windows-gnullvm --profile minimal -c clippy -c rustfmt

$existing = Get-ChildItem $InstallRoot -Directory -Filter "llvm-mingw-*" -ErrorAction SilentlyContinue | Select-Object -First 1
if ($existing) {
    $folder = $existing.FullName
    Write-Host "llvm-mingw already present: $folder"
} else {
    $release = Invoke-RestMethod "https://api.github.com/repos/mstorsjo/llvm-mingw/releases/latest"
    $asset = $release.assets | Where-Object { $_.name -match "^llvm-mingw-.*-ucrt-x86_64\.zip$" } | Select-Object -First 1
    if (-not $asset) { Write-Error "no ucrt x86_64 asset found in the latest llvm-mingw release" }
    $zip = Join-Path ([System.IO.Path]::GetTempPath()) $asset.name
    Write-Host "Downloading $($asset.name) ($([math]::Round($asset.size / 1MB)) MB) from github.com/mstorsjo/llvm-mingw"
    Invoke-WebRequest $asset.browser_download_url -OutFile $zip
    New-Item -ItemType Directory -Force $InstallRoot | Out-Null
    Expand-Archive $zip -DestinationPath $InstallRoot -Force
    Remove-Item $zip
    $folder = (Get-ChildItem $InstallRoot -Directory -Filter "llvm-mingw-*" | Select-Object -First 1).FullName
}

Write-Host ""
Write-Host "Done. In each shell where you build Crusty-Claude:"
Write-Host "  `$env:RUSTUP_TOOLCHAIN = `"stable-x86_64-pc-windows-gnullvm`""
Write-Host "  `$env:Path = `"$folder\bin;`$env:Path`""
Write-Host "  cargo build --release -p crusty-cli"
