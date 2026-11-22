//! System-tray front end: pick which program's sound to exclude, switch between added
//! programs, pick the output device. Settings persist in
//! `%APPDATA%\Ausieve\config.toml`; capture resumes automatically on start.

#![windows_subsystem = "windows"]
// The binary is named Ausieve so Task Manager shows Ausieve.exe.
#![allow(non_snake_case)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ausieve::config::Config;
use ausieve::engine::{Engine, Event, Target};
use ausieve::procs::{self, file_name, normalize_path};
use ausieve::wasapi;
use windows::core::{w, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Media::Audio::eRender;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_INFOTIP_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const WM_ENGINE: u32 = WM_APP + 2;

const ID_NONE: usize = 1;
const ID_ADD: usize = 2;
const ID_EXIT: usize = 3;
const ID_OPEN_DIR: usize = 4;
const ID_PROGRAM: usize = 1000; // + index
const ID_REMOVE: usize = 2000; // + index
const ID_OUTPUT: usize = 3000; // + index

#[derive(Default)]
struct Status {
    output: Option<Result<String, String>>,
    target_pid: Option<u32>,
    capture_error: Option<String>,
}

struct App {
    hwnd: HWND,
    cfg: Config,
    cfg_path: PathBuf,
    engine: Option<Engine>,
    events: Arc<Mutex<VecDeque<Event>>>,
    status: Status,
    /// Output device names as listed in the last menu (index -> name).
    menu_outputs: Vec<String>,
    taskbar_created: u32,
    /// Tray icons from the exe's resources (`assets/app.rc`): normal, and shown while failing.
    icon_ok: HICON,
    icon_warn: HICON,
}

/// Loads icon resource `id` at the small-icon size the tray uses.
fn load_icon(hinst: HINSTANCE, id: u16, fallback: PCWSTR) -> HICON {
    unsafe {
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        match LoadImageW(Some(hinst), PCWSTR(id as usize as *const u16), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR) {
            Ok(h) => HICON(h.0),
            Err(_) => LoadIconW(None, fallback).unwrap_or_default(),
        }
    }
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut a| a.as_mut().map(f)))
}

fn target_of(cfg: &Config) -> Target {
    match &cfg.selected {
        Some(p) => Target::Path(p.clone()),
        None => Target::None,
    }
}

/// Menu/tooltip label for a program: file name, plus the folder if two share a name.
fn label(cfg: &Config, path: &str) -> String {
    let name = file_name(path);
    let dup = cfg.programs.iter().filter(|p| file_name(p).eq_ignore_ascii_case(name)).count() > 1;
    if dup {
        let dir = path.rsplit_once(['\\', '/']).map(|(d, _)| file_name(d)).unwrap_or("");
        format!("{name} ({dir})")
    } else {
        name.to_string()
    }
}

/// Copies `s` into a fixed UTF-16 buffer (truncating, always NUL-terminated).
fn fill<const N: usize>(dst: &mut [u16; N], s: &str) {
    let v: Vec<u16> = s.encode_utf16().take(N - 1).collect();
    dst[..v.len()].copy_from_slice(&v);
    dst[v.len()] = 0;
}

