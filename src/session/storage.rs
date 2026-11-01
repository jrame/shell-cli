//! Local session records, directory protection, locks and stale-owner cleanup.
use super::*;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;

pub(super) fn state_dir() -> Result<PathBuf, Fault> {
    let dir = if let Some(p) = std::env::var_os("SHELL_CLI_STATE_DIR") {
        PathBuf::from(p)
    } else {
        #[cfg(windows)]
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")));
        base.ok_or_else(|| local("cannot locate the user's local state directory"))?
            .join("shell-cli/sessions")
    };
    fs::create_dir_all(&dir).map_err(|e| {
        local(format!("cannot create session directory '{}': {e}; choose a writable directory with SHELL_CLI_STATE_DIR", dir.display()))
    })?;
    if fs::symlink_metadata(&dir)
        .map_err(local)?
        .file_type()
        .is_symlink()
    {
        return Err(local("session directory must not be a symbolic link"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(local)?;
    }
    #[cfg(windows)]
    {
        // Protect endpoint credentials even when a custom directory is used.
        use std::os::windows::process::CommandExt;
        let account = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").map_err(local)?,
            std::env::var("USERNAME").map_err(local)?
        );
        let output = Command::new("icacls.exe")
            .arg(&dir)
            .args([
                "/inheritance:r",
                "/grant:r",
                &format!("{account}:(OI)(CI)F"),
            ])
            .creation_flags(0x08000000)
            .output()
            .map_err(|e| {
                directory_protection_error(&dir, &account, &format!("cannot start icacls.exe: {e}"))
            })?;
        if !output.status.success() {
            return Err(directory_protection_error(
                &dir,
                &account,
                &format!(
                    "icacls.exe exited with {}; stderr: {}; stdout: {}",
                    output.status,
                    lines(&output.stderr),
                    lines(&output.stdout)
                ),
            ));
        }
    }
    fs::canonicalize(dir).map_err(local)
}

#[cfg(windows)]
fn directory_protection_error(dir: &Path, account: &str, detail: &str) -> Fault {
    local(format!(
        "cannot protect session directory '{}' for account '{}': {}. shell-cli must restrict this directory's Windows permissions to protect session credentials. A sandbox or restricted process may block this ACL change even when the directory is writable. Retry outside the sandbox under the same Windows account, or set SHELL_CLI_STATE_DIR to a dedicated directory where that account can change permissions",
        dir.display(), account, detail
    ))
}

#[cfg(all(test, windows))]
mod protection_tests {
    use super::*;

    #[test]
    fn acl_failure_explains_the_path_operation_and_sandbox_remedy() {
        let error = directory_protection_error(
            Path::new(r"C:\state with spaces\sessions"),
            r"DOMAIN\user",
            "icacls.exe exited with 5; stderr: Accès refusé",
        );
        for expected in [
            r"C:\state with spaces\sessions",
            r"DOMAIN\user",
            "icacls.exe",
            "Accès refusé",
            "ACL",
            "outside the sandbox",
            "SHELL_CLI_STATE_DIR",
        ] {
            assert!(error.message.contains(expected), "{}", error.message);
        }
        assert_eq!(error.exit, 3);
    }
}

pub(super) fn entry_path(dir: &Path, host: &str, session: &str) -> PathBuf {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    host.hash(&mut hash);
    session.hash(&mut hash);
    dir.join(format!("{:016x}.json", hash.finish()))
}

pub(super) fn lock(path: &Path) -> Result<File, Fault> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .map_err(local)?;
    f.try_lock_exclusive().map_err(|_| {
        fault(
            "conflict",
            "another operation is active for this session",
            5,
        )
    })?;
    Ok(f)
}

pub(super) fn load(path: &Path) -> Result<Entry, Fault> {
    let file = File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            fault(
                "session_not_found",
                "session does not exist on this client",
                4,
            )
        } else {
            local(e)
        }
    })?;
    serde_json::from_reader(file.take(1_048_576)).map_err(local)
}

// Unlike metadata, this OS lock cannot survive a crash or a reboot. Older
// owners have no lifetime lock, so only a refused connection proves them dead.
pub(super) fn owner_stopped(path: &Path, entry: &Entry) -> Result<bool, Fault> {
    if entry.owner_lock {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("owner"))
            .map_err(local)?;
        return match file.try_lock_exclusive() {
            Ok(()) => Ok(true),
            Err(e) if e.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(false),
            Err(e) => Err(local(e)),
        };
    }
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, entry.port));
    // Windows may take roughly two seconds to report a loopback refusal.
    match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Ok(true),
        // Timeouts and protocol errors are not evidence of a stopped owner.
        _ => Ok(false),
    }
}

pub(super) fn stopped_owner() -> Fault {
    fault(
        "session_not_found",
        "local session owner is no longer running; open a new session",
        4,
    )
}

pub(super) fn remove_stopped_record(path: &Path, id: &str) -> Result<(), Fault> {
    let _lock = lock(path)?;
    match load(path) {
        Ok(entry) if entry.id == id && owner_stopped(path, &entry)? => {
            fs::remove_file(path).map_err(local)
        }
        Ok(_) => Ok(()),
        Err(e) if e.exit == 4 => Ok(()),
        Err(e) => Err(e),
    }
}
