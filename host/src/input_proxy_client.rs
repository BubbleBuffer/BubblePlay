use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufReader, BufWriter};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::IsProcessInJob;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcessId, ResumeThread, TerminateProcess, WaitForSingleObject,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

use crate::input::{DesktopRegion, InputApplication, InputOutcome};
use crate::input_proxy_protocol::{read_response, write_request, Request, Response};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT_MS: u32 = 500;
const HANDLE_FLAG_INHERIT: u32 = 1;

pub struct InputProxyClient {
    writer: Option<BufWriter<File>>,
    responses: Receiver<io::Result<(u64, Response)>>,
    reader: Option<JoinHandle<()>>,
    process: Option<OwnedHandle>,
    process_id: u32,
    next_request_id: u64,
}

impl InputProxyClient {
    pub fn launch(session_id: u32, generation: u32) -> Result<Self> {
        let proxy_path = sibling_proxy_path(&std::env::current_exe().context("locate Agent")?);
        if !proxy_path.is_file() {
            bail!("input proxy binary is missing beside the Agent");
        }
        let process = launch_user_process(&proxy_path, session_id, generation)?;
        let responses = spawn_response_reader(process.stdout)?;
        let mut client = Self {
            writer: Some(BufWriter::new(process.stdin)),
            responses: responses.0,
            reader: Some(responses.1),
            process: Some(process.process),
            process_id: process.process_id,
            next_request_id: 1,
        };
        let response = client.call(Request::Hello {
            session_id,
            generation,
        })?;
        validate_ready(response, session_id, generation, client.process_id)?;
        Ok(client)
    }

    pub fn input(&mut self, packet: &[u8]) -> Result<(InputOutcome, u64)> {
        match self.call(Request::Input(packet.to_vec()))? {
            Response::Applied {
                requested,
                applied,
                failed_calls,
                last_error_code,
                accepted_sequence,
                cursor_changed,
                sync_requested,
                feedback_reliable,
                stale_motion_dropped,
            } => Ok((
                InputOutcome {
                    accepted_sequence,
                    cursor_changed,
                    sync_requested,
                    feedback_reliable,
                    application: InputApplication {
                        requested,
                        applied,
                        failed_calls,
                        last_error_code,
                    },
                },
                stale_motion_dropped,
            )),
            response => bail!("input proxy returned unexpected input response: {response:?}"),
        }
    }

    pub fn set_region(&mut self, region: DesktopRegion) -> Result<()> {
        expect_ack(self.call(Request::SetRegion {
            left: region.x,
            top: region.y,
            width: region.width,
            height: region.height,
        })?)
    }

    pub fn release_all(&mut self) -> Result<InputApplication> {
        match self.call(Request::ReleaseAll)? {
            Response::Released {
                requested,
                applied,
                failed_calls,
                last_error_code,
            } => Ok(InputApplication {
                requested,
                applied,
                failed_calls,
                last_error_code,
            }),
            response => bail!("input proxy returned unexpected release response: {response:?}"),
        }
    }

    /// Clipboard text the interactive user changed since the last poll.
    pub fn poll_clipboard(&mut self, reset: bool) -> Result<Option<crate::clipboard::ClipboardContent>> {
        match self.call(Request::PollClipboard { reset })? {
            Response::Clipboard { text } => Ok(text.map(crate::clipboard::ClipboardContent::Text)),
            Response::ClipboardPng { png } => Ok(Some(crate::clipboard::ClipboardContent::Png(png))),
            response => bail!("input proxy returned unexpected clipboard response: {response:?}"),
        }
    }

