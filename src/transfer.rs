//! A bounded SFTP v3 client over `ssh -s sftp`. Paths are protocol strings,
//! never shell commands or sftp batch input. Only one request is in flight.
//! Wire format: draft-ietf-secsh-filexfer-02, as implemented by OpenSSH.
use super::*;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const CHUNK: usize = 32 * 1024;
const MAX_PACKET: usize = 256 * 1024;
const READ: u32 = 1;
const WRITE: u32 = 2;
const CREATE: u32 = 8;
const EXCLUSIVE: u32 = 32;

#[derive(Serialize, Deserialize)]
pub(super) struct Report {
    source: String,
    destination: String,
    bytes: u64,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    files: usize,
    #[serde(default)]
    directories: usize,
}

pub(super) fn validate_paths(local: &Path, remote: &str) -> Result<(), Fault> {
    if local.as_os_str().is_empty() || local.to_string_lossy().chars().any(char::is_control) {
        return Err(fault(
            "validation",
            "provide a local filename without control characters",
            2,
        ));
    }
    if !remote.starts_with('/')
        || remote.chars().any(char::is_control)
        || remote.len() > 32 * 1024
        || remote.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err(fault(
            "validation",
            "remote path must be absolute, without dot components or control characters",
            2,
        ));
    }
    Ok(())
}

fn local(e: impl std::fmt::Display) -> Fault {
    fault("local", e.to_string(), 3)
}

fn transport(e: impl std::fmt::Display) -> Fault {
    fault("transport", format!("SFTP: {e}"), 3)
}

fn conflict() -> Fault {
    fault(
        "conflict",
        "destination already exists; use --overwrite to replace its contents",
        5,
    )
}

const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 128;

struct Entry {
    local: PathBuf,
    remote: String,
    directory: bool,
}

fn invalid(message: impl Into<String>) -> Fault {
    fault("validation", message, 2)
}

fn remote_path(path: &str) -> String {
    format!(
        "/{}",
        path.split('/')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    )
}

fn remote_join(parent: &str, name: &str) -> String {
    format!("{}/{name}", parent.trim_end_matches('/'))
}

