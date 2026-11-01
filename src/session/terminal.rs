//! PTY startup, input delivery, output parsing and screen capture.
use super::*;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::sync::{Arc, Mutex};

pub(super) struct TerminalReplies(pub(super) mpsc::SyncSender<Input>);
impl vt100::Callbacks for TerminalReplies {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let response = match (i1, c, p) {
            (None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                format!("\x1b[{};{}R", row + 1, col + 1)
            }
            (None, 'n', 5) => "\x1b[0n".into(),
            (None, 'c', 0) => "\x1b[?1;2c".into(),
            _ => return,
        };
        let (tx, _) = mpsc::channel();
        let _ = self.0.try_send((response.into_bytes(), tx));
    }
}
pub(super) struct Terminal {
    pub(super) child: Box<dyn Child + Send + Sync>,
    pub(super) master: Box<dyn MasterPty + Send>,
    pub(super) parser: Arc<Mutex<TerminalScreen>>,
    read_baseline: Option<ReadBaseline>,
    input: mpsc::SyncSender<Input>,
    pub(super) dead_since: Option<Instant>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn pty_size(width: u16, height: u16) -> PtySize {
    PtySize {
        rows: height,
        cols: width,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl Terminal {
    pub(super) fn open(config: &Startup) -> Result<Self, Fault> {
        let pair = native_pty_system()
            .openpty(pty_size(config.width, config.height))
            .map_err(local)?;
        let mut reader = pair.master.try_clone_reader().map_err(local)?;
        let mut writer = pair.master.take_writer().map_err(local)?;
        let marker = format!("SHELL_CLI_READY_{}", Uuid::new_v4().simple());
        let bootstrap = format!(
            "stty cols {} rows {} && printf '%s\\n' {} && exec \"${{SHELL:-/bin/sh}}\" -l",
            config.width,
            config.height,
            quote(&marker)
        );
        let mut cmd = CommandBuilder::new(&config.ssh);
        cmd.args([
            "-tt",
            "-e",
            "none",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]);
        cmd.args(config.connection.args());
        cmd.args(["--", &config.host, &bootstrap]);
        cmd.env("TERM", "xterm-256color");
        let (input, rx) = mpsc::sync_channel::<Input>(32);
        thread::spawn(move || {
            while let Ok((bytes, ack)) = rx.recv() {
                let result = writer
                    .write_all(&bytes)
                    .and_then(|_| writer.flush())
                    .map_err(|e| e.to_string());
                let _ = ack.send(result);
            }
        });
        let parser = Arc::new(Mutex::new(TerminalScreen {
            parser: vt100::Parser::new_with_callbacks(
                config.height,
                config.width,
                HISTORY,
                TerminalReplies(input.clone()),
            ),
            audit: AppendAudit::default(),
            activity: OutputActivity::default(),
        }));
        let screen = parser.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut pending = Vec::new();
            let mut ready = false;
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                screen.lock().unwrap().process(&buf[..n]);
                if !ready {
                    pending.extend_from_slice(&buf[..n]);
                    if pending
                        .windows(marker.len())
                        .any(|s| s == marker.as_bytes())
                    {
                        ready = true;
                        let _ = ready_tx.send(());
                    } else if pending.len() > marker.len() {
                        pending.drain(..pending.len() - marker.len());
                    }
                }
            }
        });
        // ConPTY requests a cursor report during startup. Drain and answer output
        // before spawning the child, otherwise startup can wait indefinitely.
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| fault("transport", format!("cannot start SSH: {e}"), 3))?;
        drop(pair.slave);
        let mut terminal = Self {
            child,
            master: pair.master,
            parser,
            read_baseline: None,
            input,
            dead_since: None,
        };
        let started = Instant::now();
        loop {
            if ready_rx.try_recv().is_ok() {
                return Ok(terminal);
            }
            if terminal.dead()? {
                return Err(fault(
                    "transport",
                    format!(
                        "SSH startup failed: {}",
                        terminal.parser.lock().unwrap().screen().contents()
                    ),
                    3,
                ));
            }
            if started.elapsed() >= Duration::from_secs(config.timeout) {
                return Err(fault(
                    "timeout",
                    format!(
                        "SSH did not open a terminal before the deadline: {}",
                        terminal.parser.lock().unwrap().screen().contents()
                    ),
                    6,
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub(super) fn dead(&mut self) -> Result<bool, Fault> {
        if self.dead_since.is_none() && self.child.try_wait().map_err(local)?.is_some() {
            self.dead_since = Some(Instant::now());
        }
        Ok(self.dead_since.is_some())
    }

    pub(super) fn send(&mut self, bytes: Vec<u8>, timeout: u64) -> Result<(), Fault> {
        if self.dead()? {
            return Err(fault(
                "session_closed",
                "SSH disconnected; open a new session explicitly",
                3,
            ));
        }
        let (ack, rx) = mpsc::channel();
        self.input
            .try_send((bytes, ack))
            .map_err(|_| fault("conflict", "terminal input is busy", 5))?;
        match rx.recv_timeout(Duration::from_secs(timeout)) {
            Ok(Ok(())) => Ok(()),
            result => Err(Fault {
                code: "transport",
                exit: 3,
                unknown: true,
                message: format!("terminal input outcome is unknown: {result:?}"),
            }),
        }
    }

    pub(super) fn snapshot(
        &mut self,
        history: u16,
        read_mode: Option<ScreenMode>,
        reply: &mut Reply,
    ) -> Result<(), Fault> {
        reply.pane_dead = Some(self.dead()?);
        reply.timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let (row, col) = screen.cursor_position();
        reply.size = Some(Size {
            width: cols,
            height: rows,
        });
        reply.cursor = Some(Cursor { x: col, y: row });
        // ConPTY may flatten application buffer switches into repaint commands.
        // The visible grid remains valid, but its buffer flag is not authoritative.
        #[cfg(windows)]
        {
            reply.alternate_screen = None;
        }
        #[cfg(not(windows))]
        {
            reply.alternate_screen = Some(screen.alternate_screen());
        }
        reply.screen = Some(grid(screen));
        reply.screen_mode = Some("full".into());
        reply.row_wrapped = Some((0..rows).map(|row| screen.row_wrapped(row)).collect());
        if history > 0 {
            let mut past = screen.clone();
            past.set_scrollback(HISTORY);
            let available = past.scrollback();
            reply.truncated = available > history as usize || available == HISTORY;
            let wanted = available.min(history as usize);
            let mut lines = Vec::new();
            let mut offset = wanted;
            while offset > 0 {
                past.set_scrollback(offset);
                let count = offset.min(rows as usize);
                lines.extend(grid(&past).split('\n').take(count).map(str::to_owned));
                offset -= count;
            }
            reply.history = Some(lines.join("\n"));
        }
        if let Some(mode) = read_mode {
            let next = ReadBaseline::capture(screen);
            if matches!(mode, ScreenMode::Delta) && history == 0 {
                if let Some((screen, wrapped)) = self
                    .read_baseline
                    .as_ref()
                    .and_then(|baseline| baseline.delta(&next, parser.audit.safe()))
                {
                    reply.screen = Some(screen);
                    reply.row_wrapped = Some(wrapped);
                    reply.screen_mode = Some("delta".into());
                }
            }
            self.read_baseline = Some(next);
            // Preserve a partial escape/CR across read boundaries. The next
            // read must fall back until its complete effect is known.
            parser.audit.unsafe_output = parser.audit.escape != 0 || parser.audit.carriage_return;
        }
        Ok(())
    }
}
