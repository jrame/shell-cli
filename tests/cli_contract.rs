use serde_json::Value;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn invoke(args: &[&str], input: Option<&[u8]>) -> Output {
    invoke_in_state(args, input, None)
}

fn invoke_in_state(args: &[&str], input: Option<&[u8]>, state: Option<&Path>) -> Output {
    invoke_with_format(args, input, state, true)
}

fn invoke_with_format(
    args: &[&str],
    input: Option<&[u8]>,
    state: Option<&Path>,
    json: bool,
) -> Output {
    // A nonexistent executable makes accidental transport access fail immediately.
    let mut command = Command::new(env!("CARGO_BIN_EXE_shell"));
    if let Some(state) = state {
        command.env("SHELL_CLI_STATE_DIR", state);
    }
    if json {
        command.args(["--format", "json"]);
    }
    let mut child = command
        .args(["--ssh", "shell-cli-no-ssh-for-contract-tests"])
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    child.wait_with_output().unwrap()
}

#[test]
fn executable_and_help_use_shell_name() {
    let output = invoke(&["--version"], None);
    assert!(output.status.success());
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        reply["screen"],
        format!("shell {}\n", env!("CARGO_PKG_VERSION"))
    );

    let output = invoke(&["--help"], None);
    assert!(output.status.success());
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    let help = reply["screen"].as_str().unwrap();
    assert!(help.contains("Usage: shell "));
    assert!(!help.contains("Usage: shell-cli"));
}

#[test]
fn markdown_is_default_and_errors_automatically_include_details() {
    let output = invoke_with_format(&["doctor", "example"], None, None, false);
    assert_eq!(output.status.code(), Some(3));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("###screen\n###metadata\ncursor: null\nsize: null\ntimestamp_ms: "));
    assert!(text.contains("action: doctor\n"));
    assert!(text.contains("code: transport\n"));
    assert!(text.contains("delivery: not_sent\n"));

    let output = invoke_with_format(&["unknown-command"], None, None, false);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("action: parse\n"));
}

#[test]
fn detail_flag_works_before_and_after_the_command() {
    for args in [
        vec!["--detail", "--help"],
        vec!["read", "--detail", "--help"],
    ] {
        let output = invoke_with_format(&args, None, None, false);
        assert_eq!(output.status.code(), Some(0));
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.starts_with("###screen\n"));
        assert!(text.contains("action: parse\n"));
        assert!(text.contains("alternate_screen: null\n"));
        assert!(text.contains("error: null\n"));
    }
    let output = invoke_with_format(&["--help"], None, None, false);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("action: parse\n"));
}

fn validation(output: Output) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "expected one JSON response: {e}; stdout={:?}; stderr={:?}",
            output.stdout, output.stderr
        )
    });
    assert_eq!(reply["version"], 1);
    assert_eq!(reply["error"]["code"], "validation");
    assert_eq!(reply["delivery"], "not_sent");
}

#[test]
fn malformed_invocations_are_machine_readable() {
    validation(invoke(&[], None));
    validation(invoke(&["unknown-command"], None));
    validation(invoke(&["open", "example"], None));
}

#[test]
fn invalid_targets_and_dimensions_never_start_transport() {
    validation(invoke(&["open", "session", "host;injected"], None));
    validation(invoke(&["open", "../../session", "example"], None));
    validation(invoke(
        &["open", "session", "example", "--width", "0"],
        None,
    ));
    validation(invoke(&["--timeout", "0", "doctor", "example"], None));
}

#[test]
fn transfer_paths_are_validated_before_transport() {
    for remote in [
        "relative/file",
        "/",
        "/tmp/",
        "/tmp/../file",
        "/tmp/./file",
        "/tmp/a\nfile",
    ] {
        validation(invoke(&["upload", "debug", "file.bin", remote], None));
        validation(invoke(&["download", "debug", remote, "file.bin"], None));
    }
    validation(invoke(
        &["download", "bad/session", "/tmp/file", "file.bin"],
        None,
    ));
    let help = invoke(&["upload", "--help"], None);
    assert_eq!(help.status.code(), Some(0));
    let reply: Value = serde_json::from_slice(&help.stdout).unwrap();
    assert!(reply["screen"].as_str().unwrap().contains("--overwrite"));
}