fn component(name: &str) -> Result<(), Fault> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(invalid("unsafe filename in directory or source selection"));
    }
    #[cfg(windows)]
    {
        let stem = name.split('.').next().unwrap().to_ascii_uppercase();
        if name.contains([':', '*', '?', '"', '<', '>', '|'])
            || name.ends_with(['.', ' '])
            || matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
            || (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(
                    &stem[3..],
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
        {
            return Err(invalid(
                "remote filename cannot be represented safely on Windows",
            ));
        }
    }
    Ok(())
}

fn add_entry(plan: &mut Vec<Entry>, entry: Entry, depth: usize) -> Result<(), Fault> {
    if depth > MAX_DEPTH || plan.len() >= MAX_ENTRIES {
        return Err(invalid(
            "transfer exceeds 128 directory levels or 100000 entries",
        ));
    }
    validate_paths(&entry.local, &entry.remote)?;
    plan.push(entry);
    Ok(())
}

fn upload_tree(
    plan: &mut Vec<Entry>,
    path: &Path,
    remote: &str,
    recursive: bool,
    depth: usize,
    deadline: Instant,
) -> Result<(), Fault> {
    if Instant::now() >= deadline {
        return Err(fault(
            "timeout",
            "transfer deadline exceeded while inspecting local sources",
            6,
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(local)?;
    if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
        return Err(invalid(format!(
            "source must be a regular file or directory: {}",
            path.display()
        )));
    }
    let directory = metadata.is_dir();
    if directory && !recursive {
        return Err(invalid("directory sources require --recursive"));
    }
    add_entry(
        plan,
        Entry {
            local: path.into(),
            remote: remote.into(),
            directory,
        },
        depth,
    )?;
    if directory {
        let mut children = fs::read_dir(path)
            .map_err(local)?
            .take(MAX_ENTRIES + 1)
            .collect::<Result<Vec<_>, _>>()
            .map_err(local)?;
        if children.len() + plan.len() > MAX_ENTRIES {
            return Err(invalid("transfer exceeds 100000 entries"));
        }
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("source filename must be UTF-8"))?;
            component(name)?;
            upload_tree(
                plan,
                &child.path(),
                &remote_join(remote, name),
                recursive,
                depth + 1,
                deadline,
            )?;
        }
    }
    Ok(())
}

fn unique_destinations(plan: &[Entry], upload: bool) -> Result<(), Fault> {
    let mut seen = std::collections::HashSet::new();
    for entry in plan {
        let key = if upload {
            entry.remote.clone()
        } else {
            entry.local.to_string_lossy().into_owned()
        };
        #[cfg(windows)]
        let key = if upload { key } else { key.to_lowercase() };
        if !seen.insert(key) {
            return Err(invalid(
                "selected sources have overlapping destination paths",
            ));
        }
    }
    Ok(())
}

fn local_directory(path: &Path, changed: &AtomicBool) -> Result<(), Fault> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(invalid(format!(
            "destination must be a directory, not a file or symlink: {}",
            path.display()
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(local)?;
            changed.store(true, Ordering::SeqCst);
            Ok(())
        }
        Err(e) => Err(local(e)),
    }
}

fn check_destination(path: &Path, overwrite: bool) -> Result<(), Fault> {
    match fs::symlink_metadata(path) {
        Ok(_) if !overwrite => Err(conflict()),
        Ok(m) if !m.is_file() => Err(fault(
            "validation",
            "destination must be a regular file, not a directory or symlink",
            2,
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(local(e)),
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn execute(
    ssh: &Path,
    connection: &config::Connection,
    cli: &Cli,
    reply: &mut Reply,
) -> Result<(), Fault> {
    let (upload, locals, remotes, overwrite, recursive) = match &cli.command {
        Action::Upload {
            local,
            remote,
            overwrite,
            recursive,
            ..
        } => (
            true,
            local.clone(),
            vec![remote_path(remote)],
            *overwrite,
            *recursive,
        ),
        Action::Download {
            local,
            remote,
            overwrite,
            recursive,
            ..
        } => (
            false,
            vec![local.clone()],
            remote.iter().map(|p| remote_path(p)).collect(),
            *overwrite,
            *recursive,
        ),
        _ => return Err(fault("validation", "expected a transfer command", 2)),
    };
    let deadline = Instant::now() + Duration::from_secs(cli.timeout);
    let sources: Vec<String> = if upload {
        locals
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    } else {
        remotes.clone()
    };
    let destination = if upload {
        remotes[0].clone()
    } else {
        locals[0].to_string_lossy().into_owned()
    };
    let mut plan = Vec::new();
    if upload {
        if locals.len() > 1 {
            add_entry(
                &mut plan,
                Entry {
                    local: locals[0].clone(),
                    remote: remotes[0].clone(),
                    directory: true,
                },
                0,
            )?;
        }
        for path in &locals {
            let remote = if locals.len() > 1 {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| invalid("source must have a UTF-8 basename"))?;
                component(name)?;
                remote_join(&remotes[0], name)
            } else {
                remotes[0].clone()
            };
            upload_tree(&mut plan, path, &remote, recursive, 0, deadline)?;
        }
        unique_destinations(&plan, true)?;
    } else if remotes.len() == 1 && !recursive {
        check_destination(&locals[0], overwrite)?;
    }
    let mut command = Command::new(ssh);
    command
        .args(connection.for_transfer().args())
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ConnectTimeout=10",
            "-s",
            "--",
            &reply.host,
            "sftp",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut process = Process(
        command
            .spawn()
            .map_err(|e| transport(format!("cannot start SSH: {e}")))?,
    );
    let input = process.0.stdin.take().unwrap();
    let output = process.0.stdout.take().unwrap();
    let stderr = process.0.stderr.take().unwrap();
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = err_tx.send(drain(stderr).0);
    });
    let changed = Arc::new(AtomicBool::new(false));
    let mutation = changed.clone();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut client = Client::connect(input, output)?;
            if !upload {
                if remotes.len() > 1 {
                    add_entry(
                        &mut plan,
                        Entry {
                            local: locals[0].clone(),
                            remote: remotes[0].clone(),
                            directory: true,
                        },
                        0,
                    )?;
                }
                for remote in &remotes {
                    let path = if remotes.len() > 1 {
                        let name = remote.rsplit('/').next().unwrap();
                        component(name)?;
                        locals[0].join(name)
                    } else {
                        locals[0].clone()
                    };
                    if recursive {
                        client.download_tree(&mut plan, &path, remote, 0)?;
                    } else {
                        add_entry(
                            &mut plan,
                            Entry {
                                local: path,
                                remote: remote.clone(),
                                directory: false,
                            },
                            0,
                        )?;
                    }
                }
                unique_destinations(&plan, false)?;
            }
            client.copy_plan(plan, upload, overwrite, &mutation, deadline)
        })();
        let _ = tx.send(result);
    });
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let timeout = || {
        fault(
            "timeout",
            "file transfer deadline exceeded; destination may contain partial data",
            6,
        )
    };
    let mut result = match rx.recv_timeout(remaining()) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(timeout()),
        Err(_) => Err(transport("transfer worker stopped")),
    };
    if result.is_ok() {
        loop {
            match process.0.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        result = Err(transport(format!("SSH exited with {status}")));
                    }
                    break;
                }
                Ok(None) if !remaining().is_zero() => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    result = Err(timeout());
                    break;
                }
                Err(e) => {
                    result = Err(transport(e));
                    break;
                }
            }
        }
    }
    // Stop SSH before returning errors/timeouts; no worker can continue remote
    // I/O after the connection is killed. Never retry an ambiguous operation.
    drop(process);
    if let Err(mut e) = result {
        if let Ok(stderr) = err_rx.recv_timeout(Duration::from_millis(100)) {
            let diagnostic = lines(&stderr);
            if !diagnostic.is_empty() {
                e.message.push_str(&format!("; SSH: {diagnostic}"));
            }
        }
        e.unknown = changed.load(Ordering::SeqCst);
        return Err(e);
    }
    let (bytes, files, directories) = result.unwrap();
    reply.transfer = Some(Report {
        source: sources[0].clone(),
        destination,
        bytes,
        sources,
        files,
        directories,
    });
    reply.delivery = "acknowledged".into();
    reply.screen = Some(format!(
        "{} {files} files, {directories} directories, {bytes} bytes.",
        if upload { "Uploaded" } else { "Downloaded" }
    ));
    Ok(())
}

