use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

fn write_profile(path: &Path, host: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        serde_json::to_vec(&json!({"profiles": {"lab": {"host": host}}})).unwrap(),
    )
    .unwrap();
}

fn invoke(cwd: &Path, home: &Path, xdg: Option<&Path>, args: &[&str]) -> (i32, Value) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shell"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_CONFIG_HOME")
        .args([
            "--format",
            "json",
            "--ssh",
            "shell-cli-no-ssh-for-config-discovery-tests",
        ])
        .args(args);
    if let Some(xdg) = xdg {
        command.env("XDG_CONFIG_HOME", xdg);
    }
    let output = command.output().unwrap();
    (
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

#[test]
fn config_discovery_prioritizes_explicit_then_current_directory_then_user() {
    let root = std::env::temp_dir().join(format!("shell-cli-config-{}", uuid::Uuid::new_v4()));
    let cwd = root.join("project");
    let home = root.join("home");
    let xdg = root.join("xdg");
    let local = cwd.join(".config/shell-cli/config.json");
    let user = home.join(".config/shell-cli/config.json");
    let explicit = cwd.join("explicit.json");
    std::fs::create_dir_all(&cwd).unwrap();
    let args = ["doctor", "--profile", "lab"];

    let assert_host = |xdg: Option<&Path>, args: &[&str], host: &str| {
        let (code, reply) = invoke(&cwd, &home, xdg, args);
        // A missing SSH executable proves profile resolution succeeded without connecting.
        assert_eq!(code, 3, "{reply}");
        assert_eq!(reply["error"]["code"], "transport");
        assert_eq!(reply["host"], host);
    };

    write_profile(&user, "user@home");
    assert_host(None, &args, "user@home");
    write_profile(&xdg.join("shell-cli/config.json"), "user@xdg");
    assert_host(Some(&xdg), &args, "user@xdg");
    write_profile(&local, "user@local");
    assert_host(None, &args, "user@local");
    assert_host(Some(&xdg), &args, "user@local");

    write_profile(&explicit, "user@explicit");
    let explicit_args = ["doctor", "--profile", "lab", "--config", "explicit.json"];
    assert_host(Some(&xdg), &explicit_args, "user@explicit");
    std::fs::remove_file(&explicit).unwrap();
    let (code, reply) = invoke(&cwd, &home, None, &explicit_args);
    assert_eq!(code, 2, "{reply}");
    assert!(reply["error"]["message"]
        .as_str()
        .unwrap()
        .contains("cannot open connection config"));

    // Invalid or empty local configurations must not silently use a user profile.
    for content in ["not JSON", r#"{"profiles":{"lab":{"port":0}}}"#, "{}"] {
        std::fs::write(&local, content).unwrap();
        let (code, reply) = invoke(&cwd, &home, None, &args);
        assert_eq!(code, 2, "{reply}");
        assert_eq!(reply["error"]["code"], "validation");
    }
    assert_host(None, &["doctor", "user@direct"], "user@direct");

    std::fs::remove_file(&local).unwrap();
    std::fs::create_dir(&local).unwrap();
    let (code, reply) = invoke(&cwd, &home, None, &args);
    assert_eq!(code, 2, "{reply}");
    assert_eq!(reply["error"]["code"], "validation");
    std::fs::remove_dir(&local).unwrap();
    assert_host(None, &args, "user@home");
    std::fs::remove_file(&user).unwrap();
    assert_host(None, &["doctor", "user@direct"], "user@direct");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn local_diagnostics_report_source_and_effective_overrides_without_transport() {
    let root = std::env::temp_dir().join(format!("shell-cli-diagnostic-{}", uuid::Uuid::new_v4()));
    let cwd = root.join("project");
    let home = root.join("home");
    let path = cwd.join(".config/shell-cli/config.json");
    write_profile(&path, "user@profile");
    let (code, reply) = invoke(&cwd, &home, None, &["profiles", "list"]);
    assert_eq!(code, 0, "{reply}");
    assert_eq!(reply["configuration"]["profiles"][0]["name"], "lab");
    assert_eq!(
        reply["configuration"]["profiles"][0]["host"],
        "user@profile"
    );
    assert_eq!(
        reply["configuration"]["config_path"],
        path.to_str().unwrap()
    );
    let (code, reply) = invoke(
        &cwd,
        &home,
        None,
        &[
            "config",
            "show",
            "user@override",
            "--profile",
            "lab",
            "--port",
            "2222",
        ],
    );
    assert_eq!(code, 0, "{reply}");
    let report = &reply["configuration"];
    assert_eq!(report["effective"]["host"], "user@override");
    assert_eq!(report["effective"]["port"], 2222);
    assert_eq!(report["cli_overrides"]["host"], "user@override");
    assert_eq!(report["profile"], "lab");
    let (code, reply) = invoke(&cwd, &home, None, &["config", "show", "invalid;host"]);
    assert_eq!(code, 2);
    assert_eq!(reply["error"]["code"], "validation");
    let (code, _) = invoke(&cwd, &home, None, &["config", "show"]);
    assert_eq!(code, 0);
    std::fs::remove_dir_all(root).unwrap();
}
