<#
.SYNOPSIS
  Fetch the ffmpeg shared build this project links and embeds.

.DESCRIPTION
  The binaries are not in the repository, for two reasons that both get worse
  with time:

    Size.       A full shared build is ~149 MB. Git keeps every blob forever, so
                committing it once puts it in the history permanently — deleting
                it later frees nothing, and rewriting history on a shared
                repository is its own problem.

    Licensing.  These are third-party binaries under the LGPL (or GPL, depending
                how the build was configured). Redistributing them carries
                obligations — license notices, an offer of corresponding source.
                Fetching them instead means this project never redistributes
                them at all, which sidesteps the question rather than answering
                it badly.

  The whole build is fetched rather than only the three DLLs currently embedded,
  because the codec set is still growing: AV1 is already stubbed in the decode
  shim, and pruning now would only mean fetching again later.

.NOTES
  Run from anywhere; paths are resolved relative to this script.
#>

[CmdletBinding()]
param(
    # Set once, then left alone. A moving "latest" URL would make every clone a
    # different build, which is the opposite of what pinning is for.
    [string]$Url = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl-shared.zip',

    # SHA-256 of the archive above.
    #
    # Deliberately empty rather than filled with a plausible-looking value: a
    # wrong hash that nobody notices is worse than no hash, because it looks
    # like verification is happening. Run this script once, it will print the
    # hash of what it downloaded, then paste it here and commit.
    [string]$Sha256 = '',

    # Re-download even if the files are already present.
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$root   = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$target = Join-Path $root 'third_party\ffmpeg'
$marker = Join-Path $target 'bin\avcodec-61.dll'

if ((Test-Path $marker) -and -not $Force) {
    Write-Host "ffmpeg already present at $target"
    exit 0
}

$temp    = Join-Path ([System.IO.Path]::GetTempPath()) "tain-ffmpeg-$([guid]::NewGuid())"
$archive = Join-Path $temp 'ffmpeg.zip'
New-Item -ItemType Directory -Path $temp -Force | Out-Null

try {
    Write-Host "Downloading $Url"
    # The progress bar makes Invoke-WebRequest an order of magnitude slower on
    # large files, and this is a large file.
    $previousProgress = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'
    try {
        Invoke-WebRequest -Uri $Url -OutFile $archive -UseBasicParsing
    } finally {
        $ProgressPreference = $previousProgress
    }

    $actual = (Get-FileHash -Path $archive -Algorithm SHA256).Hash.ToLower()

    if ([string]::IsNullOrWhiteSpace($Sha256)) {
        # Refuses rather than proceeding unverified. Downloading a compiler
        # input over the network and running it without checking what arrived
        # is exactly the step worth being strict about.
        Write-Host ''
        Write-Host 'No checksum is pinned yet. The archive just downloaded has:'
        Write-Host ''
        Write-Host "    $actual"
        Write-Host ''
        Write-Host 'Verify that against the publisher, then set $Sha256 at the'
        Write-Host 'top of this script and commit it. Nothing has been extracted.'
        exit 1
    }

    if ($actual -ne $Sha256.ToLower()) {
        throw "Checksum mismatch.`n  expected $($Sha256.ToLower())`n  got      $actual`nThe download is not the pinned build. Nothing has been extracted."
    }

    Write-Host 'Checksum matches. Extracting.'
    Expand-Archive -Path $archive -DestinationPath $temp -Force

    # Archives from every publisher wrap their contents in one versioned
    # directory, and the name changes with the build, so it is found rather
    # than assumed.
    $inner = Get-ChildItem -Path $temp -Directory |
             Where-Object { Test-Path (Join-Path $_.FullName 'bin') } |
             Select-Object -First 1
    if (-not $inner) {
        throw "The archive does not contain a directory with a bin\ inside it. Layout may have changed."
    }

    if (Test-Path $target) { Remove-Item -Recurse -Force $target }
    New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
    Move-Item -Path $inner.FullName -Destination $target

    if (-not (Test-Path $marker)) {
        throw "Extracted, but $marker is missing. This build may not be a *shared* build — the DLLs are what this project needs, not static libraries."
    }

    Write-Host "ffmpeg ready at $target"
} finally {
    Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
}
