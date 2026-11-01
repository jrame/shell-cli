use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

mod background;
mod config;
mod exec;
mod output;
mod session;
mod transfer;

const MAX_OUTPUT: usize = 1_048_576;
const MAX_INPUT: usize = 16_384;

#[derive(Parser)]
#[command(
    name = "shell",
    bin_name = "shell",
    version,
    about = "Persistent SSH terminal sessions for agents"
)]
struct Cli {
    #[arg(long, global = true, default_value = "ssh")]
    ssh: PathBuf,
    #[arg(long, global = true, default_value_t = 15)]
    timeout: u64,
    #[command(flatten)]
    output: output::Options,
    #[command(flatten)]
    connection_options: config::Options,
    #[arg(skip)]
    connection: config::Connection,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand, Serialize, Deserialize, Clone)]
enum Action {
    /// Run a command over a separate SSH connection without a terminal; wait for its exit.
    Exec {
        #[arg(default_value = "")]
        host: String,
        #[arg(long, conflicts_with_all = ["stdin", "file"])]
        text: Option<String>,
        #[arg(long, conflicts_with = "file")]
        stdin: bool,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Inspect configured connection profiles without connecting.
    Profiles {
        #[command(subcommand)]
        command: ProfilesAction,
    },
    /// Inspect shell-cli connection settings without connecting.
    Config {
        #[command(subcommand)]
        command: ConfigAction,
    },
    /// Upload local files or directory trees over a separate SFTP connection.
    Upload {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
        #[arg(required = true, num_args = 1..)]
        local: Vec<PathBuf>,
        /// Absolute destination path; a directory for multiple sources.
        remote: String,
        #[arg(short, long)]
        #[serde(default)]
        recursive: bool,
        // Internal instance guard; kept in the owner protocol.
        #[arg(skip)]
        id: Option<String>,
        #[arg(long)]
        overwrite: bool,
    },
    /// Download remote files or directory trees over a separate SFTP connection.
    Download {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
        /// Absolute source paths.
        #[arg(required = true, num_args = 1..)]
        remote: Vec<String>,
        local: PathBuf,
        #[arg(short, long)]
        #[serde(default)]
        recursive: bool,
        // Internal instance guard; kept in the owner protocol.
        #[arg(skip)]
        id: Option<String>,
        #[arg(long)]
        overwrite: bool,
    },
    Doctor {
        #[arg(default_value = "")]
        host: String,
    },
    Show {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
    },
    #[command(hide = true)]
    View {
        host: String,
        session: String,
        id: String,
        #[arg(long, value_delimiter = ',')]
        bytes: Vec<u8>,
        #[arg(long)]
        width: u16,
        #[arg(long)]
        height: u16,
    },
    Open {
        session: String,
        #[arg(default_value = "")]
        host: String,
        #[arg(long)]
        headed: bool,
        #[arg(long, default_value_t = 120)]
        width: u16,
        #[arg(long, default_value_t = 40)]
        height: u16,
    },
    /// List local sessions, optionally filtered by host (or selected profile).
    List {
        #[arg(default_value = "")]
        host: String,
    },
    Status {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
    },
    Read {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
        #[arg(long, default_value_t = 0)]
        history: u16,
        #[arg(long, value_enum, default_value = "delta")]
        #[serde(default = "legacy_screen_mode")]
        screen: ScreenMode,
    },
    Send {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
        // Internal instance guard; kept in the owner protocol.
        #[arg(skip)]
        id: Option<String>,
        /// Acknowledge local input delivery immediately, without a screen snapshot.
        #[arg(long = "async")]
        #[serde(default)]
        async_mode: bool,
        #[arg(long, value_enum, default_value = "delta")]
        #[serde(default)]
        screen: ScreenMode,
        #[arg(long, conflicts_with_all = ["stdin", "file"])]
        text: Option<String>,
        #[arg(long, conflicts_with = "file")]
        stdin: bool,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        enter: bool,
    },
    Keys {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
        // Internal instance guard; kept in the owner protocol.
        #[arg(skip)]
        id: Option<String>,
        /// Acknowledge local input delivery immediately, without a screen snapshot.
        #[arg(long = "async")]
        #[serde(default)]
        async_mode: bool,
        #[arg(long, value_enum, default_value = "delta")]
        #[serde(default)]
        screen: ScreenMode,
        keys: Vec<String>,
    },
    Resize {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
        // Internal instance guard; kept in the owner protocol.
        #[arg(skip)]
        id: Option<String>,
        width: u16,
        height: u16,
    },
    Close {
        #[arg(long, default_value = "")]
        host: String,
        session: String,
        // Internal instance guard; kept in the owner protocol.
        #[arg(skip)]
        id: Option<String>,
    },
}

#[derive(Subcommand, Serialize, Deserialize, Clone)]
enum ProfilesAction {
    List,
}

#[derive(Subcommand, Serialize, Deserialize, Clone)]
enum ConfigAction {
    /// Show effective shell-cli settings; OpenSSH configuration is not expanded.
    Show {
        #[arg(default_value = "")]
        host: String,
    },
}

#[derive(Clone, Copy, Default, ValueEnum, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum ScreenMode {
    Full,
    #[default]
    Delta,
}

