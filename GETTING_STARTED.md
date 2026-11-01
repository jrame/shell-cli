# Getting started

Build and install the command from this directory:

```text
cargo install --path .
shell --version
```

You need OpenSSH, key or agent authentication, and a host key already verified through your trusted SSH setup. The remote machine needs an SSH server and a POSIX shell. Authentication is noninteractive; unlock encrypted keys with `ssh-add` first.

Check access, then open a persistent terminal (replace `lab` with your SSH alias or `user@host`):

```text
shell doctor lab
shell open debug lab
shell list
```

Use the session name for subsequent operations:

```text
shell send debug --text "pwd" --enter
shell read debug
shell keys debug Ctrl-C
shell close debug
```

`send` waits for terminal activity to become quiet; this does not prove that the remote command has finished. After an ambiguous delivery failure, inspect the session before deciding whether to send again.

For a command with a definite result and output that is independent of terminal dimensions, use:

```text
shell exec lab --text "ls -la /srv/app" --format json
```

The JSON response includes `stdout`, `stderr`, `exit_code` and `completed`. `exec` opens a separate connection with a fresh shell; it does not inherit the terminal session's directory or environment. Interactive terminals now default to 120 × 40; adjust with `open --width N --height N` or `resize`.

On Windows, `cannot protect session directory` means the directory's ACL could not be restricted. The error includes the path and account. A sandbox can deny this operation: retry outside it under the same account, or choose a dedicated directory with `SHELL_CLI_STATE_DIR` where permission changes are allowed.

The session keeps its current directory, environment and foreground program between calls. Commands after `open` can use the session name alone. `list` shows all sessions on this client; `list lab` filters by host. Default output is Markdown; add `--format json` for scripts.

For saved connection settings, copy [config.example.json](config.example.json) to `.config/shell-cli/config.json` (ignored by Git), edit the local copy, then inspect and use a profile:

```text
shell profiles list
shell config show --profile lab
shell open debug --profile lab
```

Use an absolute identity path in JSON, or null to use OpenSSH/agent defaults. On Windows, select the real OpenSSH executable with `--ssh C:\Windows\System32\OpenSSH\ssh.exe` if needed. See the [full reference](README.md) for configuration discovery, forwarding, transfers, screen modes, viewer windows and error handling.

Copy a directory tree with `shell upload debug .\assets /srv/app/assets -r`, or select several files with `shell upload debug .\config.json .\script.sh /srv/app/`. Downloads use the same source(s)-then-destination order: `shell download debug /srv/app/assets .\assets -r`. Files are transferred one at a time within the command.
