<#
.SYNOPSIS
Upgrades an existing portable Spotifast Mix copy to the update-enabled build.
.DESCRIPTION
Run once for the original update-disabled preview. The initial bootstrap trusts
this script and this repository's release over HTTPS, then checks the published
SHA-256. Subsequent updates in the application require its embedded publisher's
Ed25519 signature. No credential-store entries are read and no app is launched.

Quit Spotifast Mix from its tray menu when prompted. The script never terminates
it. Only the executable is replaced. For the original preview (0.14.2 or older),
the existing update-check preference is enabled once; all other settings, saved
accounts and playlists stay in place. A backup executable is retained.
.EXAMPLE
.\bootstrap-mix.ps1 -WhatIf
.EXAMPLE
.\bootstrap-mix.ps1 -InstallDirectory 'C:\Apps\Spotifast Mix'
#>
[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string]$InstallDirectory,
    [ValidateRange(1, 3600)]
    [int]$WaitSeconds = 300
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repository = 'rotwithme/spotifast-mix'
$command = 'spotifast-mix'
$maximumArchive = 256MB
$maximumExecutable = 256MB

function Get-MixProcesses([string]$Executable) {
    @(Get-CimInstance Win32_Process -Filter "Name='spotifast-mix.exe'" | Where-Object {
        $_.ExecutablePath -and (
            -not $Executable -or [IO.Path]::GetFullPath($_.ExecutablePath).Equals(
                $Executable, [StringComparison]::OrdinalIgnoreCase))
    })
}

function Assert-RegularFile([string]$Path) {
    $item = Get-Item -LiteralPath $Path
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Expected a regular file: $Path"
    }
}

function Get-MixIdentity([string]$Path) {
    Assert-RegularFile $Path
    $info = [Diagnostics.FileVersionInfo]::GetVersionInfo($Path)
    if ($info.ProductName -cne 'Spotifast Mix' -or $info.FileDescription -cne 'Spotifast Mix') {
        throw 'This executable is not identified as Spotifast Mix. An upstream installation will not be changed.'
    }
    if ($info.ProductVersion -notmatch '^((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))(?:\.0)?$') {
        throw 'Cannot identify this executable as a stable version of Spotifast Mix.'
    }
    $version = $Matches[1]
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($Path))
    try {
        if ($reader.BaseStream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5a4d) {
            throw 'The executable has no valid Windows header.'
        }
        $reader.BaseStream.Position = 0x3c
        $offset = $reader.ReadUInt32()
        if ($offset -gt $reader.BaseStream.Length - 6) { throw 'Invalid Windows executable header.' }
        $reader.BaseStream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x00004550) { throw 'Invalid Windows executable signature.' }
        $target = switch ($reader.ReadUInt16()) {
            0x8664 { 'x86_64-pc-windows-msvc' }
            0xaa64 { 'aarch64-pc-windows-msvc' }
            default { throw 'This Windows processor architecture has no supported update package.' }
        }
    } finally { $reader.Dispose() }
    [pscustomobject]@{ Version = [version]$version; Text = $version; Target = $target }
}

