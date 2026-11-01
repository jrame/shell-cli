param(
    [Parameter(Mandatory = $true)][string]$SshHost,
    [string]$SshClient = "$env:WINDIR\System32\OpenSSH\ssh.exe",
    [string]$CliPath = "$PSScriptRoot\..\target\debug\shell.exe",
    [int]$Port = 22,
    [string]$Identity = ''
)
$ErrorActionPreference = 'Stop'
$CliPath = (Resolve-Path -LiteralPath $CliPath).Path
$originalLocation = $PWD.Path
$originalState = $env:SHELL_CLI_STATE_DIR
$testId = [guid]::NewGuid().ToString('N')
$testRoot = Join-Path ([IO.Path]::GetTempPath()) "shell-cli-transfers-$testId"
$remoteRoot = "/tmp/shell-cli-transfers-$testId"
$sessionName = "transfer-$($testId.Substring(0, 12))"
$opened = $null
$sshArgs = @('-T', '-o', 'BatchMode=yes', '-o', 'StrictHostKeyChecking=yes', '-p', "$Port")
if ($Identity) { $Identity = (Resolve-Path -LiteralPath $Identity).Path; $sshArgs += @('-i', $Identity) }

function Invoke-Cli {
    param([string[]]$CliArgs, [int]$ExpectedExit = 0)
    $raw = & $CliPath --format json @CliArgs
    $actualExit = $LASTEXITCODE
    if ($actualExit -ne $ExpectedExit) { throw "[$($CliArgs -join ' ')] exit ${actualExit}: $raw" }
    return ($raw | ConvertFrom-Json)
}
function Assert-SameFile {
    param([string]$First, [string]$Second)
    if ((Get-FileHash -LiteralPath $First).Hash -ne (Get-FileHash -LiteralPath $Second).Hash) {
        throw 'Transferred file differs from source'
    }
}

