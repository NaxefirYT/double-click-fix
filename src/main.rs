#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering::Relaxed};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const APP: &str = "DoubleClickFix";
const SETTINGS_KEY: &str = "Software\\DoubleClickFix";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const WM_TRAY: u32 = WM_APP + 1;
const WM_REFRESH: u32 = WM_APP + 2;
const ID_ENABLE: usize = 100;
const ID_AUTOSTART: usize = 101;
const ID_EXIT: usize = 102;
const ID_THRESHOLD: usize = 200;
const THRESHOLDS: [u32; 7] = [20, 30, 40, 50, 60, 80, 100];
const UP_FLAGS: [u32; 3] = [MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_MIDDLEUP];

static ENABLED: AtomicBool = AtomicBool::new(true);
static THRESHOLD: AtomicU32 = AtomicU32::new(40);
static BLOCKED: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static WINDOW: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static HOOK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static ICON_ON: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static ICON_OFF: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static PENDING: [AtomicBool; 3] = [AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false)];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && ENABLED.load(Relaxed) {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        if info.flags & LLMHF_INJECTED == 0 {
            let button = match wparam as u32 {
                WM_LBUTTONDOWN => Some((0, true)),
                WM_LBUTTONUP => Some((0, false)),
                WM_RBUTTONDOWN => Some((1, true)),
                WM_RBUTTONUP => Some((1, false)),
                WM_MBUTTONDOWN => Some((2, true)),
                WM_MBUTTONUP => Some((2, false)),
                _ => None,
            };

            if let Some((index, down)) = button {
                let hwnd = WINDOW.load(Relaxed);
                if down {
                    if PENDING[index].swap(false, Relaxed) {
                        KillTimer(hwnd, index + 1);
                        BLOCKED.fetch_add(1, Relaxed);
                        PostMessageW(hwnd, WM_REFRESH, 0, 0);
                        return 1;
                    }
                } else {
                    PENDING[index].store(true, Relaxed);
                    SetTimer(hwnd, index + 1, THRESHOLD.load(Relaxed), None);
                    return 1;
                }
            }
        }
    }

    CallNextHookEx(null_mut(), code, wparam, lparam)
}

unsafe fn flush(index: usize) {
    KillTimer(WINDOW.load(Relaxed), index + 1);
    if !PENDING[index].swap(false, Relaxed) {
        return;
    }

    let mut input: INPUT = zeroed();
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi.dwFlags = UP_FLAGS[index];
    SendInput(1, &input, size_of::<INPUT>() as i32);
}

unsafe fn flush_all() {
    for index in 0..3 {
        flush(index);
    }
}

unsafe fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_TIP | NIF_ICON;
    let enabled = ENABLED.load(Relaxed);
    nid.hIcon = if enabled { ICON_ON.load(Relaxed) } else { ICON_OFF.load(Relaxed) };
    let tip = if enabled {
        format!("{APP}: {} мс, отсечено {}", THRESHOLD.load(Relaxed), BLOCKED.load(Relaxed))
    } else {
        format!("{APP}: выключен")
    };
    for (dst, src) in nid.szTip.iter_mut().take(127).zip(tip.encode_utf16()) {
        *dst = src;
    }
    nid
}

unsafe fn add_tray(hwnd: HWND) {
    let mut nid = tray_data(hwnd);
    nid.uFlags |= NIF_MESSAGE;
    nid.uCallbackMessage = WM_TRAY;
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn load_icon(instance: HINSTANCE, id: usize) -> HICON {
    LoadImageW(
        instance,
        id as *const u16,
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_DEFAULTCOLOR,
    )
}

unsafe fn refresh_tray(hwnd: HWND) {
    Shell_NotifyIconW(NIM_MODIFY, &tray_data(hwnd));
}

unsafe fn load_threshold() -> u32 {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = RegGetValueW(
        HKEY_CURRENT_USER,
        wide(SETTINGS_KEY).as_ptr(),
        wide("Threshold").as_ptr(),
        RRF_RT_REG_DWORD,
        null_mut(),
        &mut value as *mut u32 as *mut c_void,
        &mut size,
    );
    if status == ERROR_SUCCESS && value > 0 { value } else { 40 }
}

unsafe fn save_threshold(value: u32) {
    RegSetKeyValueW(
        HKEY_CURRENT_USER,
        wide(SETTINGS_KEY).as_ptr(),
        wide("Threshold").as_ptr(),
        REG_DWORD,
        &value as *const u32 as *const c_void,
        size_of::<u32>() as u32,
    );
}

unsafe fn is_autostart() -> bool {
    RegGetValueW(
        HKEY_CURRENT_USER,
        wide(RUN_KEY).as_ptr(),
        wide(APP).as_ptr(),
        RRF_RT_REG_SZ,
        null_mut(),
        null_mut(),
        null_mut(),
    ) == ERROR_SUCCESS
}

unsafe fn toggle_autostart() {
    if is_autostart() {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), wide(APP).as_ptr());
        return;
    }

    let Ok(exe) = std::env::current_exe() else { return };
    let command = wide(&format!("\"{}\"", exe.display()));
    RegSetKeyValueW(
        HKEY_CURRENT_USER,
        wide(RUN_KEY).as_ptr(),
        wide(APP).as_ptr(),
        REG_SZ,
        command.as_ptr() as *const c_void,
        (command.len() * 2) as u32,
    );
}

