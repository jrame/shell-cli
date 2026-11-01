//! Independent visible display process and Windows console rendering.
use super::*;

/// Launch an independent display process; it never owns the SSH child.
pub(crate) fn show(cli: &Cli, id: &str) -> Result<(), Fault> {
    let (_, host, name) = context(&cli.command);
    #[cfg(windows)]
    {
        background::Background::visible(
            &std::env::current_exe().map_err(local)?,
            &format!("__view-session {} {} {}", host, name.unwrap(), id),
        )
        .map_err(local)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (host, name, id);
        Err(local(
            "visible windows are currently supported on Windows only",
        ))
    }
}

pub(crate) fn view() {
    #[cfg(windows)]
    if let Err(e) = view_windows() {
        eprintln!("shell display: {}", e.message);
    }
}

#[cfg(windows)]
pub(crate) fn view_windows() -> Result<(), Fault> {
    use windows_sys::Win32::System::Console::*;
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        return Err(local("invalid display arguments"));
    }
    let (host, name, id) = (&args[2], &args[3], &args[4]);
    validate(host, Some(name))?;
    let entry = load(&entry_path(&state_dir()?, host, name))?;
    if &entry.id != id {
        return Err(local("session identity changed"));
    }
    // This process has its own console and owns no SSH/PTY handles. Closing it
    // cannot terminate the owner. Raw VT input preserves control keys and Unicode.
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let mut input_mode = 0;
    let mut output_mode = 0;
    if unsafe { GetConsoleMode(input, &mut input_mode) } == 0
        || unsafe { GetConsoleMode(output, &mut output_mode) } == 0
    {
        return Err(local("display requires a Windows console"));
    }
    if unsafe { SetConsoleMode(input, ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS) } == 0
        || unsafe { SetConsoleMode(output, output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) } == 0
    {
        return Err(local(std::io::Error::last_os_error()));
    }
    struct Restore(windows_sys::Win32::Foundation::HANDLE, u32);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(self.0, self.1);
            }
        }
    }
    unsafe {
        SetConsoleCP(65001);
        SetConsoleOutputCP(65001);
    }
    let _restore_input = Restore(input, input_mode);
    let _restore_output = Restore(output, output_mode);
    let (tx, rx) = mpsc::sync_channel(32);
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0; 1024];
        while let Ok(n) = stdin.read(&mut buffer) {
            if n == 0 || tx.send(buffer[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut stdout = std::io::stdout().lock();
    let mut previous = String::new();
    loop {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(output, &mut info) } == 0 {
            break;
        }
        let width = (info.srWindow.Right - info.srWindow.Left + 1).clamp(20, 500) as u16;
        let height = (info.srWindow.Bottom - info.srWindow.Top + 1).clamp(5, 200) as u16;
        let bytes = match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(bytes) => bytes,
            Err(mpsc::RecvTimeoutError::Timeout) => Vec::new(),
            Err(_) => break,
        };
        // Ctrl-] detaches without sending it to the remote program.
        if bytes.contains(&0x1d) {
            break;
        }
        let reply = request(
            &entry,
            Action::View {
                host: host.clone(),
                session: name.clone(),
                id: id.clone(),
                bytes,
                width,
                height,
            },
            2,
        )?;
        if let Some(error) = reply.error {
            return Err(local(error.message));
        }
        if let Some(screen) = reply.screen {
            if screen != previous {
                stdout.write_all(screen.as_bytes()).map_err(local)?;
                stdout.flush().map_err(local)?;
                previous = screen;
            }
        }
        if reply.pane_dead == Some(true) {
            break;
        }
    }
    Ok(())
}