function Read-ReleaseBytes([string]$Address, [long]$Maximum, [string]$Accept) {
    $current = [uri]$Address
    for ($hop = 0; $hop -le 5; $hop++) {
        if ($current.Scheme -cne 'https' -or -not $current.IsDefaultPort -or $current.UserInfo -or
            $current.Fragment -or $current.Host.ToLowerInvariant() -notin @(
                'api.github.com', 'github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com')) {
            throw 'The release download redirected outside the permitted HTTPS hosts.'
        }
        $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::Get, $current)
        $null = $request.Headers.TryAddWithoutValidation('User-Agent', 'SpotifastMixBootstrap/1')
        $null = $request.Headers.TryAddWithoutValidation('Accept', $Accept)
        $response = $null
        try {
            $response = $http.SendAsync($request, [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            $status = [int]$response.StatusCode
            if ($status -ge 300 -and $status -lt 400) {
                if (-not $response.Headers.Location) { throw 'The release redirect has no destination.' }
                $current = [uri]::new($current, $response.Headers.Location)
                continue
            }
            if ($status -ne 200) { throw "GitHub could not provide this release file (HTTP $status)." }
            if ($response.Content.Headers.ContentLength -gt $Maximum) { throw 'The release response exceeds the permitted size.' }
            $inputStream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
            $output = [IO.MemoryStream]::new()
            try {
                $buffer = [byte[]]::new(65536)
                while (($count = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                    if ($output.Length + $count -gt $Maximum) { throw 'The release response exceeds the permitted size.' }
                    $output.Write($buffer, 0, $count)
                }
                return ,$output.ToArray()
            } finally { $inputStream.Dispose(); $output.Dispose() }
        } finally {
            if ($response) { $response.Dispose() }
            $request.Dispose()
        }
    }
    throw 'The release download has too many redirects.'
}

function Get-ReleaseAsset([object]$Release, [string]$Name, [string]$Tag, [long]$Maximum) {
    $found = @($Release.assets | Where-Object { $_.name -ceq $Name })
    if ($found.Count -ne 1) { throw "The latest release does not have exactly one $Name asset." }
    $asset = $found[0]
    $expected = "https://github.com/$repository/releases/download/$Tag/$Name"
    if ($asset.browser_download_url -cne $expected -or [long]$asset.size -le 0 -or [long]$asset.size -gt $Maximum) {
        throw "The release metadata for $Name is invalid."
    }
    $asset
}

function Write-DurableBytes([string]$Path, [byte[]]$Bytes) {
    $output = [IO.FileStream]::new($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write,
        [IO.FileShare]::None, 65536, [IO.FileOptions]::WriteThrough)
    try { $output.Write($Bytes, 0, $Bytes.Length); $output.Flush($true) }
    finally { $output.Dispose() }
}

function Expand-MixEntry([IO.Compression.ZipArchive]$Archive, [string]$Name, [string]$Destination, [long]$Maximum) {
    $entries = @($Archive.Entries | Where-Object { $_.FullName -ceq $Name })
    if ($entries.Count -ne 1 -or $entries[0].Length -le 0 -or $entries[0].Length -gt $Maximum) {
        throw "The release archive has an invalid $Name member."
    }
    $fullDestination = [IO.Path]::GetFullPath($Destination)
    if (-not $fullDestination.StartsWith($scratchPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Archive extraction escaped its temporary directory.'
    }
    # Never extract archive-selected paths. Only these two exact members are read.
    $inputStream = $entries[0].Open()
    $output = [IO.FileStream]::new($fullDestination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write,
        [IO.FileShare]::None, 65536, [IO.FileOptions]::WriteThrough)
    try {
        $buffer = [byte[]]::new(65536)
        [long]$written = 0
        while (($count = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            $written += $count
            if ($written -gt $Maximum -or $written -gt $entries[0].Length) { throw 'The archive member exceeds its declared size.' }
            $output.Write($buffer, 0, $count)
        }
        if ($written -ne $entries[0].Length) { throw 'The archive member is incomplete.' }
        $output.Flush($true)
    } finally { $inputStream.Dispose(); $output.Dispose() }
}

if (-not $InstallDirectory) {
    $running = @(Get-MixProcesses '')
    $directories = @($running | ForEach-Object { [IO.Path]::GetDirectoryName($_.ExecutablePath) } | Sort-Object -Unique)
    if ($directories.Count -ne 1) {
        throw 'Specify -InstallDirectory for the portable copy to update, or leave exactly one copy running so its folder can be identified.'
    }
    $InstallDirectory = $directories[0]
}
$resolvedDirectory = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $InstallDirectory).ProviderPath)
if ($resolvedDirectory -eq [IO.Path]::GetPathRoot($resolvedDirectory)) {
    throw 'Choose an application folder rather than the root of a drive.'
}
$directory = $resolvedDirectory.TrimEnd('\')
if ((Get-Item -LiteralPath $directory).Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw 'Use the actual installation directory, rather than a linked directory.'
}
$executable = Join-Path $directory "$command.exe"
$marker = Join-Path $directory "$command-portable.txt"
Assert-RegularFile $marker
if ([IO.File]::ReadAllText($marker).Trim() -cne 'spotifast-mix-portable-v1') {
    throw 'This directory has no valid Spotifast Mix portable marker.'
}
$installed = Get-MixIdentity $executable
$originalHash = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash

Add-Type -AssemblyName System.Net.Http
Add-Type -AssemblyName System.IO.Compression
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$handler = [Net.Http.HttpClientHandler]::new()
$handler.AllowAutoRedirect = $false
$http = [Net.Http.HttpClient]::new($handler)
$http.Timeout = [TimeSpan]::FromMinutes(15)
$scratch = $null
$lock = $null
$lockPath = Join-Path $directory '.spotifast-mix-bootstrap.lock'
$binaryBackup = Join-Path $directory ("spotifast-mix-before-bootstrap-" + [guid]::NewGuid().ToString('N') + '.exe')
$settingsBackup = $null
$settingsTemporary = $null
$binaryReplaced = $false
$settingsReplaced = $false
try {
    [byte[]]$metadata = Read-ReleaseBytes "https://api.github.com/repos/$repository/releases/latest" 4MB 'application/vnd.github+json'
    $release = [Text.Encoding]::UTF8.GetString($metadata) | ConvertFrom-Json
    if ($release.draft -or $release.prerelease -or
        $release.tag_name -notmatch '^v((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))$') {
        throw 'The latest release is not a stable Spotifast Mix version.'
    }
    $version = $Matches[1]
    $tag = "v$version"
    if ($release.html_url -cne "https://github.com/$repository/releases/tag/$tag") { throw 'The release belongs to a different repository.' }
    if ([version]$version -le $installed.Version) {
        Write-Output "This copy is already at version $($installed.Text) or newer than the latest stable release."
        return
    }
    $stem = "$command-v$version-$($installed.Target)"
    $packageName = "$stem.zip"
    $package = Get-ReleaseAsset $release $packageName $tag $maximumArchive
    $manifest = Get-ReleaseAsset $release 'checksums.txt' $tag 1MB
    if (-not $PSCmdlet.ShouldProcess($executable, "Install Spotifast Mix $version after it quits, preserving its data")) { return }
    $lock = [IO.FileStream]::new($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $scratch = Join-Path $directory ('.spotifast-mix-bootstrap-' + [guid]::NewGuid().ToString('N'))
    $null = [IO.Directory]::CreateDirectory($scratch)
    $scratchPrefix = [IO.Path]::GetFullPath($scratch).TrimEnd('\') + '\'
    [byte[]]$checksums = Read-ReleaseBytes $manifest.browser_download_url 1MB 'application/octet-stream'
    if ($checksums.LongLength -ne [long]$manifest.size) { throw 'The checksum download is incomplete.' }
    $checksumText = [Text.UTF8Encoding]::new($false, $true).GetString($checksums)
    $pattern = '(?m)^([a-fA-F0-9]{64})[ \t]+\*?' + [regex]::Escape($packageName) + '[ \t]*\r?$'
    $matchesForPackage = [regex]::Matches($checksumText, $pattern)
    if ($matchesForPackage.Count -ne 1) { throw 'The checksum manifest does not identify this package exactly once.' }
    [byte[]]$archiveBytes = Read-ReleaseBytes $package.browser_download_url $maximumArchive 'application/octet-stream'
    if ($archiveBytes.LongLength -ne [long]$package.size) { throw 'The package download is incomplete.' }
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try { $digest = [BitConverter]::ToString($sha256.ComputeHash($archiveBytes)).Replace('-', '').ToLowerInvariant() }
    finally { $sha256.Dispose() }
    if ($digest -cne $matchesForPackage[0].Groups[1].Value.ToLowerInvariant()) { throw 'The downloaded package failed its SHA-256 check.' }
    $archiveStream = [IO.MemoryStream]::new($archiveBytes, $false)
    $archive = [IO.Compression.ZipArchive]::new($archiveStream, [IO.Compression.ZipArchiveMode]::Read)
    $replacement = Join-Path $scratch "$command.exe"
    $newMarker = Join-Path $scratch "$command-portable.txt"
    try {
        if ($archive.Entries.Count -gt 1000) { throw 'The release archive has too many members.' }
        Expand-MixEntry $archive "$stem/$command.exe" $replacement $maximumExecutable
        Expand-MixEntry $archive "$stem/$command-portable.txt" $newMarker 1024
    } finally { $archive.Dispose(); $archiveStream.Dispose() }
    if ([IO.File]::ReadAllText($newMarker).Trim() -cne 'spotifast-mix-portable-v1') { throw 'The replacement portable marker is invalid.' }
    $replacementIdentity = Get-MixIdentity $replacement
    if ($replacementIdentity.Text -cne $version -or $replacementIdentity.Target -cne $installed.Target) {
        throw 'The replacement executable has a different version or processor architecture.'
    }
    Write-Host 'Quit Spotifast Mix from its tray menu. Waiting for this copy to exit; it will not be terminated or restarted.'
    $deadline = [DateTime]::UtcNow.AddSeconds($WaitSeconds)
    while (@(Get-MixProcesses $executable).Count -gt 0) {
        if ([DateTime]::UtcNow -ge $deadline) { throw 'The app is still running. Quit it from the tray and run the bootstrap again.' }
        Start-Sleep -Milliseconds 500
    }
    if ((Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash -cne $originalHash) {
        throw 'The installed executable changed while preparing this update. Run the bootstrap again.'
    }

    $settings = Join-Path $directory 'spotifast-mix-data\config\settings.json'
    if ($installed.Version -le [version]'0.14.2' -and (Test-Path -LiteralPath $settings)) {
        foreach ($path in @((Split-Path $settings -Parent), (Join-Path $directory 'spotifast-mix-data'))) {
            if ((Get-Item -LiteralPath $path).Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw 'The portable settings directory is linked. Enable update checks in Settings instead.'
            }
        }
        Assert-RegularFile $settings
        if ((Get-Item -LiteralPath $settings).Length -gt 4MB) { throw 'The settings file is too large to migrate safely.' }
        $text = [IO.File]::ReadAllText($settings)
        try { $preferences = $text | ConvertFrom-Json }
        catch { throw 'The portable settings are unreadable. No update-check preference will be overwritten.' }
        if ($preferences -isnot [pscustomobject]) { throw 'The portable settings must be a JSON object.' }
        $property = $preferences.PSObject.Properties['check_for_updates']
        if ($property -and $property.Value -isnot [bool]) { throw 'The update-check preference is not a boolean.' }
        if ($property -and -not $property.Value) {
            # Preserve the rest of the JSON text. A duplicate or nested
            # lookalike is refused rather than risking an unrelated preference.
            $keyPattern = '"check_for_updates"\s*:'
            $valuePattern = '("check_for_updates"\s*:\s*)false(?=\s*[,}])'
            if ([regex]::Matches($text, $keyPattern).Count -ne 1 -or [regex]::Matches($text, $valuePattern).Count -ne 1) {
                throw 'The settings have an ambiguous update-check preference. Enable it in Settings instead.'
            }
            $updated = [regex]::Replace($text, $valuePattern, '${1}true')
            $settingsTemporary = Join-Path (Split-Path $settings -Parent) ('.settings-bootstrap-' + [guid]::NewGuid().ToString('N') + '.json')
            $settingsBackup = $settingsTemporary + '.previous'
            Write-DurableBytes $settingsTemporary ([Text.UTF8Encoding]::new($false).GetBytes($updated))
        }
    }
    try {
        [IO.File]::Replace($replacement, $executable, $binaryBackup)
        $binaryReplaced = $true
        if ($settingsTemporary) {
            [IO.File]::Replace($settingsTemporary, $settings, $settingsBackup)
            $settingsReplaced = $true
        }
    } catch {
        if ($settingsReplaced) { [IO.File]::Replace($settingsBackup, $settings, $null) }
        if ($binaryReplaced) { [IO.File]::Replace($binaryBackup, $executable, $null) }
        throw
    } finally {
        if ($settingsTemporary -and (Test-Path -LiteralPath $settingsTemporary)) { Remove-Item -LiteralPath $settingsTemporary -Force }
    }
    if ($settingsBackup -and (Test-Path -LiteralPath $settingsBackup)) {
        Remove-Item -LiteralPath $settingsBackup -Force -ErrorAction SilentlyContinue
    }
    Write-Output "Updated to Spotifast Mix $version. Open the app when ready. Future updates use its update icon."
    Write-Output "Previous executable: $binaryBackup"
} finally {
    $http.Dispose()
    try {
        if ($settingsTemporary -and (Test-Path -LiteralPath $settingsTemporary)) {
            Remove-Item -LiteralPath $settingsTemporary -Force -ErrorAction SilentlyContinue
        }
        if ($scratch -and (Test-Path -LiteralPath $scratch)) {
            $fullScratch = [IO.Path]::GetFullPath($scratch)
            $expectedPrefix = $directory.TrimEnd('\') + '\.spotifast-mix-bootstrap-'
            if (-not $fullScratch.StartsWith($expectedPrefix, [StringComparison]::OrdinalIgnoreCase) -or
                ((Get-Item -LiteralPath $fullScratch).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                throw 'Refusing to remove an unexpected bootstrap directory.'
            }
            Remove-Item -LiteralPath $fullScratch -Recurse -Force
        }
    } finally {
        if ($lock) {
            $lock.Dispose()
            Remove-Item -LiteralPath $lockPath -Force -ErrorAction SilentlyContinue
        }
    }
}