// Old clients omitted the read mode and expected a complete screen.
fn legacy_screen_mode() -> ScreenMode {
    ScreenMode::Full
}

#[derive(Serialize, Deserialize)]
struct Reply {
    version: u8,
    action: String,
    host: String,
    session: Option<String>,
    id: Option<String>,
    delivery: String,
    timestamp_ms: u128,
    screen: Option<String>,
    #[serde(default)]
    screen_mode: Option<String>,
    #[serde(default)]
    wait_status: Option<String>,
    #[serde(default)]
    row_wrapped: Option<Vec<bool>>,
    cursor: Option<Cursor>,
    size: Option<Size>,
    pane_dead: Option<bool>,
    alternate_screen: Option<bool>,
    history: Option<String>,
    truncated: bool,
    sessions: Option<Vec<Session>>,
    error: Option<Problem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transfer: Option<transfer::Report>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    configuration: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completed: Option<bool>,
}
#[derive(Serialize, Deserialize)]
struct Cursor {
    x: u16,
    y: u16,
}
#[derive(Serialize, Deserialize)]
struct Size {
    width: u16,
    height: u16,
}
#[derive(Serialize, Deserialize)]
struct Session {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    name: String,
    id: String,
}
#[derive(Serialize, Deserialize)]
struct Problem {
    code: String,
    message: String,
}
impl Reply {
    fn new(action: &str, host: &str, session: Option<&str>) -> Self {
        Self {
            version: 1,
            action: action.into(),
            host: host.into(),
            session: session.map(str::to_owned),
            id: None,
            delivery: "not_sent".into(),
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            screen: None,
            screen_mode: None,
            wait_status: None,
            row_wrapped: None,
            cursor: None,
            size: None,
            pane_dead: None,
            alternate_screen: None,
            history: None,
            truncated: false,
            sessions: None,
            error: None,
            transfer: None,
            configuration: None,
            stdout: None,
            stderr: None,
            exit_code: None,
            completed: None,
        }
    }
}
#[derive(Debug)]
struct Fault {
    code: &'static str,
    message: String,
    exit: i32,
    unknown: bool,
}
fn fault(code: &'static str, message: impl Into<String>, exit: i32) -> Fault {
    Fault {
        code,
        message: message.into(),
        exit,
        unknown: false,
    }
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn valid_host(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_' || b == b'@')
        && s.matches('@').count() <= 1
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn lines(out: &[u8]) -> String {
    String::from_utf8_lossy(out)
        .trim_end_matches(['\n', '\r'])
        .to_owned()
}

struct Run {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: i32,
    truncated: bool,
}
fn drain<R: Read>(mut r: R) -> (Vec<u8>, bool) {
    let mut result = Vec::new();
    let mut truncated = false;
    let mut buf = [0; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = MAX_OUTPUT.saturating_sub(result.len());
                result.extend_from_slice(&buf[..n.min(room)]);
                truncated |= n > room;
            }
            Err(_) => break,
        }
    }
    (result, truncated)
}
fn run(
    ssh: &PathBuf,
    connection: &config::Connection,
    host: &str,
    script: &str,
    timeout: Duration,
    mutation: bool,
) -> Result<Run, Fault> {
    let mut child = Command::new(ssh)
        .args(connection.args())
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            host,
            script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| fault("transport", format!("cannot start SSH: {e}"), 3))?;
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn({
        let pipe = child.stdout.take().unwrap();
        move || {
            let _ = out_tx.send(drain(pipe));
        }
    });
    thread::spawn({
        let pipe = child.stderr.take().unwrap();
        move || {
            let _ = err_tx.send(drain(pipe));
        }
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(255),
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Fault {
                    code: "timeout",
                    message: "SSH deadline exceeded; remote outcome may be unknown".into(),
                    exit: 6,
                    unknown: mutation,
                });
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Fault {
                    code: "transport",
                    message: e.to_string(),
                    exit: 3,
                    unknown: mutation,
                });
            }
        }
    };
    let remaining = || timeout.saturating_sub(start.elapsed());
    let (stdout, a) = out_rx.recv_timeout(remaining()).map_err(|_| Fault {
        code: "timeout",
        message: "SSH output pipe did not close before deadline".into(),
        exit: 6,
        unknown: mutation,
    })?;
    let (stderr, b) = err_rx.recv_timeout(remaining()).map_err(|_| Fault {
        code: "timeout",
        message: "SSH error pipe did not close before deadline".into(),
        exit: 6,
        unknown: mutation,
    })?;
    if status == 255 {
        return Err(Fault {
            code: "transport",
            message: format!("SSH failed: {}", lines(&stderr)),
            exit: 3,
            unknown: mutation,
        });
    }
    Ok(Run {
        stdout,
        stderr,
        status,
        truncated: a || b,
    })
}
fn check(run: Run, mutation: bool) -> Result<Run, Fault> {
    if run.status == 0 {
        Ok(run)
    } else {
        let mut e = fault(
            "remote",
            format!(
                "remote command failed ({}): {}",
                run.status,
                lines(&run.stderr)
            ),
            3,
        );
        e.unknown = mutation;
        Err(e)
    }
}
fn size(w: u16, h: u16) -> Result<(), Fault> {
    if (20..=500).contains(&w) && (5..=200).contains(&h) {
        Ok(())
    } else {
        Err(fault(
            "validation",
            "size must be 20..500 columns and 5..200 rows",
            2,
        ))
    }
}
fn validate(host: &str, session: Option<&str>) -> Result<(), Fault> {
    if !valid_host(host) {
        return Err(fault("validation", "invalid SSH host or alias", 2));
    }
    if let Some(s) = session {
        if !valid_name(s) {
            return Err(fault("validation", "invalid session name", 2));
        }
    }
    Ok(())
}
fn input(
    cli: &Cli,
    text: Option<&String>,
    stdin: bool,
    file: Option<&PathBuf>,
) -> Result<String, Fault> {
    let data = if let Some(s) = text {
        s.as_bytes().to_vec()
    } else if stdin {
        let mut b = Vec::new();
        std::io::stdin()
            .take((MAX_INPUT + 1) as u64)
            .read_to_end(&mut b)
            .map_err(|e| fault("validation", e.to_string(), 2))?;
        b
    } else if let Some(p) = file {
        let mut b = Vec::new();
        File::open(p)
            .map_err(|e| fault("validation", e.to_string(), 2))?
            .take((MAX_INPUT + 1) as u64)
            .read_to_end(&mut b)
            .map_err(|e| fault("validation", e.to_string(), 2))?;
        b
    } else {
        return Err(fault("validation", "provide --text, --stdin, or --file", 2));
    };
    let _ = cli;
    if data.len() > MAX_INPUT {
        return Err(fault("validation", "input exceeds 16 KiB", 2));
    }
    let s = String::from_utf8(data).map_err(|_| fault("validation", "input must be UTF-8", 2))?;
    if s.chars()
        .any(|c| c == '\0' || (c.is_control() && c != '\n' && c != '\r' && c != '\t'))
    {
        return Err(fault(
            "validation",
            "unsupported control character; use keys",
            2,
        ));
    }
    Ok(s)
}

