//! One-shot SYSTEM launcher and bounded console-session pipe server.
use super::{
    desktop::{self, Command, Event, Worker},
    mapping::Mapping,
    pipe,
    protocol::{self, FrameInfo, Message, Region},
    trust::{self, wide, Descriptor, Handle},
};
use anyhow::{ensure, Result};
use std::{
    collections::BTreeMap,
    fs::File,
    os::windows::io::FromRawHandle,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{
            GetLastError, ERROR_ALREADY_EXISTS, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE,
        },
        Security::{
            DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary,
            TokenSessionId, TOKEN_ALL_ACCESS,
        },
        Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_NOWAIT,
                PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
            },
            Threading::{
                CreateMutexW, CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId,
                OpenProcessToken, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
                PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
    },
};
pub fn launch() -> Result<()> {
    trust::guard()?;
    let session = trust::console();
    ensure!(session != u32::MAX, "no active console session");
    let (_, sid) = trust::console_user()?;
    // Identity attestation must work from the medium-integrity host. Grant
    // only token QUERY and process QUERY_LIMITED_INFORMATION, never duplicate,
    // assign, terminate, VM access or any token privilege modification.
    let token_descriptor = Descriptor::new(&trust::query_sddl(&sid, 0x8))?;
    let token_attributes = token_descriptor.attributes();
    let process_descriptor = Descriptor::new(&trust::query_sddl(&sid, 0x1000))?;
    let process_attributes = process_descriptor.attributes();
    // The child holds a protected per-session mutex. Concurrent task launches
    // can race safely: only one child can create the first pipe instance.
    let image = trust::installed_image().unwrap();
    let app = wide(&image);
    let mut command = wide(format!("\"{}\" --serve", image.display()));
    let mut desktop = wide(r"winsta0\default");
    let cwd = wide(image.parent().unwrap());
    let mut source = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut source) }?;
    let source = Handle(source);
    let mut token = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            source.0,
            TOKEN_ALL_ACCESS,
            Some(&token_attributes),
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        )
    }?;
    let token = Handle(token);
    unsafe { SetTokenInformation(token.0, TokenSessionId, (&session as *const u32).cast(), 4) }?;
    let mut environment = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut environment, Some(token.0), false) }?;
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    let result = unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(app.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            Some(&process_attributes),
            None,
            false,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.cast_const()),
            PCWSTR(cwd.as_ptr()),
            &startup,
            &mut process,
        )
    };
    let _ = unsafe { DestroyEnvironmentBlock(environment) };
    result?;
    drop(Handle(process.hThread));
    drop(Handle(process.hProcess));
    Ok(())
}
struct Server {
    session: u32,
    sid: String,
    descriptor: Descriptor,
    host: AtomicBool,
    ended: AtomicBool,
    stopping: AtomicBool,
}
impl Server {
    fn instance(&self, first: bool) -> Result<Handle> {
        let name = wide(protocol::pipe_name(self.session));
        let a = self.descriptor.attributes();
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX
                    | if first {
                        FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        Default::default()
                    },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                4,
                65536,
                65536,
                0,
                Some(&a),
            )
        };
        ensure!(
            !h.is_invalid(),
            "create secure helper pipe: {}",
            std::io::Error::last_os_error()
        );
        Ok(Handle(h))
    }
    fn client(self: Arc<Self>, instance: Handle) {
        let result = (|| -> Result<()> {
            let mut pid = 0;
            unsafe { GetNamedPipeClientProcessId(instance.0, &mut pid) }?;
            let session = crate::admin_agent::trust::process_session_id(pid);
            let image = crate::admin_agent::trust::process_image_path(pid);
            tracing::info!(client_pid=pid,?session,image=%image.as_deref().map(|p|p.display().to_string()).unwrap_or_default(),"secure helper client connected");
            ensure!(
                session == Some(self.session) && trust::process_sid(Some(pid))? == self.sid,
                "secure helper client identity mismatch"
            );
            let h = instance.0;
            std::mem::forget(instance);
            let mut file = unsafe { File::from_raw_handle(h.0.cast()) };
            let hello = pipe::read_deadline(&mut file, Instant::now() + Duration::from_secs(2))?;
            let (version, role) = match protocol::hello(&hello) {
                protocol::HelloResult::Accepted { version, role } => (version, role as u8),
                protocol::HelloResult::Incompatible => {
                    protocol::write(
                        &mut file,
                        &Message::Incompatible {
                            min: protocol::PROTOCOL_MIN,
                            max: protocol::PROTOCOL_MAX,
                        },
                    )?;
                    return Ok(());
                }
                protocol::HelloResult::Invalid => {
                    protocol::write(&mut file, &Message::Refused)?;
                    anyhow::bail!("invalid hello");
                }
            };
            if role == 1 {
                ensure!(!self.stopping.load(Ordering::Acquire), "helper stopping");
                if self
                    .host
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    protocol::write(&mut file, &Message::Busy)?;
                    return Ok(());
                }
            }
            let result = protocol::write(
                &mut file,
                &Message::Welcome {
                    version,
                    min: protocol::PROTOCOL_MIN,
                    max: protocol::PROTOCOL_MAX,
                    pid: unsafe { GetCurrentProcessId() },
                },
            );
            if role == 2 {
                result?;
                return Ok(());
            }
            // Every accepted Host path, including welcome failure, tears down all
            // desktop-bound resources/releases held input before stopping server.
            let result = result
                .map_err(anyhow::Error::from)
                .and_then(|_| self.host_session(&mut file));
            self.ended.store(true, Ordering::Release);
            result
        })();
        if let Err(e) = result {
            tracing::info!(error=%e,"secure helper connection closed");
        }
    }
    fn host_session(&self, file: &mut File) -> Result<()> {
        let _desktop_switches = desktop::SwitchWatcher::new()?;
        let (mapping, name) = Mapping::create(&self.sid, self.session)?;
        protocol::write(file, &Message::Mapping { name })?;
        let mut slots = [None, None];
        let mut sequence = 0u64;
        let mut epoch = 0u64;
        let mut worker: Option<Worker> = None;
        let mut region: Option<Region> = None;
        let mut last_request = Instant::now();
        let mut spawn_at = Instant::now();
        let mut frame_budget =
            crate::admin_agent::session::RateLimiter::new(2000., 2000., Instant::now());
        let mut deny_budget =
            crate::admin_agent::session::RateLimiter::new(1., 0.2, Instant::now());
        let mut counters = super::stats::Counters::default();
        let mut pending = BTreeMap::<u64, u16>::new();
        let mut last_input_id = 0;
        let mut secure = false;
        let mut admin = false;
        loop {
            ensure!(!self.stopping.load(Ordering::Acquire), "helper stopping");
            ensure!(trust::console() == self.session, "active console changed");
            ensure!(
                last_request.elapsed() < Duration::from_secs(6),
                "host stopped polling"
            );
            if let Some(message) = pipe::poll(file)? {
                last_request = Instant::now();
                ensure!(trust::console_user()?.1 == self.sid, "console user changed");
                let policy = desktop::admin_mode(&self.sid);
                let admitted = frame_budget.take(1., Instant::now());
                if !admitted && !matches!(message, Message::Input { .. }) {
                    anyhow::bail!("host rate limited");
                }
                match message {
                    Message::Poll { region: r } => {
                        r.validate()?;
                        if region != Some(r) {
                            region = Some(r);
                            retire_worker(file, &mut worker, &mut pending, &mut counters)?;
                            spawn_at = Instant::now();
                        }
                    }
                    Message::Input {
                        id,
                        records,
                        epoch: target,
                    } => {
                        let received = records
                            .get(..2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]]))
                            .filter(|n| *n <= 1024)
                            .unwrap_or(0);
                        let decoded = crate::admin_agent::protocol::ClientMessage::decode(
                            crate::admin_agent::protocol::KIND_INJECT,
                            &records,
                        );
                        let reason = protocol::InputGate {
                            last_id: last_input_id,
                            epoch,
                            policy_on: policy,
                            worker_ready: worker.as_ref().is_some_and(|w| !w.done()),
                            outstanding: pending.len(),
                            rate_allowed: admitted,
                        }
                        .rejection(id, target, decoded.is_ok());
                        last_input_id = last_input_id.max(id);
                        if let Some((reason, policy_accepted)) = reason {
                            emit_receipt(
                                file,
                                &mut counters,
                                if policy_accepted {
                                    protocol::InputReceipt::rejected_after_policy(
                                        id, received, reason,
                                    )
                                } else {
                                    protocol::InputReceipt::rejected(id, received, reason)
                                },
                            )?;
                        } else {
                            let Ok(crate::admin_agent::protocol::ClientMessage::Inject(records)) =
                                decoded
                            else {
                                unreachable!();
                            };
                            if worker
                                .as_ref()
                                .unwrap()
                                .send
                                .try_send(Command::Input { id, records })
                                .is_err()
                            {
                                emit_receipt(
                                    file,
                                    &mut counters,
                                    protocol::InputReceipt::rejected_after_policy(
                                        id,
                                        received,
                                        "worker-full",
                                    ),
                                )?;
                            } else {
                                pending.insert(id, received);
                            }
                        }
                    }
                    Message::DenyConsent { epoch: target, id } => {
                        if target != epoch {
                            protocol::write(
                                file,
                                &Message::Denied {
                                    id,
                                    ok: false,
                                    reason: "desktop-changed".into(),
                                },
                            )?;
                        } else if deny_budget.take(1., Instant::now()) {
                            if let Some(w) = worker.as_ref() {
                                if w.send.try_send(Command::Deny(id)).is_err() {
                                    protocol::write(
                                        file,
                                        &Message::Denied {
                                            id,
                                            ok: false,
                                            reason: "busy".into(),
                                        },
                                    )?;
                                }
                            } else {
                                protocol::write(
                                    file,
                                    &Message::Denied {
                                        id,
                                        ok: false,
                                        reason: "secure-desktop-unavailable".into(),
                                    },
                                )?;
                            }
                        } else {
                            protocol::write(
                                file,
                                &Message::Denied {
                                    id,
                                    ok: false,
                                    reason: "rate-limited".into(),
                                },
                            )?;
                        }
                    }
                    Message::Ack { slot, seq } => {
                        ensure!(
                            slot <= 1 && slots[slot as usize] == Some(seq),
                            "invalid frame acknowledgment"
                        );
                        slots[slot as usize] = None;
                    }
                    _ => anyhow::bail!("unexpected host message"),
                }
            }
            if let Some(w) = worker.as_ref() {
                for _ in 0..32 {
                    let Ok(receipt) = w.receipts.try_recv() else {
                        break;
                    };
                    pending.remove(&receipt.id);
                    emit_receipt(file, &mut counters, receipt)?;
                }
                for _ in 0..8 {
                    let Ok(e) = w.recv.try_recv() else {
                        break;
                    };
                    match e {
                        Event::State {
                            secure: current_secure,
                            kind,
                            admin: current_admin,
                            available,
                            epoch,
                        } => {
                            secure = current_secure;
                            admin = current_admin;
                            protocol::write(
                                file,
                                &Message::State {
                                    secure: current_secure,
                                    kind,
                                    admin: current_admin,
                                    available,
                                    epoch,
                                },
                            )?;
                        }
                        Event::Denied { id, ok, reason } => {
                            protocol::write(file, &Message::Denied { id, ok, reason })?
                        }
                        Event::Pixels {
                            input_id,
                            region,
                            pixels,
                            hash,
                            epoch,
                        } => {
                            if let Some(slot) = slots.iter().position(Option::is_none) {
                                sequence = sequence
                                    .checked_add(1)
                                    .ok_or_else(|| anyhow::anyhow!("sequence exhausted"))?;
                                let f = FrameInfo {
                                    slot: slot as u8,
                                    seq: sequence,
                                    width: region.width,
                                    height: region.height,
                                    stride: region.width * 4,
                                    hash,
                                    epoch,
                                };
                                mapping.publish(f, &pixels)?;
                                slots[slot] = Some(sequence);
                                protocol::write(
                                    file,
                                    &Message::Frame {
                                        input_id,
                                        slot: f.slot,
                                        seq: f.seq,
                                        width: f.width,
                                        height: f.height,
                                        stride: f.stride,
                                        hash: f.hash,
                                        epoch,
                                    },
                                )?;
                            }
                        }
                    }
                }
            }
            if worker.as_ref().is_some_and(Worker::done) {
                retire_worker(file, &mut worker, &mut pending, &mut counters)?;
                spawn_at = Instant::now() + Duration::from_millis(100);
            }
            counters.log("helper", epoch, secure, admin);
            if worker.is_none() && Instant::now() >= spawn_at {
                if let Some(r) = region {
                    epoch = epoch
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("desktop epoch exhausted"))?;
                    worker = Some(Worker::spawn(self.session, self.sid.clone(), r, epoch));
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
fn emit_receipt(
    file: &mut File,
    counters: &mut super::stats::Counters,
    receipt: protocol::InputReceipt,
) -> Result<()> {
    counters.receipt(&receipt);
    protocol::write(file, &Message::InputReceipt(receipt))?;
    Ok(())
}
fn retire_worker(
    file: &mut File,
    worker: &mut Option<Worker>,
    pending: &mut BTreeMap<u64, u16>,
    counters: &mut super::stats::Counters,
) -> Result<()> {
    if let Some(mut w) = worker.take() {
        w.stop_and_join();
        while let Ok(receipt) = w.receipts.try_recv() {
            pending.remove(&receipt.id);
            emit_receipt(file, counters, receipt)?;
        }
    }
    for (id, received) in std::mem::take(pending) {
        emit_receipt(
            file,
            counters,
            protocol::InputReceipt::rejected_after_policy(id, received, "worker-stopped"),
        )?;
    }
    Ok(())
}
pub fn serve() -> Result<()> {
    trust::guard()?;
    let session = trust::console();
    ensure!(
        crate::admin_agent::trust::current_session_id() == session,
        "helper must run in active console session"
    );
    let (_, sid) = trust::console_user()?;
    let descriptor = Descriptor::new(&trust::pipe_sddl(&sid))?;
    let a = descriptor.attributes();
    let mutex_name = wide(format!(r"Local\BubblePlay.SecureHelper.s{session}"));
    let mutex = Handle(unsafe { CreateMutexW(Some(&a), false, PCWSTR(mutex_name.as_ptr())) }?);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(());
    }
    let server = Arc::new(Server {
        session,
        sid,
        descriptor,
        host: AtomicBool::new(false),
        ended: AtomicBool::new(false),
        stopping: AtomicBool::new(false),
    });
    let mut listening = server.instance(true)?;
    let started = Instant::now();
    let result = (|| -> Result<()> {
        while !server.ended.load(Ordering::Acquire) {
            if !server.host.load(Ordering::Acquire) && started.elapsed() >= Duration::from_secs(30)
            {
                break;
            }
            ensure!(trust::console() == session, "console session changed");
            let result = unsafe { ConnectNamedPipe(listening.0, None) };
            match result {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
                Err(e) if e.code() == ERROR_PIPE_LISTENING.to_hresult() => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
            let next = match server.instance(false) {
                Ok(n) => n,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            let connected = std::mem::replace(&mut listening, next);
            let s = server.clone();
            std::thread::spawn(move || s.client(connected));
        }
        Ok(())
    })();
    server.stopping.store(true, Ordering::Release);
    // Do not exit underneath a desktop-bound worker during console changes
    // or listener failures. Its bounded cancellation wait is at most 4 s.
    let deadline = Instant::now() + Duration::from_secs(6);
    while server.host.load(Ordering::Acquire)
        && !server.ended.load(Ordering::Acquire)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(mutex);
    result
}