#[test]
fn unsupported_keys_are_rejected_before_transport() {
    validation(invoke(&["keys", "example", "session", "not-a-key"], None));
}

#[test]
fn stdin_must_be_bounded_valid_text() {
    let args = ["send", "example", "session", "--stdin"];
    validation(invoke(&args, Some(&[0xff, 0xfe])));
    validation(invoke(&args, Some(b"text\0hidden")));
    validation(invoke(&args, Some(&vec![b'x'; 16_385])));
}

#[test]
fn failure_to_start_ssh_cannot_claim_delivery() {
    let output = invoke(&["doctor", "example"], None);
    assert_ne!(output.status.code(), Some(0));
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["error"]["code"], "transport");
    assert_eq!(reply["delivery"], "not_sent");
}

#[test]
fn headed_and_show_validate_targets_before_transport() {
    validation(invoke(
        &["open", "session", "host;injected", "--headed"],
        None,
    ));
    validation(invoke(&["show", "example", "../session"], None));
    let output = invoke(&["--help"], None);
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(reply["screen"].as_str().unwrap().contains("show"));
}

#[test]
fn session_id_option_is_removed_from_the_cli() {
    for args in [
        vec!["send", "debug", "--text", "hello"],
        vec!["keys", "debug", "Enter"],
        vec!["resize", "debug", "80", "24"],
        vec!["close", "debug"],
        vec!["upload", "debug", "file.bin", "/tmp/file"],
        vec!["download", "debug", "/tmp/file", "file.bin"],
    ] {
        let help = invoke(&[args[0], "--help"], None);
        assert!(help.status.success());
        let reply: Value = serde_json::from_slice(&help.stdout).unwrap();
        assert!(!reply["screen"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .any(|word| word == "--id"));
        let mut removed = args;
        removed.extend(["--id", "9658bd1a-a533-4b70-80dc-3a7416796b63"]);
        let rejected = invoke(&removed, None);
        let reply: Value = serde_json::from_slice(&rejected.stdout).unwrap();
        assert_eq!(reply["action"], "parse");
        validation(rejected);
    }
}

#[test]
fn close_by_name_cleans_stopped_owner_and_pins_running_owner() {
    let state = std::env::temp_dir().join(format!("shell-cli-close-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&state).unwrap();
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    "example".hash(&mut hash);
    "debug".hash(&mut hash);
    let path = state.join(format!("{:016x}.json", hash.finish()));
    let new_id = uuid::Uuid::new_v4().to_string();
    // A released loopback port simulates an owner that has stopped.
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let metadata = serde_json::json!({
        "host": "example", "session": "debug", "id": new_id,
        "token": "test", "port": port
    });
    std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let closed = invoke_in_state(&["close", "example", "debug"], None, Some(&state));
    assert_eq!(closed.status.code(), Some(0), "{:?}", closed.stdout);
    let reply: Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(reply["id"], new_id);
    assert_eq!(reply["delivery"], "acknowledged");
    assert!(reply["error"].is_null());
    assert!(!path.exists());

    // A running owner from an older binary still requires an ID on the wire.
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut metadata = metadata;
    metadata["port"] = listener.local_addr().unwrap().port().into();
    std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let owner_id = new_id.clone();
    let owner = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(req["action"]["Close"]["id"], owner_id);
        assert_eq!(req["token"], "test");
        let reply = serde_json::json!({
            "version": 1, "action": "close", "host": "example", "session": "debug",
            "id": owner_id, "delivery": "acknowledged", "timestamp_ms": 0,
            "truncated": false
        });
        writeln!(stream, "{reply}").unwrap();
    });
    let closed = invoke_in_state(&["close", "example", "debug"], None, Some(&state));
    owner.join().unwrap();
    assert_eq!(closed.status.code(), Some(0), "{:?}", closed.stdout);
    let reply: Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(reply["id"], new_id);
    assert_eq!(reply["delivery"], "acknowledged");
    std::fs::remove_dir_all(state).unwrap();
}

fn metadata(state: &Path, host: &str, name: &str, port: u16, id: &str) -> std::path::PathBuf {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    host.hash(&mut hash);
    name.hash(&mut hash);
    let path = state.join(format!("{:016x}.json", hash.finish()));
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "host": host, "session": name, "id": id, "token": "test", "port": port
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn session_only_commands_automatically_pin_owner_identity() {
    use std::io::{BufRead, BufReader};
    let state = std::env::temp_dir().join(format!("shell-cli-names-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&state).unwrap();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    metadata(
        &state,
        "user@example",
        "debug",
        listener.local_addr().unwrap().port(),
        &id,
    );
    let owner_id = id.clone();
    let owner = std::thread::spawn(move || {
        for (index, variant) in [
            "Status", "Read", "Read", "Send", "Keys", "Resize", "Send", "Keys", "Resize", "Close",
        ]
        .into_iter()
        .enumerate()
        {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let action = &req["action"][variant];
            assert_eq!(action["host"], "user@example");
            assert_eq!(action["session"], "debug");
            if !matches!(variant, "Status" | "Read") {
                assert_eq!(action["id"], owner_id);
            }
            if variant == "Read" {
                assert_eq!(action["screen"], if index == 1 { "delta" } else { "full" });
            }
            if matches!(variant, "Send" | "Keys") {
                assert_eq!(action["screen"], "delta");
            }
            let reply = serde_json::json!({
                "version": 1, "action": variant.to_lowercase(), "host": "user@example",
                "session": "debug", "id": owner_id, "delivery": "acknowledged",
                "timestamp_ms": 0, "truncated": false,
                "screen": if variant == "Read" { Some("legacy full screen") } else { None }
            });
            writeln!(stream, "{reply}").unwrap();
        }
    });
    for args in [
        vec!["status", "debug"],
        vec!["read", "debug", "--history", "10"],
        vec!["read", "debug", "--screen", "full"],
        vec!["send", "debug", "--text", "hello", "--enter"],
        vec!["keys", "debug", "Ctrl-C", "Enter"],
        vec!["resize", "debug", "120", "35"],
        vec!["send", "debug", "--text", "hello", "--async"],
        vec!["keys", "debug", "--async", "Ctrl-C"],
        vec!["resize", "debug", "100", "30"],
        vec!["close", "debug"],
    ] {
        let output = invoke_in_state(&args, None, Some(&state));
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stdout);
        if args[0] == "read" {
            let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(reply["screen_mode"], "full");
            assert_eq!(reply["screen"], "legacy full screen");
            assert!(reply["row_wrapped"].is_null());
        }
        if matches!(args[0], "send" | "keys") {
            let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                reply["wait_status"],
                if args.contains(&"--async") {
                    "skipped"
                } else {
                    "unsupported"
                }
            );
        }
    }
    owner.join().unwrap();
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn open_rejects_a_name_on_another_host_before_starting_ssh() {
    let state =
        std::env::temp_dir().join(format!("shell-cli-global-name-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&state).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let original = metadata(
        &state,
        "one",
        "debug",
        listener.local_addr().unwrap().port(),
        &id,
    );
    let before = std::fs::read(&original).unwrap();
    let output = invoke_in_state(&["open", "debug", "two"], None, Some(&state));
    assert_eq!(output.status.code(), Some(5), "{:?}", output.stdout);
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["error"]["code"], "conflict");
    assert_eq!(reply["delivery"], "not_sent");
    assert!(reply["error"]["message"]
        .as_str()
        .unwrap()
        .contains("shell close debug --host one"));
    assert_eq!(std::fs::read(&original).unwrap(), before);
    assert_eq!(
        std::fs::read_dir(&state)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count(),
        1
    );
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn stopped_owner_records_are_removed_automatically() {
    let state = std::env::temp_dir().join(format!("shell-cli-stopped-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&state).unwrap();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let id = uuid::Uuid::new_v4().to_string();
    let path = metadata(&state, "user@example", "debug", port, &id);
    for (args, exit, code) in [
        (vec!["open", "debug", "user@example"], 3, "transport"),
        (vec!["open", "debug", "another-host"], 3, "transport"),
        (vec!["show", "debug"], 4, "session_not_found"),
        (vec!["status", "debug"], 4, "session_not_found"),
        (vec!["list", "user@example"], 0, ""),
    ] {
        metadata(&state, "user@example", "debug", port, &id);
        let output = invoke_in_state(&args, None, Some(&state));
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        if exit == 0 {
            assert!(reply["error"].is_null());
            assert_eq!(reply["sessions"], serde_json::json!([]));
        } else {
            assert_eq!(reply["error"]["code"], code);
            assert_eq!(reply["delivery"], "not_sent");
        }
        assert!(!path.exists());
    }
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn lifetime_lock_distinguishes_live_owner_from_reused_port_after_reboot() {
    use fs2::FileExt;
    let state = std::env::temp_dir().join(format!("shell-cli-reboot-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&state).unwrap();
    // A different process may listen on the old port after a reboot.
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let path = metadata(
        &state,
        "one",
        "debug",
        listener.local_addr().unwrap().port(),
        &id,
    );
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["owner_lock"] = true.into();
    let bytes = serde_json::to_vec(&record).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let owner = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("owner"))
        .unwrap();
    owner.try_lock_exclusive().unwrap();
    let output = invoke_in_state(&["open", "debug", "two"], None, Some(&state));
    assert_eq!(
        output.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    // Process exit/reboot releases the lock but leaves the metadata on disk.
    drop(owner);
    for args in [
        vec!["open", "debug", "one"],
        vec!["open", "debug", "two"],
        vec!["status", "debug"],
        vec!["list", "one"],
    ] {
        std::fs::write(&path, &bytes).unwrap();
        let output = invoke_in_state(&args, None, Some(&state));
        let expected = match args[0] {
            "open" => 3,
            "status" => 4,
            _ => 0,
        };
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!path.exists());
    }
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn profile_host_and_connection_options_parse_before_transport() {
    let state = std::env::temp_dir().join(format!("shell-cli-profile-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&state).unwrap();
    let config = state.join("connections.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "profiles": {"lab": {
                "host": "user@lab", "identity": "a key.pem", "port": 2222,
                "jump": "user@bastion:22", "ssh_config": "ssh config",
                "local_forward": ["127.0.0.1:15432:127.0.0.1:5432"],
                "remote_forward": ["127.0.0.1:18080:127.0.0.1:8080"],
                "dynamic_forward": ["127.0.0.1:1080"]
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let path = config.to_str().unwrap();
    for (args, host) in [
        (
            vec!["--config", path, "doctor", "--profile", "lab"],
            "user@lab",
        ),
        (
            vec![
                "doctor",
                "user@override",
                "--config",
                path,
                "--profile",
                "lab",
                "-i",
                "other key.pem",
                "-p",
                "22",
                "-J",
                "other@jump",
                "-F",
                "other config",
                "-L",
                "127.0.0.1:15433:localhost:5432",
                "-R",
                "127.0.0.1:18081:localhost:8080",
                "-D",
                "127.0.0.1:1081",
            ],
            "user@override",
        ),
    ] {
        let output = invoke_in_state(&args, None, Some(&state));
        // Reaching our deliberately missing SSH executable proves parsing and
        // profile validation succeeded without opening a real connection.
        assert_eq!(output.status.code(), Some(3), "{:?}", output.stdout);
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["error"]["code"], "transport");
        assert_eq!(reply["host"], host);
        assert_eq!(reply["delivery"], "not_sent");
    }
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn invalid_profiles_and_forwarding_fail_without_ssh() {
    let state = std::env::temp_dir().join(format!(
        "shell-cli-invalid-profile-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&state).unwrap();
    let config = state.join("connections.json");
    let path = config.to_str().unwrap();
    for content in [
        r#"{"profiles":{"lab":{"host":"user@lab","password":"secret"}}}"#,
        r#"{"profiles":{"lab":{"host":"user@lab","port":0}}}"#,
        r#"{"profiles":{"lab":{"host":"user@lab","local_forward":["invalid"]}}}"#,
        r#"{"profiles":{"lab":{"host":"-injected"}}}"#,
        "not JSON",
    ] {
        std::fs::write(&config, content).unwrap();
        validation(invoke_in_state(
            &["doctor", "--config", path, "--profile", "lab"],
            None,
            Some(&state),
        ));
    }
    std::fs::write(&config, r#"{"profiles":{}}"#).unwrap();
    validation(invoke_in_state(
        &[
            "doctor",
            "example",
            "--config",
            path,
            "--profile",
            "missing",
        ],
        None,
        Some(&state),
    ));
    for args in [
        vec!["doctor", "example", "--config", path, "--port", "0"],
        vec![
            "doctor",
            "example",
            "--config",
            path,
            "--local-forward",
            "invalid",
        ],
        vec![
            "doctor",
            "example",
            "--config",
            path,
            "--remote-forward",
            "invalid",
        ],
        vec![
            "doctor",
            "example",
            "--config",
            path,
            "--dynamic-forward",
            "localhost:0",
        ],
        vec!["doctor", "example", "--config", path, "--jump", "-injected"],
    ] {
        validation(invoke_in_state(&args, None, Some(&state)));
    }
    std::fs::remove_file(&config).unwrap();
    validation(invoke_in_state(
        &["doctor", "example", "--config", path],
        None,
        Some(&state),
    ));
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn session_names_require_unambiguous_host() {
    let state = std::env::temp_dir().join(format!("shell-cli-ambiguity-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&state).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let first = metadata(&state, "one", "debug", port, &id);
    metadata(&state, "two", "debug", port, &id);
    for args in [vec!["read", "debug"], vec!["close", "debug"]] {
        let output = invoke_in_state(&args, None, Some(&state));
        assert_eq!(output.status.code(), Some(5), "{:?}", output.stdout);
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["error"]["code"], "conflict");
        assert_eq!(reply["delivery"], "not_sent");
    }
    assert!(first.exists());
    let closed = invoke_in_state(&["close", "debug", "--host", "one"], None, Some(&state));
    assert_eq!(closed.status.code(), Some(0), "{:?}", closed.stdout);
    assert!(!first.exists());
    let missing = invoke_in_state(&["status", "missing"], None, Some(&state));
    assert_eq!(missing.status.code(), Some(4), "{:?}", missing.stdout);
    // Both syntaxes of keys could identify real metadata: refuse to guess.
    metadata(&state, "debug", "Enter", port, &id);
    let ambiguous = invoke_in_state(&["keys", "debug", "Enter", "Ctrl-C"], None, Some(&state));
    validation(ambiguous);
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn list_without_host_includes_hosts_and_preserves_filtering() {
    use std::io::{BufRead, BufReader};
    let state = std::env::temp_dir().join(format!("shell-cli-list-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&state).unwrap();
    let mut owners = Vec::new();
    for (host, name, requests) in [("one", "alpha", 3), ("two", "beta", 1)] {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        metadata(
            &state,
            host,
            name,
            listener.local_addr().unwrap().port(),
            &uuid::Uuid::new_v4().to_string(),
        );
        owners.push(std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["action"]["Status"]["host"], host);
                // Legacy owner reply intentionally omits newer optional fields.
                let reply = serde_json::json!({"version": 1, "action": "status", "host": host, "session": name, "delivery": "acknowledged", "timestamp_ms": 0, "truncated": false});
                writeln!(stream, "{reply}").unwrap();
            }
        }));
    }
    let output = invoke_in_state(&["list"], None, Some(&state));
    assert!(output.status.success(), "{:?}", output.stdout);
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(reply["sessions"][0]["host"], "one");
    assert_eq!(reply["sessions"][1]["host"], "two");
    let output = invoke_in_state(&["list", "one"], None, Some(&state));
    assert!(output.status.success());
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["sessions"].as_array().unwrap().len(), 1);
    let output = invoke_with_format(&["list", "one"], None, Some(&state), false);
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("| alpha | one |"));
    for owner in owners {
        owner.join().unwrap();
    }
    std::fs::remove_dir_all(state).unwrap();
}
