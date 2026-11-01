# shell-cli

The project is named `shell-cli`; its command is `shell` (`shell.exe` on Windows). Build with `cargo build --release` or install with `cargo install --path .`.

Rust CLI for an interactive SSH terminal with Markdown output by default and optional JSON output for scripts. A hidden local session owner keeps OpenSSH and its terminal open between calls. The remote host needs an SSH server, a POSIX shell and standard `stty`; no additional remote service or terminal package is installed.

For a first session, follow the short [getting started guide](GETTING_STARTED.md). This README is the detailed command and behavior reference.


## Windows releases

Download `shell.exe` (Windows x64) from the GitHub release assets. Rust is only
needed to build from source; OpenSSH is still required to connect.

The [Windows release workflow](.github/workflows/release.yml) tests and builds
the release tag when a release is published (including pre-releases), then
attaches `shell.exe` and `shell.exe.sha256`. Commit and push the workflow before
creating the release tag and publishing the release. To build an existing
release, use **Actions → Windows release → Run workflow** and enter its tag
(for example, `v0.1.0`). A rerun replaces assets with the same names.

## Requirements

- Rust and cached dependencies for `cargo build --release --offline` (or build online).
- A real OpenSSH executable. On Windows use `--ssh C:\Windows\System32\OpenSSH\ssh.exe` if `ssh` resolves to a wrapper.
- Windows 10 version 1809 or later for ConPTY, or a Unix system with PTY support.
- SSH key/agent authentication and a previously verified host key. OpenSSH configuration aliases, ports and jump hosts are supported through `~/.ssh/config`.
- Access to the remote tools you intend to run. Docker debugging uses the remote account's existing Docker permissions.

Authentication is noninteractive (`BatchMode=yes`) and host-key verification remains enabled (`StrictHostKeyChecking=yes`). `doctor` checks a simple SSH command; `open` checks allocation of an interactive terminal.

## Usage

```text
shell doctor lab
shell exec lab --text "ls -la /srv/app" --format json
shell open debug lab --width 120 --height 35
shell list
shell list lab
shell send debug --text "cd /srv/app" --enter
shell send debug --text "pwd; ls -la; ps aux" --enter
shell read debug --history 100
shell read debug --screen delta
shell keys debug Ctrl-C
shell resize debug 140 40
shell status debug
shell upload debug .\script.sh /tmp/script.sh
shell download debug /var/log/app.log .\app.log
shell close debug
```

After `open SESSION HOST`, use the session name alone for `show`, `status`, `read`, `send`, `keys`, `resize` and `close`. The client resolves the host from its local session records; it does not start another SSH connection. Session names are unique across all hosts on this client. Opening a name held by a running owner returns `conflict` before starting SSH. Records whose owners have stopped are removed automatically, so their names can be reused after a crash or PC restart. `doctor` takes a host. `list` lists all local sessions with their hosts; `list HOST` filters by host. With `--profile`, an omitted list host uses that profile's host. Existing `COMMAND HOST SESSION ...` forms for commands other than `open` remain supported. If older records contain duplicate names, use `--host HOST` to select one and close it. For `keys`, an ambiguous host/session interpretation requires `--host`.

Commands select the current session by name. The client automatically includes its internal instance identifier in terminal mutations, so a session replaced during an IPC request is rejected. Names contain 1–64 ASCII letters, digits, `_` or `-`. Hosts are OpenSSH aliases or `user@host`; use `--port` or an alias for a nondefault port and an alias for IPv6 addresses.

`open` starts one background owner on the local machine and one persistent SSH connection. Terminal commands contact that owner; the shell, current directory, environment variables and foreground application are preserved. `upload` and `download` use a separate SFTP connection with the saved settings. `list` discovers this user's sessions on this client. `close` terminates SSH and removes the local session record.

`open` defaults to 120 columns × 40 rows. Override these dimensions with `--width` and `--height`, or use `resize` for an existing terminal. Programs can still truncate their own tabular output to the terminal width.

## Command execution for scripts