fn key(s: &str) -> Option<&'static str> {
    match s.to_ascii_lowercase().as_str() {
        "enter" => Some("\r"),
        "escape" | "esc" => Some("\x1b"),
        "tab" => Some("\t"),
        "backspace" => Some("\x7f"),
        "delete" => Some("\x1b[3~"),
        "up" => Some("\x1b[A"),
        "down" => Some("\x1b[B"),
        "right" => Some("\x1b[C"),
        "left" => Some("\x1b[D"),
        "home" => Some("\x1b[H"),
        "end" => Some("\x1b[F"),
        "pageup" => Some("\x1b[5~"),
        "pagedown" => Some("\x1b[6~"),
        "ctrl-c" => Some("\x03"),
        "ctrl-d" => Some("\x04"),
        "ctrl-z" => Some("\x1a"),
        "ctrl-l" => Some("\x0c"),
        _ => None,
    }
}

fn context(action: &Action) -> (&'static str, &str, Option<&str>) {
    match action {
        Action::Profiles { .. } => ("profiles", "", None),
        Action::Config {
            command: ConfigAction::Show { host },
        } => ("config", host, None),
        Action::Show { host, session } => ("show", host, Some(session)),
        Action::View { host, session, .. } => ("view", host, Some(session)),
        Action::Doctor { host } => ("doctor", host, None),
        Action::Exec { host, .. } => ("exec", host, None),
        Action::Open { host, session, .. } => ("open", host, Some(session)),
        Action::List { host } => ("list", host, None),
        Action::Status { host, session } => ("status", host, Some(session)),
        Action::Read { host, session, .. } => ("read", host, Some(session)),
        Action::Send { host, session, .. } => ("send", host, Some(session)),
        Action::Keys { host, session, .. } => ("keys", host, Some(session)),
        Action::Resize { host, session, .. } => ("resize", host, Some(session)),
        Action::Close { host, session, .. } => ("close", host, Some(session)),
        Action::Upload { host, session, .. } => ("upload", host, Some(session)),
        Action::Download { host, session, .. } => ("download", host, Some(session)),
    }
}

