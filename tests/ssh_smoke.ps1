param(
    [Parameter(Mandatory = $true)]
    [string]$SshHost,
    [string]$SshClient = "$env:WINDIR\System32\OpenSSH\ssh.exe",
    [switch]$Headed,
    [string]$CliPath = "$PSScriptRoot\..\target\release\shell.exe"
)

$ErrorActionPreference = 'Stop'

function Invoke-TestCli {
    param([string[]]$CliArgs, [int]$ExpectedExit = 0, [string]$Client = $SshClient)
    $raw = & $CliPath --format json --ssh $Client @CliArgs
    $exitCode = $LASTEXITCODE
    $reply = $raw | ConvertFrom-Json
    if ($exitCode -ne $ExpectedExit) {
        throw "Command [$($CliArgs -join ' ')] expected exit $ExpectedExit, received ${exitCode}: $raw"
    }
    return $reply
}

function Wait-Screen {
    param([string]$Pattern)
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        $read = Invoke-TestCli -CliArgs @('read', $SshHost, $sessionName, '--history', '100')
        if ($read.screen -match $Pattern) { return $read }
        Start-Sleep -Milliseconds 100
    }
    throw "Screen did not match ${Pattern}: $($read.screen)"
}

$null = Invoke-TestCli -CliArgs @('doctor', $SshHost)
$sessionName = 'smoke-' + [guid]::NewGuid().ToString('N').Substring(0, 12)
$failed = Invoke-TestCli -ExpectedExit 3 -Client 'shell-cli-missing-ssh-test' -CliArgs @('open', $sessionName, $SshHost)
if ($failed.error.code -ne 'transport' -or $failed.delivery -ne 'not_sent') {
    throw 'Failed startup incorrectly claimed an established connection'
}
$openArgs = @('open', $sessionName, $SshHost)
if ($Headed) { $openArgs += '--headed' }
$opened = Invoke-TestCli -CliArgs $openArgs
try {
    $null = Invoke-TestCli -ExpectedExit 5 -CliArgs @('open', $sessionName, $SshHost)
    # Names stay unique across hosts, before any SSH attempt to that host.
    $duplicate = Invoke-TestCli -ExpectedExit 5 -Client 'shell-cli-missing-ssh-test' -CliArgs @('open', $sessionName, 'different-host')
    if ($duplicate.error.code -ne 'conflict') { throw 'Cross-host duplicate name was not rejected' }
    # Omitted UUIDs resolve to the same owner, and --async acknowledges input
    # without waiting for the running remote command to finish.
    $async = Invoke-TestCli -CliArgs @('send', $sessionName, '--async', '--text', 'sleep 30', '--enter')
    if ($async.delivery -ne 'acknowledged' -or $async.id -ne $opened.id -or $async.wait_status -ne 'skipped' -or $null -ne $async.screen) {
        throw 'Async send without an ID did not acknowledge the existing session'
    }
    Start-Sleep -Milliseconds 200
    $async = Invoke-TestCli -CliArgs @('keys', $sessionName, '--async', 'Ctrl-C')
    if ($async.delivery -ne 'acknowledged' -or $async.wait_status -ne 'skipped' -or $null -ne $async.screen) { throw 'Async keys without an ID were not acknowledged' }
    $sync = Invoke-TestCli -CliArgs @('send', $sessionName, '--text', 'printf "sync_wait_ok\n"', '--enter', '--screen', 'full')
    if ($sync.wait_status -ne 'quiet' -or $sync.screen_mode -ne 'full' -or $sync.screen -notmatch '(?m)^sync_wait_ok[ \t]*\r?$') {
        throw 'Synchronous send did not return its quiet screen snapshot'
    }
    $null = Invoke-TestCli -CliArgs @('resize', $sessionName, '90', '28')
    $full = Invoke-TestCli -CliArgs @('read', $sessionName, '--screen', 'full')
    if ($full.screen_mode -ne 'full' -or $full.row_wrapped.Count -ne $full.size.height) {
        throw 'Full read did not report its mode and physical-row wrap metadata'
    }
    $delta = Invoke-TestCli -CliArgs @('read', $sessionName, '--screen', 'delta')
    if ($delta.screen_mode -notin @('full', 'delta') -or $null -eq $delta.row_wrapped) {
        throw 'Delta read did not report its actual mode and row wrap metadata'
    }
    # A second idle read must consume no rows, unless an asynchronous repaint
    # intervened; poll briefly to allow the interrupted shell prompt to settle.
    $settled = $false
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        $delta = Invoke-TestCli -CliArgs @('read', $sessionName, '--screen', 'delta')
        if ($delta.screen_mode -eq 'delta' -and $delta.screen -eq '' -and $delta.row_wrapped.Count -eq 0) {
            $settled = $true
            break
        }
        Start-Sleep -Milliseconds 100
    }
    if (!$settled) { throw 'Idle delta reads did not settle to an empty delta' }
    $listed = Invoke-TestCli -CliArgs @('list', $SshHost)
    if (!($listed.sessions | Where-Object { $_.name -eq $sessionName -and $_.id -eq $opened.id })) {
        throw 'The opened session is missing from list'
    }
    # Exercise the same authenticated viewer request through the CLI executable.
    $viewerBytes = ([Text.Encoding]::UTF8.GetBytes("export SHELL_CLI_VIEWER=shared_viewer_ok" + [char]13)) -join ','
    $viewerReply = Invoke-TestCli -CliArgs @('view', $SshHost, $sessionName, $opened.id,
        '--bytes', $viewerBytes, '--width', '90', '--height', '28')
    if ($viewerReply.id -ne $opened.id) { throw 'Viewer IPC changed identity' }
    # Dropping the viewer connection must leave the same shell alive.
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'printenv SHELL_CLI_VIEWER', '--enter')
    $null = Wait-Screen -Pattern '(?m)^shared_viewer_ok[ \t]*\r?$'
    if ($Headed) {
        $shown = Invoke-TestCli -CliArgs @('show', $SshHost, $sessionName)
        if ($shown.id -ne $opened.id) { throw 'Show changed the session identity' }
        Start-Sleep -Milliseconds 500
        $views = Get-CimInstance Win32_Process | Where-Object {
            $_.Name -eq 'shell.exe' -and $_.CommandLine -like "*__view-session*$sessionName*"
        }
        if (!$views) { throw 'No visible display process survived attachment' }
        $views | ForEach-Object { Stop-Process -Id $_.ProcessId -ErrorAction Stop }
        $status = Invoke-TestCli -CliArgs @('status', $SshHost, $sessionName)
        if ($status.pane_dead -or $status.id -ne $opened.id) { throw 'Viewer termination closed SSH' }
    }
    $marker = 'shell_cli_' + [guid]::NewGuid().ToString('N')
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', "export SHELL_CLI_SMOKE=$marker", '--enter')
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'printenv SHELL_CLI_SMOKE', '--enter')
    $found = $false
    for ($attempt = 0; $attempt -lt 10; $attempt++) {
        $read = Invoke-TestCli -CliArgs @('read', $SshHost, $sessionName, '--history', '20')
        if ($read.screen -match ('(?m)^' + [regex]::Escape($marker) + '[ \t]*\r?$')) {
            $found = $true
            break
        }
        Start-Sleep -Milliseconds 100
    }
    if (!$found) { throw 'The shell did not preserve the environment variable between calls' }
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'cd /tmp', '--enter')
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'pwd', '--enter')
    $null = Wait-Screen -Pattern '(?m)^/tmp[ \t]*\r?$'
    $null = Invoke-TestCli -CliArgs @('resize', $SshHost, $sessionName, '100', '30')
    $status = Invoke-TestCli -CliArgs @('status', $SshHost, $sessionName)
    if ($status.size.width -ne 100 -or $status.size.height -ne 30) {
        throw 'The terminal dimensions did not update'
    }
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'stty size', '--enter')
    $null = Wait-Screen -Pattern '(?m)^30 100[ \t]*\r?$'
    $null = Invoke-TestCli -CliArgs @('keys', $SshHost, $sessionName, 'Ctrl-L')

    # A foreground command must be interruptible through the same terminal.
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'sleep 30', '--enter')
    Start-Sleep -Milliseconds 200
    $null = Invoke-TestCli -CliArgs @('keys', $SshHost, $sessionName, 'Ctrl-C')
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'echo interrupted_ok', '--enter')
    $null = Wait-Screen -Pattern '(?m)^interrupted_ok[ \t]*\r?$'

    # Exercise an actual full-screen editor, then verify the saved bytes.
    # Keep a real display attached during vi. Earlier coverage stopped displays
    # before reaching this step, which missed failures in the polling viewer.
    if ($Headed) {
        $null = Invoke-TestCli -CliArgs @('show', $SshHost, $sessionName)
        Start-Sleep -Milliseconds 300
    }
    $remoteFile = '/tmp/' + $sessionName + '.txt'
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', "vi $remoteFile", '--enter')
    $null = Wait-Screen -Pattern '(?m)^~'
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'ieditor_saved_ok')
    $null = Wait-Screen -Pattern '(?m)^editor_saved_ok[ \t]*\r?$'
    if ($Headed) {
        Start-Sleep -Milliseconds 1500
        $views = Get-CimInstance Win32_Process | Where-Object {
            $_.Name -eq 'shell.exe' -and $_.CommandLine -like "*__view-session*$sessionName*"
        }
        if (!$views) { throw 'The visible display exited while vi was active' }
    }
    # Escape may have no visible terminal reaction; its delivery is sufficient.
    $null = Invoke-TestCli -CliArgs @('keys', $SshHost, $sessionName, '--async', 'Escape')
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', ':wq', '--enter')
    Start-Sleep -Milliseconds 200
    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', "cat $remoteFile; rm -f $remoteFile", '--enter')
    $null = Wait-Screen -Pattern '(?m)^editor_saved_ok[ \t]*\r?$'

    if ($Headed) {
        $views = Get-CimInstance Win32_Process | Where-Object {
            $_.Name -eq 'shell.exe' -and $_.CommandLine -like "*__view-session*$sessionName*"
        }
        if (!$views) { throw 'The visible display exited when returning from vi' }
        $views | ForEach-Object { Stop-Process -Id $_.ProcessId -ErrorAction Stop }
    }

    $null = Invoke-TestCli -CliArgs @('send', $SshHost, $sessionName, '--text', 'exit', '--enter')
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        $status = Invoke-TestCli -CliArgs @('status', $SshHost, $sessionName)
        if ($status.pane_dead) { break }
        Start-Sleep -Milliseconds 100
    }
    if (!$status.pane_dead) { throw 'SSH exit was not detected' }
    $closed = Invoke-TestCli -ExpectedExit 3 -CliArgs @('send', $SshHost, $sessionName,
        '--text', 'echo must_not_run', '--enter')
    if ($closed.error.code -ne 'session_closed' -or $closed.delivery -ne 'not_sent') {
        throw 'Input was not rejected after disconnect'
    }
}
finally {
    if ($Headed) {
        Get-CimInstance Win32_Process | Where-Object {
            $_.Name -eq 'shell.exe' -and $_.CommandLine -like "*__view-session*$sessionName*"
        } | ForEach-Object { Stop-Process -Id $_.ProcessId -ErrorAction SilentlyContinue }
    }
    $null = Invoke-TestCli -CliArgs @('close', $SshHost, $sessionName)
}
$missing = Invoke-TestCli -ExpectedExit 4 -CliArgs @('status', $SshHost, $sessionName)
if ($missing.error.code -ne 'session_not_found') { throw 'The closed session is still present' }
Write-Output "PASS: viewer IPC/shared shell/detach, SSH, detached open/list, duplicate/UUID guards, shell persistence, remote resize, Ctrl-C, vi, disconnect, close ($SshHost)"