```text
shell exec lab --text "find /srv/app -maxdepth 1 -type f" --format json
shell exec --profile lab --file ./diagnostic.sh --format json
shell exec lab --stdin --format json
```

`exec [HOST]` starts a separate noninteractive SSH connection with no PTY and waits for SSH to exit and both output pipes to close. It accepts one command source: `--text`, `--stdin` (read the command from local stdin), or `--file`, up to 16 KiB of UTF-8. The command runs through the remote login shell; quote it for the local shell as usual. Remote stdin is closed. Connection profiles, identities, ports, jump hosts and forwarding options work as for `doctor`.

The response contains separate `stdout` and `stderr` strings, `exit_code` and `completed`. Whitespace, newlines and long lines are preserved without terminal wrapping or scrollback limits. Capture is bounded to 1 MiB **per stream**, with `truncated: true` if either exceeds it; excess output is drained so commands can finish. Non-UTF-8 bytes are replaced with the Unicode replacement character. Default Markdown output uses `###stdout` and `###stderr`, followed by metadata; use `--format json` for automation.

`completed: true` means SSH returned a remote exit code (0–254) and its output pipes closed. The CLI exits 0 for remote success and 3 for a nonzero remote code, keeping that code in `exit_code` and both streams in the response. A transport failure, interruption or deadline returns `completed: false`, `exit_code: null`, `delivery: unknown` and the output captured so far. If SSH could not start, delivery is `not_sent` and the streams are empty. OpenSSH reserves 255 for errors, so a remote command that exits 255 is also treated as an unknown transport outcome. A deadline returns CLI exit 6 and stops the local SSH process; it does not establish whether remote work stopped. Commands are never retried automatically.

`exec` uses a host or profile, not a session name, and does not access the local session directory. It starts with a fresh remote shell environment and directory; it cannot inherit a persistent terminal's `cd`, variables, container shell or database transaction. Put required setup in the command, for example `cd /srv/app && ls -la`. Use `send` and `keys` for interactive programs; their `wait_status: quiet` remains a screen stabilization indication, not command completion. For automation requiring a definite command result, use `exec`.

## Local configuration inspection

```text
shell profiles list
shell --config config.json profiles list
shell config show --profile lab
shell config show user@other --profile lab --port 2222
```

These commands validate the discovered configuration and never start SSH or read private key contents. `profiles list` reports profile names and hosts. `config show [HOST]` reports the source file, selected profile, explicit CLI overrides and effective shell-cli connection settings. Scalar CLI options and explicit host override the profile; forwarding options append. Relative identity and SSH config paths are shown as absolute paths, matching session startup. OpenSSH configuration aliases, `~` expansion, agent identities and OpenSSH defaults are **not** expanded. With no configuration file, `config_path` is null. JSON output includes the structured `configuration` field; the default Markdown output displays the same report in the screen section.

## SSH keys and connection profiles

Use `--identity` / `-i` for a private key, including an OpenSSH-compatible PEM file:

```powershell
shell open debug user@lab -i "$env:USERPROFILE\.ssh\example_key.pem" -p 2222
```

Encrypted keys must be unlocked in your SSH agent before opening a session. On Windows, enable the agent once in an administrator PowerShell, then load the key from your normal PowerShell:

```powershell
# Administrator PowerShell: one-time service setup
Get-Service ssh-agent | Set-Service -StartupType Automatic
Start-Service ssh-agent

# Normal PowerShell: ssh-add prompts for the key's passphrase
ssh-add "$env:USERPROFILE\.ssh\example_key.pem"
shell doctor user@lab -i "$env:USERPROFILE\.ssh\example_key.pem"
shell open debug user@lab -i "$env:USERPROFILE\.ssh\example_key.pem"
```

