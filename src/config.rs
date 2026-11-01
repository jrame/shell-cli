//! Strict connection profiles and shared OpenSSH argument construction.
use super::{context, fault, valid_host, valid_name, Action, ConfigAction, Fault};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

#[derive(Args, Default)]
#[group(id = "connection_options")]
pub(super) struct Options {
    /// Connection profile JSON (default: .config/shell-cli/config.json, then ~/.config/shell-cli/config.json).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Named connection profile. Explicit connection options take precedence.
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(flatten)]
    direct: Connection,
}

#[derive(Args, Clone, Serialize, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Connection {
    #[arg(skip)]
    pub(super) host: Option<String>,
    /// Private key path. Unlock encrypted keys through ssh-agent.
    #[arg(long, short = 'i', global = true)]
    identity: Option<PathBuf>,
    #[arg(long, short = 'p', global = true, value_parser = clap::value_parser!(u16).range(1..))]
    port: Option<u16>,
    /// OpenSSH ProxyJump destination(s).
    #[arg(long, short = 'J', global = true)]
    jump: Option<String>,
    /// TCP local forwarding: [bind_address:]port:host:hostport. Repeatable.
    #[arg(long, short = 'L', global = true, action = clap::ArgAction::Append)]
    local_forward: Vec<String>,
    /// TCP remote forwarding: [bind_address:]port:host:hostport. Repeatable.
    #[arg(long, short = 'R', global = true, action = clap::ArgAction::Append)]
    remote_forward: Vec<String>,
    /// SOCKS forwarding: [bind_address:]port. Repeatable.
    #[arg(long, short = 'D', global = true, action = clap::ArgAction::Append)]
    dynamic_forward: Vec<String>,
    /// Alternative OpenSSH client configuration file.
    #[arg(long, short = 'F', global = true)]
    ssh_config: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Config {
    profiles: BTreeMap<String, Connection>,
}

fn invalid(message: impl Into<String>) -> Fault {
    fault("validation", message, 2)
}

fn default_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
                .map(|p| PathBuf::from(p).join(".config"))
        })
        .map(|p| p.join("shell-cli/config.json"))
}

