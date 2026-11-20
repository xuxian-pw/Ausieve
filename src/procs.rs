//! Process enumeration helpers.

use std::collections::HashMap;

use windows::Win32::Foundation::{CloseHandle, FILETIME, WAIT_TIMEOUT};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
};

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
}

/// All processes currently running.
pub fn snapshot() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push(ProcInfo {
                pid: e.th32ProcessID,
                parent_pid: e.th32ParentProcessID,
                name: String::from_utf16_lossy(&e.szExeFile[..len]),
            });
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Process creation time (FILETIME as u64), if the process can be opened.
pub fn creation_time(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let r = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64)
    }
}

/// True if a process with this PID is running.
pub fn is_alive(pid: u32) -> bool {
    unsafe {
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(h) => {
                let r = WaitForSingleObject(h, 0);
                let _ = CloseHandle(h);
                r == WAIT_TIMEOUT
            }
            Err(_) => false,
        }
    }
}

/// Normalises a user-supplied executable name: adds `.exe` if missing.
pub fn normalize_name(name: &str) -> String {
    if name.to_ascii_lowercase().ends_with(".exe") {
        name.to_string()
    } else {
        format!("{name}.exe")
    }
}

/// Finds the *root* process for an executable name: a process with that name whose
/// parent is not also a (live, older) process with that name. Multi-process apps such
/// as Discord/Chrome play audio from child processes; excluding the root's tree covers
/// them all. If several independent roots exist, the oldest one is returned.
pub fn find_root(name: &str) -> Option<u32> {
    let name = normalize_name(name);
    let procs = snapshot();
    let matching: HashMap<u32, &ProcInfo> = procs
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(&name))
        .map(|p| (p.pid, p))
        .collect();
    let mut roots: Vec<(u64, u32)> = Vec::new();
    for p in matching.values() {
        let ctime = creation_time(p.pid).unwrap_or(u64::MAX);
        let parent_is_same_app = matching.contains_key(&p.parent_pid)
            // guard against PID reuse: a real parent is older than its child
            && creation_time(p.parent_pid).is_some_and(|pc| pc <= ctime);
        if !parent_is_same_app {
            roots.push((ctime, p.pid));
        }
    }
    roots.sort();
    roots.first().map(|&(_, pid)| pid)
}
