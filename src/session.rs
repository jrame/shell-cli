//! A local session owner keeps OpenSSH and its terminal alive between CLI calls.
use super::*;
mod ipc;
mod snapshots;
mod storage;
mod terminal;
mod viewer;
use fs2::FileExt;
use ipc::*;
use snapshots::*;
use std::fs::{self, OpenOptions};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use storage::*;
use terminal::*;
pub(super) use viewer::{show, view};

const MAX_FRAME: u64 = 8 * 1024 * 1024;
const HISTORY: usize = 2000;
const SCREEN_QUIET: Duration = Duration::from_millis(200);

#[derive(Serialize, Deserialize)]
struct Entry {
    host: String,
    session: String,
    id: String,
    token: String,
    port: u16,
    #[serde(default)]
    owner_lock: bool,
    #[serde(default)]
    transfer: Option<TransferConnection>,
}

#[derive(Serialize, Deserialize)]
struct TransferConnection {
    ssh: PathBuf,
    connection: config::Connection,
}

#[derive(Serialize, Deserialize)]
struct Startup {
    ssh: PathBuf,
    #[serde(default)]
    connection: config::Connection,
    host: String,
    session: String,
    id: String,
    width: u16,
    height: u16,
    timeout: u64,
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    action: Action,
    timeout: u64,
}

fn local(e: impl std::fmt::Display) -> Fault {
    fault("local", e.to_string(), 3)
}

fn io_fault(e: std::io::Error) -> Fault {
    if matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        fault("timeout", e.to_string(), 6)
    } else {
        local(e)
    }
}

fn input_mode(action: &Action) -> Option<(bool, ScreenMode)> {
    match action {
        Action::Send {
            async_mode, screen, ..
        }
        | Action::Keys {
            async_mode, screen, ..
        } => Some((*async_mode, *screen)),
        _ => None,
    }
}

/// Keep the serialized owner protocol unchanged while accepting session-only CLI targets.
pub(super) fn cli_args(
    mut args: Vec<std::ffi::OsString>,
) -> Result<Vec<std::ffi::OsString>, clap::Error> {
    let mut command = None;
    let mut positions = Vec::new();
    let mut explicit_host = false;
    let mut skip = false;
    let mut positional_only = false;
    for (i, arg) in args.iter().enumerate().skip(1) {
        if skip {
            skip = false;
            continue;
        }
        let value = arg.to_string_lossy();
        if !positional_only && value == "--" {
            positional_only = true;
            continue;
        }
        if !positional_only && value.starts_with('-') {
            let option = value.split('=').next().unwrap();
            explicit_host |= option == "--host";
            skip = !value.contains('=')
                && matches!(
                    option,
                    "--ssh"
                        | "--timeout"
                        | "--host"
                        | "--text"
                        | "--file"
                        | "--width"
                        | "--height"
                        | "--history"
                        | "--bytes"
                        | "--format"
                        | "--screen"
                        | "--config"
                        | "--profile"
                        | "--identity"
                        | "--port"
                        | "--jump"
                        | "--local-forward"
                        | "--remote-forward"
                        | "--dynamic-forward"
                        | "--ssh-config"
                        | "-i"
                        | "-p"
                        | "-J"
                        | "-L"
                        | "-R"
                        | "-D"
                        | "-F"
                );
            continue;
        }
        if command.is_none() {
            command = Some(value.into_owned());
        } else {
            positions.push(i);
        }
    }
    if explicit_host {
        return Ok(args);
    }
    let legacy = match command.as_deref() {
        Some("show" | "status" | "read" | "send" | "close") => positions.len() == 2,
        Some("resize") => positions.len() == 4,
        Some("keys") if positions.len() >= 2 => {
            let second = args[positions[1]].to_string_lossy();
            if key(&second).is_none() {
                true
            } else if positions.len() >= 3 {
                // A session may itself be named Enter. Never silently choose
                // between two locally known destinations in that case.
                let first = args[positions[0]].to_string_lossy();
                let dir = state_dir().map_err(|e| {
                    clap::Error::raw(clap::error::ErrorKind::InvalidValue, e.message)
                })?;
                let old = entry_path(&dir, &first, &second).exists();
                if old
                    && !matching_entries(&dir, &first)
                        .map_err(|e| {
                            clap::Error::raw(clap::error::ErrorKind::InvalidValue, e.message)
                        })?
                        .is_empty()
                {
                    return Err(clap::Error::raw(
                        clap::error::ErrorKind::InvalidValue,
                        "ambiguous keys target; use keys SESSION --host HOST KEYS",
                    ));
                }
                old
            } else {
                false
            }
        }
        _ => false,
    };
    if legacy {
        let index = positions[0];
        if let Some(separator) = args[..index].iter().position(|arg| arg == "--") {
            let host = args.remove(index);
            args.insert(separator, host);
            args.insert(separator, "--host".into());
        } else {
            args.insert(index, "--host".into());
        }
    }
    Ok(args)
}

