//! Anti-recursion guard: detect if the current process is a child of the same
//! executable (e.g. saw-shell spawning a shell that re-execs saw-shell).
//!
//! Primary defense: `SAW_SKIP` env var set by the parent when spawning the
//! child shell. This module provides the fallback: check the parent and
//! grandparent process's executable path against the current executable's path.

/// Check if a path refers to a saw-* executable (saw-shell, saw-server, saw-client).
pub fn is_saw_executable(path: &str) -> bool {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| {
            let lower = n.to_lowercase();
            lower.starts_with("saw-")
        })
}

/// Returns true if the parent or grandparent process's executable path matches
/// the current executable path, indicating we're already inside a remote session.
///
/// Checks two levels: saw-shell → fish → saw-shell (grandparent match).
pub fn is_parent_same_exe() -> bool {
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };

    let ancestors = get_ancestor_exe_paths(2);
    ancestors.contains(&current_exe)
}

#[cfg(target_os = "linux")]
fn get_ancestor_exe_paths(max_levels: usize) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    let mut ppid = unsafe { libc::getppid() };
    for _ in 0..max_levels {
        if ppid <= 1 {
            break;
        }
        if let Ok(path) = std::fs::read_link(format!("/proc/{}/exe", ppid)) {
            result.push(path);
        }
        // Walk up: read /proc/{ppid}/stat to get its ppid
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", ppid)) else {
            break;
        };
        // Format: pid (comm) state ppid ...
        // Skip past comm (may contain spaces/parens) by finding last ')'
        let Some(after_comm) = stat.rsplit(')').next() else {
            break;
        };
        let mut fields = after_comm.split_whitespace();
        // fields: state ppid ...
        fields.next(); // state
        ppid = fields
            .next()
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);
    }
    result
}

#[cfg(target_os = "macos")]
fn get_ancestor_exe_paths(max_levels: usize) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    let mut ppid = unsafe { libc::getppid() };
    for _ in 0..max_levels {
        if ppid <= 1 {
            break;
        }
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let len = unsafe {
            libc::proc_pidpath(
                ppid,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as u32,
            )
        };
        if len > 0 {
            buf.truncate(len as usize);
            if let Ok(s) = String::from_utf8(buf) {
                result.push(std::path::PathBuf::from(s));
            }
        }

        // Walk up: use proc_pidinfo to get parent PID
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let info_size = std::mem::size_of::<libc::proc_bsdinfo>();
        let ret = unsafe {
            libc::proc_pidinfo(
                ppid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                info_size as i32,
            )
        };
        if ret != info_size as i32 {
            break;
        }
        ppid = info.pbi_ppid as i32;
    }
    result
}

#[cfg(target_os = "windows")]
fn get_ancestor_exe_paths(max_levels: usize) -> Vec<std::path::PathBuf> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    // Build PID -> parent PID map from snapshot
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Vec::new();
    }

    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    // Collect all PID -> parent PID mappings
    let mut pid_map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    if unsafe { Process32FirstW(snap, &mut entry) } != 0 {
        loop {
            pid_map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            if unsafe { Process32NextW(snap, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snap) };

    let pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };
    let mut current_pid = pid;
    let mut result = Vec::new();

    for _ in 0..max_levels {
        let Some(&parent_pid) = pid_map.get(&current_pid) else {
            break;
        };
        if parent_pid == 0 {
            break;
        }

        let handle = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                parent_pid,
            )
        };
        if handle.is_null() {
            break;
        }

        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = unsafe {
            windows_sys::Win32::System::Threading::QueryFullProcessImageNameW(
                handle,
                0,
                buf.as_mut_ptr(),
                &mut size,
            )
        };
        unsafe { CloseHandle(handle) };

        if ok != 0 && size > 0 {
            let s = String::from_utf16_lossy(&buf[..size as usize]);
            result.push(std::path::PathBuf::from(s));
        }

        current_pid = parent_pid;
    }
    result
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn get_ancestor_exe_paths(_max_levels: usize) -> Vec<std::path::PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_nested() {
        // When running tests, the parent is cargo/ctest, not saw-shell.
        // So is_parent_same_exe() should return false.
        assert!(!is_parent_same_exe());
    }
}