fn expected_id(action: &Action) -> Option<&str> {
    match action {
        Action::View { id, .. } => Some(id),
        Action::Send { id, .. }
        | Action::Keys { id, .. }
        | Action::Resize { id, .. }
        | Action::Upload { id, .. }
        | Action::Download { id, .. }
        | Action::Close { id, .. } => id.as_deref(),
        _ => None,
    }
}

fn validate_action(action: &Action) -> Result<(), Fault> {
    let (_, host, session) = context(action);
    if matches!(action, Action::Profiles { .. } | Action::Config { .. })
        || matches!(action, Action::List { host } if host.is_empty())
    {
        return Ok(());
    }
    if host.is_empty()
        && matches!(
            action,
            Action::Show { .. }
                | Action::Status { .. }
                | Action::Read { .. }
                | Action::Send { .. }
                | Action::Keys { .. }
                | Action::Resize { .. }
                | Action::Close { .. }
                | Action::Upload { .. }
                | Action::Download { .. }
        )
    {
        if !valid_name(session.unwrap()) {
            return Err(fault("validation", "invalid session name", 2));
        }
    } else {
        validate(host, session)?;
    }
    if expected_id(action).is_some_and(|id| Uuid::parse_str(id).is_err()) {
        return Err(fault("validation", "invalid session id", 2));
    }
    match action {
        Action::Upload {
            local,
            remote,
            recursive,
            ..
        } => {
            if !recursive && local.len() == 1 && remote.ends_with('/') {
                return Err(fault(
                    "validation",
                    "a single-file destination must be a filename without a trailing slash",
                    2,
                ));
            }
            for path in local {
                transfer::validate_paths(path, remote)?;
            }
        }
        Action::Download {
            local,
            remote,
            recursive,
            ..
        } => {
            if !recursive && remote.len() == 1 && remote[0].ends_with('/') {
                return Err(fault(
                    "validation",
                    "a single-file source must be a filename without a trailing slash",
                    2,
                ));
            }
            for path in remote {
                transfer::validate_paths(local, path)?;
            }
        }
        Action::Open { width, height, .. } | Action::Resize { width, height, .. } => {
            size(*width, *height)?
        }
        Action::Read { history, .. } if *history > 2000 => {
            return Err(fault("validation", "history is limited to 2000 lines", 2))
        }
        Action::Keys { keys, .. }
            if keys.is_empty() || keys.len() > 32 || keys.iter().any(|k| key(k).is_none()) =>
        {
            return Err(fault("validation", "provide 1..32 supported keys", 2));
        }
        _ => {}
    }
    Ok(())
}