fn u32_bytes(data: &mut Vec<u8>, value: u32) {
    data.extend(value.to_be_bytes());
}
fn string(data: &mut Vec<u8>, value: &[u8]) {
    u32_bytes(data, value.len() as u32);
    data.extend(value);
}

struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn attributes(&mut self) -> Result<u32, Fault> {
        let flags = self.number()?;
        if flags & !0x8000000f != 0 {
            return Err(transport("unsupported attribute flags"));
        }
        if flags & 1 != 0 {
            self.take(8)?;
        }
        if flags & 2 != 0 {
            self.take(8)?;
        }
        let mode = if flags & 4 != 0 { self.number()? } else { 0 };
        if flags & 8 != 0 {
            self.take(8)?;
        }
        if flags & 0x80000000 != 0 {
            let count = self.number()?;
            for _ in 0..count {
                self.string()?;
                self.string()?;
            }
        }
        Ok(mode & 0o170000)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Fault> {
        if n > self.0.len() {
            return Err(transport("truncated packet"));
        }
        let (value, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(value)
    }
    fn number(&mut self) -> Result<u32, Fault> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<&'a [u8], Fault> {
        let n = self.number()? as usize;
        self.take(n)
    }
}

struct Packet {
    kind: u8,
    body: Vec<u8>,
}
impl Packet {
    fn file_type(&self) -> Result<u32, Fault> {
        if self.kind != 105 {
            return Err(self.error("stat"));
        }
        Decoder(&self.body).attributes()
    }
    fn status(&self) -> Result<(u32, String), Fault> {
        if self.kind != 101 {
            return Err(transport("expected STATUS response"));
        }
        let mut data = Decoder(&self.body);
        let code = data.number()?;
        let message = String::from_utf8_lossy(data.string()?).into_owned();
        Ok((code, message))
    }
    fn error(&self, operation: &str) -> Fault {
        match self.status() {
            Ok((code, message)) => fault(
                "remote",
                format!("SFTP {operation} failed (status {code}): {message}"),
                3,
            ),
            Err(e) => e,
        }
    }
    fn ok(&self, operation: &str) -> Result<(), Fault> {
        if self.status()?.0 == 0 {
            Ok(())
        } else {
            Err(self.error(operation))
        }
    }
    fn regular(&self) -> Result<(), Fault> {
        if self.file_type()? != 0o100000 {
            return Err(fault("validation", "remote path must be a regular file", 2));
        }
        Ok(())
    }
}

struct Client<R, W> {
    input: W,
    output: R,
    id: u32,
}
impl<R: Read, W: Write> Client<R, W> {
    fn copy_plan(
        &mut self,
        plan: Vec<Entry>,
        upload: bool,
        overwrite: bool,
        changed: &AtomicBool,
        deadline: Instant,
    ) -> Result<(u64, usize, usize), Fault> {
        let (mut bytes, mut files, mut directories) = (0u64, 0usize, 0usize);
        for entry in plan {
            // Empty directory trees may perform no SFTP I/O on download, so
            // killing SSH alone cannot enforce the command's deadline.
            if Instant::now() >= deadline {
                return Err(fault(
                    "timeout",
                    "file transfer deadline exceeded; destination may contain partial data",
                    6,
                ));
            }
            let result = if entry.directory {
                if upload {
                    self.directory(&entry.remote, changed)
                } else {
                    local_directory(&entry.local, changed)
                }
                .map(|()| 0)
            } else {
                self.copy(upload, &entry.local, &entry.remote, overwrite, changed)
            };
            let n = result.map_err(|mut e| {
                e.message = format!(
                    "{} -> {}: {}; completed {files} files, {bytes} bytes",
                    entry.local.display(),
                    entry.remote,
                    e.message
                );
                e
            })?;
            bytes = bytes
                .checked_add(n)
                .ok_or_else(|| transport("total byte count overflow"))?;
            if entry.directory {
                directories += 1;
            } else {
                files += 1;
            }
        }
        Ok((bytes, files, directories))
    }

    fn directory(&mut self, path: &str, changed: &AtomicBool) -> Result<(), Fault> {
        let existing = self.stat(7, path.as_bytes())?;
        if existing.kind == 105 {
            return if existing.file_type()? == 0o040000 {
                Ok(())
            } else {
                Err(invalid(
                    "destination must be a directory, not a file or symlink",
                ))
            };
        }
        if existing.status()?.0 != 2 {
            return Err(existing.error("directory stat"));
        }
        let mut data = Vec::new();
        string(&mut data, path.as_bytes());
        u32_bytes(&mut data, 4);
        u32_bytes(&mut data, 0o700);
        changed.store(true, Ordering::SeqCst);
        self.request(14, &data)?.ok("mkdir")
    }

