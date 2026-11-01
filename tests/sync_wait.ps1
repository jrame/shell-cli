param(
    [Parameter(Mandatory = $true)][string]$SshHost,
    [string]$CliPath = "$PSScriptRoot\..\target\sync-default\release\shell.exe",
    [string]$SshClient = "$env:WINDIR\System32\OpenSSH\ssh.exe"
)
$ErrorActionPreference = 'Stop'
$session = 'syncwait-' + [guid]::NewGuid().ToString('N').Substring(0, 12)
$oldState = $env:SHELL_CLI_STATE_DIR
$env:SHELL_CLI_STATE_DIR = Join-Path $PSScriptRoot "..\target\sync-wait-state\$session"
$pending = $null
$opened = $null

function Invoke-SyncTest {
    param([string[]]$CliArgs, [int]$ExpectedExit = 0)
    $raw = & $CliPath --format json --ssh $SshClient @CliArgs
    $code = $LASTEXITCODE
    $reply = $raw | ConvertFrom-Json
    if ($code -ne $ExpectedExit) { throw "Expected $ExpectedExit, received ${code}: $raw" }
    return $reply
}

try {
    $opened = Invoke-SyncTest -CliArgs @('open', $session, $SshHost)
    $quiet = Invoke-SyncTest -CliArgs @('send', $session, '--screen', 'full', '--text', 'printf "quiet_ok\n"', '--enter')
    if ($quiet.wait_status -ne 'quiet' -or $quiet.screen_mode -ne 'full' -or $quiet.screen -notmatch '(?m)^quiet_ok[ \t]*\r?$') {
        throw 'Synchronous send did not return a quiet full snapshot'
    }
    foreach ($command in @('cd /tmp', 'cd /tmp', 'printf "delta_ok\n"')) {
        $delta = Invoke-SyncTest -CliArgs @('send', $session, '--text', $command, '--enter')
        if ($delta.wait_status -ne 'quiet' -or $delta.screen_mode -ne 'delta' -or $delta.screen -notmatch [regex]::Escape($command) -or $delta.screen -match 'quiet_ok|SHELL_CLI_READY_') {
            throw "Ordinary shell command did not return only its delta: $($delta | ConvertTo-Json -Compress)"
        }
    }
    # Disable echo so the command line itself cannot satisfy the reaction wait.
    $null = Invoke-SyncTest -CliArgs @('send', $session, '--text', 'stty -echo', '--enter')
    $null = Invoke-SyncTest -CliArgs @('send', $session, '--async', '--text', 'sleep 30', '--enter')
    Start-Sleep -Milliseconds 300
    $clock = [Diagnostics.Stopwatch]::StartNew()
    # A shell submitting Enter can emit a cursor move even with echo disabled.
    # Type into an already sleeping process without Enter for actual silence.
    $silent = Invoke-SyncTest -ExpectedExit 6 -CliArgs @('--timeout', '1', 'send', $session, '--text', 'probe')
    if ($silent.wait_status -ne 'timeout' -or $silent.delivery -ne 'acknowledged' -or $null -eq $silent.screen -or $silent.pane_dead -or $clock.Elapsed.TotalSeconds -gt 4) {
        throw 'No-output wait did not return an acknowledged timeout snapshot within its budget'
    }
    $null = Invoke-SyncTest -CliArgs @('keys', $session, '--async', 'Ctrl-C')
    Start-Sleep -Milliseconds 300
    $streaming = Invoke-SyncTest -ExpectedExit 6 -CliArgs @('--timeout', '1', 'send', $session, '--text', 'while :; do printf "stream_tick\n"; sleep 0.05; done', '--enter')
    if ($streaming.wait_status -ne 'timeout' -or $streaming.delivery -ne 'acknowledged' -or $streaming.screen -notmatch 'stream_tick' -or $streaming.pane_dead) {
        throw 'Continuous-output wait did not return its available screen at timeout'
    }
    $null = Invoke-SyncTest -CliArgs @('keys', $session, '--async', 'Ctrl-C')
    Start-Sleep -Milliseconds 300

    # A second CLI process holds a synchronous wait. Viewer IPC and another CLI
    # input must remain available; the latter also exercises the dispatch lock.
    $null = Invoke-SyncTest -CliArgs @('send', $session, '--async', '--text', 'sleep 30', '--enter')
    Start-Sleep -Milliseconds 300
    $pending = Start-Job -ArgumentList $CliPath, $session -ScriptBlock {
        param($Cli, $Name)
        $raw = & $Cli --format json --timeout 4 send $Name --text 'probe'
        [pscustomobject]@{ Code = $LASTEXITCODE; Raw = ($raw -join "`n") }
    }
    Start-Sleep -Milliseconds 1200
    if ($pending.State -ne 'Running') { throw 'Silent synchronous input was not still waiting' }
    $clock.Restart()
    $view = Invoke-SyncTest -CliArgs @('view', $SshHost, $session, $opened.id, '--width', '80', '--height', '24')
    if ($view.id -ne $opened.id -or $clock.Elapsed.TotalSeconds -gt 2) { throw 'Pending wait blocked viewer refresh' }
    $clock.Restart()
    $interrupt = Invoke-SyncTest -CliArgs @('keys', $session, '--async', 'Ctrl-C')
    if ($interrupt.wait_status -ne 'skipped' -or $null -ne $interrupt.screen -or $clock.Elapsed.TotalSeconds -gt 2) { throw 'Pending wait blocked async Ctrl-C' }
    $done = $pending | Wait-Job -Timeout 6 | Receive-Job
    if (!$done) { throw 'Pending send never returned after interruption' }
    $reply = $done.Raw | ConvertFrom-Json
    if ($done.Code -ne 0 -or $reply.wait_status -ne 'quiet' -or $reply.delivery -ne 'acknowledged') {
        throw "Pending send did not observe terminal reaction after Ctrl-C: $($done.Raw)"
    }
    $after = Invoke-SyncTest -CliArgs @('send', $session, '--text', 'printf "still_alive\n"', '--enter')
    if ($after.wait_status -ne 'quiet' -or $after.screen -notmatch 'still_alive') { throw 'Wait timeout or interrupt lost the shell' }
    Write-Output 'PASS: quiet/full snapshot, default command deltas, no-output timeout, continuous-output timeout, responsive viewer and async Ctrl-C during a pending CLI wait, same shell survives'
}
finally {
    if ($pending) { $pending | Stop-Job; $pending | Remove-Job }
    if ($opened) { $null = Invoke-SyncTest -CliArgs @('close', $session) }
    $env:SHELL_CLI_STATE_DIR = $oldState
}