    fn call(&mut self, request: Request) -> Result<Response> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .context("input proxy request identity exhausted")?;
        let writer = self
            .writer
            .as_mut()
            .context("input proxy request stream is closed")?;
        if let Err(error) = write_request(writer, request_id, &request) {
            self.terminate();
            return Err(error).context("write input proxy request");
        }
        let received = match self.responses.recv_timeout(RESPONSE_TIMEOUT) {
            Ok(Ok(received)) => received,
            Ok(Err(error)) => {
                self.terminate();
                return Err(error).context("read input proxy response");
            }
            Err(RecvTimeoutError::Timeout) => {
                self.terminate();
                bail!("input proxy response timed out");
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.terminate();
                bail!("input proxy response stream closed");
            }
        };
        if received.0 != request_id {
            self.terminate();
            bail!("input proxy response identity mismatch");
        }
        if let Response::Rejected { code } = received.1 {
            bail!("input proxy rejected request with code {code}");
        }
        Ok(received.1)
    }

    fn terminate(&mut self) {
        self.writer.take();
        if let Some(process) = self.process.as_ref() {
            let _ = unsafe { TerminateProcess(process.0, 1) };
            let _ = unsafe { WaitForSingleObject(process.0, SHUTDOWN_TIMEOUT_MS) };
        }
        self.process.take();
    }
}

pub fn current_process_session_id() -> Result<u32> {
    let mut session_id = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) }
        .context("resolve Agent process session")?;
    Ok(session_id)
}

impl Drop for InputProxyClient {
    fn drop(&mut self) {
        if self.process.is_some() && self.writer.is_some() {
            let request_id = self.next_request_id;
            if let Some(writer) = self.writer.as_mut() {
                let _ = write_request(writer, request_id, &Request::Shutdown);
            }
            if let Some(process) = self.process.as_ref() {
                if unsafe { WaitForSingleObject(process.0, SHUTDOWN_TIMEOUT_MS) } == WAIT_TIMEOUT {
                    let _ = unsafe { TerminateProcess(process.0, 1) };
                    let _ = unsafe { WaitForSingleObject(process.0, SHUTDOWN_TIMEOUT_MS) };
                }
            }
        }
        self.writer.take();
        self.process.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

struct LaunchedProxy {
    process: OwnedHandle,
    process_id: u32,
    stdin: File,
    stdout: File,
}

fn launch_user_process(path: &Path, session_id: u32, generation: u32) -> Result<LaunchedProxy> {
    let (child_stdin, parent_stdin) = anonymous_pipe()?;
    let (parent_stdout, child_stdout) = anonymous_pipe()?;
    clear_inherit(parent_stdin.0)?;
    clear_inherit(parent_stdout.0)?;

    let mut user_token = HANDLE::default();
    unsafe { WTSQueryUserToken(session_id, &mut user_token) }
        .context("query interactive-user token for input proxy")?;
    let user_token = OwnedHandle::new(user_token)?;

    let mut environment = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut environment, Some(user_token.0), false) }
        .context("create input proxy user environment")?;
    let environment = EnvironmentBlock(environment);

    let stderr = File::open("NUL").context("open input proxy null stderr")?;
    set_inherit(HANDLE(stderr.as_raw_handle().cast()), true)?;
    let path_wide = wide_null(path.as_os_str());
    let current_dir = path
        .parent()
        .map(|value| wide_null(value.as_os_str()))
        .context("input proxy binary has no parent directory")?;
    let command = format!(
        "\"{}\" --session {session_id} --generation {generation}",
        path.display()
    );
    let mut command_wide = wide_null(OsStr::new(&command));
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: child_stdin.0,
        hStdOutput: child_stdout.0,
        hStdError: HANDLE(stderr.as_raw_handle().cast()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessAsUserW(
            Some(user_token.0),
            PCWSTR(path_wide.as_ptr()),
            Some(PWSTR(command_wide.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED | CREATE_NO_WINDOW,
            Some(environment.0.cast_const()),
            PCWSTR(current_dir.as_ptr()),
            &startup,
            &mut process,
        )
    }
    .context("launch input proxy as interactive user")?;
    let process_handle = OwnedHandle::new(process.hProcess)?;
    let thread_handle = OwnedHandle::new(process.hThread)?;

    let mut in_job = false.into();
    unsafe { IsProcessInJob(process_handle.0, None, &mut in_job) }
        .context("verify input proxy inherited Agent job")?;
    if !in_job.as_bool() {
        let _ = unsafe { TerminateProcess(process_handle.0, 1) };
        bail!("input proxy did not inherit the Agent supervision job");
    }
    if unsafe { ResumeThread(thread_handle.0) } == u32::MAX {
        let error = io::Error::last_os_error();
        let _ = unsafe { TerminateProcess(process_handle.0, 1) };
        return Err(error).context("resume input proxy process");
    }
    drop(thread_handle);
    drop(child_stdin);
    drop(child_stdout);

    Ok(LaunchedProxy {
        process: process_handle,
        process_id: process.dwProcessId,
        stdin: parent_stdin.into_file(),
        stdout: parent_stdout.into_file(),
    })
}

fn spawn_response_reader(
    stdout: File,
) -> Result<(Receiver<io::Result<(u64, Response)>>, JoinHandle<()>)> {
    let (sender, receiver) = mpsc::sync_channel(8);
    let join = thread::Builder::new()
        .name("bubbleplay-input-proxy-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let response = read_response(&mut reader);
                let stop = response.is_err();
                if sender.send(response).is_err() || stop {
                    break;
                }
            }
        })
        .context("spawn input proxy response reader")?;
    Ok((receiver, join))
}