impl Options {
    fn load_config(&self) -> Result<(Config, Option<PathBuf>), Fault> {
        let paths = if let Some(path) = &self.config {
            vec![path.clone()]
        } else {
            std::iter::once(PathBuf::from(".config/shell-cli/config.json"))
                .chain(default_path())
                .collect()
        };
        for path in paths {
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.config.is_none() => {
                    continue;
                }
                Err(e) => return Err(invalid(format!("cannot open connection config: {e}"))),
            };
            let mut bytes = Vec::new();
            file.take(1_048_577)
                .read_to_end(&mut bytes)
                .map_err(|e| invalid(format!("cannot read connection config: {e}")))?;
            if bytes.len() > 1_048_576 {
                return Err(invalid("connection config exceeds 1 MiB"));
            }
            return serde_json::from_slice::<Config>(&bytes)
                .map(|config| (config, Some(path)))
                .map_err(|e| invalid(format!("invalid connection config: {e}")));
        }
        Ok((Config::default(), None))
    }

    pub(super) fn resolve(
        &self,
        action: &mut Action,
    ) -> Result<(Connection, Option<serde_json::Value>), Fault> {
        let (config, source) = self.load_config()?;
        // Reject invalid data even in an unselected profile, so config mistakes
        // cannot become latent behavior changes when the profile is selected.
        for (name, connection) in &config.profiles {
            if !valid_name(name) {
                return Err(invalid(format!("invalid connection profile name: {name}")));
            }
            connection.validate()?;
        }
        let mut resolved = if let Some(profile) = &self.profile {
            config
                .profiles
                .get(profile)
                .cloned()
                .ok_or_else(|| invalid(format!("unknown connection profile: {profile}")))?
        } else {
            Connection::default()
        };
        resolved.overlay(&self.direct);
        resolved.validate()?;
        if matches!(action, Action::Profiles { .. } | Action::Config { .. }) {
            let explicit_host = context(action).1;
            if !explicit_host.is_empty() {
                if !valid_host(explicit_host) {
                    return Err(invalid("invalid SSH host or alias"));
                }
                resolved.host = Some(explicit_host.to_owned());
            }
            resolved.absolute_paths()?;
            let source = source.map(|p| {
                if p.is_absolute() {
                    p
                } else {
                    std::env::current_dir().unwrap_or_default().join(p)
                }
            });
            let report = if matches!(action, Action::Profiles { .. }) {
                serde_json::json!({"config_path": source, "profiles": config.profiles.iter().map(|(name, value)| serde_json::json!({"name": name, "host": value.host})).collect::<Vec<_>>()})
            } else {
                let mut overrides = self.direct.clone();
                if !explicit_host.is_empty() {
                    overrides.host = Some(explicit_host.to_owned());
                }
                serde_json::json!({
                    "scope": "shell-cli settings only; OpenSSH configuration and agent defaults are not expanded",
                    "config_path": source, "profile": self.profile,
                    "precedence": "explicit host and CLI scalar options override profile; CLI forwards append to profile forwards",
                    "cli_overrides": overrides, "effective": resolved,
                })
            };
            if let Action::Config {
                command: ConfigAction::Show { host },
            } = action
            {
                *host = resolved.host.clone().unwrap_or_default();
            }
            return Ok((resolved, Some(report)));
        }
        let host = match action {
            Action::Profiles { .. } => unreachable!("diagnostics resolved above"),
            Action::Config {
                command: ConfigAction::Show { host },
            } => host,
            Action::Doctor { host }
            | Action::Exec { host, .. }
            | Action::Show { host, .. }
            | Action::View { host, .. }
            | Action::Open { host, .. }
            | Action::List { host }
            | Action::Status { host, .. }
            | Action::Read { host, .. }
            | Action::Send { host, .. }
            | Action::Keys { host, .. }
            | Action::Resize { host, .. }
            | Action::Upload { host, .. }
            | Action::Download { host, .. }
            | Action::Close { host, .. } => host,
        };
        if host.is_empty() {
            if let Some(profile_host) = &resolved.host {
                *host = profile_host.clone();
            }
        }
        resolved.host = Some(context(action).1.to_owned());
        Ok((resolved, None))
    }
}

impl Connection {
    pub(super) fn for_transfer(&self) -> Self {
        let mut connection = self.clone();
        connection.local_forward.clear();
        connection.remote_forward.clear();
        connection.dynamic_forward.clear();
        connection
    }

    // Preserve relative key/config paths when later calls use another directory.
    pub(super) fn absolute_paths(&mut self) -> Result<(), Fault> {
        let cwd = std::env::current_dir().map_err(|e| invalid(e.to_string()))?;
        for path in [&mut self.identity, &mut self.ssh_config]
            .into_iter()
            .flatten()
        {
            // OpenSSH expands ~ and ~user itself, including for identity files.
            if path.is_relative() && !path.to_string_lossy().starts_with('~') {
                *path = cwd.join(&*path);
            }
        }
        Ok(())
    }

    fn overlay(&mut self, direct: &Self) {
        if direct.identity.is_some() {
            self.identity.clone_from(&direct.identity);
        }
        if direct.port.is_some() {
            self.port = direct.port;
        }
        if direct.jump.is_some() {
            self.jump.clone_from(&direct.jump);
        }
        if direct.ssh_config.is_some() {
            self.ssh_config.clone_from(&direct.ssh_config);
        }
        self.local_forward
            .extend(direct.local_forward.iter().cloned());
        self.remote_forward
            .extend(direct.remote_forward.iter().cloned());
        self.dynamic_forward
            .extend(direct.dynamic_forward.iter().cloned());
    }