impl App {
    fn status_lines(&self) -> (String, String) {
        let excl = match &self.cfg.selected {
            None => "不排除任何程序（转发全部声音）".to_string(),
            Some(p) => match self.status.target_pid {
                Some(pid) => format!("排除 {}（运行中，PID {pid}）", label(&self.cfg, p)),
                None => format!("排除 {}（未运行，暂时转发全部声音）", label(&self.cfg, p)),
            },
        };
        let out = match &self.status.output {
            None => format!("输出：{}（正在打开…）", self.cfg.output),
            Some(Ok(d)) => format!("输出：{d}"),
            Some(Err(e)) => format!("输出错误：{e}（自动重试中）"),
        };
        (excl, out)
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        };
        nid.uCallbackMessage = WM_TRAY;
        nid
    }

    fn refresh_icon(&self, add: bool) {
        let mut nid = self.nid();
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        let bad = matches!(self.status.output, Some(Err(_))) || self.status.capture_error.is_some();
        nid.hIcon = if bad { self.icon_warn } else { self.icon_ok };
        let (a, b) = self.status_lines();
        fill(&mut nid.szTip, &format!("Ausieve\n{a}\n{b}"));
        unsafe {
            let _ = Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &nid);
        }
    }

    fn notify(&self, title: &str, text: &str, warn: bool) {
        let mut nid = self.nid();
        nid.uFlags = NIF_INFO;
        fill(&mut nid.szInfoTitle, title);
        fill(&mut nid.szInfo, text);
        nid.dwInfoFlags = if warn { NIIF_WARNING } else { NOTIFY_ICON_INFOTIP_FLAGS(NIIF_INFO.0) };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn save(&self) {
        if let Err(e) = self.cfg.save(&self.cfg_path) {
            self.notify("无法保存设置", &e, true);
        }
    }

    fn on_engine_events(&mut self) {
        let evs: Vec<Event> = self.events.lock().unwrap().drain(..).collect();
        for ev in evs {
            match ev {
                Event::Output(r) => {
                    let was_err = matches!(self.status.output, Some(Err(_)));
                    match &r {
                        Err(e) => self.notify("输出设备不可用", &format!("{e}\n每 2 秒自动重试。"), true),
                        Ok(d) if was_err => self.notify("输出设备已恢复", d, false),
                        Ok(_) => {}
                    }
                    self.status.output = Some(r);
                }
                Event::Target(p) => self.status.target_pid = p,
                Event::Running => self.status.capture_error = None,
                Event::CaptureError(e) => {
                    if self.status.capture_error.as_ref() != Some(&e) {
                        self.notify("无法采集系统声音", &format!("{e}\n将自动重试。"), true);
                    }
                    self.status.capture_error = Some(e);
                }
            }
        }
        self.refresh_icon(false);
    }

    fn apply_selection(&mut self) {
        self.save();
        if let Some(e) = &self.engine {
            e.set_target(target_of(&self.cfg));
        }
        self.status.target_pid = None;
        self.refresh_icon(false);
    }

    fn build_menu(&mut self) -> windows::core::Result<HMENU> {
        unsafe {
            let m = CreatePopupMenu()?;
            // Normal status is in the tooltip; the menu only surfaces an output error.
            if let Some(Err(_)) = &self.status.output {
                let (_, out) = self.status_lines();
                AppendMenuW(m, MF_STRING | MF_GRAYED, 0, &HSTRING::from(out))?;
                AppendMenuW(m, MF_SEPARATOR, 0, None)?;
            }
            AppendMenuW(m, MF_STRING, ID_NONE, w!("不排除"))?;
            let mut checked = ID_NONE;
            for (i, p) in self.cfg.programs.iter().enumerate() {
                let selected = self.cfg.selected.as_deref().is_some_and(|s| normalize_path(s) == normalize_path(p));
                let running = if selected { self.status.target_pid.is_some() } else { procs::find_root_by_path(p).is_some() };
                let missing = !procs::program_exists(p);
                let mut text = label(&self.cfg, p);
                if missing {
                    text.push_str("（文件不存在）");
                } else if !running {
                    text.push_str("（未运行）");
                }
                AppendMenuW(m, MF_STRING, ID_PROGRAM + i, &HSTRING::from(text))?;
                if selected {
                    checked = ID_PROGRAM + i;
                }
            }
            set_radio(m, checked);
            AppendMenuW(m, MF_SEPARATOR, 0, None)?;
            AppendMenuW(m, MF_STRING, ID_ADD, w!("添加程序…"))?;

            let rm = CreatePopupMenu()?;
            if self.cfg.programs.is_empty() {
                AppendMenuW(rm, MF_STRING | MF_GRAYED, 0, w!("（列表为空）"))?;
            }
            for (i, p) in self.cfg.programs.iter().enumerate() {
                AppendMenuW(rm, MF_STRING, ID_REMOVE + i, &HSTRING::from(p.as_str()))?;
            }
            AppendMenuW(m, MF_POPUP, rm.0 as usize, w!("移除程序"))?;

            let om = CreatePopupMenu()?;
            self.menu_outputs = wasapi::list_devices(eRender).map(|d| d.into_iter().map(|(n, _)| n).collect()).unwrap_or_default();
            if !self.menu_outputs.iter().any(|n| n.eq_ignore_ascii_case(&self.cfg.output)) {
                self.menu_outputs.push(self.cfg.output.clone()); // shown as "not found"
            }
            for (i, n) in self.menu_outputs.iter().enumerate() {
                let present = i + 1 < self.menu_outputs.len() || wasapi::find_device(n, eRender).is_ok();
                let text = if present { n.clone() } else { format!("{n}（未找到）") };
                AppendMenuW(om, MF_STRING, ID_OUTPUT + i, &HSTRING::from(text))?;
                if n.eq_ignore_ascii_case(&self.cfg.output) {
                    set_radio(om, ID_OUTPUT + i);
                }
            }
            AppendMenuW(m, MF_POPUP, om.0 as usize, w!("输出设备"))?;

            AppendMenuW(m, MF_STRING, ID_OPEN_DIR, w!("打开配置文件夹"))?;
            AppendMenuW(m, MF_SEPARATOR, 0, None)?;
            AppendMenuW(m, MF_STRING, ID_EXIT, w!("退出"))?;
            Ok(m)
        }
    }
}