    fn download_tree(
        &mut self,
        plan: &mut Vec<Entry>,
        path: &Path,
        remote: &str,
        depth: usize,
    ) -> Result<(), Fault> {
        let kind = self.stat(7, remote.as_bytes())?.file_type()?;
        if !matches!(kind, 0o100000 | 0o040000) {
            return Err(invalid(format!(
                "recursive source must be a regular file or directory, not a symlink: {remote}"
            )));
        }
        let directory = kind == 0o040000;
        add_entry(
            plan,
            Entry {
                local: path.into(),
                remote: remote.into(),
                directory,
            },
            depth,
        )?;
        if !directory {
            return Ok(());
        }
        let packet = self.stat(11, remote.as_bytes())?; // OPENDIR
        if packet.kind != 102 {
            return Err(packet.error("opendir"));
        }
        let handle = Decoder(&packet.body).string()?.to_vec();
        let mut children = Vec::new();
        loop {
            let packet = self.stat(12, &handle)?; // READDIR
            if packet.kind == 101 && packet.status()?.0 == 1 {
                break;
            }
            if packet.kind != 104 {
                return Err(packet.error("readdir"));
            }
            let mut data = Decoder(&packet.body);
            let count = data.number()?;
            if count == 0 {
                return Err(transport("empty directory listing without EOF"));
            }
            for _ in 0..count {
                let name = std::str::from_utf8(data.string()?)
                    .map_err(|_| invalid("remote filename must be UTF-8"))?
                    .to_owned();
                data.string()?; // SFTP v3 longname is presentation text only.
                data.attributes()?;
                if matches!(name.as_str(), "." | "..") {
                    continue;
                }
                component(&name)?;
                if children.len() + plan.len() >= MAX_ENTRIES {
                    return Err(invalid("transfer exceeds 100000 entries"));
                }
                children.push(name);
            }
        }
        self.stat(4, &handle)?.ok("closedir")?;
        children.sort();
        for name in children {
            self.download_tree(
                plan,
                &path.join(&name),
                &remote_join(remote, &name),
                depth + 1,
            )?;
        }
        Ok(())
    }

