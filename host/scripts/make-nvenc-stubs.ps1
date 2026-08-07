# Generates import libraries (nvEncodeAPI.lib, nvcuvid.lib) from the driver DLLs in
# System32, so the nvidia-video-codec-sdk crate can link without a full Video Codec SDK
# install. Output: host/sdk-stub/. Run once per machine.
$ErrorActionPreference = 'Stop'

$here = Split-Path -Parent $PSScriptRoot
$out = Join-Path $here 'sdk-stub'
New-Item -ItemType Directory -Force $out | Out-Null

# Locate VS's lib.exe + dumpbin.exe via vswhere.
$vswhere = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path $vswhere)) { throw "vswhere not found: $vswhere" }
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vsPath) { throw 'No Visual Studio with C++ tools found.' }
$msvcDir = Get-ChildItem (Join-Path $vsPath 'VC\Tools\MSVC') -Directory |
    Sort-Object Name -Descending | Select-Object -First 1
$lib = Join-Path $msvcDir.FullName 'bin\Hostx64\x64\lib.exe'
$dumpbin = Join-Path $msvcDir.FullName 'bin\Hostx64\x64\dumpbin.exe'
if (-not (Test-Path $lib)) { throw "lib.exe not found: $lib" }
if (-not (Test-Path $dumpbin)) { throw "dumpbin.exe not found: $dumpbin" }

function New-ImportLib {
    param([string]$DllName, [string]$LibName)
    $dll = "C:\Windows\System32\$DllName"
    if (-not (Test-Path $dll)) { throw "Missing driver DLL: $dll (is the NVIDIA driver installed?)" }

    # 1) Dump the export table.
    $exports = & $dumpbin /exports $dll

    # 2) Build a .def file from the named exports.
    #    dumpbin lines look like:  "          1    0 0001234F NvEncodeAPICreateInstance"
    $names = @()
    foreach ($line in $exports) {
        if ($line -match '^\s*\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+([A-Za-z_][A-Za-z0-9_]*)') {
            $names += $Matches[1]
        }
    }
    if ($names.Count -eq 0) { throw "No named exports parsed from $DllName" }

    $defPath = Join-Path $out "$LibName.def"
    @('EXPORTS') + $names | Set-Content -Path $defPath -Encoding ascii

    # 3) Generate the import library.
    $libPath = Join-Path $out "$LibName.lib"
    & $lib /def:$defPath /machine:x64 /nologo /out:$libPath
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $libPath)) {
        throw "lib.exe failed for $DllName"
    }
    Remove-Item -Force (Join-Path $out "$LibName.exp") -ErrorAction SilentlyContinue
    Write-Host "Generated $LibName.lib ($($names.Count) exports)"
}

New-ImportLib -DllName 'nvEncodeAPI64.dll' -LibName 'nvEncodeAPI'
New-ImportLib -DllName 'nvcuvid.dll' -LibName 'nvcuvid'

Write-Host "Done. Import libs in $out"
