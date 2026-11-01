use super::*;

#[test]
fn stale_cleanup_preserves_a_replacement_generation() {
    let dir = std::env::temp_dir().join(format!("shell-cli-cleanup-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let path = entry_path(&dir, "lab", "debug");
    let entry = Entry {
        host: "lab".into(),
        session: "debug".into(),
        id: Uuid::new_v4().to_string(),
        token: "test".into(),
        port: 1,
        owner_lock: true,
        transfer: None,
    };
    let bytes = serde_json::to_vec(&entry).unwrap();
    fs::write(&path, &bytes).unwrap();
    remove_stopped_record(&path, &Uuid::new_v4().to_string()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes);
    remove_stopped_record(&path, &entry.id).unwrap();
    assert!(!path.exists());
    remove_stopped_record(&path, &entry.id).unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn screen_wait_requires_reaction_then_quiet_and_is_bounded() {
    let start = Instant::now();
    let deadline = start + Duration::from_secs(1);
    let mut activity = OutputActivity::default();
    assert_eq!(activity.outcome(0, deadline, start + SCREEN_QUIET), None);
    assert_eq!(activity.outcome(0, deadline, deadline), Some("timeout"));

    activity.record(start + Duration::from_millis(100));
    assert_eq!(
        activity.outcome(0, deadline, start + Duration::from_millis(250)),
        None
    );
    assert_eq!(
        activity.outcome(0, deadline, start + Duration::from_millis(300)),
        Some("quiet")
    );
    // Output already present before delivery cannot count as a reaction.
    assert_eq!(
        activity.outcome(1, deadline, start + Duration::from_millis(500)),
        None
    );

    for millis in [200, 350, 500, 650, 800, 950] {
        activity.record(start + Duration::from_millis(millis));
        assert_eq!(
            activity.outcome(0, deadline, start + Duration::from_millis(millis)),
            None
        );
    }
    assert_eq!(activity.outcome(0, deadline, deadline), Some("timeout"));
    // Even a late poll cannot call quiet reached after the deadline success.
    assert_eq!(
        activity.outcome(0, deadline, deadline + SCREEN_QUIET),
        Some("timeout")
    );
}

#[test]
fn sync_transport_allows_owner_to_return_wait_timeout_and_screen() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let entry = Entry {
        host: "lab".into(),
        session: "debug".into(),
        id: Uuid::new_v4().to_string(),
        token: "test".into(),
        port: listener.local_addr().unwrap().port(),
        owner_lock: false,
        transfer: None,
    };
    let action = Action::Send {
        host: entry.host.clone(),
        session: entry.session.clone(),
        id: Some(entry.id.clone()),
        async_mode: false,
        screen: ScreenMode::Delta,
        text: Some("work".into()),
        stdin: false,
        file: None,
        enter: true,
    };
    let owner = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let req: Request = read_frame(&stream).unwrap();
        assert_eq!(req.timeout, 1);
        thread::sleep(Duration::from_millis(1100));
        let mut reply = Reply::new("send", "lab", Some("debug"));
        reply.delivery = "acknowledged".into();
        reply.screen = Some("partial output".into());
        reply.screen_mode = Some("delta".into());
        reply.wait_status = Some("timeout".into());
        apply_fault(&mut reply, fault("timeout", "screen wait deadline", 6));
        write_frame(&mut stream, &reply).unwrap();
    });
    let reply = request(&entry, action, 1).unwrap();
    owner.join().unwrap();
    assert_eq!(reply.delivery, "acknowledged");
    assert_eq!(reply.wait_status.as_deref(), Some("timeout"));
    assert_eq!(reply.screen.as_deref(), Some("partial output"));
    assert_eq!(reply.error.unwrap().code, "timeout");
}

fn delta_after(parser: &mut vt100::Parser, bytes: &[u8]) -> Option<(String, Vec<bool>)> {
    let baseline = ReadBaseline::capture(parser.screen());
    let mut audit = AppendAudit::default();
    audit.process(bytes);
    parser.process(bytes);
    baseline.delta(&ReadBaseline::capture(parser.screen()), audit.safe())
}