The [Windows OpenSSH key guide](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement) documents agent setup; [ssh-add](https://man.openbsd.org/ssh-add) handles the passphrase prompt. shell-cli does not accept or store passwords or key passphrases. Password-only login and interactive authentication prompts are unsupported because connections use `BatchMode=yes`. Verify the host key through your normal trusted SSH setup first.

Without `--config`, connection settings are loaded from `.config/shell-cli/config.json` in the current working directory first, then from `~/.config/shell-cli/config.json` (`$env:USERPROFILE\.config\shell-cli\config.json` on Windows) if the local file is absent. `XDG_CONFIG_HOME`, when set, replaces `~/.config` for the user file. The first file found is used without merging profiles; an unreadable or invalid local file returns an error. `--config PATH` selects only that file and takes precedence over both locations.

A generic example is included at [config.example.json](config.example.json). Copy it to the ignored local configuration file, then edit its `lab` profile before connecting:

```powershell
New-Item -ItemType Directory -Force .\.config\shell-cli | Out-Null
Copy-Item .\config.example.json .\.config\shell-cli\config.json
# Edit .config/shell-cli/config.json before connecting.
shell open debug --profile lab
```

Alternatively, copy [config.example.json](config.example.json) to the ignored `config.json`, then replace `host` and `identity` with your SSH destination and private key path. Set `identity` to `null` to use your SSH agent or OpenSSH defaults. Paths in JSON are literal: use an absolute path rather than `$env:USERPROFILE` or `$HOME`. Forward slashes work on Windows; backslashes must be escaped as `\\`.

```powershell
Copy-Item .\config.example.json .\config.json
# Edit config.json before connecting.
shell --config .\config.json doctor --profile lab
shell --config .\config.json open debug --profile lab
```

For a connection through a jump host with a local PostgreSQL tunnel:

```json
{
  "profiles": {
    "lab": {
      "host": "user@lab",
      "identity": "C:/path/to/private_key",
      "port": 2222,
      "jump": "user@bastion",
      "local_forward": ["127.0.0.1:15432:127.0.0.1:5432"],
      "remote_forward": [],
      "dynamic_forward": [],
      "ssh_config": "C:/path/to/ssh_config"
    }
  }
}
```

```powershell
shell open debug --profile lab
shell open staging user@staging --profile lab --port 22
shell --config .\connections.json open debug --profile lab
```

Every profile field is optional. Explicit host and scalar CLI options override the selected profile; repeated forwarding options append to the profile's forwarding lists. Unknown profile fields and invalid options fail validation. The default file may be absent; an explicit `--config` must be readable. Connection options apply when starting SSH with `open` or `doctor`; changing them does not alter an existing session.

## SSH tunnels

`--jump` / `-J`, `--local-forward` / `-L`, `--remote-forward` / `-R`, `--dynamic-forward` / `-D` and `--ssh-config` / `-F` map to the corresponding [OpenSSH options](https://man.openbsd.org/ssh). Forwarding options may be repeated. The CLI supports TCP forwarding specifications:

```powershell
# Local port 15432 reaches PostgreSQL from the remote host
shell open database user@lab -L "127.0.0.1:15432:127.0.0.1:5432"
# Remote loopback port 18080 reaches a service on this computer
shell open preview user@lab -R "127.0.0.1:18080:127.0.0.1:8080"
# Local SOCKS proxy, reached through a jump host
shell open proxy user@lab -J user@bastion -D "127.0.0.1:1080"
```

These examples explicitly bind listening ports to loopback. Remote forwarding depends on server policy. Tunnels belong to that session's existing SSH connection and end when it closes or disconnects. A forwarding listener setup failure fails startup (`ExitOnForwardFailure=yes`); this does not establish that the final forwarded destination is reachable. See the [OpenSSH forwarding settings](https://man.openbsd.org/OpenBSD-current/man/ssh_config). `doctor` also applies forwarding options and can encounter a port already used by an open session. Configure jump-host keys and ports in your OpenSSH config, since destination options need not apply to jump hosts.

## Debugging Docker

Connect to the host that runs Docker and use its normal commands:

```text
shell send debug --text "docker ps -a" --enter
shell read debug
shell send debug --text "docker logs --tail 200 app" --enter
shell read debug --history 200
shell send debug --text "docker inspect app" --enter
shell read debug --history 200
shell send debug --text "docker exec -it app sh" --enter
shell read debug
shell send debug --text "pwd; ls -la; ps" --enter
```

Replace `app` with the actual container name. Once inside the container, commands run there until `exit`/Ctrl-D. Use `docker logs -f app` for following logs and Ctrl-C to stop following. Interactive programs are limited by what is installed in that container; for example a minimal image may lack a shell or `ps`. This CLI does not install anything in the host or container.

## psql and vi

Launch `psql` in the session, then send SQL statements and read their output between steps. `BEGIN`, queries and an explicit `COMMIT`/`ROLLBACK` all reach the same running `psql` process. Test with a disposable database. An input acknowledgement does not establish that SQL succeeded. Database timeouts, a lost database connection or an ended SSH session can terminate the transaction; there is no automatic commit, rollback or replay.

For editing, send `vi /tmp/example.txt` with `--enter`, send `i` without `--enter`, send the desired text, send `keys ... Escape`, and finally send `:wq` with `--enter`. `read` returns the current visible screen, including interactive editor screens, and cursor coordinates. A line longer than the terminal width can occupy several display rows without adding a newline to the file. `row_wrapped` identifies terminal soft wraps; it cannot recover actual file line boundaries from vi's screen redraws. In Vim, `:set nowrap` changes display wrapping; `:set textwidth=0 wrapmargin=0` disables automatic line breaking from those settings ([Vim option reference](https://vimhelp.org/options.txt.html#%27wrap%27)). Inspect the saved file to establish its actual bytes and newlines.

## Input and output

- `send` accepts exactly one of `--text`, `--stdin`, or `--file PATH`, up to 16 KiB of UTF-8. `--file` types a file's contents into the terminal; it is not a file upload.
- Text is sent directly to the terminal. `--enter` appends Enter. Embedded newlines can submit commands. NUL and other control characters are rejected except tab, LF and CR.
- `keys` supports Enter, Escape, Tab, Backspace, Delete, arrows, Home, End, PageUp, PageDown, Ctrl-C, Ctrl-D, Ctrl-Z and Ctrl-L (case insensitive). Literal editor keys such as `i` use `send`.
- `send` and `keys` normally wait for terminal output after input delivery, then 200 ms without further output, and return a screen snapshot. This is a screen stabilization heuristic, not remote command completion: an echoed command alone can satisfy it before the command produces results, and background output can satisfy it too. A silent command or key with no terminal reaction waits until the deadline. Use `exec` for a structured command result, or a command-specific completion marker and subsequent reads when work must run in the interactive session.
- `--async` on `send` and `keys` returns immediately after local PTY delivery acknowledgement, with `wait_status: skipped` and no snapshot. For example, `send debug --async --text "sleep 30" --enter` returns while `sleep` is still running.
- `read --history N` requests up to 2,000 prior screen rows. Output is a terminal snapshot, not an exhaustive log or a structured command result. Fast output can leave the bounded history. Use remote log files or targeted commands when complete output matters.
- `read`, synchronous `send` and synchronous `keys` default to `--screen delta`: appended cells/rows since the preceding snapshot that consumed the shared baseline, including an empty screen when nothing changed. Use `--screen full` on any of these commands for the whole visible screen. The first snapshot, repaint, editor screen, resize, lost bounded history or `read --history N` greater than zero can return a full snapshot automatically. Always inspect `screen_mode` (`full` or `delta`), included in ordinary metadata and JSON. The baseline is shared by reads and synchronous input responses for one session; use a single reader when consuming deltas. `status`, viewer refreshes and async acknowledgements do not consume it. Concurrent readers or input can affect another pending request's screen. `row_wrapped` is a Boolean array for physical rows in `screen` only, excluding history; an empty delta has an empty array.
- `wait_status` is always shown for acknowledged input: `quiet` means terminal output was observed followed by the quiet interval; `timeout` means the wait deadline arrived without that condition; `closed` means SSH exited during the wait; `skipped` means async delivery. None proves a remote command completed successfully. A wait timeout returns exit 6, `delivery: acknowledged`, an explicit error and the available screen, without killing, cancelling or replaying the command. Inspect that screen and poll `read` as needed.
- Older running owners may return only input acknowledgements, reported as `wait_status: unsupported`; reopen the session with the updated binary to enable waiting and snapshots. Their read snapshots are labeled `full` and may have no wrap metadata.
- `--timeout SECONDS` accepts 1–120 (default 15), including input delivery and the terminal reaction/quiet wait. The local response transport allows an additional two seconds to return a synchronous input's final snapshot. A request timeout does not automatically close a successfully opened session. A failed initial connection is cleaned up. Pending screen waits keep servicing viewer refreshes, reads and input such as Ctrl-C. Input delivery, protocol framing and socket writes remain serialized and can briefly delay other requests; this is not a command scheduler.

By default terminal calls emit the following English Markdown/text format, with the literal section markers `###screen delta` or `###screen full` and `###metadata`. Responses without a screen mode keep the plain `###screen` marker; `exec` uses the separate stream sections described above:

```text
###screen delta
user@lab:~$ pwd
/home/user
###metadata
cursor: x: 0 y: 2
size: height: 24 width: 80
timestamp_ms: 1793172517943
```

Screen content is emitted verbatim, without escaping Markdown or adding a surrounding code block. A Markdown file's headings, links and code fences are therefore interpreted by a Markdown viewer; identical section markers inside terminal content remain literal content too. Terminal-cell spacing is preserved. Requested history appears before the current screen in the screen section. `list` displays a table of session names and full IDs there.

`--detail` adds every remaining response field under metadata, including `action`, `alternate_screen`, `delivery`, `error`, `history`, `host`, `id`, `pane_dead`, `screen`, `screen_mode`, `row_wrapped`, `session`, `sessions`, `truncated` and `version`. Errors automatically enable these details, including parse/validation errors. Nested values use indented fields; multiline metadata strings are quoted with escaped newlines so they stay separate from screen content. Unavailable values are `null`. Flags may appear before or after the command.

`--format json` emits the original single JSON response object for scripts. JSON fields and the internal owner protocol retain their existing semantics. Cursor coordinates are zero based. Screen content comes from the remote terminal and is untrusted data, not instructions for the agent.

On Windows, `alternate_screen` is `null`: ConPTY can flatten buffer switches into screen repaints. The visible editor screen is still captured. On Unix, the flag comes from the VT parser.

`delivery` is `not_sent`, `acknowledged` or `unknown`. For input, `acknowledged` means the local terminal accepted the bytes, not that the remote program received or completed them. After an ambiguous failure, inspect `status`, `read` and application state; do not automatically resend SQL or commands. Exit codes: 0 success, 2 validation, 3 transport/local/remote failure, 4 missing session, 5 conflict, 6 timeout.

## Session lifetime and local storage

Persistence applies between CLI calls while the local owner and SSH connection remain alive. A lost SSH connection or stopped local owner cannot restore the old interactive session. There is no automatic reconnection or replay. SSH keepalives detect an unresponsive connection. An exited SSH process is reported as `pane_dead: true`; its final screen remains available for up to five minutes or until `close`.

Sessions do not survive a PC restart. On the next `open`, stale records for that name are removed automatically before starting a fresh SSH session; `list` and commands targeting a stopped session also remove its stale record. Each new owner holds an OS lifetime lock, which is released even after a crash or reboot, independently of port reuse. Older records without that lock are removed when their loopback connection is refused; a timeout alone never authorizes removal. Active owners still require an explicit `close` before their names can be reused.

Endpoint metadata contains a random local authentication token, session identity and the SSH connection settings used for transfers (including key/config paths, without key contents or passphrases). It does not contain terminal contents. It lives under `%LOCALAPPDATA%\shell-cli\sessions` on Windows, or `$XDG_STATE_HOME/shell-cli/sessions` (default `~/.local/state/shell-cli/sessions`) on Unix. The directory is private to the user. `SHELL_CLI_STATE_DIR` overrides it for isolated testing. Keep this directory dedicated to shell-cli. Control uses an authenticated loopback socket; input is serialized per session. Use one controller per session.

On Windows, shell-cli uses `icacls.exe` to restrict the session directory's permissions before storing credentials. An error beginning `cannot protect session directory` now includes the directory, Windows account, operation, exit status and diagnostic output. A sandbox or restricted process can deny this ACL change even when ordinary file writes are allowed. Retry outside the sandbox under the same Windows account, or set `SHELL_CLI_STATE_DIR` to a dedicated directory where that account can change permissions. The CLI keeps the permission requirement in place. `exec` and `doctor` do not need this session directory.

Terminal output stays in bounded memory and is not logged by the CLI. Command-line `--text` arguments can be visible in local process listings; stdin avoids putting input in the CLI arguments. Remote shell history and application logging follow their own configuration.

## File transfers

Upload or download files and directory trees using the settings saved when the session was opened:

```powershell
shell upload debug .\script.sh /tmp/script.sh
shell download debug /var/log/app.log .\app.log
shell upload debug .\config.json /srv/app/config.json --overwrite
shell download debug /tmp/result.bin .\result.bin --timeout 120
shell upload debug .\assets /srv/app/assets --recursive
shell download debug /srv/app/assets .\assets --recursive
shell upload debug .\config.json .\script.sh /srv/app/
shell download debug /tmp/result.bin /var/log/app.log .\results
shell upload debug .\assets .\config.json /srv/app/ -r --overwrite
```

The commands reuse the session's SSH executable, host, identity, port, jump host and SSH config path. Connection profiles are resolved at `open`; transfer calls use those saved settings even from another local directory. They start a separate SFTP v3 connection, with host-key checking and noninteractive authentication, without reopening tunnels or changing the terminal. Transfers work while `vi`, `psql` or another program is in the foreground. The server must enable the SFTP subsystem. Sessions created by an older binary must be closed and reopened to save their connection settings.

Place one or more sources before the final destination. With one source, the destination is its exact target path: copying `assets` to `/srv/app/static` puts its contents directly inside `static`. With multiple sources, the destination is a directory and each source keeps its basename; `assets` and `config.json` copied to `/srv/app` become `/srv/app/assets` and `/srv/app/config.json`. Directory sources require `--recursive` (`-r`). Destination roots and copied subdirectories are created as needed, including empty directories; the destination root's parent must already exist. Existing directories are merged, and unrelated destination files remain in place. Sources that map to the same destination are rejected.

Remote paths must be absolute, without `.`/`..` components. Trailing slashes are accepted for recursive or multiple-source commands; single-file commands still require a filename. Local paths are relative to the caller's working directory. No wildcard or home-directory expansion is performed by shell-cli. Spaces, quotes and shell metacharacters in remote paths are sent literally through SFTP. Recursive sources must contain regular files and directories; symlinks and special files are rejected. Single-file downloads may still read a symlink to a regular file. Destination symlinks are rejected. Recursive and multiple-source downloads reject names that cannot be represented safely on the local platform, including Windows device names and alternate data streams. New remote files use mode `0600` and directories use `0700` (subject to the server's umask); source permissions and timestamps are not copied. A command is limited to 100000 entries and 128 nested directory levels.

Existing destination files return `conflict` (exit 5) unless `--overwrite` is supplied. Exclusive file creation also prevents overwriting a destination created concurrently. `--overwrite` replaces the contents of an existing regular file. Files are transferred sequentially over one SFTP connection, with one file open at a time. The command stops at the first error and does not retry. Transfers write directly to their destination: an error, timeout or disconnect can leave previously copied files, created directories and a partial current file; overwriting is not rolled back. Inspect the destination before retrying. `--timeout` bounds the whole operation (default 15 seconds, maximum 120), not each individual file; it terminates the transfer SSH process on expiry. The session terminal remains available. The selected owner is authenticated before starting the transfer; once started, a transfer uses its selected settings independently of the terminal's lifetime.

Success reports file, directory and byte totals in the screen and a `transfer` object in JSON and Markdown metadata: `source` (first selected source), `sources` (all selected sources), `destination`, `bytes`, `files` and `directories` (including the destination root when it is a directory). Success means every copy and file close completed. Ambiguous failures after the destination may have changed report `delivery: unknown`. For a file inside a container, use `docker cp` between the container and remote host before or after the transfer. `send --file` remains terminal text input.

## Validation

```powershell
cargo fmt --check
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
.\tests\ssh_smoke.ps1 -SshHost your_user@localhost -CliPath .\target\debug\shell.exe
.\tests\sync_wait.ps1 -SshHost your_user@localhost -CliPath .\target\debug\shell.exe
.\tests\transfers.ps1 -SshHost your_user@localhost -CliPath .\target\debug\shell.exe
```

The SSH smoke test requires `vi` and standard Unix tools. It checks failed startup, creates a unique session, verifies persistent shell state, reading, remote resizing, Ctrl-C, editor display and saved file contents, and disconnect behavior. It closes the session in `finally`. `sync_wait.ps1` uses a separate session and state directory to check quiet snapshots, silent and continuous-output timeout snapshots, and viewer/Ctrl-C responsiveness during a pending synchronous request. The Windows → WSL release test passed; PostgreSQL transactions and a real Docker container still need suitable test access. See `PLAN.md` for recorded validation and remaining limits.

## Visible Windows terminal

`shell open debug lab` starts without a window. Use
`shell open debug lab --headed` to open a separate Windows console, or
`shell show debug` to display an existing session. Both commands return
the usual response immediately after launching the display process. The display
authenticates to the existing local owner using its private metadata and checks
the session UUID; it never starts SSH or a remote shell.

Type normally in the window, including Ctrl-C, arrow keys and editor keys.
Ctrl-] detaches. Closing the console window also detaches; `shell close debug`
terminates the SSH session. The visible console reflects the owner's VT screen,
including colors and cursor, refreshed about every 40 ms when IPC is idle.
It follows the console's visible dimensions (within the CLI size limits).
While attached, it reapplies those dimensions after an agent resize. Multiple
windows can display the same session, but their dimensions compete; prefer one
window per session.

