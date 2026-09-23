//! Process spawning without flashing console windows.
//!
//! On Windows every child process (ffmpeg, tesseract, winget, powershell…)
//! gets CREATE_NO_WINDOW so everything runs in the background.

pub fn cmd<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    let mut c = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

pub fn tokio_cmd<S: AsRef<std::ffi::OsStr>>(program: S) -> tokio::process::Command {
    tokio::process::Command::from(cmd(program))
}
