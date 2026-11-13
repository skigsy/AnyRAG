#Requires -Version 5.1

[CmdletBinding(DefaultParameterSetName = 'Container')]
param(
    [Parameter(Mandatory, ParameterSetName = 'Container')]
    [string]$Executable,

    # Microsoft Nano Server LTSC 2025 x64, build 26100.33674.
    # https://mcr.microsoft.com/en-us/artifact/mar/windows/nanoserver/tag/ltsc2025-amd64
    [Parameter(ParameterSetName = 'Container')]
    [ValidatePattern('\Amcr\.microsoft\.com/windows/(nanoserver|servercore)@sha256:[a-f0-9]{64}\z')]
    [string]$BaseImage = 'mcr.microsoft.com/windows/nanoserver@sha256:6020b6edd804e7cf8cbca9142c351f597a59f53c5c7f07a6caa2261dcc1d8e8f',

    [Parameter(Mandatory, ParameterSetName = 'Consumer')]
    [string]$Archive,

    [Parameter(Mandatory, ParameterSetName = 'Consumer')]
    [ValidatePattern('\A[a-fA-F0-9]{64}\z')]
    [string]$ExpectedSha256
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-QualificationParent([string]$Path) {
    $directory = Get-Item -LiteralPath $Path
    if (-not $directory.PSIsContainer -or $directory.FullName -notmatch '^[A-Za-z]:\\') {
        throw 'Qualification needs a local directory.'
    }
    for ($ancestor = $directory; $ancestor; $ancestor = $ancestor.Parent) {
        if ($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw 'Qualification directories cannot traverse reparse points.'
        }
    }
    $directory.FullName
}

function Remove-QualificationFolder([string]$Directory, [string]$Parent) {
    if (Test-Path -LiteralPath $Directory) {
        $resolved = Get-QualificationParent $Directory
        if ([IO.Path]::GetDirectoryName($resolved).TrimEnd('\') -ne $Parent.TrimEnd('\') -or
            [IO.Path]::GetFileName($resolved) -notmatch '^rust-rag-guest-[a-f0-9]{32}$') {
            throw 'Refusing to clean a directory outside the synthetic qualification scope.'
        }
        [IO.Directory]::Delete(('\\?\' + $resolved), $true)
    }
}

function Test-ConsumerFixture([string]$Candidate, [string]$Scratch, [string]$DocumentsFixture) {
    $utf8 = New-Object Text.UTF8Encoding($false)
    $original = 'The Ternmarker is in drawer seven.'
    $revised = 'The Wrenmarker is in drawer nine.'
    $index = Join-Path $Scratch 'Index with spaces'
    $folder = $DocumentsFixture
    while ($folder.Length -lt 280) {
        $folder = [IO.Path]::Combine($folder, ('Evidence ' + [char]0x00E9 + ('a' * 42)))
    }
    $note = [IO.Path]::Combine($folder, ('field-' + [char]0x6771 + '.md'))
    $longNote = '\\?\' + $note
    [void][IO.Directory]::CreateDirectory(('\\?\' + $folder))
    [IO.File]::WriteAllText($longNote, $original, $utf8)

    function Invoke-Guest([string[]]$Arguments, [string]$State = $index, [switch]$Reject) {
        # Inspect expected nonzero exits explicitly, including when run from PowerShell 7.
        $PSNativeCommandUseErrorActionPreference = $false
        $stdout = & $Candidate @Arguments --state-dir $State --json
        $exit = $LASTEXITCODE
        if (($exit -ne 0) -ne [bool]$Reject -or @($stdout).Count -ne 1) {
            throw "Unexpected guest CLI exit $exit while running $($Arguments[0])."
        }
        $result = ConvertFrom-Json -InputObject $stdout
        if ($Reject -and $result.status -ne 'error') {
            throw 'The guest CLI did not report its expected refusal.'
        }
        $result
    }

    function Assert-GuestCitation($Result, [string]$Quote) {
        $hasher = [Security.Cryptography.SHA256]::Create()
        try {
            $hash = [BitConverter]::ToString($hasher.ComputeHash($utf8.GetBytes($Quote))).Replace('-', '')
        }
        finally {
            $hasher.Dispose()
        }
        if ($Result.status -ne 'evidence' -or $Result.evidence.Count -ne 1) {
            throw 'The guest CLI did not return exactly one citation.'
        }
        $hit = $Result.evidence[0]
        if ($hit.text -cne $Quote -or $hit.start -ne 0 -or $hit.end -ne $Quote.Length -or
            $hit.path -ne $note -or $hit.source_hash -ne $hash) {
            throw 'The guest citation does not match the long Unicode source path and bytes.'
        }
    }

    $savedPath = $env:PATH
    $savedEncoding = [Console]::OutputEncoding
    try {
        $env:PATH = "$env:SystemRoot\System32;$env:SystemRoot"
        [Console]::OutputEncoding = $utf8
        $initial = Invoke-Guest @('init')
        $doctor = Invoke-Guest @('doctor')
        $ingest = Invoke-Guest @('ingest', $DocumentsFixture)
        if ($initial.storage_version -ne 1 -or $doctor.status -ne 'ready' -or
            -not $doctor.sqlite.fts5 -or $ingest.updated_sources -ne 1) {
            throw 'Guest initialization, bundled SQLite, or first ingestion failed.'
        }
        Assert-GuestCitation (Invoke-Guest @('query', 'Ternmarker')) $original

        $database = Join-Path $index 'personal.sqlite3'
        $originalHash = (Get-FileHash -LiteralPath $database).Hash
        $upgraded = Invoke-Guest @('init', '--upgrade')
        $backup = Get-Item -LiteralPath $upgraded.upgrade_backup
        if ($upgraded.storage_version -le $initial.storage_version -or
            $backup.DirectoryName -ne (Get-Item -LiteralPath $index).FullName -or
            $backup.Name -notmatch '^personal-v1-[0-9]+\.sqlite3\.backup$' -or
            (Get-FileHash -LiteralPath $backup.FullName).Hash -ne $originalHash) {
            throw 'The explicit upgrade did not preserve an exact rollback copy.'
        }
        Assert-GuestCitation (Invoke-Guest @('query', 'Ternmarker')) $original

        [IO.File]::WriteAllText($longNote, $revised, $utf8)
        $update = Invoke-Guest @('ingest', $DocumentsFixture)
        if ($update.updated_sources -ne 1 -or (Invoke-Guest @('query', 'Ternmarker')).status -ne 'abstained') {
            throw 'The upgraded guest index did not retire the previous revision.'
        }
        Assert-GuestCitation (Invoke-Guest @('query', 'Wrenmarker')) $revised

        $rollback = Join-Path $Scratch 'Restored Index'
        [void][IO.Directory]::CreateDirectory($rollback)
        $restoredDb = Join-Path $rollback 'personal.sqlite3'
        Copy-Item -LiteralPath $backup.FullName -Destination $restoredDb
        if ((Invoke-Guest @('status') -State $rollback).storage_version -ne 1 -or
            (Invoke-Guest @('query', 'Wrenmarker') -State $rollback).status -ne 'abstained' -or
            (Get-FileHash -LiteralPath $restoredDb).Hash -ne $originalHash) {
            throw 'The copied rollback index did not restore the pre-upgrade state.'
        }
        Assert-GuestCitation (Invoke-Guest @('query', 'Ternmarker') -State $rollback) $original

        # SQLite stores user_version as a four-byte big-endian value at offset 60.
        # https://www.sqlite.org/fileformat.html#user_version_number
        $future = Join-Path $Scratch 'Future Index'
        [void][IO.Directory]::CreateDirectory($future)
        $futureDb = Join-Path $future 'personal.sqlite3'
        $futureBytes = [IO.File]::ReadAllBytes($restoredDb)
        [Array]::Copy([byte[]](0, 0, 0, 99), 0, $futureBytes, 60, 4)
        [IO.File]::WriteAllBytes($futureDb, $futureBytes)
        $futureHash = (Get-FileHash -LiteralPath $futureDb).Hash
        $null = Invoke-Guest @('status') -State $future -Reject
        $null = Invoke-Guest @('init', '--upgrade') -State $future -Reject
        if ((Get-FileHash -LiteralPath $futureDb).Hash -ne $futureHash -or
            @(Get-ChildItem -LiteralPath $future -Force).Count -ne 1) {
            throw 'Refusing an unsupported index version changed its files.'
        }

        $removed = Invoke-Guest @('remove', '--all')
        if ($removed.removed_sources -ne 1 -or -not $removed.logical_removal -or
            (Invoke-Guest @('query', 'Wrenmarker')).status -ne 'abstained' -or
            [IO.File]::ReadAllText($longNote, $utf8) -cne $revised) {
            throw 'Guest deletion did not preserve the source and retire its citation.'
        }
        Write-Output "PASS: Unicode source path ($($note.Length) characters), upgrade, rollback, refusal, and source-preserving deletion."
    }
    finally {
        $env:PATH = $savedPath
        [Console]::OutputEncoding = $savedEncoding
    }
}

if ($PSCmdlet.ParameterSetName -eq 'Consumer') {
    $ProgressPreference = 'SilentlyContinue'
    $windows = Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    if ($windows.InstallationType -ne 'Client') {
        throw 'Consumer qualification requires a Windows client guest.'
    }
    $groups = & "$env:SystemRoot\System32\whoami.exe" /groups /fo csv /nh
    if ($LASTEXITCODE -ne 0 -or ($groups -match '"S-1-5-32-544"')) {
        throw 'Run consumer qualification from a standard account, outside Administrators.'
    }
    if ([Net.NetworkInformation.NetworkInterface]::GetAllNetworkInterfaces() | Where-Object {
        $_.OperationalStatus -eq 'Up' -and $_.NetworkInterfaceType -ne 'Loopback'
    }) {
        throw 'Disconnect the guest network before qualification.'
    }
    $userDirectory = [Environment]::GetFolderPath('UserProfile')
    if ($userDirectory -notmatch '[^\u0000-\u007f]' -or $userDirectory -notmatch ' ') {
        throw 'Use a guest account whose profile path contains Unicode and spaces.'
    }
    $local = Get-QualificationParent ([Environment]::GetFolderPath('LocalApplicationData'))
    if (-not $local.StartsWith($userDirectory.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Keep the guest local application folder under its Unicode profile.'
    }
    $documents = Get-QualificationParent ([Environment]::GetFolderPath('MyDocuments'))
    if ($documents -eq (Join-Path $userDirectory 'Documents') -or
        $documents -match '\\(OneDrive(?: - [^\\]+)?|Dropbox|Google Drive|iCloudDrive)(\\|$)') {
        throw 'Set the guest Documents location to a different local, unsynced folder first.'
    }
    $archiveFile = Get-Item -LiteralPath $Archive
    if ($archiveFile.PSIsContainer -or $archiveFile.Extension -ne '.zip' -or
        $archiveFile.FullName -notmatch '^[A-Za-z]:\\') {
        throw 'Supply the candidate ZIP from a local drive.'
    }
    [AppContext]::SetSwitch('Switch.System.IO.UseLegacyPathHandling', $false)
    [AppContext]::SetSwitch('Switch.System.IO.BlockLongPaths', $false)
    $folderName = 'rust-rag-guest-' + [guid]::NewGuid().ToString('N')
    $scratch = Join-Path $local $folderName
    $documentsFixture = Join-Path $documents $folderName
    try {
        [void][IO.Directory]::CreateDirectory($scratch)
        $stagedArchive = Join-Path $scratch 'candidate.zip'
        Copy-Item -LiteralPath $archiveFile.FullName -Destination $stagedArchive
        if ((Get-FileHash -LiteralPath $stagedArchive).Hash -ne $ExpectedSha256) {
            throw 'The supplied ZIP does not match the recorded archive SHA-256.'
        }
        $extract = Join-Path $scratch ('Evidence ' + [char]0x00C9 + 'xecutable')
        Expand-Archive -LiteralPath $stagedArchive -DestinationPath $extract
        $candidate = Join-Path $extract 'rust-rag.exe'
        $candidateHash = (Get-FileHash -LiteralPath $candidate).Hash
        Test-ConsumerFixture $candidate $scratch $documentsFixture
        if ((Get-FileHash -LiteralPath $candidate).Hash -ne $candidateHash) {
            throw 'The extracted guest executable changed during qualification.'
        }
        Write-Output "Windows client build $($windows.CurrentBuildNumber). ZIP SHA-256 $ExpectedSha256. Executable SHA-256 $candidateHash."
        Write-Output 'Record Defender and SmartScreen observations separately from this synthetic CLI check.'
    }
    finally {
        try {
            Remove-QualificationFolder $documentsFixture $documents
        }
        finally {
            Remove-QualificationFolder $scratch $local
        }
    }
    return
}

$binary = Get-Item -LiteralPath $Executable
if ($binary.PSIsContainer -or $binary.Extension -ne '.exe') {
    throw 'Supply a built Windows x64 executable.'
}
$binaryHash = (Get-FileHash -LiteralPath $binary.FullName -Algorithm SHA256).Hash

$dumpbin = Get-Command dumpbin.exe -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -ExpandProperty Source -First 1
if (-not $dumpbin) {
    $vswhere = Join-Path ([Environment]::GetEnvironmentVariable('ProgramFiles(x86)')) 'Microsoft Visual Studio/Installer/vswhere.exe'
    $dumpbin = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' |
        Select-Object -First 1
}
if (-not $dumpbin) {
    throw 'MSVC dumpbin is required for the import check.'
}
$headers = & $dumpbin /nologo /headers $binary.FullName
if ($LASTEXITCODE -ne 0 -or -not ($headers -match '^\s*8664 machine \(x64\)\s*$')) {
    throw 'The candidate is not a readable x64 PE executable.'
}
$importReport = & $dumpbin /nologo /imports $binary.FullName
if ($LASTEXITCODE -ne 0) {
    throw 'Failed to inspect the candidate imports.'
}
$imports = @($importReport | ForEach-Object {
    if ($_ -match '^\s*([a-z0-9_.-]+\.dll)\s*$') { $Matches[1].ToLowerInvariant() }
} | Sort-Object -Unique)
# The dashboard's local listener uses the Windows Winsock library.
$windowsDlls = 'advapi32.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'kernel32.dll', 'kernelbase.dll', 'ntdll.dll', 'ws2_32.dll'
$unexpected = @($imports | Where-Object {
    $_ -notin $windowsDlls -and $_ -notmatch '^api-ms-win-core-[a-z0-9-]+\.dll$'
})
if (-not $imports.Count -or $unexpected.Count) {
    throw "Unexpected PE import list: $($imports -join ', ')."
}

$docker = (Get-Command docker.exe -CommandType Application).Source
$dockerOs = & $docker info --format '{{.OSType}}'
if ($LASTEXITCODE -ne 0 -or $dockerOs -ne 'windows') {
    throw 'Qualification needs a Docker daemon in Windows container mode.'
}

$original = 'The Ternmarker is in drawer seven.'
$revised = 'The Wrenmarker is in drawer nine.'
$token = [guid]::NewGuid().ToString('N')
$context = Join-Path ([IO.Path]::GetTempPath()) "rust-rag-qualification-$token"
$image = "rust-rag-qualification:$token"
$imageBuilt = $false
$contextFiles = 'Dockerfile', 'smoke.cmd', 'original.txt', 'revised.txt', 'rust-rag.exe'

try {
    [void][IO.Directory]::CreateDirectory($context)
    $staged = Join-Path $context 'rust-rag.exe'
    Copy-Item -LiteralPath $binary.FullName -Destination $staged
    if ((Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash -ne $binaryHash) {
        throw 'Staged executable differs from the candidate.'
    }
    $originalFile = Join-Path $context 'original.txt'
    $revisedFile = Join-Path $context 'revised.txt'
    Set-Content -LiteralPath $originalFile -Value $original -Encoding ascii -NoNewline
    Set-Content -LiteralPath $revisedFile -Value $revised -Encoding ascii -NoNewline
    @"
FROM $BaseImage
COPY ["rust-rag.exe", "smoke.cmd", "original.txt", "revised.txt", "C:/rust-rag-input/"]
"@ | Set-Content -LiteralPath (Join-Path $context 'Dockerfile') -Encoding ascii
    @'
@echo off
setlocal
set "PATH=%SystemRoot%\System32;%SystemRoot%"
for %%F in (python.exe python3.exe cargo.exe rustc.exe sqlite3.exe) do if not "%%~$PATH:F"=="" exit /b 21
if exist "%SystemRoot%\System32\vcruntime14*.dll" exit /b 22
if exist "%SystemRoot%\System32\msvcp14*.dll" exit /b 23
if exist "%SystemRoot%\System32\whoami.exe" (
    "%SystemRoot%\System32\whoami.exe"
    if errorlevel 1 exit /b 24
)
set "work=C:\Users\ContainerUser\rust-rag-smoke"
set "program=C:\rust-rag-input\rust-rag.exe"
set "state=%work%\index"
set "source=%work%\source"
set "note=%source%\field-note.md"
mkdir "%source%" || exit /b 25
copy /b /y C:\rust-rag-input\original.txt "%note%" >nul || exit /b 26
"%program%" init --state-dir "%state%" --json || exit /b 30
"%program%" doctor --state-dir "%state%" --json || exit /b 31
"%program%" ingest "%source%" --state-dir "%state%" --json || exit /b 32
"%program%" query Ternmarker --state-dir "%state%" --json || exit /b 33
copy /b /y C:\rust-rag-input\revised.txt "%note%" >nul || exit /b 34
"%program%" ingest "%source%" --state-dir "%state%" --json || exit /b 35
"%program%" query Ternmarker --state-dir "%state%" --json || exit /b 36
"%program%" query Wrenmarker --state-dir "%state%" --json || exit /b 37
"%program%" remove --all --state-dir "%state%" --json || exit /b 38
"%program%" query Wrenmarker --state-dir "%state%" --json || exit /b 39
"%program%" status --state-dir "%state%" --json || exit /b 40
if not exist "%note%" exit /b 41
for %%F in ("%note%") do set "noteSize=%%~zF"
for %%F in ("C:\rust-rag-input\revised.txt") do set "fixtureSize=%%~zF"
if not "%noteSize%"=="%fixtureSize%" exit /b 42
set /p "noteText="<"%note%"
set /p "fixtureText="<"C:\rust-rag-input\revised.txt"
if not "%noteText%"=="%fixtureText%" exit /b 43
echo source-preserved
'@ | Set-Content -LiteralPath (Join-Path $context 'smoke.cmd') -Encoding ascii

    & $docker build --pull --isolation=process --tag $image $context
    if ($LASTEXITCODE -ne 0) {
        throw "Could not build the isolated Windows test image from $BaseImage."
    }
    $imageBuilt = $true
    $output = & $docker run --rm --pull=never --isolation=process --network=none --user ContainerUser --entrypoint cmd.exe $image /d /s /c 'C:\rust-rag-input\smoke.cmd'
    if ($LASTEXITCODE -ne 0) {
        throw "Isolated Windows smoke failed at exit code $LASTEXITCODE."
    }
    $lines = @($output | Where-Object { $_.StartsWith('{') })
    if ($lines.Count -ne 10 -or 'source-preserved' -notin $output) {
        throw 'The container did not complete the expected lifecycle.'
    }
    $identity = @($output | Where-Object { $_ -match '\\[^\\]+$' -and -not $_.StartsWith('{') })
    if ($identity.Count -and $identity[0] -notmatch '\\containeruser$') {
        throw 'The Windows test did not run as ContainerUser.'
    }
    $init, $doctor, $ingest, $first, $update, $old, $current, $removed, $after, $status =
        $lines | ForEach-Object { ConvertFrom-Json -InputObject $_ }
    if ($init.sources -ne 0 -or $doctor.status -ne 'ready' -or -not $doctor.sqlite.fts5 -or
        -not $doctor.sqlite.secure_delete -or -not $doctor.sqlite.fts_secure_delete) {
        throw 'Bundled SQLite preflight failed.'
    }
    if ($ingest.updated_sources -ne 1 -or $update.updated_sources -ne 1 -or
        $old.status -ne 'abstained' -or $old.evidence.Count -ne 0) {
        throw 'Ingestion or replacement failed.'
    }
    foreach ($case in @(
        @{ Result = $first; Quote = $original; File = $originalFile },
        @{ Result = $current; Quote = $revised; File = $revisedFile }
    )) {
        if ($case.Result.status -ne 'evidence' -or $case.Result.evidence.Count -ne 1) {
            throw 'Expected exactly one source citation.'
        }
        $hit = $case.Result.evidence[0]
        $expectedHash = (Get-FileHash -LiteralPath $case.File -Algorithm SHA256).Hash
        if ($hit.text -cne $case.Quote -or $hit.start -ne 0 -or $hit.end -ne $case.Quote.Length -or
            $hit.path -ne 'C:\Users\ContainerUser\rust-rag-smoke\source\field-note.md' -or
            $hit.source_hash -ne $expectedHash) {
            throw 'The citation does not match the synthetic source.'
        }
    }
    if ($removed.removed_sources -ne 1 -or -not $removed.logical_removal -or
        $removed.source_files_changed -or $after.status -ne 'abstained' -or
        $after.evidence.Count -ne 0 -or $status.sources -ne 0 -or $status.chunks -ne 0) {
        throw 'Source-preserving deletion failed.'
    }
    if ((Get-FileHash -LiteralPath $binary.FullName -Algorithm SHA256).Hash -ne $binaryHash) {
        throw 'The supplied executable changed during qualification.'
    }
    Write-Output "PASS: isolated Windows Server container, ContainerUser, network disabled, SHA-256 $binaryHash."
    Write-Output "Base image: $BaseImage"
    Write-Output "Windows imports: $($imports -join ', '). Bundled SQLite: $($doctor.sqlite.version)."
}
finally {
    if ($imageBuilt) {
        & $docker image rm --no-prune $image | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Warning 'Could not remove the isolated test image.'
        }
    }
    foreach ($name in $contextFiles) {
        $file = Join-Path $context $name
        if (Test-Path -LiteralPath $file -PathType Leaf) {
            Remove-Item -LiteralPath $file
        }
    }
    if ((Test-Path -LiteralPath $context -PathType Container) -and
        -not (Get-ChildItem -LiteralPath $context -Force | Select-Object -First 1)) {
        Remove-Item -LiteralPath $context
    }
}
