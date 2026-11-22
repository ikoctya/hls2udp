#[cfg(windows)]
pub mod win_tray {
    use crate::config::APP_VERSION;
    use crate::logger;
    use crate::manager::{BroadcastManager, ManagerTelemetry};
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU16, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
        NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer,
        PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer, ShowWindow,
        TrackPopupMenu, TranslateMessage, HICON, ICONINFO, MF_SEPARATOR, MF_STRING, MSG, SW_HIDE,
        SW_RESTORE, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_LEFTALIGN, WM_APP, WM_CLOSE, WM_COMMAND,
        WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
    };

    const WM_TRAY: u32 = WM_APP + 1;
    const TIMER_STATUS_ID: usize = 1;
    const CMD_OPEN_APP: usize = 1001;
    const CMD_OPEN_WEB: usize = 1002;
    const CMD_START_ALL: usize = 1003;
    const CMD_STOP_ALL: usize = 1004;
    const CMD_OPEN_LOG: usize = 1005;
    const CMD_EXIT: usize = 1006;

    pub static GUI_HWND: AtomicIsize = AtomicIsize::new(0);
    pub static ALLOW_EXIT: AtomicBool = AtomicBool::new(false);

    static TRAY_PORT: AtomicU16 = AtomicU16::new(8088);
    static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
    static LAST_STATE: AtomicU8 = AtomicU8::new(255);

    struct TrayRuntime {
        mgr: Option<Arc<BroadcastManager>>,
        rt: Option<tokio::runtime::Handle>,
        cached_tel: Arc<Mutex<Option<ManagerTelemetry>>>,
        icon_gray: isize,
        icon_green: isize,
        icon_yellow: isize,
        icon_red: isize,
    }

    static TRAY_RT: OnceLock<Mutex<TrayRuntime>> = OnceLock::new();

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn set_gui_hwnd(hwnd: isize) {
        GUI_HWND.store(hwnd, Ordering::SeqCst);
    }

    pub fn hide_gui_to_tray() {
        let h = GUI_HWND.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                ShowWindow(h as HWND, SW_HIDE);
            }
        }
    }

    pub fn restore_gui_window() -> bool {
        let h = GUI_HWND.load(Ordering::SeqCst);
        if h != 0 {
            unsafe {
                let hwnd = h as HWND;
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
            return true;
        }
        false
    }

    pub fn open_dashboard(port: u16) {
        let url = wide(&format!("http://localhost:{}", port));
        let verb = wide("open");
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                url.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    pub fn open_log_file() {
        let p = wide(&logger::get_log_file_path().to_string_lossy());
        let verb = wide("open");
        unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                p.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    unsafe fn make_dot_icon(r: u8, g: u8, b: u8) -> HICON {
        const SIZE: i32 = 16;
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = SIZE;
        bmi.bmiHeader.biHeight = -SIZE;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hdc = GetDC(std::ptr::null_mut());
        let hbm_color = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        ReleaseDC(std::ptr::null_mut(), hdc);

        if !bits.is_null() {
            let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize);
            let cx = 7.5_f32;
            let cy = 7.5_f32;
            for y in 0..SIZE {
                for x in 0..SIZE {
                    let dx = x as f32 - cx;
                    let dy = y as f32 - cy;
                    let dist = (dx * dx + dy * dy).sqrt();
                    let px = if dist <= 5.4 {
                        (0xFF_u32 << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
                    } else if dist <= 7.0 {
                        0xFF_FF_FF_FF
                    } else {
                        0x00_00_00_00
                    };
                    pixels[(y * SIZE + x) as usize] = px;
                }
            }
        }

        let mask_bytes = [0u8; 64];
        let hbm_mask = CreateBitmap(SIZE, SIZE, 1, 1, mask_bytes.as_ptr() as *const _);
        let ii = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let icon = CreateIconIndirect(&ii);
        if !hbm_color.is_null() {
            DeleteObject(hbm_color);
        }
        if !hbm_mask.is_null() {
            DeleteObject(hbm_mask);
        }
        icon
    }

    unsafe fn update_tray_status() {
        let hwnd_raw = TRAY_HWND.load(Ordering::Relaxed);
        if hwnd_raw == 0 {
            return;
        }
        let (tel_opt, icons) = match TRAY_RT.get().and_then(|m| m.try_lock().ok()) {
            Some(guard) => {
                let tel = if let Some(ref mgr) = guard.mgr {
                    Some(mgr.telemetry())
                } else {
                    guard.cached_tel.try_lock().ok().and_then(|c| c.clone())
                };
                (tel, (guard.icon_gray, guard.icon_green, guard.icon_yellow, guard.icon_red))
            }
            None => return,
        };

        let tel = match tel_opt {
            Some(t) => t,
            None => return,
        };

        let mut running_cnt = 0usize;
        let mut ok_cnt = 0usize;
        let mut warn_cnt = 0usize;
        let mut err_cnt = 0usize;
        let mut total_out = 0.0_f64;

        for c in &tel.channels {
            if c.is_running {
                running_cnt += 1;
                total_out += c.output_bitrates_mbps;
                if c.input_alive && c.output_alive && !c.using_backup {
                    ok_cnt += 1;
                } else if c.input_alive && c.using_backup {
                    warn_cnt += 1;
                } else {
                    err_cnt += 1;
                }
            }
        }

        let (state_code, icon_raw) = if running_cnt == 0 {
            (0u8, icons.0)
        } else if err_cnt > 0 {
            (3u8, icons.3)
        } else if warn_cnt > 0 {
            (2u8, icons.2)
        } else {
            (1u8, icons.1)
        };

        let tip_str = format!(
            "Hls2Udp v{}: {}/{} OK | Out: {:.1}M | CPU: {:.1}%",
            APP_VERSION, ok_cnt, running_cnt, total_out, tel.cpu_process
        );
        let tip = wide(&tip_str);

        let prev_state = LAST_STATE.load(Ordering::Relaxed);
        let change_icon = state_code != prev_state && icon_raw != 0;

        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd_raw as HWND;
        nid.uID = 1;
        nid.uFlags = NIF_TIP | if change_icon { NIF_ICON } else { 0 };
        if change_icon {
            nid.hIcon = icon_raw as HICON;
            LAST_STATE.store(state_code, Ordering::Relaxed);
        }
        let copy_len = tip.len().min(nid.szTip.len().saturating_sub(1));
        nid.szTip[..copy_len].copy_from_slice(&tip[..copy_len]);
        Shell_NotifyIconW(NIM_MODIFY, &nid);
    }

    fn trigger_remote_post(path: &'static str) {
        let port = TRAY_PORT.load(Ordering::Relaxed);
        if let Some(guard) = TRAY_RT.get().and_then(|m| m.try_lock().ok()) {
            if let Some(ref m) = guard.mgr {
                if path == "/api/start_all" {
                    m.start_all_enabled();
                } else if path == "/api/stop_all" {
                    m.stop_all();
                }
                return;
            }
            if let Some(ref rt) = guard.rt {
                let host = std::net::Ipv4Addr::LOCALHOST;
                let url = format!("http://{}:{}{}", host, port, path);
                rt.spawn(async move {
                    let _ = reqwest::Client::new().post(&url).send().await;
                });
            }
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TIMER => {
                if wparam == TIMER_STATUS_ID {
                    update_tray_status();
                }
                0
            }
            WM_TRAY => {
                let event = (lparam & 0xFFFF) as u32;
                let port = TRAY_PORT.load(Ordering::Relaxed);
                if event == WM_LBUTTONDBLCLK || event == WM_LBUTTONUP {
                    if !restore_gui_window() {
                        open_dashboard(port);
                    }
                } else if event == WM_RBUTTONUP {
                    let hmenu = CreatePopupMenu();
                    if !hmenu.is_null() {
                        let s_app = wide("Открыть окно приложения");
                        let s_web = wide(&format!("Открыть веб-панель (: {})", port));
                        let s_start = wide("Запустить все каналы");
                        let s_stop = wide("Остановить все");
                        let s_log = wide("Открыть файл лога");
                        let s_exit = wide("Выход");

                        AppendMenuW(hmenu, MF_STRING, CMD_OPEN_APP, s_app.as_ptr());
                        AppendMenuW(hmenu, MF_STRING, CMD_OPEN_WEB, s_web.as_ptr());
                        AppendMenuW(hmenu, MF_SEPARATOR, 0, std::ptr::null());
                        AppendMenuW(hmenu, MF_STRING, CMD_START_ALL, s_start.as_ptr());
                        AppendMenuW(hmenu, MF_STRING, CMD_STOP_ALL, s_stop.as_ptr());
                        AppendMenuW(hmenu, MF_STRING, CMD_OPEN_LOG, s_log.as_ptr());
                        AppendMenuW(hmenu, MF_SEPARATOR, 0, std::ptr::null());
                        AppendMenuW(hmenu, MF_STRING, CMD_EXIT, s_exit.as_ptr());

                        let mut pt = POINT { x: 0, y: 0 };
                        GetCursorPos(&mut pt);
                        SetForegroundWindow(hwnd);
                        TrackPopupMenu(
                            hmenu,
                            TPM_LEFTALIGN | TPM_BOTTOMALIGN,
                            pt.x,
                            pt.y,
                            0,
                            hwnd,
                            std::ptr::null(),
                        );
                        DestroyMenu(hmenu);
                    }
                }
                0
            }
            WM_COMMAND => {
                let cmd = wparam & 0xFFFF;
                let port = TRAY_PORT.load(Ordering::Relaxed);
                match cmd {
                    CMD_OPEN_APP => {
                        if !restore_gui_window() {
                            if let Ok(exe) = std::env::current_exe() {
                                let _ = std::process::Command::new(exe).spawn();
                            }
                        }
                    }
                    CMD_OPEN_WEB => open_dashboard(port),
                    CMD_START_ALL => trigger_remote_post("/api/start_all"),
                    CMD_STOP_ALL => trigger_remote_post("/api/stop_all"),
                    CMD_OPEN_LOG => open_log_file(),
                    CMD_EXIT => {
                        ALLOW_EXIT.store(true, Ordering::SeqCst);
                        let gh = GUI_HWND.load(Ordering::SeqCst);
                        if gh != 0 {
                            PostMessageW(gh as HWND, WM_CLOSE, 0, 0);
                        }
                        PostQuitMessage(0);
                    }
                    _ => {}
                }
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, TIMER_STATUS_ID);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    pub fn run_tray_loop(
        mgr: Option<Arc<BroadcastManager>>,
        rt: Option<tokio::runtime::Handle>,
        port: u16,
        open_ui_on_start: bool,
    ) {
        TRAY_PORT.store(port, Ordering::SeqCst);
        let cached_tel = Arc::new(Mutex::new(None));

        // В режиме службы опрашиваем телеметрию в фоне без блокировки Win32 message loop
        if mgr.is_none() {
            if let Some(ref handle) = rt {
                let cache = cached_tel.clone();
                handle.spawn(async move {
                    let client = match reqwest::Client::builder()
                        .timeout(std::time::Duration::from_millis(350))
                        .build()
                    {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let host = std::net::Ipv4Addr::LOCALHOST;
                    let url = format!("http://{}:{}/status", host, port);
                    loop {
                        if let Ok(resp) = client.get(&url).send().await {
                            if let Ok(txt) = resp.text().await {
                                if let Ok(tel) = serde_json::from_str::<ManagerTelemetry>(&txt) {
                                    if let Ok(mut g) = cache.lock() {
                                        *g = Some(tel);
                                    }
                                }
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    }
                });
            }
        }

        unsafe {
            let ic_gray = make_dot_icon(148, 163, 184);
            let ic_green = make_dot_icon(16, 185, 129);
            let ic_yellow = make_dot_icon(245, 158, 11);
            let ic_red = make_dot_icon(239, 68, 68);

            let _ = TRAY_RT.set(Mutex::new(TrayRuntime {
                mgr,
                rt,
                cached_tel,
                icon_gray: ic_gray as isize,
                icon_green: ic_green as isize,
                icon_yellow: ic_yellow as isize,
                icon_red: ic_red as isize,
            }));

            let hinst = GetModuleHandleW(std::ptr::null());
            let cls_name = wide("Hls2UdpTrayClass");
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: ic_gray,
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: cls_name.as_ptr(),
            };
            RegisterClassW(&wc);

            let title = wide("Hls2Udp Tray");
            let hwnd = CreateWindowExW(
                0,
                cls_name.as_ptr(),
                title.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinst,
                std::ptr::null(),
            );
            TRAY_HWND.store(hwnd as isize, Ordering::SeqCst);

            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_TRAY;
            nid.hIcon = ic_gray;
            LAST_STATE.store(0, Ordering::Relaxed);
            let tip = wide(&format!("Hls2Udp v{} — Остановлен", APP_VERSION));
            let copy_len = tip.len().min(nid.szTip.len().saturating_sub(1));
            nid.szTip[..copy_len].copy_from_slice(&tip[..copy_len]);

            Shell_NotifyIconW(NIM_ADD, &nid);
            SetTimer(hwnd, TIMER_STATUS_ID, 1000, None);

            if open_ui_on_start {
                open_dashboard(port);
            }

            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            Shell_NotifyIconW(NIM_DELETE, &nid);
            for ic in [ic_gray, ic_green, ic_yellow, ic_red] {
                if !ic.is_null() {
                    DestroyIcon(ic);
                }
            }
        }
    }
}
