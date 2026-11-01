use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn mock() -> &'static PathBuf {
    static MOCK: OnceLock<PathBuf> = OnceLock::new();
    MOCK.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("shell-exec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(format!("exec-transport{}", std::env::consts::EXE_SUFFIX));
        assert!(Command::new("rustc")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/exec_transport.rs"
            ))
            .arg("-o")
            .arg(&exe)
            .status()
            .unwrap()
            .success());
        exe
    })
}

fn invoke(args: &[&str], input: Option<&str>, json: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shell"));
    command.args(["--ssh"]).arg(mock());
    if json {
        command.args(["--format", "json"]);
    }
    // If exec accidentally accesses session storage, this ordinary file makes it fail.
    command.env("SHELL_CLI_STATE_DIR", mock());
    let mut child = command
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
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn reply(args: &[&str], input: Option<&str>, exit: i32) -> Value {
    let output = invoke(args, input, true);
    assert_eq!(
        output.status.code(),
        Some(exit),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn captures_separate_unwrapped_streams_and_exact_whitespace() {
    let r = reply(&["exec", "lab", "--text", "outputs"], None, 0);
    assert_eq!(
        r["stdout"],
        format!("  stdout 雪\n\n{} ", "long_filename_".repeat(40))
    );
    assert_eq!(r["stderr"], "stderr without newline");
    assert_eq!(r["exit_code"], 0);
    assert_eq!(r["completed"], true);
    assert_eq!(r["delivery"], "acknowledged");
    assert!(r["screen"].is_null());
    assert_eq!(r["truncated"], false);
    let output = invoke(&["exec", "lab", "--text", "outputs"], None, false);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("###stdout\n  stdout 雪\n\n"));
    assert!(text.contains("###stderr\nstderr without newline\n###metadata\n"));
    assert!(text.contains("completed: true\nexit_code: 0\n"));
}

#[test]
fn silence_and_output_gaps_do_not_signal_completion() {
    for script in ["delayed", "silent"] {
        let start = Instant::now();
        let r = reply(&["exec", "lab", "--text", script], None, 0);
        assert!(start.elapsed() >= Duration::from_millis(600));
        assert!(r["stdout"].as_str().unwrap().ends_with("finished\n"));
        assert_eq!(r["completed"], true);
    }
}

#[test]
fn remote_failure_keeps_streams_and_transport_failure_stays_unknown() {
    let r = reply(&["exec", "lab", "--text", "nonzero"], None, 3);
    assert_eq!(r["stdout"], "partial stdout\n");
    assert_eq!(r["stderr"], "command failed\n");
    assert_eq!(r["exit_code"], 42);
    assert_eq!(r["completed"], true);
    assert_eq!(r["error"]["code"], "remote");
    let r = reply(&["exec", "lab", "--text", "transport"], None, 3);
    assert_eq!(r["stderr"], "connection lost\n");
    assert_eq!(r["completed"], false);
    assert!(r["exit_code"].is_null());
    assert_eq!(r["delivery"], "unknown");
    assert_eq!(r["error"]["code"], "transport");
    let output = Command::new(env!("CARGO_BIN_EXE_shell"))
        .args([
            "--format",
            "json",
            "--ssh",
            "shell-cli-missing-exec-transport",
            "exec",
            "lab",
            "--text",
            "outputs",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let r: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(r["completed"], false);
    assert_eq!(r["delivery"], "not_sent");
    assert_eq!(r["stdout"], "");
    assert_eq!(r["stderr"], "");
    assert!(r["exit_code"].is_null());
}

#[test]
fn timeout_retains_partial_output_and_does_not_wait_for_inherited_pipes() {
    for (script, stdout) in [
        ("timeout", "before timeout\n"),
        ("inherited", "parent output\n"),
    ] {
        let start = Instant::now();
        let r = reply(
            &["--timeout", "1", "exec", "lab", "--text", script],
            None,
            6,
        );
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(r["stdout"], stdout);
        if script == "timeout" {
            assert_eq!(r["stderr"], "diagnostic\n");
        }
        assert_eq!(r["completed"], false);
        assert!(r["exit_code"].is_null());
        assert_eq!(r["delivery"], "unknown");
        assert_eq!(r["error"]["code"], "timeout");
    }
}

#[test]
fn drains_both_streams_past_the_capture_limit() {
    let r = reply(&["exec", "lab", "--text", "large"], None, 0);
    assert_eq!(r["stdout"].as_str().unwrap().len(), 1_048_576);
    assert_eq!(r["stderr"].as_str().unwrap().len(), 1_048_576);
    assert_eq!(r["truncated"], true);
    assert_eq!(r["completed"], true);
}

#[test]
fn accepts_command_sources_and_profiles_and_validates_before_transport() {
    let script = "printf '%s' 'a b;$`雪'\nprintf done\n";
    let file = mock().with_extension("script");
    std::fs::write(&file, script).unwrap();
    for (args, input) in [
        (vec!["exec", "lab", "--text", script], None),
        (vec!["exec", "lab", "--stdin"], Some(script)),
        (vec!["exec", "lab", "--file", file.to_str().unwrap()], None),
    ] {
        assert_eq!(reply(&args, input, 0)["stdout"], script);
    }
    let config = mock().with_extension("json");
    std::fs::write(
        &config,
        r#"{"profiles":{"test":{"host":"user@lab","port":2222}}}"#,
    )
    .unwrap();
    let r = reply(
        &[
            "exec",
            "--profile",
            "test",
            "--config",
            config.to_str().unwrap(),
            "--text",
            "args",
        ],
        None,
        0,
    );
    assert_eq!(r["host"], "user@lab");
    assert!(r["stdout"].as_str().unwrap().contains("-p|2222"));
    assert!(r["stdout"].as_str().unwrap().ends_with("--|user@lab|args"));
    for args in [
        vec!["exec", "lab"],
        vec!["exec", "lab", "--text", "  "],
        vec!["exec", "bad;host", "--text", "outputs"],
        vec!["exec", "lab", "--text", "outputs", "--stdin"],
        vec!["exec", "lab", "--text", "outputs", "--file", "absent"],
    ] {
        let r = reply(&args, None, 2);
        assert_eq!(r["error"]["code"], "validation");
        assert_eq!(r["delivery"], "not_sent");
    }
    let r = reply(&["exec", "lab", "--stdin"], Some(&"x".repeat(16_385)), 2);
    assert_eq!(r["error"]["code"], "validation");
    std::fs::remove_file(file).unwrap();
    std::fs::remove_file(config).unwrap();
}
