# Stages a self-contained corcel distribution for Windows: corcel.exe with
# every GStreamer runtime DLL beside it and the plugin subset corcel's
# pipelines actually use under lib\gstreamer-1.0\ (which main.rs points
# GST_PLUGIN_PATH at when present). The result runs on a machine with no
# GStreamer installed and no environment setup — feed dist\corcel to a zip
# or to the Inno Setup script (corcel.iss) next to this file.
#
# Usage (from the repo root, after `cargo build --release`):
#   powershell -ExecutionPolicy Bypass -File packaging\windows\package.ps1
#
# The full bin\*.dll copy is deliberate: walking each plugin's true DLL
# dependency graph is fragile across GStreamer releases, and the extra
# unused DLLs cost tens of MB in the installer — the wrong thing to
# optimize while the port is this young. Trim later with a dependency walk
# if size starts to matter.

param(
    # The MSVC GStreamer *runtime* install to stage DLLs from.
    [string]$GStreamerRoot = $(if ($env:GSTREAMER_1_0_ROOT_MSVC_X86_64) { $env:GSTREAMER_1_0_ROOT_MSVC_X86_64 } else { 'C:\gstreamer\1.0\msvc_x86_64' }),
    [string]$Target = 'target\release\corcel.exe',
    [string]$Out = 'dist\corcel'
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Target)) {
    Write-Error "$Target not found - run 'cargo build --release' first"
}
if (-not (Test-Path "$GStreamerRoot\bin\gstreamer-1.0-0.dll")) {
    Write-Error "no GStreamer runtime at $GStreamerRoot (install the runtime MSI, Complete mode)"
}

# The plugins corcel's pipelines reference (see corcel-media): capture,
# encode, RTP, decode, playback — plus the hardware encoder/decoder sets
# whose availability depends on the *user's* GPU, so all of them ship and
# codec.rs picks at runtime exactly like it does against a system install.
# Bare plugin names: the DLL is gst<name>.dll in the official MSVC
# packages and libgst<name>.dll in MinGW-convention builds.
$plugins = @(
    'coreelements',      # queue, capsfilter, ...
    'app',               # appsrc / appsink
    'typefindfunctions',
    'audioconvert',
    'audioresample',
    'level',             # speaking detection
    'opus',
    'rtp',               # rtp{opus,h264}{pay,depay}
    'rtpmanager',        # rtpjitterbuffer
    'videoconvertscale',
    'videoparsersbad',   # h264parse
    'wasapi2',           # mic + speakers
    'mediafoundation',   # camera + mfh264enc
    'd3d11',             # screen capture, d3d11download, DXVA decode
    'd3d12',
    'nvcodec',
    'qsv',
    'amfcodec'
)

if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
New-Item -ItemType Directory -Force "$Out\lib\gstreamer-1.0" | Out-Null

Copy-Item $Target $Out
Copy-Item "$GStreamerRoot\bin\*.dll" $Out

$missing = @()
foreach ($plugin in $plugins) {
    $path = @("gst$plugin.dll", "libgst$plugin.dll") |
        ForEach-Object { Join-Path $GStreamerRoot "lib\gstreamer-1.0\$_" } |
        Where-Object { Test-Path $_ } |
        Select-Object -First 1
    if ($path) {
        Copy-Item $path "$Out\lib\gstreamer-1.0"
    } else {
        $missing += $plugin
    }
}
if ($missing) {
    # Hardware plugin sets legitimately vary with the GStreamer build; the
    # core ones missing means a non-Complete install and a broken bundle.
    $core = $missing | Where-Object { $_ -notmatch 'nvcodec|qsv|amfcodec|d3d12' }
    if ($core) {
        Write-Error "core plugins missing from ${GStreamerRoot}: $($core -join ', ') - reinstall the runtime MSI in Complete mode"
    }
    Write-Warning "optional hardware plugins not in this GStreamer build: $($missing -join ', ')"
}

$size = [math]::Round((Get-ChildItem -Recurse $Out | Measure-Object Length -Sum).Sum / 1MB)
Write-Host "staged $Out (${size} MB)"
