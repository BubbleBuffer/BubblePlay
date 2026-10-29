//! Console access for a GUI-subsystem executable: the `key`/`pcs` subcommands print to the
//! console they were started from, and a protected key asks for its passphrase there.
//!
//! The executable is linked as a Windows GUI program, so shells do not wait for it. Run the
//! subcommands piped (`... | Out-Host` in PowerShell) or with `start /wait` in cmd so the shell
//! does not read the console at the same time.

use std::io::Write as _;

use anyhow::{bail, Context, Result};
use windows::Win32::System::Console::{
    AllocConsole, AttachConsole, GetConsoleMode, GetStdHandle, SetConsoleMode,
    ATTACH_PARENT_PROCESS, CONSOLE_MODE, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
};
use zeroize::Zeroizing;

/// Attaches to the parent's console, if there is one. Standard handles that were redirected
/// (pipes, files) stay as they are.
pub fn attach() {
    // SAFETY: plain Win32 call; failure (no parent console, already attached) is harmless.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Reads one line without echo when standard input is a console; a redirected input is read
/// as a plain line. The trailing line break is removed. End of input is an error.
pub fn read_passphrase(prompt: &str) -> Result<Zeroizing<String>> {
    attach();
    // SAFETY: plain Win32 calls on this process's standard input handle.
    let mut input = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok();
    if input.is_none_or(|handle| handle.is_invalid()) {
        // Started without any console (for example from Explorer): open one to ask.
        // SAFETY: plain Win32 calls.
        let _ = unsafe { AllocConsole() };
        input = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok();
    }
    let input = input
        .filter(|handle| !handle.is_invalid())
        .context("no console to ask for the key passphrase")?;
    let mut mode = CONSOLE_MODE(0);
    // SAFETY: `input` is this process's standard input handle; `mode` is a live out pointer.
    let console = unsafe { GetConsoleMode(input, &mut mode) }.is_ok();
    if console {
        // SAFETY: as above; the original mode is restored below.
        let _ = unsafe { SetConsoleMode(input, CONSOLE_MODE(mode.0 & !ENABLE_ECHO_INPUT.0)) };
    }
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();
    let mut line = Zeroizing::new(String::new());
    let read = std::io::stdin().read_line(&mut line);
    if console {
        // SAFETY: restores the mode read above.
        let _ = unsafe { SetConsoleMode(input, mode) };
        let _ = writeln!(stderr);
    }
    if read.context("read the passphrase")? == 0 {
        bail!("no passphrase entered");
    }
    let kept = line.trim_end_matches(['\r', '\n']).len();
    line.truncate(kept);
    Ok(line)
}