    pub(super) fn args(&self) -> Vec<OsString> {
        let mut args = Vec::new();
        if let Some(path) = &self.ssh_config {
            args.extend([OsString::from("-F"), path.as_os_str().to_owned()]);
        }
        if let Some(path) = &self.identity {
            args.extend([OsString::from("-i"), path.as_os_str().to_owned()]);
        }
        if let Some(port) = self.port {
            args.extend([OsString::from("-p"), port.to_string().into()]);
        }
        if let Some(jump) = &self.jump {
            args.extend([OsString::from("-J"), jump.into()]);
        }
        for (flag, forwards) in [
            ("-L", &self.local_forward),
            ("-R", &self.remote_forward),
            ("-D", &self.dynamic_forward),
        ] {
            for forward in forwards {
                args.extend([OsString::from(flag), forward.into()]);
            }
        }
        if !(self.local_forward.is_empty()
            && self.remote_forward.is_empty()
            && self.dynamic_forward.is_empty())
        {
            args.extend(["-o".into(), "ExitOnForwardFailure=yes".into()]);
        }
        args
    }

    fn validate(&self) -> Result<(), Fault> {
        if self.host.as_deref().is_some_and(|s| !valid_host(s)) {
            return Err(invalid("invalid SSH host in connection profile"));
        }
        if self.port == Some(0) {
            return Err(invalid("SSH port must be 1..65535"));
        }
        for path in [&self.identity, &self.ssh_config].into_iter().flatten() {
            if path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control) {
                return Err(invalid(
                    "SSH file paths must be nonempty and contain no control characters",
                ));
            }
        }
        if self
            .jump
            .as_deref()
            .is_some_and(|jump| jump.split(',').any(|destination| !valid_jump(destination)))
        {
            return Err(invalid(
                "invalid jump destination; use [user@]host[:port], comma separated",
            ));
        }
        for (kind, forwards) in [
            ("local", &self.local_forward),
            ("remote", &self.remote_forward),
            ("dynamic", &self.dynamic_forward),
        ] {
            for forward in forwards {
                if !valid_forward(forward, kind == "dynamic", kind == "remote") {
                    return Err(invalid(format!(
                        "invalid {kind} TCP forwarding specification: {forward}"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn port(value: &str, allow_zero: bool) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u16>().is_ok_and(|p| allow_zero || p != 0)
}

// Split colons outside IPv6 brackets. OpenSSH receives the original string.
fn parts(value: &str) -> Option<Vec<&str>> {
    if value.is_empty()
        || value.starts_with('-')
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let mut depth = false;
    let mut start = 0;
    let mut fields = Vec::new();
    for (index, ch) in value.char_indices() {
        match ch {
            '[' if !depth => depth = true,
            '[' => return None,
            ']' if depth => depth = false,
            ']' => return None,
            ':' if !depth => {
                fields.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if depth {
        return None;
    }
    fields.push(&value[start..]);
    Some(fields)
}

fn address(value: &str, bind: bool) -> bool {
    if bind && (value.is_empty() || value == "*") {
        return true;
    }
    if let Some(ip) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return ip.parse::<std::net::Ipv6Addr>().is_ok();
    }
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn valid_jump(value: &str) -> bool {
    let Some(fields) = parts(value) else {
        return false;
    };
    if !(1..=2).contains(&fields.len()) || (fields.len() == 2 && !port(fields[1], false)) {
        return false;
    }
    let target = fields[0];
    let host = if let Some((user, host)) = target.split_once('@') {
        if user.is_empty()
            || !user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            return false;
        }
        host
    } else {
        target
    };
    address(host, false)
}

fn valid_forward(value: &str, dynamic: bool, remote: bool) -> bool {
    let Some(fields) = parts(value) else {
        return false;
    };
    if dynamic {
        return match fields.as_slice() {
            [p] => port(p, false),
            [bind, p] => address(bind, true) && port(p, false),
            _ => false,
        };
    }
    match fields.as_slice() {
        [p, host, target] => port(p, remote) && address(host, false) && port(target, false),
        [bind, p, host, target] => {
            address(bind, true) && port(p, remote) && address(host, false) && port(target, false)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;

    #[test]
    fn strict_profiles_reject_passwords_and_typos() {
        for json in [
            r#"{"password":"secret"}"#,
            r#"{"profiles":{"lab":{"password":"secret"}}}"#,
            r#"{"profiles":{"lab":{"ports":22}}}"#,
        ] {
            assert!(serde_json::from_str::<Config>(json).is_err());
        }
    }

    #[test]
    fn forwarding_and_jump_validation() {
        for value in [
            "8080:localhost:80",
            "127.0.0.1:8080:host:443",
            "[::1]:8080:[2001:db8::1]:443",
            ":8080:host:80",
        ] {
            assert!(valid_forward(value, false, false), "{value}");
        }
        for value in [
            "0:localhost:80",
            "8080:host:0",
            "65536:host:80",
            "-L8080:host:80",
            "8080:bad host:80",
            "[::1:80:host:80",
        ] {
            assert!(!valid_forward(value, false, false), "{value}");
        }
        assert!(valid_forward("0:localhost:80", false, true));
        assert!(valid_forward("[::1]:1080", true, false));
        assert!(valid_jump("user@[::1]:2222"));
        assert!(!valid_jump("-oProxyCommand=evil"));
        assert!(!valid_jump("host:0"));
    }

    #[test]
    fn args_preserve_paths_and_request_forward_failures() {
        let connection: Connection = serde_json::from_str(r#"{"identity":"a key.pem","port":2222,"jump":"bastion","local_forward":["8080:host:80"],"remote_forward":["9090:host:90"],"dynamic_forward":["1080"],"ssh_config":"ssh config"}"#).unwrap();
        connection.validate().unwrap();
        assert_eq!(
            connection.args(),
            [
                "-F",
                "ssh config",
                "-i",
                "a key.pem",
                "-p",
                "2222",
                "-J",
                "bastion",
                "-L",
                "8080:host:80",
                "-R",
                "9090:host:90",
                "-D",
                "1080",
                "-o",
                "ExitOnForwardFailure=yes"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn transfer_connection_keeps_authentication_without_reopening_tunnels() {
        let mut connection: Connection = serde_json::from_str(r#"{"identity":"key.pem","ssh_config":"ssh.conf","port":2222,"jump":"bastion","local_forward":["8080:host:80"],"remote_forward":["9090:host:90"],"dynamic_forward":["1080"]}"#).unwrap();
        connection.absolute_paths().unwrap();
        let transfer = connection.for_transfer();
        assert!(transfer.identity.as_ref().unwrap().is_absolute());
        assert!(transfer.ssh_config.as_ref().unwrap().is_absolute());
        assert_eq!(transfer.port, Some(2222));
        assert_eq!(transfer.jump.as_deref(), Some("bastion"));
        assert!(transfer.local_forward.is_empty());
        assert!(transfer.remote_forward.is_empty());
        assert!(transfer.dynamic_forward.is_empty());
        assert_eq!(connection.local_forward.len(), 1);
        let mut home_key: Connection =
            serde_json::from_str(r#"{"identity":"~/.ssh/key.pem"}"#).unwrap();
        home_key.absolute_paths().unwrap();
        assert_eq!(home_key.identity.unwrap(), PathBuf::from("~/.ssh/key.pem"));
    }

    #[test]
    fn profile_resolution_and_direct_precedence() {
        let path =
            std::env::temp_dir().join(format!("shell-cli-profile-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, r#"{"profiles":{"lab":{"host":"user@lab","port":22,"local_forward":["8080:localhost:80"]}}}"#).unwrap();
        let mut cli = Cli::try_parse_from([
            OsString::from("shell"),
            "--config".into(),
            path.as_os_str().to_owned(),
            "open".into(),
            "debug".into(),
            "--profile".into(),
            "lab".into(),
            "--port".into(),
            "2222".into(),
            "-L".into(),
            "9090:localhost:90".into(),
        ])
        .unwrap();
        let (resolved, _) = cli.connection_options.resolve(&mut cli.command).unwrap();
        assert_eq!(context(&cli.command).1, "user@lab");
        assert_eq!(resolved.port, Some(2222));
        assert_eq!(
            resolved.local_forward,
            ["8080:localhost:80", "9090:localhost:90"]
        );
        assert!(matches!(cli.command, Action::Open { session, .. } if session == "debug"));
        std::fs::remove_file(path).unwrap();
    }
}