    fn connect(input: W, output: R) -> Result<Self, Fault> {
        let mut client = Self {
            input,
            output,
            id: 0,
        };
        client.send(1, &3u32.to_be_bytes())?;
        let packet = client.receive()?;
        if packet.kind != 2 || Decoder(&packet.body).number()? != 3 {
            return Err(transport("server must support SFTP version 3"));
        }
        Ok(client)
    }
    fn send(&mut self, kind: u8, body: &[u8]) -> Result<(), Fault> {
        self.input
            .write_all(&((body.len() + 1) as u32).to_be_bytes())
            .map_err(transport)?;
        self.input.write_all(&[kind]).map_err(transport)?;
        self.input.write_all(body).map_err(transport)?;
        self.input.flush().map_err(transport)
    }
    fn receive(&mut self) -> Result<Packet, Fault> {
        let mut length = [0; 4];
        self.output.read_exact(&mut length).map_err(transport)?;
        let length = u32::from_be_bytes(length) as usize;
        if !(1..=MAX_PACKET).contains(&length) {
            return Err(transport("invalid packet length"));
        }
        let mut data = vec![0; length];
        self.output.read_exact(&mut data).map_err(transport)?;
        Ok(Packet {
            kind: data[0],
            body: data[1..].to_vec(),
        })
    }
    fn request(&mut self, kind: u8, body: &[u8]) -> Result<Packet, Fault> {
        self.id = self.id.wrapping_add(1);
        let mut data = self.id.to_be_bytes().to_vec();
        data.extend(body);
        self.send(kind, &data)?;
        let mut packet = self.receive()?;
        let mut response = Decoder(&packet.body);
        if response.number()? != self.id {
            return Err(transport("response ID mismatch"));
        }
        packet.body = response.0.to_vec();
        Ok(packet)
    }
    fn stat(&mut self, kind: u8, value: &[u8]) -> Result<Packet, Fault> {
        let mut data = Vec::new();
        string(&mut data, value);
        self.request(kind, &data)
    }
    fn open(&mut self, path: &str, flags: u32) -> Result<Vec<u8>, Fault> {
        let mut data = Vec::new();
        string(&mut data, path.as_bytes());
        u32_bytes(&mut data, flags);
        // New uploads are private; no source permissions/timestamps are copied.
        u32_bytes(&mut data, if flags & CREATE != 0 { 4 } else { 0 });
        if flags & CREATE != 0 {
            u32_bytes(&mut data, 0o600);
        }
        let packet = self.request(3, &data)?;
        if packet.kind != 102 {
            return Err(packet.error("open"));
        }
        Ok(Decoder(&packet.body).string()?.to_vec())
    }
    fn copy(
        &mut self,
        upload: bool,
        local_path: &Path,
        remote: &str,
        overwrite: bool,
        changed: &AtomicBool,
    ) -> Result<u64, Fault> {
        let (mut file, handle) = if upload {
            let file = File::open(local_path).map_err(local)?;
            if !file.metadata().map_err(local)?.is_file() {
                return Err(local("source is not a regular file"));
            }
            let existing = self.stat(7, remote.as_bytes())?; // LSTAT does not follow symlinks.
            if existing.kind == 105 {
                if !overwrite {
                    return Err(conflict());
                }
                existing.regular()?;
            } else if existing.status()?.0 != 2 {
                return Err(existing.error("destination stat"));
            }
            // OPEN may create the destination even if its response is lost.
            changed.store(true, Ordering::SeqCst);
            let handle = self.open(
                remote,
                WRITE | CREATE | if overwrite { 0 } else { EXCLUSIVE },
            )?;
            self.stat(8, &handle)?.regular()?; // FSTAT validates the opened file.
            if overwrite {
                let mut data = Vec::new();
                string(&mut data, &handle);
                u32_bytes(&mut data, 1);
                data.extend(0u64.to_be_bytes());
                self.request(10, &data)?.ok("truncate")?;
            }
            (file, handle)
        } else {
            let handle = self.open(remote, READ)?;
            self.stat(8, &handle)?.regular()?;
            check_destination(local_path, overwrite)?;
            let mut options = OpenOptions::new();
            options.write(true);
            if overwrite {
                options.create(true);
            } else {
                options.create_new(true);
            }
            let file = options.open(local_path).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    conflict()
                } else {
                    local(e)
                }
            })?;
            changed.store(true, Ordering::SeqCst);
            if !file.metadata().map_err(local)?.is_file() {
                return Err(local("destination is not a regular file"));
            }
            file.set_len(0).map_err(local)?;
            (file, handle)
        };
        let mut offset = 0u64;
        let mut buffer = vec![0; CHUNK];
        loop {
            let mut data = Vec::new();
            string(&mut data, &handle);
            data.extend(offset.to_be_bytes());
            let n = if upload {
                let n = file.read(&mut buffer).map_err(local)?;
                if n == 0 {
                    break;
                }
                string(&mut data, &buffer[..n]);
                self.request(6, &data)?.ok("write")?;
                n
            } else {
                u32_bytes(&mut data, CHUNK as u32);
                let packet = self.request(5, &data)?;
                if packet.kind == 101 && packet.status()?.0 == 1 {
                    break;
                }
                if packet.kind != 103 {
                    return Err(packet.error("read"));
                }
                let bytes = Decoder(&packet.body).string()?;
                if bytes.is_empty() || bytes.len() > CHUNK {
                    return Err(transport("invalid data length"));
                }
                file.write_all(bytes).map_err(local)?;
                bytes.len()
            };
            offset = offset
                .checked_add(n as u64)
                .ok_or_else(|| transport("file offset overflow"))?;
        }
        self.stat(4, &handle)?.ok("close")?;
        if !upload {
            file.sync_all().map_err(local)?;
        }
        Ok(offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 5) as u32).to_be_bytes().to_vec();
        bytes.push(kind);
        bytes.extend(id.to_be_bytes());
        bytes.extend(payload);
        bytes
    }
    fn status(id: u32, code: u32) -> Vec<u8> {
        let mut payload = code.to_be_bytes().to_vec();
        string(&mut payload, b"test status");
        string(&mut payload, b"");
        frame(101, id, &payload)
    }
    fn attrs(id: u32, mode: u32) -> Vec<u8> {
        let mut payload = 4u32.to_be_bytes().to_vec();
        payload.extend(mode.to_be_bytes());
        frame(105, id, &payload)
    }
    fn handle(id: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        string(&mut payload, b"handle");
        frame(102, id, &payload)
    }
    fn client(frames: Vec<Vec<u8>>) -> Client<Cursor<Vec<u8>>, Vec<u8>> {
        let mut bytes = frame(2, 3, b""); // VERSION has no request ID.
        for frame in frames {
            bytes.extend(frame);
        }
        Client::connect(Vec::new(), Cursor::new(bytes)).unwrap()
    }
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("shell-cli-transfer-{}", Uuid::new_v4()))
    }

    fn names(id: u32, names: &[&str]) -> Vec<u8> {
        let mut payload = (names.len() as u32).to_be_bytes().to_vec();
        for name in names {
            string(&mut payload, name.as_bytes());
            string(&mut payload, b"ignored longname");
            u32_bytes(&mut payload, 0); // Attributes may be absent in READDIR.
        }
        frame(104, id, &payload)
    }

    #[test]
    fn transfer_cli_selects_multiple_sources_and_preserves_single_file_syntax() {
        for (args, count, recursive) in [
            (vec!["upload", "debug", "a.bin", "/dest"], 1, false),
            (
                vec!["upload", "debug", "a.bin", "b.bin", "/dest/"],
                2,
                false,
            ),
            (
                vec!["upload", "debug", "a", "b.bin", "/dest/", "-r"],
                2,
                true,
            ),
            (
                vec!["download", "debug", "/a", "/b", "dest", "--recursive"],
                2,
                true,
            ),
            (vec!["download", "debug", "/tree/", "dest", "-r"], 1, true),
        ] {
            let cli = Cli::try_parse_from(std::iter::once("shell").chain(args)).unwrap();
            validate_action(&cli.command).unwrap();
            match cli.command {
                Action::Upload {
                    local,
                    remote,
                    recursive: r,
                    ..
                } => {
                    assert_eq!(local.len(), count);
                    assert!(remote.starts_with("/dest"));
                    assert_eq!(r, recursive);
                }
                Action::Download {
                    remote,
                    local,
                    recursive: r,
                    ..
                } => {
                    assert_eq!(remote.len(), count);
                    assert_eq!(local, PathBuf::from("dest"));
                    assert_eq!(r, recursive);
                }
                _ => unreachable!(),
            }
        }
        for args in [
            vec!["upload", "debug", "a"],
            vec!["download", "debug", "/a"],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("shell").chain(args)).is_err());
        }
    }

    #[test]
    fn recursive_upload_preserves_tree_and_empty_directories_with_sequential_files() {
        let root = temp();
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("empty")).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/a.bin"), b"abc").unwrap();
        fs::write(root.join("z.bin"), b"12345").unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        assert_eq!(
            upload_tree(&mut Vec::new(), &root, "/tree", false, 0, deadline)
                .unwrap_err()
                .code,
            "validation"
        );
        let mut plan = Vec::new();
        upload_tree(&mut plan, &root, "/tree", true, 0, deadline).unwrap();
        assert_eq!(
            plan.iter().map(|e| e.remote.as_str()).collect::<Vec<_>>(),
            [
                "/tree",
                "/tree/empty",
                "/tree/nested",
                "/tree/nested/a.bin",
                "/tree/z.bin"
            ]
        );
        let mut frames = Vec::new();
        let mut id = 1;
        for entry in &plan {
            frames.push(status(id, 2));
            id += 1;
            if entry.directory {
                frames.push(status(id, 0));
                id += 1;
            } else {
                frames.extend([
                    handle(id),
                    attrs(id + 1, 0o100600),
                    status(id + 2, 0),
                    status(id + 3, 0),
                ]);
                id += 4;
            }
        }
        let changed = AtomicBool::new(false);
        let mut upload = client(frames);
        assert_eq!(
            upload
                .copy_plan(plan, true, false, &changed, deadline)
                .unwrap(),
            (8, 2, 3)
        );
        // Every file is closed before the next OPEN; no file transfers overlap.
        let mut sent = Decoder(&upload.input);
        let mut open = false;
        while !sent.0.is_empty() {
            let n = sent.number().unwrap() as usize;
            let packet = sent.take(n).unwrap();
            match packet[0] {
                3 => {
                    assert!(!open);
                    open = true;
                }
                4 => {
                    assert!(open);
                    open = false;
                }
                _ => {}
            }
        }
        assert!(!open);
        assert!(changed.load(Ordering::SeqCst));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recursive_download_reads_paginated_names_and_copies_nested_files() {
        let root = temp();
        let mut download = client(vec![
            attrs(1, 0o040700),
            handle(2),
            names(3, &[".", "..", "nested", "empty"]),
            names(4, &["z.bin"]),
            status(5, 1),
            status(6, 0),
            attrs(7, 0o040700),
            handle(8),
            status(9, 1),
            status(10, 0),
            attrs(11, 0o040700),
            handle(12),
            names(13, &["a.bin"]),
            status(14, 1),
            status(15, 0),
            attrs(16, 0o100600),
            attrs(17, 0o100600),
            handle(18),
            attrs(19, 0o100600),
            {
                let mut p = Vec::new();
                string(&mut p, b"abc");
                frame(103, 20, &p)
            },
            status(21, 1),
            status(22, 0),
            handle(23),
            attrs(24, 0o100600),
            {
                let mut p = Vec::new();
                string(&mut p, b"12345");
                frame(103, 25, &p)
            },
            status(26, 1),
            status(27, 0),
        ]);
        let mut plan = Vec::new();
        download
            .download_tree(&mut plan, &root, "/tree", 0)
            .unwrap();
        assert!(!root.exists()); // Discovery completes before any destination mutation.
        unique_destinations(&plan, false).unwrap();
        let changed = AtomicBool::new(false);
        assert_eq!(
            download
                .copy_plan(
                    plan,
                    false,
                    false,
                    &changed,
                    Instant::now() + Duration::from_secs(15)
                )
                .unwrap(),
            (8, 2, 3)
        );
        assert!(root.join("empty").is_dir());
        assert_eq!(fs::read(root.join("nested/a.bin")).unwrap(), b"abc");
        assert_eq!(fs::read(root.join("z.bin")).unwrap(), b"12345");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recursive_download_rejects_unsafe_names_symlinks_and_duplicate_targets() {
        for name in ["../escape", "/absolute", "a/b", "a\\b", "bad\nname"] {
            let root = temp();
            let mut download = client(vec![attrs(1, 0o040700), handle(2), names(3, &[name])]);
            assert_eq!(
                download
                    .download_tree(&mut Vec::new(), &root, "/tree", 0)
                    .unwrap_err()
                    .code,
                "validation"
            );
            assert!(!root.exists());
        }
        let root = temp();
        let mut symlink = client(vec![attrs(1, 0o120777)]);
        assert_eq!(
            symlink
                .download_tree(&mut Vec::new(), &root, "/link", 0)
                .unwrap_err()
                .code,
            "validation"
        );
        let mut duplicate = client(vec![
            attrs(1, 0o040700),
            handle(2),
            names(3, &["a", "a"]),
            status(4, 1),
            status(5, 0),
            attrs(6, 0o100600),
            attrs(7, 0o100600),
        ]);
        let mut plan = Vec::new();
        duplicate
            .download_tree(&mut plan, &root, "/tree", 0)
            .unwrap();
        assert_eq!(
            unique_destinations(&plan, false).unwrap_err().code,
            "validation"
        );
        assert!(!root.exists());
        #[cfg(windows)]
        for name in ["NUL.txt", "COM1", "LPT¹", "file:stream", "name.", "name "] {
            assert!(component(name).is_err(), "{name}");
        }
    }

    #[test]
    fn batch_stops_on_conflict_and_reports_completed_files() {
        let root = temp();
        fs::create_dir(&root).unwrap();
        let paths = [root.join("a"), root.join("b"), root.join("c")];
        for path in &paths {
            fs::write(path, b"abc").unwrap();
        }
        let plan = paths
            .iter()
            .map(|p| Entry {
                local: p.clone(),
                remote: format!("/{}", p.file_name().unwrap().to_str().unwrap()),
                directory: false,
            })
            .collect();
        let mut upload = client(vec![
            status(1, 2),
            handle(2),
            attrs(3, 0o100600),
            status(4, 0),
            status(5, 0),
            attrs(6, 0o100600),
        ]);
        let changed = AtomicBool::new(false);
        let error = upload
            .copy_plan(
                plan,
                true,
                false,
                &changed,
                Instant::now() + Duration::from_secs(15),
            )
            .unwrap_err();
        assert_eq!(error.code, "conflict");
        assert!(error.message.contains("completed 1 files, 3 bytes"));
        assert!(changed.load(Ordering::SeqCst));
        assert_eq!(upload.id, 6); // The third source was never attempted.
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn destination_directories_never_replace_files_or_symlinks() {
        let path = temp();
        fs::write(&path, b"keep").unwrap();
        let changed = AtomicBool::new(false);
        assert_eq!(
            local_directory(&path, &changed).unwrap_err().code,
            "validation"
        );
        for mode in [0o100600, 0o120777] {
            assert_eq!(
                client(vec![attrs(1, mode)])
                    .directory("/dest", &changed)
                    .unwrap_err()
                    .code,
                "validation"
            );
        }
        assert!(!changed.load(Ordering::SeqCst));
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn deadline_also_bounds_local_discovery_and_empty_directory_downloads() {
        let root = temp();
        let deadline = Instant::now();
        assert_eq!(
            upload_tree(&mut Vec::new(), &root, "/tree", true, 0, deadline)
                .unwrap_err()
                .code,
            "timeout"
        );
        let plan = vec![Entry {
            local: root.clone(),
            remote: "/tree".into(),
            directory: true,
        }];
        let changed = AtomicBool::new(false);
        assert_eq!(
            client(vec![])
                .copy_plan(plan, false, false, &changed, deadline)
                .unwrap_err()
                .code,
            "timeout"
        );
        assert!(!root.exists());
        assert!(!changed.load(Ordering::SeqCst));
    }

    #[test]
    fn binary_download_overwrites_only_when_requested_and_truncates_old_tail() {
        let path = temp();
        fs::write(&path, vec![42; CHUNK * 2]).unwrap();
        let content = b"\0\xff\r\n\x1b binary";
        let mut data = Vec::new();
        string(&mut data, content);
        let changed = AtomicBool::new(false);
        let mut refused = client(vec![handle(1), attrs(2, 0o100600)]);
        assert_eq!(
            refused
                .copy(false, &path, "/file", false, &changed)
                .unwrap_err()
                .code,
            "conflict"
        );
        assert!(!changed.load(Ordering::SeqCst));
        assert_eq!(fs::metadata(&path).unwrap().len(), (CHUNK * 2) as u64);
        let mut download = client(vec![
            handle(1),
            attrs(2, 0o100600),
            frame(103, 3, &data),
            status(4, 1),
            status(5, 0),
        ]);
        assert_eq!(
            download
                .copy(false, &path, "/file", true, &changed)
                .unwrap(),
            content.len() as u64
        );
        assert_eq!(fs::read(&path).unwrap(), content);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn upload_uses_exclusive_creation_and_literal_paths_and_handles_multiple_chunks() {
        let path = temp();
        let content: Vec<u8> = (0..CHUNK + 13).map(|n| n as u8).collect();
        fs::write(&path, &content).unwrap();
        let remote = "/tmp/a b'\";$[]雪";
        let changed = AtomicBool::new(false);
        let mut upload = client(vec![
            status(1, 2),
            handle(2),
            attrs(3, 0o100600),
            status(4, 0),
            status(5, 0),
            status(6, 0),
        ]);
        assert_eq!(
            upload.copy(true, &path, remote, false, &changed).unwrap(),
            content.len() as u64
        );
        // Decode the actual OPEN request to verify race-safe no-clobber flags.
        let mut sent = Decoder(&upload.input);
        for _ in 0..2 {
            let n = sent.number().unwrap() as usize;
            sent.take(n).unwrap();
        }
        let n = sent.number().unwrap() as usize;
        let mut open = Decoder(sent.take(n).unwrap());
        assert_eq!(open.take(1).unwrap(), &[3]);
        assert_eq!(open.number().unwrap(), 2);
        assert_eq!(open.string().unwrap(), remote.as_bytes());
        assert_eq!(open.number().unwrap(), WRITE | CREATE | EXCLUSIVE);
        assert!(changed.load(Ordering::SeqCst));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn existing_remote_file_is_preserved_and_failed_close_is_reported() {
        let path = temp();
        fs::write(&path, b"").unwrap();
        let changed = AtomicBool::new(false);
        let mut existing = client(vec![attrs(1, 0o100600)]);
        assert_eq!(
            existing
                .copy(true, &path, "/file", false, &changed)
                .unwrap_err()
                .code,
            "conflict"
        );
        assert!(!changed.load(Ordering::SeqCst));
        let mut failed = client(vec![
            status(1, 2),
            handle(2),
            attrs(3, 0o100600),
            status(4, 4),
        ]);
        assert_eq!(
            failed
                .copy(true, &path, "/file", false, &changed)
                .unwrap_err()
                .code,
            "remote"
        );
        assert!(changed.load(Ordering::SeqCst));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn nonregular_remote_sources_do_not_create_local_files() {
        let path = temp();
        let changed = AtomicBool::new(false);
        let mut download = client(vec![handle(1), attrs(2, 0o040755)]);
        assert_eq!(
            download
                .copy(false, &path, "/directory", false, &changed)
                .unwrap_err()
                .code,
            "validation"
        );
        assert!(!path.exists());
    }

    #[test]
    fn corrupt_and_oversized_packets_fail_without_panicking() {
        for bytes in [
            vec![0, 0, 0, 0],
            u32::MAX.to_be_bytes().to_vec(),
            frame(2, 4, b""),
            vec![0, 0, 0, 5, 2, 0],
        ] {
            assert!(Client::connect(Vec::new(), Cursor::new(bytes)).is_err());
        }
        let mut mismatched = client(vec![handle(99)]);
        assert_eq!(
            mismatched.open("/file", READ).unwrap_err().code,
            "transport"
        );
        let path = temp();
        let changed = AtomicBool::new(false);
        let mut empty = client(vec![
            handle(1),
            attrs(2, 0o100600),
            frame(103, 3, &[0, 0, 0, 0]),
        ]);
        assert_eq!(
            empty
                .copy(false, &path, "/file", false, &changed)
                .unwrap_err()
                .code,
            "transport"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn transport_deadline_kills_ssh_and_reports_ambiguous_creation_and_diagnostics() {
        let dir = temp();
        fs::create_dir(&dir).unwrap();
        let source = dir.join("mock.rs");
        let exe = dir.join(format!("mock{}", std::env::consts::EXE_SUFFIX));
        fs::write(&source, include_str!("../tests/fixtures/sftp_transport.rs")).unwrap();
        assert!(Command::new("rustc")
            .args(["--edition", "2021"])
            .arg(&source)
            .arg("-o")
            .arg(&exe)
            .status()
            .unwrap()
            .success());
        let local_path = dir.join("source.bin");
        fs::write(&local_path, b"payload").unwrap();
        let connection: config::Connection =
            serde_json::from_str(r#"{"local_forward":["8080:host:80"]}"#).unwrap();
        for (host, code, ambiguous) in [
            ("before", "timeout", false),
            ("after-open", "timeout", true),
            ("denied", "transport", false),
        ] {
            let cli = Cli::try_parse_from([
                "shell-cli",
                "--timeout",
                "1",
                "upload",
                "debug",
                local_path.to_str().unwrap(),
                "/tmp/file",
            ])
            .unwrap();
            let mut reply = Reply::new("upload", host, Some("debug"));
            let started = Instant::now();
            let error = execute(&exe, &connection, &cli, &mut reply).unwrap_err();
            assert_eq!(error.code, code, "{error:?}");
            assert_eq!(error.unknown, ambiguous);
            assert!(reply.transfer.is_none());
            assert!(started.elapsed() < Duration::from_secs(4));
            if host == "denied" {
                assert!(error.message.contains("Permission denied (publickey)."));
            }
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
