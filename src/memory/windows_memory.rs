// Reads Rekordbox's memory with plain winapi calls. Replaces toy-arms, which
// leaked a Toolhelp snapshot and a PROCESS_ALL_ACCESS handle on every
// (re)connect, could spin forever while enumerating, and flipped page
// protection in Rekordbox (VirtualProtectEx) before reads.

use std::mem::{size_of, zeroed};
use std::ptr::null_mut;

use winapi::shared::minwindef::{FALSE, LPCVOID, LPVOID};
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::memoryapi::ReadProcessMemory;
use winapi::um::processthreadsapi::OpenProcess;
use winapi::um::tlhelp32::{
    CreateToolhelp32Snapshot, Module32First, Module32Next, Process32First, Process32Next,
    MODULEENTRY32, PROCESSENTRY32, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use winapi::um::winnt::{HANDLE, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ};

use crate::memory::MemBackend;
use crate::memory::MemoryReadError;
use crate::memory::MemoryReadErrorType;

const PROCESS_NAME: &str = "rekordbox.exe";
const ERROR_BAD_LENGTH: u32 = 24;

/// Closes the handle when dropped, so early returns can't leak it
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn error(error_type: MemoryReadErrorType, address: usize, detail: Option<String>) -> MemoryReadError {
    MemoryReadError {
        pointer: None,
        address,
        detail,
        error_type,
    }
}

fn snapshot(flags: u32, pid: u32) -> Result<OwnedHandle, MemoryReadError> {
    // Module snapshots fail with ERROR_BAD_LENGTH while the target is loading modules: retry
    for _ in 0..5 {
        let handle = unsafe { CreateToolhelp32Snapshot(flags, pid) };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(OwnedHandle(handle));
        }
        if unsafe { GetLastError() } != ERROR_BAD_LENGTH {
            break;
        }
    }
    let code = unsafe { GetLastError() };
    Err(error(
        MemoryReadErrorType::SnapshotFailed,
        0,
        Some(format!("CreateToolhelp32Snapshot error {code}")),
    ))
}

fn name_matches(raw: &[i8], name: &str) -> bool {
    let bytes: Vec<u8> = raw.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).eq_ignore_ascii_case(name)
}

fn find_process_id(name: &str) -> Result<u32, MemoryReadError> {
    let snap = snapshot(TH32CS_SNAPPROCESS, 0)?;
    let mut entry: PROCESSENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32>() as u32;
    let mut ok = unsafe { Process32First(snap.0, &mut entry) };
    while ok != FALSE {
        if name_matches(&entry.szExeFile, name) {
            return Ok(entry.th32ProcessID);
        }
        ok = unsafe { Process32Next(snap.0, &mut entry) };
    }
    Err(error(MemoryReadErrorType::ProcessNotFound, 0, None))
}

fn find_module_base(pid: u32, name: &str) -> Result<usize, MemoryReadError> {
    let snap = snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid)?;
    let mut entry: MODULEENTRY32 = unsafe { zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32>() as u32;
    let mut ok = unsafe { Module32First(snap.0, &mut entry) };
    while ok != FALSE {
        if name_matches(&entry.szModule, name) {
            return Ok(entry.modBaseAddr as usize);
        }
        ok = unsafe { Module32Next(snap.0, &mut entry) };
    }
    Err(error(MemoryReadErrorType::ModuleNotFound, 0, Some(name.to_string())))
}

pub struct WindowsMem {
    process: OwnedHandle,
    base: usize,
}

impl WindowsMem {
    pub fn new() -> Result<Self, MemoryReadError> {
        let pid = find_process_id(PROCESS_NAME)?;
        // Read-only access is all we need
        let handle = unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if handle.is_null() {
            let code = unsafe { GetLastError() };
            return Err(error(
                MemoryReadErrorType::ProcessNotFound,
                0,
                Some(format!("OpenProcess error {code}")),
            ));
        }
        let process = OwnedHandle(handle);
        let base = find_module_base(pid, PROCESS_NAME)?;
        Ok(WindowsMem { process, base })
    }
}

impl MemBackend for WindowsMem {
    fn read<T>(&self, address: usize) -> Result<T, MemoryReadError> {
        unsafe {
            let mut buffer: T = zeroed();
            let ok = ReadProcessMemory(
                self.process.0,
                address as LPCVOID,
                &mut buffer as *mut T as LPVOID,
                size_of::<T>(),
                null_mut(),
            );
            if ok == FALSE {
                let detail = match GetLastError() {
                    6 => "ErrorInvalidHandle".to_string(),
                    299 => "ErrorPartialCopy".to_string(),
                    487 => "ErrorInvalidAddress".to_string(),
                    code => format!("UnknownError {{ error_code: {code} }}"),
                };
                return Err(error(MemoryReadErrorType::ReadMemoryFailed, address, Some(detail)));
            }
            Ok(buffer)
        }
    }

    fn get_base_offset(&self) -> usize {
        self.base
    }
}
