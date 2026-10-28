[CmdletBinding()]
param(
    [switch]$NoBuild,
    [switch]$WithoutMilkDrop,
    [string]$CargoPath = 'cargo',
    [string]$BinaryPath = 'target/release/spotifast-mix.exe',
    [string]$OutputDirectory = 'dist'
)

$ErrorActionPreference = 'Stop'
$workspace = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
Push-Location -LiteralPath $workspace
try {
    if (-not $NoBuild) {
        $buildArguments = @('build', '--release', '--locked', '--bin', 'spotifast-mix')
        if ($WithoutMilkDrop) { $buildArguments += '--no-default-features' }
        & $CargoPath @buildArguments
        if ($LASTEXITCODE -ne 0) { throw 'The release build failed.' }
    }
    $binary = (Resolve-Path -LiteralPath $BinaryPath).Path
    $reportedVersion = (& $binary --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reportedVersion -notmatch '^spotifast-mix (\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)$') {
        throw 'The supplied executable is not a versioned Spotifast Mix build.'
    }
    $version = $Matches[1]
    $output = if ([IO.Path]::IsPathRooted($OutputDirectory)) {
        [IO.Path]::GetFullPath($OutputDirectory)
    } else {
        [IO.Path]::GetFullPath((Join-Path $workspace $OutputDirectory))
    }
    $null = New-Item -ItemType Directory -Force -Path $output
    $packageName = "spotifast-mix-v$version-windows-portable"
    $staging = Join-Path $output ($packageName + '-' + [guid]::NewGuid().ToString('N'))
    $archive = Join-Path $output ($packageName + '.zip')
    if (Test-Path -LiteralPath $archive) { throw "Output already exists: $archive" }
    $null = New-Item -ItemType Directory -Path $staging
    try {
        Copy-Item -LiteralPath $binary -Destination (Join-Path $staging 'spotifast-mix.exe')
        Copy-Item -LiteralPath 'LICENSE' -Destination $staging
        # The portable payload has no source tree. Keep its instructions, but
        # point guide links at the bundled file and retained upstream references.
        $readme = [IO.File]::ReadAllText((Join-Path $workspace 'README.md'))
        $readme = $readme.Replace('(docs/_reference/spotifast-mix.md)', '(<Getting started.md>)')
        $readme = $readme.Replace('(CONTRIBUTING.md)', '(https://github.com/crmne/spotifast/blob/main/CONTRIBUTING.md)')
        $guide = [IO.File]::ReadAllText((Join-Path $workspace 'docs/_reference/spotifast-mix.md'))
        $guide = $guide.Replace('(how-it-connects.md)', '(https://github.com/crmne/spotifast/blob/main/docs/_reference/how-it-connects.md)')
        $utf8 = [Text.UTF8Encoding]::new($false)
        [IO.File]::WriteAllText((Join-Path $staging 'README.md'), $readme, $utf8)
        [IO.File]::WriteAllText((Join-Path $staging 'Getting started.md'), $guide, $utf8)
        # fastframe-dirs uses this marker to select an adjacent private profile.
        # Never package any actual profile, saved credentials, or account data.
        Set-Content -LiteralPath (Join-Path $staging 'spotifast-mix-portable.txt') -Value 'spotifast-mix-portable-v1' -Encoding Ascii
        Compress-Archive -LiteralPath (Get-ChildItem -LiteralPath $staging).FullName -DestinationPath $archive
        $digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        Set-Content -LiteralPath ($archive + '.sha256') -Value "$digest  $([IO.Path]::GetFileName($archive))" -Encoding Ascii
        Write-Output $archive
    }
    finally {
        # Resolve and check the exact scratch directory before recursive removal.
        $resolvedStaging = [IO.Path]::GetFullPath($staging)
        $expectedPrefix = $output.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
        if (-not $resolvedStaging.StartsWith($expectedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Refusing to remove a staging directory outside the package output folder.'
        }
        Remove-Item -LiteralPath $resolvedStaging -Recurse -Force
    }
}
finally { Pop-Location }
