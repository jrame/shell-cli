//! Human-facing CLI output. The local owner continues to use JSON frames.
use super::Reply;
use clap::{Args, ValueEnum};
use serde_json::Value;
use std::ffi::OsString;
use std::io::{self, Write};

#[derive(Clone, Copy, Default, ValueEnum)]
enum Format {
    #[default]
    Markdown,
    Json,
}

#[derive(Args, Default)]
pub(super) struct Options {
    /// Include every response field in metadata (also enabled on errors).
    #[arg(long, global = true)]
    detail: bool,
    /// Output format. JSON is available for scripts.
    #[arg(long, global = true, value_enum, default_value = "markdown")]
    format: Format,
}

impl Options {
    // Recover output preferences even when clap rejects the command. Skip
    // option values so command text or file paths cannot select output flags.
    pub(super) fn from_raw_args(args: &[OsString]) -> Self {
        let mut options = Self::default();
        let mut args = args.iter().skip(1);
        while let Some(arg) = args.next() {
            let arg = arg.to_string_lossy();
            match arg.as_ref() {
                "--" => break,
                "--detail" => options.detail = true,
                "--format" => {
                    if let Some(value) = args.next() {
                        options.format = if value == "json" {
                            Format::Json
                        } else {
                            Format::Markdown
                        };
                    }
                }
                "--format=json" => options.format = Format::Json,
                "--format=markdown" => options.format = Format::Markdown,
                "--ssh" | "--timeout" | "--host" | "--text" | "--file" | "--width" | "--height"
                | "--history" | "--bytes" | "--screen" | "--config" | "--profile"
                | "--identity" | "-i" | "--port" | "-p" | "--jump" | "-J" | "--local-forward"
                | "-L" | "--remote-forward" | "-R" | "--dynamic-forward" | "-D"
                | "--ssh-config" | "-F" => {
                    args.next();
                }
                _ => {}
            }
        }
        options
    }

    pub(super) fn write(&self, out: &mut impl Write, reply: &Reply) -> io::Result<()> {
        if matches!(self.format, Format::Json) {
            serde_json::to_writer(&mut *out, reply)?;
            return writeln!(out);
        }
        if reply.completed.is_some() {
            writeln!(out, "###stdout")?;
            if let Some(stdout) = &reply.stdout {
                write_raw(out, stdout)?;
            }
            writeln!(out, "###stderr")?;
            if let Some(stderr) = &reply.stderr {
                write_raw(out, stderr)?;
            }
        } else {
            match &reply.screen_mode {
                Some(mode) => writeln!(out, "###screen {mode}")?,
                None => writeln!(out, "###screen")?,
            }
            // Requested scrollback precedes the visible screen, in terminal order.
            if let Some(history) = reply.history.as_deref().filter(|s| !s.is_empty()) {
                write_raw(out, history)?;
            }
            if let Some(screen) = &reply.screen {
                write_raw(out, screen)?;
            }
            if let Some(sessions) = &reply.sessions {
                if sessions.is_empty() {
                    writeln!(out, "No sessions.")?;
                } else {
                    writeln!(out, "| Session | Host | ID |\n| --- | --- | --- |")?;
                    for session in sessions {
                        writeln!(
                            out,
                            "| {} | {} | {} |",
                            session.name,
                            session.host.as_deref().unwrap_or(&reply.host),
                            session.id
                        )?;
                    }
                }
            }
        }
        writeln!(out, "###metadata")?;
        match &reply.cursor {
            Some(c) => writeln!(out, "cursor: x: {} y: {}", c.x, c.y)?,
            None => writeln!(out, "cursor: null")?,
        }
        match &reply.size {
            Some(s) => writeln!(out, "size: height: {} width: {}", s.height, s.width)?,
            None => writeln!(out, "size: null")?,
        }
        writeln!(out, "timestamp_ms: {}", reply.timestamp_ms)?;
        if let Some(mode) = &reply.screen_mode {
            writeln!(out, "screen_mode: {mode}")?;
        }
        if let Some(status) = &reply.wait_status {
            writeln!(out, "wait_status: {status}")?;
        }
        if let Some(transfer) = &reply.transfer {
            write_field(out, "transfer", &serde_json::to_value(transfer)?, 0)?;
        }
        if let Some(completed) = reply.completed {
            writeln!(out, "completed: {completed}")?;
            write_field(out, "exit_code", &serde_json::to_value(reply.exit_code)?, 0)?;
            writeln!(out, "truncated: {}", reply.truncated)?;
        }
        if self.detail || reply.error.is_some() {
            let value = serde_json::to_value(reply)?;
            for key in [
                "action",
                "alternate_screen",
                "delivery",
                "error",
                "history",
                "host",
                "id",
                "pane_dead",
                "row_wrapped",
                "screen",
                "session",
                "sessions",
                "truncated",
                "version",
            ] {
                if key == "truncated" && reply.completed.is_some() {
                    continue;
                }
                write_field(out, key, &value[key], 0)?;
            }
        }
        Ok(())
    }
}

