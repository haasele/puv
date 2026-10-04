# Install the latest PUV release from github.com/haasele/puv.
# Usage: irm https://raw.githubusercontent.com/haasele/puv/main/install.ps1 | iex
& {
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = 'haasele/puv'

function Fail([string]$Message) {
    throw $Message
}

if ($env:OS -ne 'Windows_NT') {
    Fail "install.ps1 is for Windows. On Linux and macOS: curl -fsSL https://raw.githubusercontent.com/haasele/puv/main/install.sh | sh"
}

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {
}

function Get-MachineArch {
    $osArch = $null
    try {
        $osArch = (Get-CimInstance -ClassName Win32_OperatingSystem).OSArchitecture
    } catch {
    }
    if ($osArch -match 'ARM') {
        return 'aarch64'
    }
    if ($osArch -match '64') {
        return 'x86_64'
    }
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) {
        $arch = $env:PROCESSOR_ARCHITECTURE
    }
    switch ($arch) {
        'AMD64' { return 'x86_64' }
        'ARM64' { return 'aarch64' }
        default { Fail "Unsupported architecture: $arch" }
    }
}

function Get-Sha256Line([string]$Text, [string]$Name) {
    foreach ($line in ($Text -split '\r?\n')) {
        if ($line -match '^([0-9a-fA-F]{64})  (\S+)$' -and $Matches[2] -eq $Name) {
            return $Matches[1].ToLowerInvariant()
        }
    }
    return $null
}

function Save-Url([string]$Uri, [string]$OutFile) {
    $last = $null
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
            Invoke-WebRequest -Uri $Uri -OutFile $OutFile -UseBasicParsing
            return
        } catch {
            $last = $_
            if ($attempt -lt 3) {
                Start-Sleep -Seconds $attempt
            }
        }
    }
    throw $last
}

function Test-SamePath([string]$Left, [string]$Right) {
    $Left.TrimEnd('\').Equals($Right.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)
}

function Add-UserPath([string]$Directory) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @()
    if ($userPath) {
        $parts = $userPath -split ';' | Where-Object { $_ }
    }
    $found = $false
    foreach ($part in $parts) {
        if (Test-SamePath $part $Directory) {
            $found = $true
            break
        }
    }
    if (-not $found) {
        if ($userPath) {
            $updated = "$userPath;$Directory"
        } else {
            $updated = $Directory
        }
        [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
        Write-Host "Added $Directory to the user PATH. Open a new terminal to use puv."
    }
    $session = $env:Path -split ';' | Where-Object { $_ -and (Test-SamePath $_ $Directory) }
    if (-not $session) {
        $env:Path = "$env:Path;$Directory"
    }
}

$arch = Get-MachineArch
$target = "$arch-pc-windows-msvc"
$binName = 'puv.exe'
$asset = "puv-$target.tar.gz"
$url = "https://github.com/$Repo/releases/latest/download/$asset"
$sumsUrl = "https://github.com/$Repo/releases/latest/download/sha256sums.txt"
$source = "git clone https://github.com/$Repo && cargo install --path puv/crates/puv --locked"

if ($env:PUV_INSTALL_DIR) {
    $dest = $env:PUV_INSTALL_DIR
} else {
    $dest = Join-Path $env:USERPROFILE '.local\bin'
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("puv-install-" + [guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    $archive = Join-Path $tmp 'asset.tar.gz'
    Write-Host "Fetching the latest release asset: $asset"
    try {
        Save-Url $url $archive
    } catch {
        Fail "No $asset on the latest GitHub release of $Repo. Build from source: $source"
    }

    $sumsFile = Join-Path $tmp 'sha256sums.txt'
    try {
        Invoke-WebRequest -Uri $sumsUrl -OutFile $sumsFile -UseBasicParsing
        $expected = Get-Sha256Line (Get-Content -Raw -Path $sumsFile) $asset
        if ($expected) {
            $actual = (Get-FileHash -Algorithm SHA256 -Path $archive).Hash.ToLowerInvariant()
            if ($actual -ne $expected) {
                Fail "Checksum mismatch for $asset"
            }
            Write-Host "Checksum matched sha256sums.txt"
        }
    } catch {
        if ($_.Exception.Message -match 'Checksum mismatch') {
            throw
        }
    }

    $tar = Get-Command tar.exe -ErrorAction SilentlyContinue
    if (-not $tar) {
        $tar = Get-Command tar -ErrorAction SilentlyContinue
    }
    if (-not $tar) {
        Fail "tar is required (included with Windows 10 and later)"
    }
    & $tar.Source -xzf $archive -C $tmp
    if ($LASTEXITCODE -ne 0) {
        Fail "failed to extract $asset"
    }

    $bin = Get-ChildItem -Path $tmp -Recurse -Filter $binName -File | Select-Object -First 1
    if (-not $bin) {
        Fail "Archive did not contain a file named $binName"
    }

    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    $installed = Join-Path $dest $binName
    Copy-Item -Force -Path $bin.FullName -Destination $installed
    Write-Host "Installed $installed"
    Add-UserPath $dest
    & $installed --version
    if ($LASTEXITCODE -ne 0) {
        Fail "installed $installed, but it did not run"
    }
} finally {
    if (Test-Path $tmp) {
        Remove-Item -Recurse -Force $tmp
    }
}
}
