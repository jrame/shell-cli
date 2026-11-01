---
name: shell-cli
description: "Run SSH commands, persistent remote terminals, and SFTP transfers with shell-cli (shell). Use for remote work through this CLI, not local shell commands or development of the CLI itself."
---

# Shell CLI

Use `shell` (`shell.exe` on Windows). Replace the example host `lab`, session `debug`, and paths with the selected targets. Use `--help` for other options; add `--format json` for structured responses.

## Discover connections and sessions

OpenSSH aliases and connection settings from `~/.ssh/config` are supported; use `--ssh-config PATH` to select another OpenSSH config file.

```text
shell profiles list
shell config show --profile lab
shell doctor lab
shell list
shell status debug
```

## Standalone commands

```text
shell exec lab --text "cd /srv/app && ls -la" --format json
shell exec --profile lab --file ./diagnostic.sh --format json
shell exec lab --stdin --format json
```

Each `exec` starts a fresh shell; it does not inherit terminal state. Check `completed`, `exit_code`, and `truncated`; capture is limited to 1 MiB per stream.

## Persistent terminal

```text
shell open debug lab
shell open debug --profile lab
shell send debug --text "cd /srv/app" --enter
shell send debug --text "pwd; ls -la; ps aux" --enter
shell read debug --history 100
shell read debug --screen full
shell keys debug Ctrl-C
shell resize debug 140 40
shell close debug
```

The two `open` examples are alternatives; names are unique across hosts. Keep stateful work in the same session. Check the foreground program before typing or interrupting. Close task-only sessions when finished.

`send` / `keys` returning `quiet` or `acknowledged` does not prove completion. Inspect results or a completion marker, then `read` as needed. After uncertain delivery or timeout, inspect state before retrying; no automatic replay. SSH loss cannot restore the previous session.

Reads and synchronous input responses share a delta baseline; use one coordinated reader. Request `--screen full` for context. Screen/history capture is bounded, and terminal wrapping is not a file newline.

## File transfers

```text
shell upload debug ./script.sh /tmp/script.sh
shell download debug /var/log/app.log ./app.log
shell upload debug ./assets /srv/app/assets --recursive
shell download debug /srv/app/assets ./assets --recursive
shell upload debug ./config.json ./script.sh /srv/app/
shell upload debug ./config.json /srv/app/config.json --overwrite
```

Transfers use a separate SFTP connection with the session's saved settings. Remote paths must be absolute; no wildcard/home expansion. One source uses an exact target path; multiple sources retain their basenames inside the destination directory. Errors can leave partial files; inspect before retrying. `send --file` types text, not file bytes.

## Visible terminal (Windows)

```text
shell open debug lab --headed
shell show debug
```

Use when a visible window is requested. Viewer and agent input can interleave. Detaching keeps SSH alive; `close` ends it.
