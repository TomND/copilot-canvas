//! Owner-only, local-only named pipes with overlapped I/O so one thread can
//! block in a read while another writes on the same connection.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::CreateEventW;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Handle(HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct Event(Handle);

impl Event {
    fn new() -> io::Result<Self> {
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Handle(handle)))
    }
}

/// One end of a connected pipe.
pub struct Pipe {
    handle: Handle,
    /// Set by `PipeCloser::close`: later reads end and writes fail at once.
    closed: std::sync::atomic::AtomicBool,
}

impl Pipe {
    fn from_handle(handle: Handle) -> Self {
        Self { handle, closed: std::sync::atomic::AtomicBool::new(false) }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The process at the client end of a server pipe.
    pub fn client_pid(&self) -> Option<u32> {
        let mut pid = 0u32;
        // SAFETY: a connected pipe handle and a valid output pointer.
        (unsafe { windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId(self.handle.0, &mut pid) } != 0 && pid != 0).then_some(pid)
    }

    /// Splits into independently usable read and write halves.
    pub fn split(self) -> io::Result<(PipeReader, PipeWriter, PipeCloser)> {
        let shared = Arc::new(self);
        Ok((
            PipeReader { pipe: shared.clone(), event: Event::new()? },
            PipeWriter { pipe: shared.clone(), event: Event::new()? },
            PipeCloser { pipe: shared },
        ))
    }

    fn overlapped<F>(&self, event: &Event, op: F) -> io::Result<u32>
    where
        F: FnOnce(*mut OVERLAPPED) -> i32,
    {
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.0.0;
        let started = op(&mut overlapped);
        if started == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        let mut transferred = 0u32;
        if unsafe { GetOverlappedResult(self.handle.0, &overlapped, &mut transferred, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(transferred)
    }
}

fn is_disconnect(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error().map(|c| c as u32),
        Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED | 995 /* ERROR_OPERATION_ABORTED */)
    )
}

pub struct PipeReader {
    pipe: Arc<Pipe>,
    event: Event,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pipe.is_closed() {
            return Ok(0);
        }
        let len = buf.len().min(u32::MAX as usize) as u32;
        let handle = self.pipe.handle.0;
        match self.pipe.overlapped(&self.event, |ov| unsafe {
            ReadFile(handle, buf.as_mut_ptr(), len, std::ptr::null_mut(), ov)
        }) {
            Ok(n) => Ok(n as usize),
            Err(error) if is_disconnect(&error) => Ok(0),
            Err(error) => Err(error),
        }
    }
}

pub struct PipeWriter {
    pipe: Arc<Pipe>,
    event: Event,
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pipe.is_closed() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed"));
        }
        let len = buf.len().min(1 << 20) as u32;
        let handle = self.pipe.handle.0;
        match self.pipe.overlapped(&self.event, |ov| unsafe {
            WriteFile(handle, buf.as_ptr(), len, std::ptr::null_mut(), ov)
        }) {
            Ok(n) => Ok(n as usize),
            Err(error) if is_disconnect(&error) => Err(io::Error::new(io::ErrorKind::BrokenPipe, error)),
            Err(error) => Err(error),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Cancels pending I/O so blocked reader/writer threads return.
#[derive(Clone)]
pub struct PipeCloser {
    pipe: Arc<Pipe>,
}

impl PipeCloser {
    pub fn close(&self) {
        self.pipe.closed.store(true, std::sync::atomic::Ordering::Release);
        unsafe { CancelIoEx(self.pipe.handle.0, std::ptr::null()) };
    }
}

/// Security attributes granting access only to the given user SID.
struct OwnerOnly {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

unsafe impl Send for OwnerOnly {}

impl OwnerOnly {
    fn new(user_sid: &str) -> io::Result<Self> {
        let sddl = wide(&format!("D:P(A;;GA;;;{user_sid})"));
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut())
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        Ok(Self { descriptor, attributes })
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        unsafe { LocalFree(self.descriptor as _) };
    }
}

pub struct PipeServer {
    name: Vec<u16>,
    security: OwnerOnly,
    pending: Option<Handle>,
}

impl PipeServer {
    /// Creates the first pipe instance. Fails with `AlreadyExists` when another
    /// process (another service, or a squatter) already owns the name.
    pub fn bind(name: &str) -> io::Result<Self> {
        let sid = crate::win::current_user_sid()?;
        let mut server = Self { name: wide(name), security: OwnerOnly::new(&sid)?, pending: None };
        server.pending = Some(server.instance(true)?);
        Ok(server)
    }

    fn instance(&self, first: bool) -> io::Result<Handle> {
        let mut flags = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            flags |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let handle = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                256 * 1024,
                64 * 1024,
                0,
                &self.security.attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) && first {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "pipe name already in use"));
            }
            return Err(error);
        }
        Ok(Handle(handle))
    }

    /// Blocks until a client connects.
    pub fn accept(&mut self) -> io::Result<Pipe> {
        let handle = match self.pending.take() {
            Some(handle) => handle,
            None => self.instance(false)?,
        };
        let pipe = Pipe::from_handle(handle);
        let event = Event::new()?;
        let raw = pipe.handle.0;
        match pipe.overlapped(&event, |ov| unsafe { ConnectNamedPipe(raw, ov) }) {
            Ok(_) => {}
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) => {}
            Err(error) => return Err(error),
        }
        self.pending = Some(self.instance(false)?);
        Ok(pipe)
    }
}

/// Connects to a server pipe, waiting up to `timeout` for a free instance.
pub fn connect(name: &str, timeout: Duration) -> io::Result<Pipe> {
    let wide_name = wide(name);
    let deadline = Instant::now() + timeout;
    loop {
        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                // Identification-only: a hostile server cannot impersonate us.
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(Pipe::from_handle(Handle(handle)));
        }
        let error = io::Error::last_os_error();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) || remaining.is_zero() {
            return Err(error);
        }
        unsafe { WaitNamedPipeW(wide_name.as_ptr(), remaining.as_millis().clamp(1, 5_000) as u32) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplex_reads_and_writes_do_not_block_each_other() {
        let name = format!(r"\\.\pipe\copilot-canvas-test-{}", uuid::Uuid::new_v4());
        let mut server = PipeServer::bind(&name).unwrap();
        assert_eq!(PipeServer::bind(&name).err().map(|e| e.kind()), Some(io::ErrorKind::AlreadyExists));
        let client_name = name.clone();
        let client = std::thread::spawn(move || {
            let pipe = connect(&client_name, Duration::from_secs(5)).unwrap();
            let (mut reader, mut writer, _closer) = pipe.split().unwrap();
            // Reader blocks first; the write must still complete.
            let read = std::thread::spawn(move || {
                let mut buf = [0u8; 5];
                reader.read_exact(&mut buf).unwrap();
                buf
            });
            std::thread::sleep(Duration::from_millis(50));
            writer.write_all(b"ping!").unwrap();
            read.join().unwrap()
        });
        let pipe = server.accept().unwrap();
        assert_eq!(pipe.client_pid(), Some(std::process::id()), "the client is this process");
        let (mut reader, mut writer, closer) = pipe.split().unwrap();
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping!");
        writer.write_all(b"pong!").unwrap();
        assert_eq!(&client.join().unwrap(), b"pong!");
        // Peer gone: reads report end of stream.
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        closer.close();
    }
}
