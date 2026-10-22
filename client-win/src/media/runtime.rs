use anyhow::Result;
use std::marker::PhantomData;
use windows::Win32::{
    Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION},
    System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
};

/// RAII guard for COM (multithreaded apartment) and Media Foundation on one thread.
///
/// Hold one on every thread that creates or drives a decoder or renderer. COM
/// init is per thread, so the guard is `!Send`. Media Foundation startup is
/// reference-counted per process, and each guard balances its own call.
pub struct MediaRuntime {
    _thread_bound: PhantomData<*const ()>,
}

impl MediaRuntime {
    pub fn new() -> Result<Self> {
        unsafe {
            // Fails with RPC_E_CHANGED_MODE on an STA thread. Nothing is
            // initialized then, so nothing is undone.
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(error.into());
            }
        }
        Ok(Self {
            _thread_bound: PhantomData,
        })
    }
}

impl Drop for MediaRuntime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}