#[test]
fn delta_returns_only_appended_cells_and_sgr_does_not_force_full() {
    let mut parser = vt100::Parser::new(5, 20, HISTORY);
    parser.process(b"prompt$ ");
    let (text, wrapped) = delta_after(&mut parser, b"\x1b[32mhello\x1b[0m").unwrap();
    assert_eq!(text, "hello");
    assert_eq!(wrapped, vec![false]);
    assert_eq!(
        delta_after(&mut parser, b"").unwrap(),
        (String::new(), vec![])
    );
}

#[test]
#[ignore = "requires SHELL_CLI_TEST_HOST with configured SSH access"]
fn live_shell_commands_return_deltas() {
    let host = std::env::var("SHELL_CLI_TEST_HOST").unwrap();
    let config = Startup {
        ssh: std::env::var_os("SHELL_CLI_TEST_SSH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("ssh")),
        connection: config::Connection::default(),
        host,
        session: "delta-test".into(),
        id: Uuid::new_v4().to_string(),
        width: 80,
        height: 24,
        timeout: 15,
        path: PathBuf::new(),
    };
    let mut terminal = Terminal::open(&config).unwrap();
    thread::sleep(Duration::from_millis(500));
    let mut reply = Reply::new("read", &config.host, Some(&config.session));
    terminal
        .snapshot(0, Some(ScreenMode::Delta), &mut reply)
        .unwrap();
    assert_eq!(reply.screen_mode.as_deref(), Some("full"));
    for command in [
        "printf \"quiet_ok\\n\"\r",
        "cd /tmp\r",
        "cd /tmp\r",
        "printf 'delta_ok\\n'\r",
    ]
    .into_iter()
    .cycle()
    .take(12)
    {
        terminal.send(command.as_bytes().to_vec(), 15).unwrap();
        thread::sleep(Duration::from_millis(300));
        terminal
            .snapshot(0, Some(ScreenMode::Delta), &mut reply)
            .unwrap();
        assert_eq!(
            reply.screen_mode.as_deref(),
            Some("delta"),
            "{:?}",
            reply.screen
        );
        assert!(!reply.screen.as_ref().unwrap().contains("SHELL_CLI_READY_"));
    }
}

#[test]
fn delta_reports_physical_wrapping_separately_from_line_breaks() {
    let mut parser = vt100::Parser::new(5, 8, HISTORY);
    parser.process(b"123");
    let (text, wrapped) = delta_after(&mut parser, b"456789abc").unwrap();
    assert_eq!(text, "45678\n9abc");
    assert_eq!(wrapped, vec![true, false]);
    let (text, wrapped) = delta_after(&mut parser, b"\r\nnext").unwrap();
    assert_eq!(text, "    \nnext");
    assert_eq!(wrapped, vec![false, false]);
}

#[test]
fn delta_accepts_bash_prompt_controls_and_conpty_line_redraws() {
    let mut parser = vt100::Parser::new(6, 80, HISTORY);
    parser.process(b"old output\r\nprompt$ ");
    let bytes = b"\x1b[?25l\x1b[?2004lcd /tmp\r\n\x1b[?2004h\x1b[32mtmp$ \x1b[m\x1b]0;host: /tmp\x07\x1b[?25h";
    let (text, _) = delta_after(&mut parser, bytes).unwrap();
    let lines: Vec<_> = text.lines().map(str::trim_end).collect();
    assert_eq!(lines, ["cd /tmp", "tmp$"]);
    // Recorded ConPTY pattern: partially echoed input, then a CR redraw
    // of the unchanged prompt, and ECH to clear padding after output.
    let (text, _) = delta_after(
        &mut parser,
        b"printf '\x1b[?2004l\rtmp$ printf 'ok\\n'\r\nok\x1b[38X\r\n\x1b[?2004htmp$\x1b[K\x1b[1C",
    )
    .unwrap();
    let lines: Vec<_> = text.lines().map(str::trim_end).collect();
    assert_eq!(lines, ["printf 'ok\\n'", "ok", "tmp$"]);
    let (text, _) = delta_after(
        &mut parser,
        b"\x1b[25l\rtmp$ cd /tmp\r\ntmp$        \x1b[6;6H\x1b[?25h",
    )
    .unwrap();
    let lines: Vec<_> = text.lines().map(str::trim_end).collect();
    assert_eq!(lines, ["cd /tmp", "tmp$"]);
}

#[test]
fn delta_rejects_destructive_redraws_and_text_after_final_cursor() {
    for bytes in [
        b"\rOTHER$ command\r\nprompt$ ".as_slice(),
        b"\r\x1b[7X\r\nprompt$ ",
        b"\r\x1b[K\r\nprompt$ ",
        b"\x1b[1;1HOTHER$ command\r\nprompt$ ",
        b"new text\rprompt$ ",
        b"\x1b[2J\x1b[Hprompt$ command\r\nprompt$ ",
    ] {
        let mut parser = vt100::Parser::new(5, 40, HISTORY);
        parser.process(b"prompt$ ");
        assert!(delta_after(&mut parser, bytes).is_none(), "{bytes:?}");
    }
}

#[test]
fn append_audit_allows_only_complete_known_prompt_controls() {
    for bytes in [
        b"\x1b]0;title\x07".as_slice(),
        b"\x1b]1;icon\x1b\\",
        b"\x1b]2;title\x1b\\",
        b"\x1b[?2004h\x1b[?2004l\x1b[?25l\x1b[?25h",
    ] {
        // Every possible transport split must preserve the control state.
        for split in 1..bytes.len() {
            let mut audit = AppendAudit::default();
            audit.process(&bytes[..split]);
            audit.process(&bytes[split..]);
            assert!(audit.safe(), "{bytes:?} split at {split}");
        }
    }
    for bytes in [
        b"\x1b]0;unfinished".as_slice(),
        b"\x1b]2;unfinished\x1b",
        b"\x1b]52;selection\x07",
        b"\x1b[?1049h\x1b[?1049l",
        b"\x1b[2J",
        b"\x1b[?2004",
        b"\x1b[?25;1049h",
    ] {
        let mut audit = AppendAudit::default();
        audit.process(bytes);
        assert!(!audit.safe(), "{bytes:?}");
    }
}

#[test]
fn delta_keeps_repeated_scrolled_lines_without_suffix_guessing() {
    let mut parser = vt100::Parser::new(3, 8, HISTORY);
    parser.process(b"same\r\nsame\r\nsame\r\n");
    let (text, wrapped) = delta_after(&mut parser, b"same\r\nsame\r\n").unwrap();
    assert_eq!(text, "same    \nsame    \n");
    assert_eq!(wrapped, vec![false, false, false]);
}

#[test]
fn delta_falls_back_for_rewrites_editor_output_resize_and_history_overflow() {
    let mut parser = vt100::Parser::new(3, 8, HISTORY);
    parser.process(b"hello");
    assert!(delta_after(&mut parser, b"\rHELLO").is_none());
    assert!(delta_after(&mut parser, b"\x1b[?1049h\x1b[Heditor").is_none());
    parser.process(b"\x1b[?1049l");
    let baseline = ReadBaseline::capture(parser.screen());
    parser.screen_mut().set_size(4, 12);
    assert!(baseline
        .delta(&ReadBaseline::capture(parser.screen()), true)
        .is_none());
    let repeated = b"same\r\n".repeat(HISTORY + 10);
    parser.process(&repeated);
    // Identical visible contents after overflowing history must never be
    // mistaken for no output, even when every line is the same.
    assert!(delta_after(&mut parser, b"same\r\n").is_none());
}

#[test]
fn append_audit_handles_control_sequences_split_across_reads() {
    let mut audit = AppendAudit::default();
    audit.process(b"hello\r");
    assert!(!audit.safe());
    audit.process(b"\n\x1b[");
    assert!(!audit.safe());
    audit.process(b"32mworld");
    assert!(audit.safe());
    audit.process(b"\x1b[2");
    audit.process(b"J");
    assert!(!audit.safe());
}

#[test]
fn open_lock_serializes_different_hosts_and_releases_after_drop() {
    let dir = std::env::temp_dir().join(format!("shell-cli-open-lock-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("open-global");
    let first = lock(&path).unwrap();
    let contender = path.clone();
    assert_eq!(
        thread::spawn(move || lock(&contender).err().unwrap().code)
            .join()
            .unwrap(),
        "conflict"
    );
    drop(first);
    drop(lock(&path).unwrap());
    fs::remove_file(path.with_extension("lock")).unwrap();
    fs::remove_dir(dir).unwrap();
}

#[test]
fn cli_argument_scanner_skips_connection_and_screen_option_values() {
    let args = cli_args(
        [
            "shell",
            "--profile",
            "lab",
            "-p",
            "2222",
            "read",
            "old-host",
            "debug",
            "--screen",
            "delta",
            "-L8080:localhost:80",
            "--config=config.json",
        ]
        .map(std::ffi::OsString::from)
        .to_vec(),
    )
    .unwrap();
    let cli = Cli::try_parse_from(args).unwrap();
    assert!(
        matches!(cli.command, Action::Read { host, session, screen: ScreenMode::Delta, .. } if host == "old-host" && session == "debug")
    );
}

#[test]
fn alternate_screen_unicode_and_cursor() {
    let mut parser = vt100::Parser::new(10, 40, HISTORY);
    parser.process(b"shell\x1b[?1049h\x1b[H");
    parser.process("édition 雪".as_bytes());
    assert!(parser.screen().alternate_screen());
    assert!(grid(parser.screen()).starts_with("édition 雪"));
    assert_eq!(parser.screen().cursor_position(), (0, 10));
    parser.process(b"\x1b[?1049l");
    assert!(!parser.screen().alternate_screen());
    assert!(grid(parser.screen()).starts_with("shell"));
}
#[test]
fn formatted_view_preserves_grid_colors_and_cursor() {
    let mut source = vt100::Parser::new(10, 40, 0);
    source.process(b"\x1b[31mred\x1b[0m\x1b[4;8Hcursor");
    let mut viewer = vt100::Parser::new(10, 40, 0);
    viewer.process(&source.screen().state_formatted());
    assert_eq!(grid(viewer.screen()), grid(source.screen()));
    assert_eq!(
        viewer.screen().cursor_position(),
        source.screen().cursor_position()
    );
    assert_eq!(
        viewer.screen().cell(0, 0).unwrap().fgcolor(),
        source.screen().cell(0, 0).unwrap().fgcolor()
    );
}

#[test]
fn viewer_protocol_roundtrip() {
    let req = br#"{"token":"test","timeout":2,"action":{"View":{"host":"user@example","session":"debug","id":"742e3019-1f56-43ef-913e-876231b1f556","bytes":[101,13],"width":90,"height":28}}}
"#;
    let parsed = read_frame::<Request>(&req[..]).unwrap();
    assert!(matches!(parsed.action, Action::View { width: 90, .. }));
}
#[test]
fn protocol_rejects_incomplete_frames() {
    assert!(read_frame::<Reply>(&b"{}"[..]).is_err());
}

#[test]
fn close_without_id_keeps_delivery_unknown_when_reply_is_lost() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let entry = Entry {
        host: "example".into(),
        session: "debug".into(),
        id: Uuid::new_v4().to_string(),
        token: "test".into(),
        port: listener.local_addr().unwrap().port(),
        owner_lock: false,
        transfer: None,
    };
    let owner = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let req = read_frame::<Request>(&stream).unwrap();
        assert!(matches!(req.action, Action::Close { id: None, .. }));
        // The owner received close, but drops the connection before replying.
    });
    let error = request(
        &entry,
        Action::Close {
            host: entry.host.clone(),
            session: entry.session.clone(),
            id: None,
        },
        2,
    )
    .err()
    .unwrap();
    owner.join().unwrap();
    assert!(error.unknown);
    let mut reply = Reply::new("close", &entry.host, Some(&entry.session));
    apply_fault(&mut reply, error);
    assert_eq!(reply.delivery, "unknown");
}

#[test]
fn cursor_reports_unblock_console_startup() {
    let (tx, rx) = mpsc::sync_channel(4);
    let mut parser = vt100::Parser::new_with_callbacks(10, 40, 0, TerminalReplies(tx));
    parser.process(b"\x1b[3;7H\x1b[6");
    assert!(rx.try_recv().is_err());
    parser.process(b"n\x1b[5n");
    assert_eq!(rx.recv().unwrap().0, b"\x1b[3;7R");
    assert_eq!(rx.recv().unwrap().0, b"\x1b[0n");
}