try {
    $null = New-Item -ItemType Directory -Path $testRoot
    $env:SHELL_CLI_STATE_DIR = Join-Path $testRoot 'state'
    Set-Location -LiteralPath $testRoot
    & $SshClient @sshArgs -- $SshHost "mkdir -m 700 $remoteRoot"
    if ($LASTEXITCODE -ne 0) { throw 'Cannot create remote test directory' }
    # Keep a tunnel open: transfers must not attempt to bind its port again.
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $forwardPort = $listener.LocalEndpoint.Port
    $listener.Stop()
    $profile = @{ host = $SshHost; port = $Port; local_forward = @("127.0.0.1:${forwardPort}:127.0.0.1:22") }
    if ($Identity) { $profile.identity = [IO.Path]::GetRelativePath($testRoot, $Identity) }
    @{ profiles = @{ test = $profile } } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath 'connections.json' -Encoding utf8NoBOM
    $opened = Invoke-Cli -CliArgs @('--ssh', $SshClient, '--config', '.\connections.json', 'open', $sessionName, '--profile', 'test')
    # Removing the profile and changing cwd must not change the saved settings.
    Remove-Item -LiteralPath 'connections.json'
    $nested = Join-Path $testRoot 'other-cwd'
    $null = New-Item -ItemType Directory -Path $nested
    Set-Location -LiteralPath $nested
    $source = Join-Path $nested "source ' et neige.bin"
    $destination = Join-Path $nested 'download.bin'
    $remote = $remoteRoot + '/a b''";$[]' + [char]0x96EA + '.bin'
    $bytes = [byte[]]::new(131089)
    [Random]::new(42).NextBytes($bytes)
    [IO.File]::WriteAllBytes($source, $bytes)
    # Leave vi in the foreground while SFTP runs independently.
    $null = Invoke-Cli -CliArgs @('send', $sessionName, '--text', "vi $remoteRoot/editor.txt", '--enter', '--async')
    Start-Sleep -Milliseconds 300
    $upload = Invoke-Cli -CliArgs @('upload', $sessionName, $source, $remote)
    if ($upload.transfer.bytes -ne $bytes.Length -or $upload.id -ne $opened.id) { throw 'Incorrect upload report' }
    $download = Invoke-Cli -CliArgs @('download', $sessionName, $remote, $destination)
    if ($download.transfer.bytes -ne $bytes.Length) { throw 'Incorrect download report' }
    Assert-SameFile $source $destination
    $null = Invoke-Cli -ExpectedExit 5 -CliArgs @('upload', $sessionName, $source, $remote)
    $null = Invoke-Cli -ExpectedExit 5 -CliArgs @('download', $sessionName, $remote, $destination)
    Assert-SameFile $source $destination
    # Replacing a longer file must remove its old tail, in both directions.
    [IO.File]::WriteAllBytes($source, [byte[]]@(0, 255, 13, 10, 1))
    $null = Invoke-Cli -CliArgs @('upload', $sessionName, $source, $remote, '--overwrite')
    $null = Invoke-Cli -CliArgs @('download', $sessionName, $remote, $destination, '--overwrite')
    Assert-SameFile $source $destination
    [IO.File]::WriteAllBytes($source, [byte[]]::new(0))
    $empty = Invoke-Cli -CliArgs @('upload', $sessionName, $source, "$remoteRoot/empty.bin")
    if ($empty.transfer.bytes -ne 0) { throw 'Empty upload report' }
    $null = Invoke-Cli -CliArgs @('download', $sessionName, "$remoteRoot/empty.bin", $destination, '--overwrite')
    Assert-SameFile $source $destination
    $null = Invoke-Cli -ExpectedExit 3 -CliArgs @('download', $sessionName, "$remoteRoot/missing.bin", "$nested\missing.bin")
    if (Test-Path -LiteralPath "$nested\missing.bin") { throw 'Missing source created a destination' }
    $null = Invoke-Cli -ExpectedExit 2 -CliArgs @('download', $sessionName, $remoteRoot, "$nested\directory.bin")
    $null = Invoke-Cli -ExpectedExit 2 -CliArgs @('upload', $sessionName, $source, $remoteRoot, '--overwrite')
    # Recursive roots are exact paths; empty directories and binary files survive.
    $tree = Join-Path $nested 'tree'
    $null = New-Item -ItemType Directory -Path (Join-Path $tree 'empty') -Force
    $null = New-Item -ItemType Directory -Path (Join-Path $tree 'niveau neige')
    [IO.File]::WriteAllBytes((Join-Path $tree 'niveau neige\payload.bin'), $bytes)
    [IO.File]::WriteAllText((Join-Path $tree 'root.txt'), "tree ' ; neige")
    $null = Invoke-Cli -ExpectedExit 2 -CliArgs @('upload', $sessionName, $tree, "$remoteRoot/tree")
    $recursiveUpload = Invoke-Cli -CliArgs @('upload', $sessionName, $tree, "$remoteRoot/tree/", '--recursive')
    if ($recursiveUpload.transfer.files -ne 2 -or $recursiveUpload.transfer.directories -ne 3) { throw 'Incorrect tree upload totals' }
    $treeCopy = Join-Path $nested 'tree-copy'
    $recursiveDownload = Invoke-Cli -CliArgs @('download', $sessionName, "$remoteRoot/tree/", $treeCopy, '-r')
    if ($recursiveDownload.transfer.bytes -ne $recursiveUpload.transfer.bytes) { throw 'Incorrect tree download bytes' }
    if (!(Test-Path -LiteralPath (Join-Path $treeCopy 'empty') -PathType Container)) { throw 'Empty directory was lost' }
    Assert-SameFile (Join-Path $tree 'root.txt') (Join-Path $treeCopy 'root.txt')
    Assert-SameFile (Join-Path $tree 'niveau neige\payload.bin') (Join-Path $treeCopy 'niveau neige\payload.bin')
    $null = Invoke-Cli -ExpectedExit 5 -CliArgs @('upload', $sessionName, $tree, "$remoteRoot/tree", '-r')
    $null = Invoke-Cli -CliArgs @('upload', $sessionName, $tree, "$remoteRoot/tree", '-r', '--overwrite')
    $null = Invoke-Cli -ExpectedExit 5 -CliArgs @('download', $sessionName, "$remoteRoot/tree", $treeCopy, '-r')
    $null = Invoke-Cli -CliArgs @('download', $sessionName, "$remoteRoot/tree", $treeCopy, '-r', '--overwrite')
    # A multi-source command creates a container and retains source basenames.
    $batchUpload = Invoke-Cli -CliArgs @('upload', $sessionName, $tree, $source, "$remoteRoot/batch/", '-r')
    if ($batchUpload.transfer.files -ne 3 -or $batchUpload.transfer.sources.Count -ne 2) { throw 'Incorrect selected-source report' }
    $batchCopy = Join-Path $nested 'batch-copy'
    $batchDownload = Invoke-Cli -CliArgs @('download', $sessionName, "$remoteRoot/batch/tree", "$remoteRoot/batch/$([IO.Path]::GetFileName($source))", $batchCopy, '-r')
    if ($batchDownload.transfer.files -ne 3 -or $batchDownload.transfer.directories -ne 4) { throw 'Incorrect batch download totals' }
    Assert-SameFile (Join-Path $tree 'root.txt') (Join-Path $batchCopy 'tree\root.txt')
    Assert-SameFile $source (Join-Path $batchCopy ([IO.Path]::GetFileName($source)))
    $selection = Join-Path $nested 'selection'
    $selected = Invoke-Cli -CliArgs @('download', $sessionName, "$remoteRoot/tree/root.txt", "$remoteRoot/empty.bin", $selection)
    if ($selected.transfer.files -ne 2) { throw 'Multiple files require one command' }
    Assert-SameFile (Join-Path $tree 'root.txt') (Join-Path $selection 'root.txt')
    $null = Invoke-Cli -CliArgs @('upload', $sessionName, (Join-Path $tree 'root.txt'), $source, "$remoteRoot/selected/")
    $collision = Join-Path $nested 'collision'
    $null = Invoke-Cli -ExpectedExit 2 -CliArgs @('download', $sessionName, "$remoteRoot/tree/root.txt", "$remoteRoot/batch/tree/root.txt", $collision, '-r')
    if (Test-Path -LiteralPath $collision) { throw 'Colliding sources created a destination' }
    $null = Invoke-Cli -ExpectedExit 2 -CliArgs @('upload', $sessionName, $source, $source, "$remoteRoot/collision")
    # Recursive symlinks are rejected before local destinations are created.
    & $SshClient @sshArgs -- $SshHost "ln -s . $remoteRoot/tree/cycle"
    if ($LASTEXITCODE -ne 0) { throw 'Cannot create test symlink' }
    $rejected = Join-Path $nested 'rejected-link'
    $null = Invoke-Cli -ExpectedExit 2 -CliArgs @('download', $sessionName, "$remoteRoot/tree", $rejected, '-r')
    if (Test-Path -LiteralPath $rejected) { throw 'Recursive symlink created a destination' }
    $null = Invoke-Cli -CliArgs @('keys', $sessionName, 'Escape', '--async')
    $null = Invoke-Cli -CliArgs @('send', $sessionName, '--text', ':q!', '--enter')
    $screen = Invoke-Cli -CliArgs @('send', $sessionName, '--text', 'printf "transfer_terminal_ok\n"', '--enter', '--screen', 'full')
    if ($screen.screen -notmatch '(?m)^transfer_terminal_ok[ \t]*\r?$') { throw 'Terminal did not survive file transfers' }
    Write-Output 'Transfer smoke passed: binary/empty files, recursive trees, empty directories, multiple sources, collisions, symlink rejection, overwrite protection, UUID, saved profile, cwd, active tunnel and vi.'
}
finally {
    Set-Location -LiteralPath $originalLocation
    if ($opened) { $null = Invoke-Cli -CliArgs @('close', $sessionName) }
    # Both cleanup targets are generated above and restricted to this test.
    if ($remoteRoot -match '^/tmp/shell-cli-transfers-[0-9a-f]{32}$') {
        & $SshClient @sshArgs -- $SshHost "rm -rf -- $remoteRoot"
    }
    $env:SHELL_CLI_STATE_DIR = $originalState
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $allowedParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if (!$resolvedRoot.StartsWith($allowedParent, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe local cleanup target' }
    if (Test-Path -LiteralPath $resolvedRoot) { Remove-Item -LiteralPath $resolvedRoot -Recurse -Force }
}