fn execute(cli: &mut Cli, reply: &mut Reply) -> Result<(), Fault> {
    if !(1..=120).contains(&cli.timeout) {
        return Err(fault("validation", "timeout must be 1..120 seconds", 2));
    }
    let (connection, diagnostic) = cli.connection_options.resolve(&mut cli.command)?;
    cli.connection = connection;
    let (action, host, name) = context(&cli.command);
    *reply = Reply::new(action, host, name);
    validate_action(&cli.command)?;
    if let Some(report) = diagnostic {
        reply.screen = Some(
            serde_json::to_string_pretty(&report).map_err(|e| fault("local", e.to_string(), 3))?,
        );
        reply.configuration = Some(report);
        return Ok(());
    }
    if let Action::Exec {
        host,
        text,
        stdin,
        file,
    } = &cli.command
    {
        let script = input(cli, text.as_ref(), *stdin, file.as_ref())?;
        if script.trim().is_empty() {
            return Err(fault("validation", "command must not be empty", 2));
        }
        return exec::execute(cli, host, &script, reply);
    }
    if let Action::Send {
        text, stdin, file, ..
    } = &cli.command
    {
        let value = input(cli, text.as_ref(), *stdin, file.as_ref())?;
        if let Action::Send {
            text,
            stdin,
            file,
            enter,
            ..
        } = &mut cli.command
        {
            if value.is_empty() && !*enter {
                return Err(fault("validation", "empty text; use keys Enter", 2));
            }
            *text = Some(value);
            *stdin = false;
            *file = None;
        }
    }
    if let Action::Doctor { host } = &cli.command {
        let r = check(
            run(
                &cli.ssh,
                &cli.connection,
                host,
                "printf 'SSH connection OK\\n'",
                Duration::from_secs(cli.timeout),
                false,
            )?,
            false,
        )?;
        reply.screen = Some(lines(&r.stdout));
        reply.truncated = r.truncated;
        return Ok(());
    }
    session::resolve_host(&mut cli.command)?;
    let (action, host, name) = context(&cli.command);
    *reply = Reply::new(action, host, name);
    *reply = session::dispatch(cli)?;
    if reply.error.is_none()
        && matches!(
            cli.command,
            Action::Show { .. } | Action::Open { headed: true, .. }
        )
    {
        session::show(cli, reply.id.as_deref().unwrap())?;
    }
    Ok(())
}

