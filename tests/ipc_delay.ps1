param(
    [string]$CliPath = "$PSScriptRoot\..\target\release\shell.exe",
    [Parameter(Mandatory = $true)][string]$SshHost
)
$ErrorActionPreference = 'Stop'
$env:SHELL_CLI_STATE_DIR = Join-Path $PSScriptRoot '..\target\ipc-regression'
$session = 'slow-ipc-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$opened = & $CliPath --format json --ssh "$env:WINDIR\System32\OpenSSH\ssh.exe" open $session $SshHost | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw $opened.error.message }
try {
    $entry = Get-ChildItem -LiteralPath $env:SHELL_CLI_STATE_DIR -Filter '*.json' | ForEach-Object { Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json } | Where-Object session -eq $session
    $client = [Net.Sockets.TcpClient]::new()
    try {
        $client.Connect('127.0.0.1', $entry.port)
        $stream = $client.GetStream()
        $stream.ReadTimeout = 5000
        # Deliberately separate accept from the arrival of the first request byte.
        Start-Sleep -Milliseconds 200
        $frame = @{ token = $entry.token; timeout = 2; action = @{ Status = @{ host = $SshHost; session = $session } } } | ConvertTo-Json -Depth 5 -Compress
        $bytes = [Text.Encoding]::UTF8.GetBytes($frame + "`n")
        $stream.Write($bytes, 0, $bytes.Length)
        $reader = [IO.StreamReader]::new($stream)
        $line = $reader.ReadLine()
        if (!$line) { throw 'Session owner closed the accepted socket before delayed request arrived' }
        $reply = $line | ConvertFrom-Json
        if ($reply.error -or $reply.id -ne $opened.id) { throw 'Delayed IPC request was rejected' }
        Write-Output 'PASS: accepted socket waits for request bytes'
    } finally { $client.Dispose() }
} finally {
    $null = & $CliPath close $SshHost $session
}
