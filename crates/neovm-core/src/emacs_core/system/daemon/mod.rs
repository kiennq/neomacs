use std::fmt;
use std::sync::{Mutex, OnceLock};

use super::value::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DaemonRequest {
    Background { name: Option<String> },
    Foreground { name: Option<String> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonStateError {
    NotDaemon,
    AlreadyInitialized,
    ReadinessSignalFailed,
}

impl fmt::Display for DaemonStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDaemon => f.write_str("not running as a daemon"),
            Self::AlreadyInitialized => f.write_str("daemon already initialized"),
            Self::ReadinessSignalFailed => f.write_str("daemon readiness signal failed"),
        }
    }
}

impl std::error::Error for DaemonStateError {}

#[derive(Default)]
struct DaemonState {
    request: Option<DaemonRequest>,
    initialized: bool,
}

static DAEMON_STATE: OnceLock<Mutex<DaemonState>> = OnceLock::new();

fn daemon_state() -> &'static Mutex<DaemonState> {
    DAEMON_STATE.get_or_init(|| Mutex::new(DaemonState::default()))
}

pub fn configure(request: Option<DaemonRequest>) -> Result<(), DaemonStateError> {
    let mut state = daemon_state().lock().expect("daemon state mutex poisoned");
    if state.initialized {
        return Err(DaemonStateError::AlreadyInitialized);
    }
    state.request = request;
    Ok(())
}

pub fn daemon_value() -> Value {
    let state = daemon_state().lock().expect("daemon state mutex poisoned");
    match state.request.as_ref() {
        None => Value::NIL,
        Some(DaemonRequest::Background { name } | DaemonRequest::Foreground { name }) => {
            name.as_deref().map(Value::string).unwrap_or(Value::T)
        }
    }
}

pub fn mark_initialized() -> Result<(), DaemonStateError> {
    let mut state = daemon_state().lock().expect("daemon state mutex poisoned");
    if state.request.is_none() {
        return Err(DaemonStateError::NotDaemon);
    }
    if state.initialized {
        return Err(DaemonStateError::AlreadyInitialized);
    }
    signal_readiness()?;
    state.initialized = true;
    Ok(())
}

pub fn is_daemon() -> bool {
    daemon_state()
        .lock()
        .expect("daemon state mutex poisoned")
        .request
        .is_some()
}

pub fn is_initialized() -> bool {
    daemon_state()
        .lock()
        .expect("daemon state mutex poisoned")
        .initialized
}

#[cfg(unix)]
fn signal_readiness() -> Result<(), DaemonStateError> {
    if let Some(fd) = std::env::var("NEOMACS_DAEMON_READY_FD")
        .ok()
        .and_then(|value| value.parse::<libc::c_int>().ok())
    {
        signal_readiness_fd(fd)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn signal_readiness_fd(fd: libc::c_int) -> Result<(), DaemonStateError> {
    let byte = [1u8];
    loop {
        let written = unsafe { libc::write(fd, byte.as_ptr().cast(), byte.len()) };
        if written == byte.len() as isize {
            unsafe { libc::close(fd) };
            return Ok(());
        }
        if written == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(DaemonStateError::ReadinessSignalFailed);
    }
}

#[cfg(windows)]
fn signal_readiness() -> Result<(), DaemonStateError> {
    let Some(name) = std::env::var("NEOMACS_DAEMON_READY_EVENT").ok() else {
        return Ok(());
    };
    let mut wide_name: Vec<u16> = name.encode_utf16().collect();
    wide_name.push(0);

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};

    unsafe {
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, wide_name.as_ptr());
        if event.is_null() {
            return Err(DaemonStateError::ReadinessSignalFailed);
        }
        let signaled = SetEvent(event) != 0;
        CloseHandle(event);
        if !signaled {
            return Err(DaemonStateError::ReadinessSignalFailed);
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn signal_readiness() -> Result<(), DaemonStateError> {
    Ok(())
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut state = daemon_state().lock().expect("daemon state mutex poisoned");
    *state = DaemonState::default();
}
