# Builds gpui-spike against a copy of gpui-pre-windows 0.3.10 whose adapter
# selection honours GPUI_FORCE_WARP=1 (IDXGIFactory4::EnumWarpAdapter), to
# approximate a GPU-less Cloud PC on a machine that has a GPU. Upstream GPUI
# has no such switch: it takes the first DXGI adapter that creates a D3D11
# device, which on a machine without a GPU is already WARP ("Microsoft Basic
# Render Driver"). Output: target\warp\release\gpui-spike.exe (run with --warp).
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$spike = Split-Path -Parent $here
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
$source = Get-ChildItem "$env:USERPROFILE\.cargo\registry\src\*\gpui-pre-windows-0.3.10" -Directory -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $source) {
    cargo fetch --manifest-path "$spike\Cargo.toml"
    $source = Get-ChildItem "$env:USERPROFILE\.cargo\registry\src\*\gpui-pre-windows-0.3.10" -Directory | Select-Object -First 1
}
$dest = Join-Path $spike 'target\warp-src\gpui-pre-windows'
if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
New-Item -ItemType Directory -Force (Split-Path -Parent $dest) | Out-Null
Copy-Item -Recurse $source.FullName $dest
$file = Join-Path $dest 'src\directx_devices.rs'
$text = [IO.File]::ReadAllText($file)
$needle = "    for adapter_index in 0.. {"
if (-not $text.Contains($needle)) { throw "unexpected gpui-pre-windows source: $file" }
$patch = @'
    // Spike patch: GPUI_FORCE_WARP=1 selects the WARP software adapter.
    if std::env::var_os("GPUI_FORCE_WARP").is_some() {
        let adapter: IDXGIAdapter1 = unsafe { dxgi_factory.EnumWarpAdapter()? };
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        let device = get_device(&adapter, Some(&mut context), Some(&mut feature_level), debug_layer_available)?;
        return Ok((adapter, device, context.unwrap(), feature_level));
    }

'@
[IO.File]::WriteAllText($file, $text.Replace($needle, $patch + $needle))
$patchPath = $dest.Replace('\', '/')
cargo build --release --manifest-path "$spike\Cargo.toml" --target-dir "$spike\target\warp" --config "patch.crates-io.gpui-pre-windows.path='$patchPath'"
if ($LASTEXITCODE -ne 0) { throw "build failed" }
Write-Host "built $spike\target\warp\release\gpui-spike.exe (run with --warp)"