fn write_raw(out: &mut impl Write, text: &str) -> io::Result<()> {
    write!(out, "{text}")?;
    if !text.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(())
}

fn write_field(out: &mut impl Write, key: &str, value: &Value, indent: usize) -> io::Result<()> {
    write!(
        out,
        "{:indent$}{key}{}",
        "",
        if key == "-" { "" } else { ":" }
    )?;
    match value {
        Value::Object(fields) => {
            writeln!(out)?;
            for (key, value) in fields {
                write_field(out, key, value, indent + 2)?;
            }
        }
        Value::Array(items) if !items.is_empty() => {
            writeln!(out)?;
            for item in items {
                write_field(out, "-", item, indent + 2)?;
            }
        }
        Value::String(s) if !s.is_empty() && !s.contains(['\r', '\n']) => writeln!(out, " {s}")?,
        _ => writeln!(out, " {value}")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cursor, Problem, Size};

    #[test]
    fn raw_markdown_and_terminal_spacing_are_preserved() {
        let mut reply = Reply::new("read", "lab", Some("debug"));
        reply.timestamp_ms = 1793172517943;
        reply.cursor = Some(Cursor { x: 24, y: 24 });
        reply.size = Some(Size {
            height: 24,
            width: 24,
        });
        reply.screen = Some("$ cat README.md\n# Mon projet\n```bash\necho hello  \n```".into());
        let mut out = Vec::new();
        Options::default().write(&mut out, &reply).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "###screen\n$ cat README.md\n# Mon projet\n```bash\necho hello  \n```\n###metadata\ncursor: x: 24 y: 24\nsize: height: 24 width: 24\ntimestamp_ms: 1793172517943\n");
    }

    #[test]
    fn details_are_automatic_for_errors_and_never_mix_with_raw_screen() {
        let mut reply = Reply::new("send", "lab", Some("debug"));
        reply.error = Some(Problem {
            code: "timeout".into(),
            message: "first\n###screen".into(),
        });
        reply.delivery = "unknown".into();
        let mut out = Vec::new();
        Options::default().write(&mut out, &reply).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("action: send\n"));
        assert!(text.contains("delivery: unknown\n"));
        assert!(text.contains("error:\n  code: timeout\n"));
        assert!(text.contains("message: \"first\\n###screen\""));
        assert!(text.contains("screen: null\n"));
    }

    #[test]
    fn new_option_values_cannot_change_output_preferences() {
        for option in [
            "--config",
            "--profile",
            "--identity",
            "-i",
            "--jump",
            "-J",
            "--screen",
            "--local-forward",
            "-L",
            "--ssh-config",
            "-F",
        ] {
            let options =
                Options::from_raw_args(&["shell".into(), option.into(), "--format=json".into()]);
            assert!(matches!(options.format, Format::Markdown));
        }
    }

    #[test]
    fn screen_mode_is_always_visible_and_wraps_are_in_details() {
        let mut reply = Reply::new("read", "lab", Some("debug"));
        reply.screen_mode = Some("delta".into());
        reply.wait_status = Some("quiet".into());
        reply.row_wrapped = Some(vec![false, true]);
        let mut out = Vec::new();
        Options::default().write(&mut out, &reply).unwrap();
        let text = std::str::from_utf8(&out).unwrap();
        assert!(text.starts_with("###screen delta\n"));
        assert!(text.contains("screen_mode: delta\n"));
        assert!(text.contains("wait_status: quiet\n"));
        assert!(!text.contains("row_wrapped:"));
        out.clear();
        Options {
            detail: true,
            ..Options::default()
        }
        .write(&mut out, &reply)
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("screen_mode:").count(), 1);
        assert_eq!(text.matches("wait_status:").count(), 1);
        assert!(text.contains("row_wrapped:\n  - false\n  - true\n"));
    }
}
