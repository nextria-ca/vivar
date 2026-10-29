# Installs vivar (and uv, which it drives today) on Windows.
#
#   irm https://raw.githubusercontent.com/nextria-ca/vivar/main/install.ps1 | iex
#
# While the repo is private, fetch the script with gh instead:
#   gh api repos/nextria-ca/vivar/contents/install.ps1 -H "Accept: application/vnd.github.raw" | iex
#
# Offline: extract a vivar-offline-<target>.zip release bundle and run the copy of this
# script inside it. It installs the bundled vivar, uv and Python runtime without touching
# the network.
#
# Environment:
#   VIVAR_VERSION      release tag to install (default: latest)
#   VIVAR_INSTALL_DIR  where to put the binaries (default: %USERPROFILE%\.local\bin)
#   VIVAR_ARCHIVE      local file or URL of a release zip to install instead
#   GITHUB_TOKEN       used for private-repo downloads when gh isn't logged in
#   VIVAR_SKIP_UV=1    don't install uv
#   VIVAR_SKIP_PATH=1  don't add the install dir to the user PATH
#   VIVAR_POLICY_URL   org policy file to install as %APPDATA%\vivar\policy.toml
#                      (an offline bundle may carry one as policy.toml next to this script)
$ErrorActionPreference = "Stop"

function Test-GhLogin {
  if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { return $false }
  & gh auth status 2>$null | Out-Null
  return ($LASTEXITCODE -eq 0)
}

$Repo = "nextria-ca/vivar"
$Version = if ($env:VIVAR_VERSION) { $env:VIVAR_VERSION } else { "latest" }
$Dir = if ($env:VIVAR_INSTALL_DIR) { $env:VIVAR_INSTALL_DIR } else { Join-Path $env:USERPROFILE ".local\bin" }
$Here = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
$Offline = (Test-Path (Join-Path $Here "vivar.exe")) -and (Test-Path (Join-Path $Here "uv.exe"))

$arch = $env:PROCESSOR_ARCHITECTURE
$Target = switch ($arch) {
  "AMD64" { "x86_64-pc-windows-msvc" }
  "ARM64" { "aarch64-pc-windows-msvc" }
  default { throw "vivar: unsupported architecture $arch" }
}
$Asset = "vivar-$Target.zip"

New-Item -ItemType Directory -Path $Dir -Force | Out-Null
if ($Offline) {
  Write-Host "installing from offline bundle $Here"
  Copy-Item (Join-Path $Here "vivar.exe") (Join-Path $Dir "vivar.exe") -Force
} else {
  $Tmp = Join-Path ([IO.Path]::GetTempPath()) ("vivar-install-" + [IO.Path]::GetRandomFileName())
  New-Item -ItemType Directory -Path $Tmp | Out-Null
  try {
    $zip = Join-Path $Tmp $Asset
    if ($env:VIVAR_ARCHIVE) {
      if ($env:VIVAR_ARCHIVE -match "^https?://") { Invoke-WebRequest $env:VIVAR_ARCHIVE -OutFile $zip }
      else { Copy-Item $env:VIVAR_ARCHIVE $zip }
    } elseif (Test-GhLogin) {
      $ghArgs = @("release", "download")
      if ($Version -ne "latest") { $ghArgs += $Version }
      $ghArgs += @("--repo", $Repo, "--pattern", $Asset, "--dir", $Tmp)
      & gh @ghArgs
      if ($LASTEXITCODE -ne 0) { throw "gh release download failed" }
    } else {
      $api = if ($Version -eq "latest") { "https://api.github.com/repos/$Repo/releases/latest" } else { "https://api.github.com/repos/$Repo/releases/tags/$Version" }
      $headers = @{ "User-Agent" = "vivar-install" }
      if ($env:GITHUB_TOKEN) { $headers["Authorization"] = "Bearer $env:GITHUB_TOKEN" }
      $release = Invoke-RestMethod $api -Headers $headers
      $assetInfo = $release.assets | Where-Object { $_.name -eq $Asset }
      if (-not $assetInfo) { throw "vivar: $Asset not found in release $Version of $Repo (private repo? log in with gh or set GITHUB_TOKEN)" }
      $headers["Accept"] = "application/octet-stream"
      Invoke-WebRequest $assetInfo.url -Headers $headers -OutFile $zip
    }
    Expand-Archive $zip -DestinationPath $Tmp -Force
    Copy-Item (Join-Path $Tmp "vivar-$Target\vivar.exe") (Join-Path $Dir "vivar.exe") -Force
  } finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
  }
}
Write-Host "installed $(& (Join-Path $Dir 'vivar.exe') --version) to $Dir\vivar.exe"

if (-not $env:VIVAR_SKIP_UV -and -not (Get-Command uv -ErrorAction SilentlyContinue)) {
  Write-Host "installing uv (vivar drives it under the hood)"
  if ($Offline) { Copy-Item (Join-Path $Here "uv.exe") (Join-Path $Dir "uv.exe") -Force }
  else { Invoke-RestMethod https://astral.sh/uv/install.ps1 | Invoke-Expression }
}

# Bundled Python runtimes go where uv keeps its managed ones, so `vivar init` finds them.
$pythons = Join-Path $Here "python"
if (Test-Path $pythons) {
  $uv = Get-Command uv -ErrorAction SilentlyContinue
  $uv = if ($uv) { $uv.Source } else { Join-Path $Dir "uv.exe" }
  if (-not (Test-Path $uv)) { $uv = Join-Path $env:USERPROFILE ".local\bin\uv.exe" }
  $pyDir = (& $uv python dir).Trim()
  New-Item -ItemType Directory -Path $pyDir -Force | Out-Null
  foreach ($p in Get-ChildItem $pythons -Directory) {
    $dest = Join-Path $pyDir $p.Name
    if (Test-Path $dest) { Write-Host "python $($p.Name) already installed" }
    else { Copy-Item $p.FullName $dest -Recurse; Write-Host "installed python $($p.Name) to $pyDir" }
  }
}

# Org policy: from the bundle, or from a URL given by whoever rolls vivar out.
$bundledPolicy = Join-Path $Here "policy.toml"
if ((Test-Path $bundledPolicy) -or $env:VIVAR_POLICY_URL) {
  $conf = Join-Path $env:APPDATA "vivar"
  New-Item -ItemType Directory -Path $conf -Force | Out-Null
  $dest = Join-Path $conf "policy.toml"
  if (Test-Path $dest) { Write-Host "keeping existing $dest" }
  elseif (Test-Path $bundledPolicy) { Copy-Item $bundledPolicy $dest; Write-Host "installed org policy to $dest" }
  else {
    $tmpPolicy = "$dest.tmp"
    try { Invoke-WebRequest $env:VIVAR_POLICY_URL -OutFile $tmpPolicy }
    catch { Remove-Item $tmpPolicy -ErrorAction SilentlyContinue; throw "vivar: could not download the org policy from $env:VIVAR_POLICY_URL" }
    Move-Item $tmpPolicy $dest -Force
    Write-Host "installed org policy to $dest"
  }
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not $env:VIVAR_SKIP_PATH -and (($userPath -split ";") -notcontains $Dir)) {
  [Environment]::SetEnvironmentVariable("Path", "$Dir;$userPath", "User")
  $env:Path = "$Dir;$env:Path"
  Write-Host "added $Dir to your user PATH (open a new terminal to pick it up)"
}
