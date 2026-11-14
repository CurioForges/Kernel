# Gitea release helpers for Actions runners.
# Usage: . ci/gitea-release.ps1 -Repo 'atelier-engines'
# Every function accepts -Repo 'Owner/name' to act on another repository.
# Env: GITEA_TOKEN (auto-injected), GITEA_URL (optional)
param(
  [string]$Repo = $(if ($env:GITHUB_REPOSITORY) { $env:GITHUB_REPOSITORY } else { 'Atelier/atelier-engines' }),
  [string]$BaseUrl = $(if ($env:GITEA_URL) { $env:GITEA_URL } else { 'https://atelier.hurcaguari.top' }),
  [string]$Token = $(if ($env:GITEA_TOKEN) { $env:GITEA_TOKEN } else { $env:GITHUB_TOKEN })
)

if ($Repo -notmatch '/') { $Repo = "Atelier/$Repo" }
$script:DefaultRepo = $Repo
$script:BaseUrl = $BaseUrl
$script:Headers = @{ Authorization = "token $Token" }

function Get-GiteaApi {
  param([string]$RepoFull)
  if (-not $RepoFull) { $RepoFull = $script:DefaultRepo }
  if ($RepoFull -notmatch '/') { $RepoFull = "Atelier/$RepoFull" }
  return "$script:BaseUrl/api/v1/repos/$RepoFull"
}

function Gitea-GetRelease {
  param([string]$Tag, [string]$Repo)
  $api = Get-GiteaApi $Repo
  try {
    return Invoke-RestMethod -Uri "$api/releases/tags/$([uri]::EscapeDataString($Tag))" -Headers $script:Headers -TimeoutSec 30
  } catch {
    $code = 0
    if ($_.Exception.Response) {
      try { $code = [int]$_.Exception.Response.StatusCode } catch { $code = 0 }
    } elseif ($_.Exception.PSObject.Properties['StatusCode']) {
      $code = [int]$_.Exception.StatusCode
    }
    if ($code -eq 404) { return $null }
    throw
  }
}

function Gitea-EnsureRelease {
  param([string]$Tag, [string]$Title = 'Atelier domain artifacts', [string]$Notes = '', [switch]$Prerelease, [string]$Repo)
  $rel = Gitea-GetRelease -Tag $Tag -Repo $Repo
  if ($rel) { return $rel }
  $api = Get-GiteaApi $Repo
  $body = @{
    tag_name = $Tag
    name = $Title
    body = $Notes
    draft = $false
    prerelease = [bool]$Prerelease
    target_commitish = 'master'
  } | ConvertTo-Json -Compress
  return Invoke-RestMethod -Uri "$api/releases" -Method Post -Headers $script:Headers `
    -ContentType 'application/json; charset=utf-8' -Body ([Text.Encoding]::UTF8.GetBytes($body)) -TimeoutSec 60
}

function Gitea-ReleaseAssetNames {
  param([string]$Tag, [string]$Repo)
  $rel = Gitea-GetRelease -Tag $Tag -Repo $Repo
  if (-not $rel) { return @() }
  return @($rel.assets | ForEach-Object { $_.name })
}

function Gitea-DeleteAsset {
  param([string]$Tag, [string]$Name, [string]$Repo)
  $rel = Gitea-GetRelease -Tag $Tag -Repo $Repo
  if (-not $rel) { return }
  $asset = $rel.assets | Where-Object { $_.name -eq $Name }
  if ($asset) {
    $api = Get-GiteaApi $Repo
    Invoke-RestMethod -Uri "$api/releases/$($rel.id)/assets/$($asset.id)" -Method Delete -Headers $script:Headers -TimeoutSec 30 | Out-Null
  }
}

function Gitea-UploadAsset {
  param([string]$Tag, [string]$Path, [switch]$Clobber, [string]$Repo)
  $rel = Gitea-EnsureRelease -Tag $Tag -Repo $Repo
  $api = Get-GiteaApi $Repo
  $name = Split-Path -Leaf $Path
  if ($Clobber) { Gitea-DeleteAsset -Tag $Tag -Name $name -Repo $Repo }
  $bytes = [System.IO.File]::ReadAllBytes((Resolve-Path $Path).Path)
  $boundary = [guid]::NewGuid().ToString()
  $ms = New-Object System.IO.MemoryStream
  $enc = [Text.Encoding]::UTF8
  $head = "--$boundary`r`nContent-Disposition: form-data; name=`"attachment`"; filename=`"$name`"`r`nContent-Type: application/octet-stream`r`n`r`n"
  $tail = "`r`n--$boundary--`r`n"
  $hb = $enc.GetBytes($head); $tb = $enc.GetBytes($tail)
  $ms.Write($hb, 0, $hb.Length); $ms.Write($bytes, 0, $bytes.Length); $ms.Write($tb, 0, $tb.Length)
  return Invoke-RestMethod -Uri "$api/releases/$($rel.id)/assets?name=$([uri]::EscapeDataString($name))" `
    -Method Post -Headers $script:Headers -ContentType "multipart/form-data; boundary=$boundary" `
    -Body $ms.ToArray() -TimeoutSec 1800
}

function Gitea-DownloadAsset {
  param([string]$Tag, [string]$Name, [string]$OutDir = '.', [string]$Repo)
  $rel = Gitea-GetRelease -Tag $Tag -Repo $Repo
  if (-not $rel) { throw "release $Tag not found in $Repo" }
  $asset = $rel.assets | Where-Object { $_.name -eq $Name }
  if (-not $asset) { throw "asset $Name not found in $Tag of $Repo" }
  New-Item -ItemType Directory -Force $OutDir | Out-Null
  $out = Join-Path $OutDir $Name
  $url = if ($asset.browser_download_url) { $asset.browser_download_url } else { "$(Get-GiteaApi $Repo)/releases/$($rel.id)/assets/$($asset.id)" }
  try {
    Invoke-WebRequest -Uri $url -Headers $script:Headers -OutFile $out -TimeoutSec 1800
  } catch {
    Invoke-WebRequest -Uri "$url`?token=$Token" -OutFile $out -TimeoutSec 1800
  }
  return $out
}
