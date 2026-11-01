//! One-shot SSH commands: pipe output and process exit, independent of terminal sessions.
use super::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
    error: Option<String>,
}

fn capture(mut pipe: impl Read + Send + 'static) -> (Arc<Mutex<Capture>>, thread::JoinHandle<()>) {
    let data = Arc::new(Mutex::new(Capture::default()));
    let shared = Arc::clone(&data);
    let worker = thread::spawn(move || {
        let mut buf = [0; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut data = shared.lock().unwrap();
                    let room = MAX_OUTPUT.saturating_sub(data.bytes.len());
                    data.bytes.extend_from_slice(&buf[..n.min(room)]);
                    data.truncated |= n > room;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    shared.lock().unwrap().error = Some(e.to_string());
                    break;
                }
            }
        }
    });
    (data, worker)
}

pub(super) fn execute(cli: &Cli, host: &str, script: &str, reply: &mut Reply) -> Result<(), Fault> {
    reply.completed = Some(false);
    reply.stdout = Some(String::new());
    reply.stderr = Some(String::new());
    let deadline = Instant::now() + Duration::from_secs(cli.timeout);
    let mut command = Command::new(&cli.ssh);
    command
        .args(cli.connection.args())
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
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|e| fault("transport", format!("cannot start SSH: {e}"), 3))?;
    // Starting SSH cannot establish that the remote command was delivered.
    reply.delivery = "unknown".into();
    let (stdout, out_worker) = capture(child.stdout.take().unwrap());
    let (stderr, err_worker) = capture(child.stderr.take().unwrap());
    let mut status = None;
    let outcome = loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(e) => break Err(fault("transport", format!("cannot wait for SSH: {e}"), 3)),
            }
        }
        if status.is_some() && out_worker.is_finished() && err_worker.is_finished() {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(fault(
                "timeout",
                "SSH deadline exceeded; remote outcome may be unknown; command was not replayed",
                6,
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    if outcome.is_err() && status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Never join a blocked pipe reader: a descendant may still hold its pipe.
    // Incremental capture preserves output already read even at the deadline.
    let out = stdout.lock().unwrap();
    let err = stderr.lock().unwrap();
    reply.stdout = Some(String::from_utf8_lossy(&out.bytes).into_owned());
    reply.stderr = Some(String::from_utf8_lossy(&err.bytes).into_owned());
    reply.truncated = out.truncated || err.truncated;
    outcome?;
    if let Some(error) = out.error.as_ref().or(err.error.as_ref()) {
        return Err(fault(
            "local",
            format!("cannot read SSH output: {error}"),
            3,
        ));
    }
    let code = status.and_then(|s| s.code()).filter(|&code| code != 255)
        .ok_or_else(|| fault("transport", "SSH failed or was interrupted (255 is reserved by OpenSSH); inspect stderr; remote outcome may be unknown", 3))?;
    reply.delivery = "acknowledged".into();
    reply.completed = Some(true);
    reply.exit_code = Some(code);
    if code != 0 {
        return Err(fault(
            "remote",
            format!("remote command exited with code {code}; inspect stderr and stdout"),
            3,
        ));
    }
    Ok(())
}