/// Marks one item with a radio bullet (MFT_RADIOCHECK + MFS_CHECKED).
fn set_radio(menu: HMENU, id: usize) {
    unsafe {
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE,
            ..Default::default()
        };
        if GetMenuItemInfoW(menu, id as u32, false, &mut mii).is_ok() {
            mii.fType |= MFT_RADIOCHECK;
            mii.fState |= MFS_CHECKED;
            let _ = SetMenuItemInfoW(menu, id as u32, false, &mii);
        }
    }
}

fn pick_exe(owner: HWND) -> Option<String> {
    let mut buf = vec![0u16; 32768];
    let filter: Vec<u16> = "程序 (*.exe)\0*.exe\0所有文件\0*.*\0\0".encode_utf16().collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        // (owner is our hidden window; the dialog itself is a normal top-level window)
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: w!("选择要排除声音的程序"),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    unsafe {
        if GetOpenFileNameW(&mut ofn).as_bool() {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            Some(String::from_utf16_lossy(&buf[..len]))
        } else {
            None
        }
    }
}

fn show_menu(hwnd: HWND) {
    let Some(Ok(menu)) = with_app(|a| a.build_menu()) else { return };
    let cmd = unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        // no app borrow is held here: the menu's modal loop dispatches our messages
        let c = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        c.0 as usize
    };
    on_command(hwnd, cmd);
}