Human input chunks and agent requests are serialized by the owner. They are
**not** separate transactions or exclusive control: typing while an agent sends
a command can interleave text in the same command line or application. Coordinate
who is typing, especially in a database transaction. The viewer does not replay
input after a timeout. Screen refreshes are snapshots, so short-lived output,
bells and terminal features not represented by the VT parser may not appear.
The display has no separate scrollback; use `read --history`. Windows console
support for Unicode, function keys and escape sequences depends on the host.
Visible windows are currently Windows-only; the session owner remains usable
without a window on other supported platforms.

A successful `show` means the console process was launched, not that it finished
attaching: attachment failures are printed in that console. If headed launch
fails after `open`, the created session remains available through `status`.
An SSH disconnect ends the remote session; opening a display does not restore
remote processes, editor state or transactions.

The SSH smoke test accepts -Headed to exercise visible process launch with
open --headed, show, shared viewer input and display termination without
closing SSH. It also keeps a display attached while editing and saving with vi,
and checks that the display survives returning to the shell.
Set SHELL_CLI_STATE_DIR to a temporary isolated directory.
The automated smoke test sends input through the viewer IPC path and terminates
display processes; physical keyboard input and clicking the window close button
remain manual GUI checks.

`tests/ipc_delay.ps1` checks a delayed local request, reproducing the Windows
socket-inheritance bug that could close displays unexpectedly. Accepted local
sockets now explicitly use blocking reads with a timeout so they wait for the
request bytes. An already-running session owner keeps its loaded executable;
open a new session after updating the binary to pick up this fix.