fn validate_ready(
    response: Response,
    session_id: u32,
    generation: u32,
    process_id: u32,
) -> Result<()> {
    match response {
        Response::Ready {
            session_id: actual_session,
            generation: actual_generation,
            process_id: actual_process,
        } if actual_session == session_id
            && actual_generation == generation
            && actual_process == process_id =>
        {
            Ok(())
        }
        _ => bail!("input proxy ready identity mismatch"),
    }
}

fn expect_ack(response: Response) -> Result<()> {
    match response {
        Response::Ack => Ok(()),
        _ => bail!("input proxy returned unexpected acknowledgement"),
    }
}

fn sibling_proxy_path(agent_path: &Path) -> PathBuf {
    agent_path.with_file_name("bubbleplay_input_proxy.exe")
}

fn anonymous_pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe { CreatePipe(&mut read, &mut write, Some(&mut attributes), 64 * 1024) }
        .context("create private input proxy pipe")?;
    Ok((OwnedHandle::new(read)?, OwnedHandle::new(write)?))
}

fn clear_inherit(handle: HANDLE) -> Result<()> {
    set_inherit(handle, false)
}

fn set_inherit(handle: HANDLE, inherit: bool) -> Result<()> {
    let flags = if inherit { HANDLE_FLAG_INHERIT } else { 0 };
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags) } == 0 {
        return Err(io::Error::last_os_error()).context("set input proxy handle inheritance");
    }
    Ok(())
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Result<Self> {
        if handle.is_invalid() {
            bail!("Windows returned an invalid input proxy handle");
        }
        Ok(Self(handle))
    }

    fn into_file(self) -> File {
        let raw = self.0 .0 as RawHandle;
        std::mem::forget(self);
        unsafe { File::from_raw_handle(raw) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct EnvironmentBlock(*mut core::ffi::c_void);

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { DestroyEnvironmentBlock(self.0) };
        }
    }
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetHandleInformation(handle: HANDLE, mask: u32, flags: u32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_path_is_an_exact_sibling_of_the_agent() {
        assert_eq!(
            sibling_proxy_path(Path::new(r"C:\BubblePlay\bubbleplay_agent.exe")),
            PathBuf::from(r"C:\BubblePlay\bubbleplay_input_proxy.exe")
        );
    }

    #[test]
    fn ready_receipt_must_match_session_generation_and_process() {
        let ready = Response::Ready {
            session_id: 2,
            generation: 4,
            process_id: 8,
        };
        assert!(validate_ready(ready.clone(), 2, 4, 8).is_ok());
        assert!(validate_ready(ready.clone(), 3, 4, 8).is_err());
        assert!(validate_ready(ready.clone(), 2, 5, 8).is_err());
        assert!(validate_ready(ready, 2, 4, 9).is_err());
    }
}