fn on_command(hwnd: HWND, cmd: usize) {
    match cmd {
        0 => {}
        ID_EXIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        ID_ADD => {
            if let Some(path) = pick_exe(hwnd) {
                with_app(|a| {
                    let stored = a.cfg.add_program(&path);
                    // adding a program also selects it; switch back via the menu
                    a.cfg.select(Some(&stored));
                    a.apply_selection();
                });
            }
        }
        ID_NONE => {
            with_app(|a| {
                a.cfg.select(None);
                a.apply_selection();
            });
        }
        ID_OPEN_DIR => {
            with_app(|a| {
                if let Some(dir) = a.cfg_path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                    unsafe {
                        ShellExecuteW(Some(hwnd), w!("open"), &HSTRING::from(dir.as_os_str()), None, None, SW_SHOWNORMAL);
                    }
                }
            });
        }
        c if (ID_PROGRAM..ID_REMOVE).contains(&c) => {
            with_app(|a| {
                if let Some(p) = a.cfg.programs.get(c - ID_PROGRAM).cloned() {
                    a.cfg.select(Some(&p));
                    a.apply_selection();
                }
            });
        }
        c if (ID_REMOVE..ID_OUTPUT).contains(&c) => {
            with_app(|a| {
                if let Some(p) = a.cfg.programs.get(c - ID_REMOVE).cloned() {
                    a.cfg.remove_program(&p);
                    a.apply_selection();
                }
            });
        }
        c if c >= ID_OUTPUT => {
            with_app(|a| {
                if let Some(n) = a.menu_outputs.get(c - ID_OUTPUT).cloned() {
                    if n != a.cfg.output {
                        a.cfg.output = n.clone();
                        a.save();
                        a.status.output = None;
                        if let Some(e) = &a.engine {
                            e.set_output(n);
                        }
                        a.refresh_icon(false);
                    }
                }
            });
        }
        _ => {}
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let ev = (lp.0 & 0xFFFF) as u32;
            if ev == WM_RBUTTONUP || ev == WM_LBUTTONUP || ev == WM_CONTEXTMENU {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        // Same IDs as the menu items; lets scripts/tests drive the app without the menu.
        WM_COMMAND => {
            on_command(hwnd, wp.0 & 0xFFFF);
            LRESULT(0)
        }
        WM_ENGINE => {
            with_app(|a| a.on_engine_events());
            LRESULT(0)
        }
        WM_DESTROY => {
            with_app(|a| {
                let nid = a.nid();
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                }
                a.engine = None; // stops and joins the capture thread
            });
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            // Explorer restarted: the icon must be added again.
            if with_app(|a| msg == a.taskbar_created && msg != 0) == Some(true) {
                with_app(|a| a.refresh_icon(true));
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
    }
}

fn message_box(text: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), w!("Ausieve"), MB_OK | MB_ICONWARNING);
    }
}

fn main() {
    unsafe {
        // One instance only: two would fight over the exclusive output device.
        let _mutex = CreateMutexW(None, true, w!("Local\\ausieve-tray"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            message_box("Ausieve 已经在运行（见系统托盘）。");
            return;
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let cfg_path = match std::env::var_os("AUSIEVE_CONFIG") {
            Some(p) => PathBuf::from(p),
            None => Config::default_path(),
        };
        let (cfg, load_err) = match Config::load(&cfg_path) {
            Ok(c) => (c, None),
            Err(e) => {
                // keep the broken file for the user, start with defaults
                let bad = cfg_path.with_extension("toml.bad");
                let _ = std::fs::rename(&cfg_path, &bad);
                (Config::default(), Some(format!("{e}\n已改名为 {}，使用默认设置。", bad.display())))
            }
        };

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("ausieve-tray");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Ausieve"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            // A hidden top-level window, not HWND_MESSAGE: the popup menu needs a window
            // that can become the foreground window, or it won't close on outside clicks.
            None,
            None,
            Some(hinst.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                message_box(&format!("无法创建窗口：{e}"));
                return;
            }
        };
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));

        let events: Arc<Mutex<VecDeque<Event>>> = Arc::default();
        let app = App {
            hwnd,
            cfg_path,
            engine: None,
            events: events.clone(),
            status: Status::default(),
            menu_outputs: Vec::new(),
            taskbar_created,
            icon_ok: load_icon(hinst.into(), 1, IDI_APPLICATION),
            icon_warn: load_icon(hinst.into(), 2, IDI_WARNING),
            cfg,
        };
        let (target, output, buffer_ms, dither) = (target_of(&app.cfg), app.cfg.output.clone(), app.cfg.buffer_ms(), app.cfg.silence_dither);
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(|a| {
            a.refresh_icon(true);
            if let Some(e) = &load_err {
                a.notify("设置文件损坏", e, true);
            }
        });

        let hwnd_raw = hwnd.0 as isize;
        let engine = Engine::start_with(target, output, buffer_ms, move |ev| {
            events.lock().unwrap().push_back(ev);
            let _ = PostMessageW(Some(HWND(hwnd_raw as _)), WM_ENGINE, WPARAM(0), LPARAM(0));
        });
        engine.set_dither(dither);
        with_app(|a| a.engine = Some(engine));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