fn matching_entries(dir: &Path, name: &str) -> Result<Vec<Entry>, Fault> {
    let mut entries = Vec::new();
    for file in fs::read_dir(dir).map_err(local)? {
        let path = file.map_err(local)?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let entry = load(&path)?;
        if entry.session == name {
            validate(&entry.host, Some(&entry.session))?;
            if path != entry_path(dir, &entry.host, &entry.session) {
                return Err(local("session metadata collision"));
            }
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub(super) fn resolve_host(action: &mut Action) -> Result<(), Fault> {
    let (host, name) = match action {
        Action::Show { host, session }
        | Action::Status { host, session }
        | Action::Read { host, session, .. }
        | Action::Send { host, session, .. }
        | Action::Keys { host, session, .. }
        | Action::Resize { host, session, .. }
        | Action::Upload { host, session, .. }
        | Action::Download { host, session, .. }
        | Action::Close { host, session, .. } => (host, session),
        _ => return Ok(()),
    };
    if !host.is_empty() {
        return Ok(());
    }
    let entries = matching_entries(&state_dir()?, name)?;
    match entries.as_slice() {
        [entry] => {
            *host = entry.host.clone();
            Ok(())
        }
        [] => Err(fault(
            "session_not_found",
            "session does not exist on this client",
            4,
        )),
        _ => Err(fault(
            "conflict",
            "session name exists on multiple hosts; specify --host HOST",
            5,
        )),
    }
}

pub(super) fn dispatch(cli: &Cli) -> Result<Reply, Fault> {
    let dir = state_dir()?;
    let (action, host, name) = context(&cli.command);
    let mut reply = Reply::new(action, host, name);
    if matches!(cli.command, Action::List { .. }) {
        let mut sessions = Vec::new();
        for file in fs::read_dir(&dir).map_err(local)? {
            let path = file.map_err(local)?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let entry = load(&path)?;
            if !host.is_empty() && entry.host != host {
                continue;
            }
            if entry.owner_lock && owner_stopped(&path, &entry)? {
                remove_stopped_record(&path, &entry.id)?;
                continue;
            }
            match request(
                &entry,
                Action::Status {
                    host: entry.host.clone(),
                    session: entry.session.clone(),
                },
                cli.timeout,
            ) {
                Ok(r) if r.error.is_none() => sessions.push(Session {
                    host: Some(entry.host.clone()),
                    name: entry.session,
                    id: entry.id,
                }),
                Err(e) if e.exit == 4 => remove_stopped_record(&path, &entry.id)?,
                Err(e) => return Err(e),
                Ok(_) => return Err(local("local session owner rejected status")),
            }
        }
        sessions.sort_by(|a, b| a.name.cmp(&b.name));
        reply.sessions = Some(sessions);
        return Ok(reply);
    }
    let name = name.ok_or_else(|| local("missing session name"))?;
    let path = entry_path(&dir, host, name);
    // A global open lock covers the name scan through the owner's atomic
    // publication. Host-specific locks alone allow two simultaneous opens of
    // the same name on different hosts.
    let _open_lock = if matches!(cli.command, Action::Open { .. }) {
        Some(lock(&dir.join("open-global"))?)
    } else {
        None
    };
    // Protect record loading against publication/removal. Release the lock
    // before ordinary requests so a pending screen wait cannot reject Ctrl-C.
    let record_lock = lock(&path)?;
    if let Action::Open { width, height, .. } = &cli.command {
        for existing in matching_entries(&dir, name)? {
            let existing_path = entry_path(&dir, &existing.host, &existing.session);
            let _other_lock = if existing_path != path {
                Some(lock(&existing_path)?)
            } else {
                None
            };
            // A concurrent close may have removed the scanned record.
            let existing = match load(&existing_path) {
                Ok(entry) => entry,
                Err(e) if e.exit == 4 => continue,
                Err(e) => return Err(e),
            };
            if owner_stopped(&existing_path, &existing)? {
                fs::remove_file(existing_path).map_err(local)?;
                continue;
            }
            return Err(fault(
                "conflict",
                format!(
                    "session name already exists; \
                     close it explicitly first with `shell close {} --host {}`",
                    existing.session, existing.host
                ),
                5,
            ));
        }
        if path.exists() {
            return Err(local("session metadata collision"));
        }
        let mut connection = cli.connection.clone();
        connection.absolute_paths()?;
        let ssh = if cli.ssh.is_relative() && cli.ssh.components().count() > 1 {
            std::env::current_dir().map_err(local)?.join(&cli.ssh)
        } else {
            cli.ssh.clone()
        };
        return start(Startup {
            ssh,
            connection,
            host: host.into(),
            session: name.into(),
            id: Uuid::new_v4().to_string(),
            width: *width,
            height: *height,
            timeout: cli.timeout,
            path,
        });
    }
    let entry = load(&path)?;
    if entry.host != host || entry.session != name {
        return Err(local("session metadata collision"));
    }
    if expected_id(&cli.command).is_some_and(|id| id != entry.id) {
        return Err(fault("conflict", "session identity changed", 5));
    }
    if entry.owner_lock && owner_stopped(&path, &entry)? {
        fs::remove_file(&path).map_err(local)?;
        if matches!(cli.command, Action::Close { .. }) {
            reply.id = Some(entry.id);
            reply.delivery = "acknowledged".into();
            return Ok(reply);
        }
        return Err(stopped_owner());
    }
    if matches!(cli.command, Action::Upload { .. } | Action::Download { .. }) {
        let saved = entry.transfer.as_ref().ok_or_else(|| {
            local(
                "this session predates file transfers; close and reopen it with the updated binary",
            )
        })?;
        // Status authenticates the selected owner and pins its generation. This
        // does not consume the screen baseline or send input to the terminal.
        let status = request(
            &entry,
            Action::Status {
                host: host.into(),
                session: name.into(),
            },
            cli.timeout,
        )?;
        if status.id.as_deref() != Some(&entry.id) {
            return Err(fault("conflict", "session identity changed", 5));
        }
        if status.error.is_some() {
            reply.id = Some(entry.id);
            reply.error = status.error;
            return Ok(reply);
        }
        if status.pane_dead == Some(true) {
            return Err(local(
                "SSH session has ended; open a new session before transferring",
            ));
        }
        reply.id = Some(entry.id.clone());
        drop(record_lock);
        transfer::execute(&saved.ssh, &saved.connection, cli, &mut reply)?;
        return Ok(reply);
    }
    let mut command = cli.command.clone();
    if let Action::Send { id, .. }
    | Action::Keys { id, .. }
    | Action::Resize { id, .. }
    | Action::Close { id, .. } = &mut command
    {
        // Pin the instance selected by name, and keep requests compatible with
        // running owners whose protocol still requires a close ID.
        if id.is_none() {
            *id = Some(entry.id.clone());
        }
    }
    let _lifecycle_lock = if matches!(cli.command, Action::Close { .. }) {
        Some(record_lock)
    } else {
        drop(record_lock);
        None
    };
    match request(&entry, command, cli.timeout) {
        Err(e) if e.exit == 4 && matches!(cli.command, Action::Close { .. }) => {
            fs::remove_file(path).map_err(local)?;
            reply.id = Some(entry.id);
            reply.delivery = "acknowledged".into();
            Ok(reply)
        }
        Err(e) if e.exit == 4 => {
            remove_stopped_record(&path, &entry.id)?;
            Err(e)
        }
        result => result,
    }
}

fn start(config: Startup) -> Result<Reply, Fault> {
    let startup_path = config.path.with_extension(format!("{}.startup", config.id));
    let ready_path = config.path.with_extension(format!("{}.ready", config.id));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&startup_path)
        .map_err(local)?;
    write_frame(&mut file, &config)?;
    drop(file);
    let mut child = match crate::background::Background::start(
        &std::env::current_exe().map_err(local)?,
        &startup_path,
    ) {
        Ok(child) => child,
        Err(e) => {
            let _ = fs::remove_file(&startup_path);
            return Err(local(e));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(config.timeout + 5);
    let result = loop {
        if ready_path.exists() {
            break File::open(&ready_path).map_err(local).and_then(read_frame);
        }
        if child.exited().map_err(local)? {
            if ready_path.exists() {
                break File::open(&ready_path).map_err(local).and_then(read_frame);
            }
            break Err(local("local session owner exited during startup"));
        }
        if Instant::now() >= deadline {
            child.terminate();
            break Err(fault(
                "timeout",
                "local session startup exceeded its deadline",
                6,
            ));
        }
        thread::sleep(Duration::from_millis(20));
    };
    let _ = fs::remove_file(startup_path);
    let _ = fs::remove_file(ready_path);
    result
}

fn publish_ready(path: &Path, reply: &Reply) -> Result<(), Fault> {
    let temp = path.with_extension("writing");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(local)?;
    write_frame(&mut file, reply)?;
    drop(file);
    fs::rename(&temp, path).map_err(local)
}

type Input = (Vec<u8>, mpsc::Sender<Result<(), String>>);

fn handle(
    terminal: &mut Terminal,
    entry: &Entry,
    req: Request,
    reply: &mut Reply,
) -> Result<bool, Fault> {
    if req.token != entry.token {
        return Err(fault("unauthorized", "invalid local session credential", 3));
    }
    validate_action(&req.action)?;
    let (_, host, name) = context(&req.action);
    if host != entry.host || name != Some(&entry.session) {
        return Err(fault("conflict", "wrong session", 5));
    }
    if expected_id(&req.action).is_some_and(|id| id != entry.id) {
        return Err(fault("conflict", "session identity changed", 5));
    }
    reply.id = Some(entry.id.clone());
    let timeout = req.timeout.clamp(1, 120);
    match req.action {
        Action::View {
            bytes,
            width,
            height,
            ..
        } => {
            size(width, height)?;
            if bytes.len() > MAX_INPUT {
                return Err(local("viewer input too large"));
            }
            if !bytes.is_empty() {
                terminal.send(bytes, timeout)?;
            }
            {
                let mut parser = terminal.parser.lock().unwrap();
                if parser.screen().size() != (height, width) {
                    terminal
                        .master
                        .resize(pty_size(width, height))
                        .map_err(local)?;
                    parser.screen_mut().set_size(height, width);
                }
                reply.screen =
                    Some(String::from_utf8_lossy(&parser.screen().state_formatted()).into_owned());
            }
            reply.pane_dead = Some(terminal.dead()?);
        }
        Action::Show { .. } | Action::Status { .. } => terminal.snapshot(0, None, reply)?,
        Action::Read {
            history, screen, ..
        } => terminal.snapshot(history, Some(screen), reply)?,
        Action::Send {
            text: Some(text),
            enter,
            stdin: false,
            file: None,
            ..
        } => {
            if text.len() > MAX_INPUT
                || text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            {
                return Err(fault("validation", "invalid terminal input", 2));
            }
            let mut bytes = text.into_bytes();
            if enter {
                bytes.push(b'\r');
            }
            terminal.send(bytes, timeout)?;
            reply.delivery = "acknowledged".into();
        }
        Action::Keys { keys, .. } => {
            let application = terminal
                .parser
                .lock()
                .unwrap()
                .screen()
                .application_cursor();
            let mut bytes = String::new();
            for k in keys {
                let mapped = key(&k).unwrap();
                if application
                    && matches!(
                        mapped,
                        "\x1b[A" | "\x1b[B" | "\x1b[C" | "\x1b[D" | "\x1b[H" | "\x1b[F"
                    )
                {
                    bytes.push_str(&mapped.replace('[', "O"));
                } else {
                    bytes.push_str(mapped);
                }
            }
            terminal.send(bytes.into_bytes(), timeout)?;
            reply.delivery = "acknowledged".into();
        }
        Action::Resize { width, height, .. } => {
            if terminal.dead()? {
                return Err(fault("session_closed", "SSH disconnected", 3));
            }
            // Hold the screen lock so repaint bytes cannot precede the parser resize.
            let mut parser = terminal.parser.lock().unwrap();
            reply.delivery = "unknown".into();
            terminal
                .master
                .resize(pty_size(width, height))
                .map_err(local)?;
            parser.screen_mut().set_size(height, width);
            reply.delivery = "acknowledged".into();
        }
        Action::Close { .. } => {
            reply.delivery = "unknown".into();
            terminal.child.kill().map_err(local)?;
            terminal.child.wait().map_err(local)?;
            reply.delivery = "acknowledged".into();
            return Ok(true);
        }
        _ => return Err(fault("validation", "unsupported local session request", 2)),
    }
    Ok(false)
}

struct PendingResponse {
    stream: TcpStream,
    reply: Reply,
    revision: u64,
    deadline: Instant,
    mode: ScreenMode,
}

fn finish_pending(
    terminal: &mut Terminal,
    pending: &mut Vec<PendingResponse>,
) -> Result<(), Fault> {
    let dead = terminal.dead()?;
    let mut index = 0;
    while index < pending.len() {
        let wait = &pending[index];
        let outcome = if dead {
            Some("closed")
        } else {
            terminal.parser.lock().unwrap().activity.outcome(
                wait.revision,
                wait.deadline,
                Instant::now(),
            )
        };
        let Some(outcome) = outcome else {
            index += 1;
            continue;
        };
        let mut wait = pending.remove(index);
        wait.reply.wait_status = Some(outcome.into());
        if let Err(e) = terminal.snapshot(0, Some(wait.mode), &mut wait.reply) {
            apply_fault(&mut wait.reply, e);
        } else if outcome == "timeout" {
            apply_fault(
                &mut wait.reply,
                fault(
                    "timeout",
                    "terminal did not react and become quiet before the wait deadline; input was acknowledged locally; remote command completion is unknown",
                    6,
                ),
            );
        }
        // A disconnected caller never causes input to be replayed or SSH killed.
        let _ = write_frame(&mut wait.stream, &wait.reply);
    }
    Ok(())
}

fn serve_inner(config: Startup, ready_path: &Path, started: &mut bool) -> Result<(), Fault> {
    validate(&config.host, Some(&config.session))?;
    size(config.width, config.height)?;
    if !(1..=120).contains(&config.timeout) {
        return Err(local("invalid startup timeout"));
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(local)?;
    listener.set_nonblocking(true).map_err(local)?;
    let mut terminal = Terminal::open(&config)?;
    // Acquire before publication and keep until after metadata cleanup. Never
    // unlink the lock file: a concurrent opener must use the same OS lock.
    let _owner_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.path.with_extension("owner"))
        .map_err(local)?;
    _owner_lock.try_lock_exclusive().map_err(local)?;
    let entry = Entry {
        host: config.host,
        session: config.session,
        id: config.id,
        token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        port: listener.local_addr().map_err(local)?.port(),
        owner_lock: true,
        transfer: Some(TransferConnection {
            ssh: config.ssh.clone(),
            connection: config.connection.clone(),
        }),
    };
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&config.path)
        .map_err(local)?;
    write_frame(&mut file, &entry)?;
    drop(file);
    let mut ready = Reply::new("open", &entry.host, Some(&entry.session));
    ready.id = Some(entry.id.clone());
    ready.delivery = "acknowledged".into();
    if let Err(e) = publish_ready(ready_path, &ready) {
        let _ = fs::remove_file(&config.path);
        return Err(e);
    }
    *started = true;
    let mut pending = Vec::new();
    let result = (|| {
        loop {
            finish_pending(&mut terminal, &mut pending)?;
            terminal.dead()?;
            if terminal
                .dead_since
                .is_some_and(|t| t.elapsed() > Duration::from_secs(300))
            {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // Winsock inherits nonblocking mode from the listener. The
                    // protocol reader needs to wait for the complete frame;
                    // otherwise a perfectly valid client can be reset before
                    // its first byte arrives (especially the polling viewer).
                    stream.set_nonblocking(false).map_err(local)?;
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .map_err(local)?;
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .map_err(local)?;
                    // Malformed/unauthenticated clients cannot stop the session owner.
                    let Ok(req) = read_frame::<Request>(&stream) else {
                        continue;
                    };
                    let (action, host, name) = context(&req.action);
                    let mut reply = Reply::new(action, host, name);
                    let input_mode = input_mode(&req.action);
                    if matches!(input_mode, Some((false, _))) && pending.len() >= 32 {
                        apply_fault(
                            &mut reply,
                            fault("conflict", "too many pending screen waits", 5),
                        );
                        let _ = write_frame(&mut stream, &reply);
                        continue;
                    }
                    let deadline = Instant::now() + Duration::from_secs(req.timeout.clamp(1, 120));
                    let revision = terminal.parser.lock().unwrap().activity.revision;
                    let close = match handle(&mut terminal, &entry, req, &mut reply) {
                        Ok(close) => close,
                        Err(e) => {
                            apply_fault(&mut reply, e);
                            false
                        }
                    };
                    if close {
                        fs::remove_file(&config.path).map_err(local)?;
                        finish_pending(&mut terminal, &mut pending)?;
                    }
                    if reply.error.is_none() {
                        if let Some((async_mode, mode)) = input_mode {
                            if !async_mode {
                                // Leave the socket pending while accepting viewer refreshes,
                                // Ctrl-C, reads and other requests on the same owner loop.
                                pending.push(PendingResponse {
                                    stream,
                                    reply,
                                    revision,
                                    deadline,
                                    mode,
                                });
                                continue;
                            }
                            reply.wait_status = Some("skipped".into());
                        }
                    }
                    let _ = write_frame(&mut stream, &reply);
                    if close {
                        return Ok(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(e) => return Err(local(e)),
            }
        }
        Ok(())
    })();
    let _ = fs::remove_file(&config.path);
    result
}

pub(super) fn serve() {
    let Some(path) = std::env::args_os().nth(2).map(PathBuf::from) else {
        return;
    };
    let config = File::open(&path)
        .map_err(local)
        .and_then(read_frame::<Startup>);
    let _ = fs::remove_file(path);
    let Ok(config) = config else {
        return;
    };
    let ready_path = config.path.with_extension(format!("{}.ready", config.id));
    let mut reply = Reply::new("open", &config.host, Some(&config.session));
    reply.id = Some(config.id.clone());
    let mut started = false;
    if let Err(e) = serve_inner(config, &ready_path, &mut started) {
        // Startup errors are delivered through an atomic file. The background
        // process owns no pipe back to the invoking shell after it starts.
        if !started {
            apply_fault(&mut reply, e);
            let _ = publish_ready(&ready_path, &reply);
        }
    }
}

#[cfg(test)]
mod tests;
