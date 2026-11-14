//! Windows headless-host stop signal. Business domains remain supervised by the kernel.
use std::{io, ptr};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::Threading::{CreateEventW, INFINITE, WaitForSingleObject},
};

pub struct ShutdownEvent(HANDLE);

impl ShutdownEvent {
    pub fn new() -> io::Result<Self> {
        let name: Vec<u16> = format!("Local\\AtelierKernelShutdown-{}", std::process::id())
            .encode_utf16().chain(Some(0)).collect();
        let handle = unsafe { CreateEventW(ptr::null(), 1, 0, name.as_ptr()) };
        if handle.is_null() { Err(io::Error::last_os_error()) } else { Ok(Self(handle)) }
    }

    pub fn wait(&self) -> io::Result<()> {
        if unsafe { WaitForSingleObject(self.0, INFINITE) } == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Drop for ShutdownEvent {
    fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
}
