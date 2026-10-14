param([ValidateSet('x86_64-pc-windows-msvc')][string]$Target = 'x86_64-pc-windows-msvc')
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$previousFlags = $env:RUSTFLAGS
Push-Location $root
try {
    # Embed the runtime so the GUI does not require a separate VC redistributable.
    $env:RUSTFLAGS = "$previousFlags -C target-feature=+crt-static".Trim()
    & cargo build --release --locked -p rowd-app --features gui --bin rowd-app --target $Target
    if ($LASTEXITCODE -ne 0) { throw 'Falha ao compilar a GUI Windows.' }
    $buildRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $root 'target' }
    $output = Join-Path $root 'dist/windows-x64'
    New-Item -ItemType Directory -Force -Path $output | Out-Null
    Copy-Item (Join-Path $buildRoot "$Target/release/rowd-app.exe") (Join-Path $output 'Rowd.exe') -Force
    Copy-Item 'LICENSE' (Join-Path $output 'LICENSE.txt') -Force
    Copy-Item 'crates/rowd-app/ui/fonts/InstrumentSans-OFL.txt' $output -Force
    Copy-Item 'crates/rowd-app/ui/fonts/Onest-OFL.txt' $output -Force
    Copy-Item 'crates/rowd-app/WINDOWS.md' (Join-Path $output 'LEIA-ME.md') -Force
    Write-Host "GUI Windows: $output/Rowd.exe"
} finally {
    $env:RUSTFLAGS = $previousFlags
    Pop-Location
}
