//! Start an owner without inheriting the caller's redirected pipes.
use std::io;
use std::path::Path;

#[cfg(not(windows))]
pub struct Background(std::process::Child);

#[cfg(not(windows))]
impl Background {
    pub fn start(exe: &Path, config: &Path) -> io::Result<Self> {
        use std::process::{Command, Stdio};
        Command::new(exe)
            .arg("__serve-session")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(Self)
    }
    pub fn exited(&mut self) -> io::Result<bool> {
        self.0.try_wait().map(|s| s.is_some())
    }
    pub fn terminate(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
        CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
    };

    pub struct Background(HANDLE);
    impl Drop for Background {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    impl Background {
        pub fn start(exe: &Path, config: &Path) -> io::Result<Self> {
            Self::launch(
                exe,
                &format!("__serve-session \"{}\"", config.display()),
                false,
            )
        }
        pub fn visible(exe: &Path, args: &str) -> io::Result<Self> {
            Self::launch(exe, args, true)
        }
        fn launch(exe: &Path, args: &str, visible: bool) -> io::Result<Self> {
            // Both arguments are generated absolute filenames. Windows filenames
            // cannot contain quotes, and these paths do not end in a backslash.
            let mut command = Vec::new();
            command.push(b'"' as u16);
            command.extend(exe.as_os_str().encode_wide());
            command.extend("\" ".encode_utf16());
            command.extend(args.encode_utf16());
            command.push(0);
            let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            // Crucially, bInheritHandles is FALSE. A daemon must not keep any
            // PowerShell pipeline handle alive after the invoking CLI exits.
            let result = unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    if visible {
                        CREATE_NEW_CONSOLE
                    } else {
                        CREATE_NO_WINDOW
                    },
                    std::ptr::null(),
                    std::ptr::null(),
                    &startup,
                    &mut process,
                )
            };
            if result == 0 {
                return Err(io::Error::last_os_error());
            }
            unsafe {
                CloseHandle(process.hThread);
            }
            Ok(Self(process.hProcess))
        }
        pub fn exited(&mut self) -> io::Result<bool> {
            let mut code = 0;
            if unsafe { GetExitCodeProcess(self.0, &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(code != STILL_ACTIVE as u32)
        }
        pub fn terminate(&mut self) {
            unsafe {
                TerminateProcess(self.0, 1);
                WaitForSingleObject(self.0, 5000);
            }
        }
    }
}
#[cfg(windows)]
pub use windows::Background;