fn apply_fault(reply: &mut Reply, e: Fault) -> i32 {
    if e.unknown {
        reply.delivery = "unknown".into();
    }
    reply.error = Some(Problem {
        code: e.code.into(),
        message: e.message,
    });
    e.exit
}
fn main() {
    if std::env::args().nth(1).as_deref() == Some("__serve-session") {
        session::serve();
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("__view-session") {
        session::view();
        return;
    }
    let raw_args = std::env::args_os().collect::<Vec<_>>();
    let fallback_output = output::Options::from_raw_args(&raw_args);
    let args = session::cli_args(raw_args);
    let mut cli = match args.and_then(Cli::try_parse_from) {
        Ok(cli) => cli,
        Err(e) => {
            let mut reply = Reply::new("parse", "", None);
            let informational = matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            );
            if informational {
                reply.screen = Some(e.to_string());
            } else {
                reply.error = Some(Problem {
                    code: "validation".into(),
                    message: e.to_string(),
                });
            }
            let _ = fallback_output.write(&mut std::io::stdout().lock(), &reply);
            std::process::exit(if informational { 0 } else { 2 });
        }
    };
    let (action, host, session) = context(&cli.command);
    let mut reply = Reply::new(action, host, session);
    let exit = match execute(&mut cli, &mut reply) {
        Ok(()) => reply.error.as_ref().map_or(0, |e| match e.code.as_str() {
            "validation" => 2,
            "session_not_found" => 4,
            "conflict" => 5,
            "timeout" => 6,
            _ => 3,
        }),
        Err(e) => {
            if e.unknown {
                reply.delivery = "unknown".into();
            }
            reply.error = Some(Problem {
                code: e.code.into(),
                message: e.message,
            });
            e.exit
        }
    };
    let mut stdout = std::io::stdout().lock();
    let _ = cli.output.write(&mut stdout, &reply);
    std::process::exit(exit);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_target_cli_forms() {
        for args in [
            vec!["shell", "read", "lab", "debug"],
            vec!["shell", "read", "--", "lab", "debug"],
            vec!["shell", "read", "debug", "--host", "lab"],
        ] {
            let cli = Cli::try_parse_from(
                session::cli_args(args.into_iter().map(Into::into).collect()).unwrap(),
            )
            .unwrap();
            assert!(
                matches!(cli.command, Action::Read { host, session, .. } if host == "lab" && session == "debug")
            );
        }
        let cli = Cli::try_parse_from(["shell", "open", "debug"]).unwrap();
        assert!(validate_action(&cli.command).is_err());
    }

    #[test]
    fn quote_roundtrip_shape() {
        assert_eq!(quote("a'b;$\n雪"), "'a'\\''b;$\n雪'");
    }
    #[test]
    fn new_cli_forms_and_protocol_defaults() {
        let cli = Cli::try_parse_from(["shell", "open", "debug", "lab"]).unwrap();
        assert!(
            matches!(cli.command, Action::Open { session, host, .. } if session == "debug" && host == "lab")
        );
        let cli =
            Cli::try_parse_from(["shell", "send", "debug", "--text", "hello", "--async"]).unwrap();
        assert!(matches!(
            cli.command,
            Action::Send {
                id: None,
                async_mode: true,
                ..
            }
        ));
        let cli = Cli::try_parse_from(["shell", "keys", "debug", "--async", "Enter"]).unwrap();
        assert!(matches!(
            cli.command,
            Action::Keys {
                id: None,
                async_mode: true,
                ..
            }
        ));
        let cli = Cli::try_parse_from(["shell", "resize", "debug", "80", "24"]).unwrap();
        assert!(matches!(cli.command, Action::Resize { id: None, .. }));
        let cli = Cli::try_parse_from(["shell", "read", "debug", "--screen", "delta"]).unwrap();
        assert!(matches!(
            cli.command,
            Action::Read {
                screen: ScreenMode::Delta,
                ..
            }
        ));
        for args in [
            vec!["shell", "read", "debug"],
            vec!["shell", "send", "debug", "--text", "hello"],
            vec!["shell", "keys", "debug", "Enter"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(
                cli.command,
                Action::Read {
                    screen: ScreenMode::Delta,
                    ..
                } | Action::Send {
                    screen: ScreenMode::Delta,
                    async_mode: false,
                    ..
                } | Action::Keys {
                    screen: ScreenMode::Delta,
                    async_mode: false,
                    ..
                }
            ));
        }
        for args in [
            vec![
                "shell", "send", "debug", "--text", "hello", "--screen", "full",
            ],
            vec!["shell", "keys", "debug", "Enter", "--screen", "full"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(
                cli.command,
                Action::Send {
                    screen: ScreenMode::Full,
                    ..
                } | Action::Keys {
                    screen: ScreenMode::Full,
                    ..
                }
            ));
        }
        let old_read: Action =
            serde_json::from_str(r#"{"Read":{"host":"lab","session":"debug","history":0}}"#)
                .unwrap();
        assert!(matches!(
            old_read,
            Action::Read {
                screen: ScreenMode::Full,
                ..
            }
        ));
        let old_send: Action = serde_json::from_str(r#"{"Send":{"host":"lab","session":"debug","id":"f8e6cc14-4037-49b2-a55d-bb7ea6cf7069","text":"echo hello","stdin":false,"file":null,"enter":true}}"#).unwrap();
        assert!(matches!(
            old_send,
            Action::Send {
                async_mode: false,
                id: Some(_),
                ..
            }
        ));
        let mut reply = serde_json::to_value(Reply::new("read", "lab", Some("debug"))).unwrap();
        reply.as_object_mut().unwrap().remove("screen_mode");
        reply.as_object_mut().unwrap().remove("row_wrapped");
        reply.as_object_mut().unwrap().remove("wait_status");
        let old_reply: Reply = serde_json::from_value(reply).unwrap();
        assert!(
            old_reply.screen_mode.is_none()
                && old_reply.row_wrapped.is_none()
                && old_reply.wait_status.is_none()
        );
    }
    #[test]
    #[ignore = "requires a working POSIX sh; run explicitly with --ignored"]
    fn posix_shell_quote_roundtrip() {
        let sample = "a'b;\"$`\n雪";
        let script = format!("printf %s {}", quote(sample));
        let output = Command::new("sh")
            .args(["-c", &script])
            .output()
            .expect("this optional test requires a working POSIX sh");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, sample.as_bytes());
    }
    #[test]
    fn validation() {
        assert!(!valid_host("-oProxyCommand=x"));
        assert!(!valid_host("a b"));
        assert!(valid_host("user@host.example"));
        assert!(!valid_name("../../oops"));
    }
    #[test]
    fn keys_are_explicit() {
        assert_eq!(key("Ctrl-C"), Some("\u{3}"));
        assert_eq!(key("SQL"), None);
    }
    #[test]
    fn json_controls() {
        let mut r = Reply::new("read", "host", Some("s"));
        r.screen = Some("a\u{1b}[31m\n".into());
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("\\u001b"));
    }
    #[test]
    fn transport_mock_and_ambiguous_timeout() {
        let dir = std::env::temp_dir().join(format!("shell-cli-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("mock.rs");
        let exe = dir.join(format!("mock{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&source, r#"fn main() {
            let args: Vec<String> = std::env::args().skip(1).collect();
            if args.last().is_some_and(|s| s == "sleep") { std::thread::sleep(std::time::Duration::from_secs(3)); }
            else if args.last().is_some_and(|s| s == "fork") {
                let _ = std::process::Command::new(std::env::current_exe().unwrap()).arg("hold")
                    .stdout(std::process::Stdio::inherit()).stderr(std::process::Stdio::inherit()).spawn().unwrap();
            }
            else if args.last().is_some_and(|s| s == "hold") { std::thread::sleep(std::time::Duration::from_secs(3)); }
            else { print!("{}", args.join("|")); }
        }"#).unwrap();
        let built = Command::new("rustc")
            .args([source.as_os_str(), "-o".as_ref(), exe.as_os_str()])
            .status()
            .unwrap();
        assert!(built.success());
        let r = run(
            &exe,
            &config::Connection::default(),
            "example",
            "printf hello",
            Duration::from_secs(2),
            false,
        )
        .unwrap();
        let args = String::from_utf8(r.stdout).unwrap();
        assert!(args.contains("BatchMode=yes"));
        assert!(args.contains("StrictHostKeyChecking=yes"));
        assert!(args.ends_with("example|printf hello"));
        let start = Instant::now();
        let e = run(
            &exe,
            &config::Connection::default(),
            "example",
            "sleep",
            Duration::from_millis(100),
            true,
        )
        .err()
        .unwrap();
        assert_eq!(e.code, "timeout");
        assert!(e.unknown);
        assert!(start.elapsed() < Duration::from_secs(2));
        let start = Instant::now();
        let e = run(
            &exe,
            &config::Connection::default(),
            "example",
            "fork",
            Duration::from_millis(200),
            true,
        )
        .err()
        .unwrap();
        assert_eq!(e.code, "timeout");
        assert!(e.unknown);
        assert!(start.elapsed() < Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(dir);
    }
}