unsafe fn set_enabled(hwnd: HWND, value: bool) {
    if !value {
        flush_all();
    }
    ENABLED.store(value, Relaxed);
    refresh_tray(hwnd);
}

fn checked(value: bool) -> u32 {
    if value { MF_STRING | MF_CHECKED } else { MF_STRING }
}

unsafe fn show_menu(hwnd: HWND) {
    let threshold = THRESHOLD.load(Relaxed);
    let sub = CreatePopupMenu();
    for (i, value) in THRESHOLDS.iter().enumerate() {
        AppendMenuW(sub, checked(*value == threshold), ID_THRESHOLD + i, wide(&format!("{value} мс")).as_ptr());
    }

    let menu = CreatePopupMenu();
    AppendMenuW(menu, checked(ENABLED.load(Relaxed)), ID_ENABLE, wide("Включено").as_ptr());
    AppendMenuW(menu, MF_POPUP, sub as usize, wide(&format!("Порог: {threshold} мс")).as_ptr());
    AppendMenuW(menu, checked(is_autostart()), ID_AUTOSTART, wide("Автозапуск").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(
        menu,
        MF_STRING | MF_GRAYED,
        0,
        wide(&format!("Отсечено ложных кликов: {}", BLOCKED.load(Relaxed))).as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_EXIT, wide("Выход").as_ptr());

    let mut point: POINT = zeroed();
    GetCursorPos(&mut point);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if (1..=3).contains(&wparam) => flush(wparam - 1),
        WM_REFRESH => refresh_tray(hwnd),
        WM_TRAY => match lparam as u32 {
            WM_RBUTTONUP => show_menu(hwnd),
            WM_LBUTTONDBLCLK => set_enabled(hwnd, !ENABLED.load(Relaxed)),
            _ => {}
        },
        WM_COMMAND => match wparam & 0xFFFF {
            ID_ENABLE => set_enabled(hwnd, !ENABLED.load(Relaxed)),
            ID_AUTOSTART => toggle_autostart(),
            ID_EXIT => {
                DestroyWindow(hwnd);
            }
            id if (ID_THRESHOLD..ID_THRESHOLD + THRESHOLDS.len()).contains(&id) => {
                let value = THRESHOLDS[id - ID_THRESHOLD];
                THRESHOLD.store(value, Relaxed);
                save_threshold(value);
                refresh_tray(hwnd);
            }
            _ => {}
        },
        WM_DESTROY => {
            flush_all();
            UnhookWindowsHookEx(HOOK.load(Relaxed));
            let nid = tray_data(hwnd);
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
        }
        _ if msg != 0 && msg == TASKBAR_CREATED.load(Relaxed) => add_tray(hwnd),
        _ => return DefWindowProcW(hwnd, msg, wparam, lparam),
    }
    0
}

fn main() {
    unsafe {
        CreateMutexW(null(), 1, wide("DoubleClickFix_SingleInstance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        THRESHOLD.store(load_threshold(), Relaxed);
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Relaxed);

        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance = GetModuleHandleW(null());
        ICON_ON.store(load_icon(instance, 1), Relaxed);
        ICON_OFF.store(load_icon(instance, 2), Relaxed);
        let class = wide(APP);
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), instance, null());
        WINDOW.store(hwnd, Relaxed);
        add_tray(hwnd);

        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), instance, 0);
        if hook.is_null() {
            MessageBoxW(null_mut(), wide("Не удалось установить хук мыши.").as_ptr(), class.as_ptr(), MB_ICONERROR);
            DestroyWindow(hwnd);
            return;
        }
        HOOK.store(hook, Relaxed);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
