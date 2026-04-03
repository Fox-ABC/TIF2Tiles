Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# Bundle OSGeo4W GDAL runtime into src-tauri/resources/gdal for Windows installers.
$root = Split-Path -Parent $PSScriptRoot
$resGdalDir = Join-Path $root "src-tauri/resources/gdal"
$binOutDir = Join-Path $resGdalDir "bin"
$shareOutDir = Join-Path $resGdalDir "share"

function Resolve-OsgeoRoot {
  # Allow explicit override first, then probe common install paths.
  if ($env:OSGEO4W_ROOT -and (Test-Path $env:OSGEO4W_ROOT)) {
    return (Resolve-Path $env:OSGEO4W_ROOT).Path
  }

  $candidates = @(
    "C:\OSGeo4W64",
    "C:\OSGeo4W"
  )

  foreach ($candidate in $candidates) {
    if (Test-Path $candidate) {
      return (Resolve-Path $candidate).Path
    }
  }

  throw "OSGeo4W not found. Set OSGEO4W_ROOT or install to C:\OSGeo4W64/C:\OSGeo4W."
}

function Reset-OutputDir {
  param(
    [Parameter(Mandatory = $true)][string]$Path
  )

  if (Test-Path $Path) {
    Remove-Item -Path $Path -Recurse -Force
  }
  New-Item -Path $Path -ItemType Directory | Out-Null
}

function Copy-IfExists {
  param(
    [Parameter(Mandatory = $true)][string]$Source,
    [Parameter(Mandatory = $true)][string]$Destination
  )

  if (Test-Path $Source) {
    Copy-Item -Path $Source -Destination $Destination -Force
    return $true
  }
  return $false
}

function Copy-RequiredTools {
  param(
    [Parameter(Mandatory = $true)][string]$BinSrcDir,
    [Parameter(Mandatory = $true)][string]$BinDstDir
  )

  # Keep a minimal but practical command set for inspect/warp/translate/tiling.
  $required = @(
    "gdalinfo.exe",
    "gdalwarp.exe",
    "gdal_translate.exe",
    "gdal2tiles.py"
  )

  foreach ($name in $required) {
    $src = Join-Path $BinSrcDir $name
    if (-not (Copy-IfExists -Source $src -Destination $BinDstDir)) {
      throw "Missing required GDAL tool: $src"
    }
  }
}

function Copy-RuntimeDependencies {
  param(
    [Parameter(Mandatory = $true)][string]$BinSrcDir,
    [Parameter(Mandatory = $true)][string]$BinDstDir
  )

  # Copy common GDAL/PROJ/GEOS runtime dlls to avoid host machine dependency.
  $patterns = @(
    "gdal*.dll",
    "proj*.dll",
    "geos*.dll",
    "sqlite3.dll",
    "zlib*.dll",
    "libcurl*.dll",
    "libcrypto*.dll",
    "libssl*.dll",
    "libxml2*.dll",
    "iconv*.dll",
    "jpeg*.dll",
    "png*.dll",
    "tiff*.dll",
    "webp*.dll",
    "expat*.dll",
    "python*.dll"
  )

  foreach ($pattern in $patterns) {
    Get-ChildItem -Path $BinSrcDir -Filter $pattern -File -ErrorAction SilentlyContinue |
      ForEach-Object {
        Copy-Item -Path $_.FullName -Destination $BinDstDir -Force
      }
  }

  # Some OSGeo4W setups provide a batch wrapper for gdal2tiles; copy it if present.
  $bat = Join-Path $BinSrcDir "gdal2tiles.bat"
  $null = Copy-IfExists -Source $bat -Destination $BinDstDir
}

function Copy-DataDirectories {
  param(
    [Parameter(Mandatory = $true)][string]$OsgeoRoot,
    [Parameter(Mandatory = $true)][string]$ShareDstDir
  )

  $gdalDataSrc = Join-Path $OsgeoRoot "share/gdal"
  $projDataSrc = Join-Path $OsgeoRoot "share/proj"
  if (-not (Test-Path $gdalDataSrc)) {
    throw "GDAL data directory missing: $gdalDataSrc"
  }
  if (-not (Test-Path $projDataSrc)) {
    throw "PROJ data directory missing: $projDataSrc"
  }

  Copy-Item -Path $gdalDataSrc -Destination $ShareDstDir -Recurse -Force
  Copy-Item -Path $projDataSrc -Destination $ShareDstDir -Recurse -Force
}

$osgeoRoot = Resolve-OsgeoRoot
$binSrcDir = Join-Path $osgeoRoot "bin"
if (-not (Test-Path $binSrcDir)) {
  throw "OSGeo4W bin directory missing: $binSrcDir"
}

Reset-OutputDir -Path $resGdalDir
New-Item -Path $binOutDir -ItemType Directory | Out-Null
New-Item -Path $shareOutDir -ItemType Directory | Out-Null

Copy-RequiredTools -BinSrcDir $binSrcDir -BinDstDir $binOutDir
Copy-RuntimeDependencies -BinSrcDir $binSrcDir -BinDstDir $binOutDir
Copy-DataDirectories -OsgeoRoot $osgeoRoot -ShareDstDir $shareOutDir

Write-Host "[bundle-gdal-win] bundled runtime at $resGdalDir" -ForegroundColor Green
