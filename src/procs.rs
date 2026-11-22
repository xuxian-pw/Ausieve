//! Process enumeration helpers.

use std::collections::HashSet;

use windows::Win32::Foundation::{CloseHandle, FILETIME, WAIT_TIMEOUT};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::core::PWSTR;
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
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

/// Full path of a process's executable, if it can be queried.
pub fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Canonical form for comparing Windows paths: `/` becomes `\`, the `\\?\` prefix and
/// trailing separators are removed, and case is folded.
pub fn normalize_path(p: &str) -> String {
    let mut s = p.trim().replace('/', "\\");
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        s = rest.to_string();
    }
    while s.ends_with('\\') {
        s.pop();
    }
    s.to_lowercase()
}

/// Expands `%NAME%` environment variables. Unknown names and an unpaired `%` stay as
/// written.
pub fn expand_env(p: &str) -> String {
    let mut out = String::new();
    let mut rest = p;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) if j > 0 => {
                let name = &after[..j];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn has_wildcard(s: &str) -> bool {
    s.contains(['*', '?'])
}

/// Case-insensitive glob match of one path segment: `*` = any run of characters,
/// `?` = one character.
fn segment_matches(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let s: Vec<char> = s.to_lowercase().chars().collect();
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None; // (pattern index after '*', s index it took)
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi + 1, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// True if the executable `path` is the program `pattern` (as stored in the config): an
/// exact path, or one with `*` / `?` inside folder or file names (they never span a
/// `\`), e.g. `%LOCALAPPDATA%\Discord\app-*\Discord.exe` for apps that move to a new
/// version folder on every update. `%NAME%` variables are expanded.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let pat = normalize_path(&expand_env(pattern));
    let path = normalize_path(path);
    if !has_wildcard(&pat) {
        return pat == path;
    }
    let ps: Vec<&str> = pat.split('\\').collect();
    let xs: Vec<&str> = path.split('\\').collect();
    ps.len() == xs.len() && ps.iter().zip(&xs).all(|(p, x)| segment_matches(p, x))
}

/// True if at least one file matches the program `pattern` (see [`path_matches`]).
pub fn program_exists(pattern: &str) -> bool {
    let p = expand_env(pattern);
    if !has_wildcard(&p) {
        return std::path::Path::new(&p).exists();
    }
    let norm = normalize_path(&p);
    let mut segs = norm.split('\\');
    let root = segs.next().unwrap_or("");
    if root.is_empty() || has_wildcard(root) {
        return true; // UNC path or wildcard drive: not worth checking, assume it is there
    }
    let mut cands = vec![std::path::PathBuf::from(format!("{root}\\"))];
    for seg in segs {
        let mut next = Vec::new();
        for c in &cands {
            if has_wildcard(seg) {
                let Ok(rd) = std::fs::read_dir(c) else { continue };
                for e in rd.flatten() {
                    if segment_matches(seg, &e.file_name().to_string_lossy()) {
                        next.push(e.path());
                    }
                }
            } else {
                let j = c.join(seg);
                if j.exists() {
                    next.push(j);
                }
            }
        }
        if next.is_empty() {
            return false;
        }
        cands = next;
    }
    true
}

/// File-name part of a path (`C:\a\b.exe` -> `b.exe`).
pub fn file_name(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

/// Picks the root among `matching` (the PIDs of one program): a process whose parent is
/// not itself one of `matching`, or is but started later (PID reuse, not a real parent).
/// If there are several independent roots, the oldest one wins.
pub fn pick_root(procs: &[ProcInfo], matching: &HashSet<u32>, ctime: impl Fn(u32) -> Option<u64>) -> Option<u32> {
    let mut roots: Vec<(u64, u32)> = Vec::new();
    for p in procs.iter().filter(|p| matching.contains(&p.pid)) {
        let t = ctime(p.pid).unwrap_or(u64::MAX);
        let parent_is_same_app = matching.contains(&p.parent_pid) && ctime(p.parent_pid).is_some_and(|pc| pc <= t);
        if !parent_is_same_app {
            roots.push((t, p.pid));
        }
    }
    roots.sort();
    roots.first().map(|&(_, pid)| pid)
}

/// Finds the *root* process for an executable name: a process with that name whose
/// parent is not also a (live, older) process with that name. Multi-process apps such
/// as Discord/Chrome play audio from child processes; excluding the root's tree covers
/// them all.
pub fn find_root(name: &str) -> Option<u32> {
    let name = normalize_name(name);
    let procs = snapshot();
    let matching: HashSet<u32> = procs.iter().filter(|p| p.name.eq_ignore_ascii_case(&name)).map(|p| p.pid).collect();
    pick_root(&procs, &matching, creation_time)
}

/// Like [`find_root`], but matches the full executable path, so two different programs
/// that happen to share a file name are told apart. `path` may be a pattern (see
/// [`path_matches`]).
pub fn find_root_by_path(path: &str) -> Option<u32> {
    let expanded = normalize_path(&expand_env(path));
    let name = file_name(&expanded);
    let procs = snapshot();
    let matching: HashSet<u32> = procs
        .iter()
        // cheap name filter first; the full path needs an OpenProcess per candidate
        .filter(|p| segment_matches(name, &p.name))
        .filter(|p| process_path(p.pid).is_some_and(|pp| path_matches(path, &pp)))
        .map(|p| p.pid)
        .collect();
    pick_root(&procs, &matching, creation_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent_pid: u32) -> ProcInfo {
        ProcInfo { pid, parent_pid, name: "app.exe".into() }
    }

    #[test]
    fn normalize_path_variants() {
        let a = normalize_path(r"C:\Program Files\App\App.EXE");
        assert_eq!(a, normalize_path("c:/program files/app/app.exe"));
        assert_eq!(a, normalize_path(r"\\?\C:\Program Files\App\app.exe"));
        assert_eq!(a, normalize_path(" C:\\Program Files\\App\\app.exe\\ "));
        assert_ne!(a, normalize_path(r"C:\Other\App\app.exe"));
        assert_eq!(file_name(r"C:\x\Discord.exe"), "Discord.exe");
        assert_eq!(file_name("Discord.exe"), "Discord.exe");
    }

    #[test]
    fn path_patterns() {
        let d = r"%LOCALAPPDATA%\Discord\app-*\Discord.exe";
        let base = std::env::var("LOCALAPPDATA").unwrap();
        assert!(path_matches(d, &format!(r"{base}\Discord\app-1.0.9269\Discord.exe")));
        assert!(path_matches(d, &format!(r"{base}\DISCORD\app-1.0.9270\discord.EXE")));
        assert!(!path_matches(d, &format!(r"{base}\Discord\Update.exe")), "wrong depth");
        assert!(!path_matches(d, &format!(r"{base}\Discord\app-1\x\Discord.exe")), "* does not span folders");
        assert!(!path_matches(d, &format!(r"{base}\DiscordPTB\app-1.0.1\Discord.exe")));
        assert!(!path_matches(d, r"C:\Other\Discord\app-1.0.9269\Discord.exe"));
        assert!(path_matches(r"C:\A\v?.?\x*.exe", r"c:\a\v1.2\xyz.exe"));
        assert!(!path_matches(r"C:\A\v?.?\x*.exe", r"c:\a\v10.2\xyz.exe"));
        // no wildcard: exact, as before
        assert!(path_matches(r"C:\A\a.exe", r"\\?\c:/a/A.EXE"));
        assert!(!path_matches(r"C:\A\a.exe", r"C:\B\a.exe"));
        assert!(segment_matches("*", "") && segment_matches("a*b*c", "aXbYbc") && !segment_matches("a*b", "ab_"));
    }

    #[test]
    fn env_expansion() {
        let windir = std::env::var("WINDIR").unwrap();
        assert_eq!(expand_env(r"%windir%\x"), format!(r"{windir}\x"));
        assert_eq!(expand_env(r"%NO_SUCH_VAR_AUSIEVE%\x"), r"%NO_SUCH_VAR_AUSIEVE%\x");
        assert_eq!(expand_env("100% sure %%"), "100% sure %%");
    }

    #[test]
    fn program_exists_with_patterns() {
        let d = std::env::temp_dir().join(format!("ausieve-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("app-1.0.1")).unwrap();
        std::fs::write(d.join("app-1.0.1").join("Tool.exe"), b"").unwrap();
        let ds = d.to_str().unwrap();
        assert!(program_exists(&format!(r"{ds}\app-*\Tool.exe")));
        assert!(program_exists(&format!(r"{ds}\app-1.0.1\Tool.exe")));
        assert!(!program_exists(&format!(r"{ds}\app-*\Other.exe")));
        assert!(!program_exists(&format!(r"{ds}\v*\Tool.exe")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn root_is_the_process_whose_parent_is_not_the_same_app() {
        // 10 (launcher) -> 20 (app) -> 21 -> 22 (app children); 30 (app, second root)
        let procs = vec![p(10, 1), p(20, 10), p(21, 20), p(22, 21), p(30, 10)];
        let matching: HashSet<u32> = [20, 21, 22, 30].into();
        let ctime = |pid: u32| Some(pid as u64); // lower pid = started earlier here
        assert_eq!(pick_root(&procs, &matching, ctime), Some(20));
        // a "parent" 20 that started after its child 21 is PID reuse, not the parent
        let reuse = |pid: u32| Some(if pid == 20 { 100 } else { pid as u64 });
        assert_eq!(pick_root(&procs, &matching, reuse), Some(21));
        assert_eq!(pick_root(&procs, &HashSet::new(), ctime), None);
    }

    #[test]
    fn finds_own_process_by_path() {
        let me = std::env::current_exe().unwrap();
        let pid = std::process::id();
        assert_eq!(process_path(pid).map(|p| normalize_path(&p)), Some(normalize_path(me.to_str().unwrap())));
        // the test binary has no same-path parent, so it is its own root
        assert_eq!(find_root_by_path(me.to_str().unwrap()), Some(pid));
        assert_eq!(find_root_by_path(r"C:\definitely\not\here\x.exe"), None);
        // the same exe through a pattern on its folder and file name
        let dir = me.parent().unwrap();
        let stem = me.file_stem().unwrap().to_str().unwrap();
        let pat = format!(r"{}\*\{}*.exe", dir.parent().unwrap().display(), &stem[..3]);
        assert_eq!(find_root_by_path(&pat), Some(pid), "{pat}");
    }
}
