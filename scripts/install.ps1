# Downloads Kelpy from the latest GitHub release, checks it against the release's SHA256SUMS and runs it.
# Kelpy's first run then asks where to install itself. Nothing is installed by this script.
#
#   irm https://kelpy.io/install.ps1 | iex
#
# $env:KELPY_VERSION = 'v0.1.0' picks a release. Works on Windows PowerShell 5.1 (Windows Server 2016 and later).

# A script block, so that the preferences set here do not change the session that runs `iex`.
& {
    $ErrorActionPreference = 'Stop'
    # Invoke-WebRequest is many times slower on Windows PowerShell 5.1 while it draws its progress bar.
    $ProgressPreference = 'SilentlyContinue'
    # GitHub requires TLS 1.2, which Windows PowerShell 5.1 does not always offer by default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor
        [Net.SecurityProtocolType]::Tls12

    # A 32-bit PowerShell on 64-bit Windows reports x86 and keeps the real architecture in PROCESSOR_ARCHITEW6432.
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    # Windows on ARM64 runs the x86-64 build.
    if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
        throw "No Kelpy build for the $arch architecture."
    }
    $asset = 'kelpy-windows-x86_64.exe'

    $repo = 'https://github.com/Ryadel/Kelpy/releases'
    $base = if ($env:KELPY_VERSION) { "$repo/download/$env:KELPY_VERSION" } else { "$repo/latest/download" }

    $dir = Join-Path ([IO.Path]::GetTempPath()) ("kelpy-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $dir | Out-Null
    try {
        $exe = Join-Path $dir 'kelpy.exe'
        Write-Host "Downloading $asset"
        Invoke-WebRequest -Uri "$base/$asset" -OutFile $exe -UseBasicParsing
        $sums = (Invoke-WebRequest -Uri "$base/SHA256SUMS" -UseBasicParsing).Content
        if ($sums -is [byte[]]) { $sums = [Text.Encoding]::UTF8.GetString($sums) }

        $expected = $null
        foreach ($line in $sums -split "`n") {
            $fields = $line.Trim() -split '\s+'
            if ($fields.Count -eq 2 -and $fields[1].TrimStart('*') -eq $asset) { $expected = $fields[0] }
        }
        if (-not $expected) { throw "$asset is not listed in SHA256SUMS." }
        $actual = (Get-FileHash -Path $exe -Algorithm SHA256).Hash
        if ($actual -ne $expected) {
            throw "Checksum mismatch for $asset (expected $expected, got $actual)."
        }

        $env:KELPY_TEMPORARY_INSTALLER = '1'
        if ([Console]::IsInputRedirected) { & $exe --yes } else { & $exe }
    }
    finally {
        Remove-Item Env:KELPY_TEMPORARY_INSTALLER -ErrorAction SilentlyContinue
        Remove-Item -Path $dir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
