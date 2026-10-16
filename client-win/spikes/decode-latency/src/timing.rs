use anyhow::{bail, Result};
use std::mem::size_of;
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Media::{timeBeginPeriod, timeEndPeriod},
        System::{
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            Threading::*,
        },
    },
};

pub fn qpc() -> i64 {
    let mut n = 0;
    unsafe {
        QueryPerformanceCounter(&mut n).expect("QPC");
    }
    n
}
pub fn frequency() -> i64 {
    let mut n = 0;
    unsafe {
        QueryPerformanceFrequency(&mut n).expect("QPF");
    }
    n
}
pub struct Sleeper(HANDLE);
// Windows 10 2004+ scopes this request to the caller. This tool targets
// Windows 11 and balances the request when its bounded arrival child exits.
pub struct TimerResolution;
impl TimerResolution {
    pub fn new() -> Result<Self> {
        let result = unsafe { timeBeginPeriod(1) };
        if result != 0 {
            bail!("timeBeginPeriod(1) failed: {result}");
        }
        Ok(Self)
    }
}
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}
// Policy is local to this throwaway process. No power plan or registry changes.
pub fn configure_process_timing() -> Result<()> {
    let policy = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
            | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &policy as *const _ as *const _,
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )?;
    }
    Ok(())
}
pub struct ArrivalPriority(HANDLE);
impl ArrivalPriority {
    pub fn new() -> Result<Self> {
        unsafe {
            let mut index = 0;
            // This bounded, low-duty arrival clock needs prompt wakeups even
            // though its companion has no foreground HWND. Pro Audio is the
            // documented high MMCSS category; no system profile is modified.
            let task = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index)?;
            if let Err(e) = AvSetMmThreadPriority(task, AVRT_PRIORITY_CRITICAL) {
                let _ = AvRevertMmThreadCharacteristics(task);
                return Err(e.into());
            }
            Ok(Self(task))
        }
    }
}
impl Drop for ArrivalPriority {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}
impl Sleeper {
    pub fn new() -> Result<Self> {
        unsafe {
            Ok(Self(CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )?))
        }
    }
    pub fn sleep_us(&self, us: u64) -> Result<()> {
        let due = -(us.max(1) as i64 * 10);
        unsafe {
            SetWaitableTimer(self.0, &due, 0, None, None, false)?;
            if WaitForSingleObject(self.0, 1000) != WAIT_OBJECT_0 {
                bail!("timer wait failed/timed out");
            }
        }
        Ok(())
    }
}
impl Drop for Sleeper {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
