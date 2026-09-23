# Installs termphin-agent where the Termphin app keeps it, or finds the copy
# the app already installed, and puts `termphin` on the user's PATH.
#
#   irm https://github.com/Termphin/termphin-agent/releases/latest/download/install.ps1 | iex
#
# TERMPHIN_BASE_URL points it at another copy of the release assets.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$baseUrl = if ($env:TERMPHIN_BASE_URL) { $env:TERMPHIN_BASE_URL } else {
    'https://github.com/Termphin/termphin-agent/releases/latest/download'
}
$dir = Join-Path $env:USERPROFILE 'AppData\Local\termphin\bin'
$agent = Join-Path $dir 'termphin-agent.exe'

function Get-InstalledVersion {
    if (-not (Test-Path $agent)) { return $null }
    try {
        $line = & $agent version --machine 2>$null | Where-Object { $_ -like 'version=*' }
        if ($line) { return $line.Substring(8) }
    } catch {}
    return $null
}

function Install-Agent {
    if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
        throw "no build for $env:PROCESSOR_ARCHITECTURE"
    }
    $asset = 'termphin-agent-windows-x86_64.exe'
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Invoke-WebRequest -UseBasicParsing "$baseUrl/manifest.properties" -OutFile "$tmp\manifest"
        Invoke-WebRequest -UseBasicParsing "$baseUrl/$asset" -OutFile "$tmp\agent.exe"
        $manifest = @{}
        foreach ($line in Get-Content "$tmp\manifest") {
            $key, $value = $line -split '=', 2
            if ($value) { $manifest[$key] = $value }
        }
        $expected = $manifest['windows_x86_64.sha256']
        if (-not $expected) { throw "the manifest has no checksum for $asset" }
        $actual = (Get-FileHash -Algorithm SHA256 "$tmp\agent.exe").Hash.ToLowerInvariant()
        if ($actual -ne $expected) { throw "$asset does not match its checksum" }

        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        Move-Item -Force "$tmp\agent.exe" $agent
        # The app reads this to decide whether its own copy is current, so it
        # has to describe the binary exactly.
        $meta = "version=$($manifest['version'])`nprotocol=$($manifest['protocol'])`nsha256=$expected`n"
        [IO.File]::WriteAllText((Join-Path $dir 'agent.meta'), $meta)
        Write-Output "installed termphin-agent $($manifest['version'])"
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

$version = Get-InstalledVersion
if ($version) {
    Write-Output "termphin-agent $version is already installed"
} else {
    Install-Agent
}

# Windows will not let an unprivileged user create a symlink, so `termphin`
# is a shim beside the binary.
[IO.File]::WriteAllText((Join-Path $dir 'termphin.cmd'), "@`"%~dp0termphin-agent.exe`" %*`r`n")

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = if ($userPath) { $userPath -split ';' } else { @() }
if ($entries -notcontains $dir) {
    $newPath = (@($entries | Where-Object { $_ }) + $dir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Output "added $dir to your PATH"
    Write-Output 'open a new terminal, then run: termphin'
} else {
    Write-Output 'run: termphin'
}
