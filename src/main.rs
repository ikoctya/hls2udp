#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod gui;
mod http;
mod icon;
mod logger;
mod manager;
mod service;
mod tray;
mod ts;
mod worker;

use config::{AppConfig, APP_VERSION, WINDOW_TITLE};
use gui::Hls2UdpGuiApp;
use manager::BroadcastManager;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

fn is_port_alive(port: u16) -> bool {
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

#[cfg(windows)]
fn acquire_single_instance_or_focus() -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };

    let name: Vec<u16> = "Global\\Hls2UdpGatewayV6Mutex"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if !handle.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            let title: Vec<u16> = WINDOW_TITLE
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
            if !hwnd.is_null() {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
            }
            return None;
        }
        Some(handle)
    }
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        logger::write_crash(&format!("PANIC (v{}): {}", APP_VERSION, info));
    }));

    let args: Vec<String> = std::env::args().collect();
    let mut force_service = false;
    let mut start_tray_only = false;

    for a in &args[1..] {
        match a.to_lowercase().as_str() {
            "--service" | "/service" | "-s" => force_service = true,
            "--tray" | "/tray" | "--minimized" => start_tray_only = true,
            "--install" | "/install" => {
                println!("{}", service::install_and_start_service());
                return;
            }
            "--uninstall" | "/uninstall" => {
                println!("{}", service::uninstall_service());
                return;
            }
            _ => {}
        }
    }

    if force_service {
        if let Err(e) = service::run_windows_service() {
            logger::write_crash(&format!("Service error: {}", e));
        }
        return;
    }

    // Защита от повторного запуска: если окно уже открыто или в трее — просто разворачиваем его
    #[cfg(windows)]
    let _instance_guard = match acquire_single_instance_or_focus() {
        Some(h) => h,
        None => return,
    };

    let cfg = AppConfig::load();
    let port = cfg.http_monitor_port;
    let service_running = is_port_alive(port);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let mgr = if service_running {
        None
    } else {
        let m = BroadcastManager::new(cfg.clone(), rt.handle().clone(), false);
        http::spawn_http_server(m.clone(), port, rt.handle());
        Some(m)
    };

    #[cfg(windows)]
    {
        let tray_mgr = mgr.clone();
        let tray_rt = rt.handle().clone();
        std::thread::Builder::new()
            .name("Hls2Udp-Tray".to_string())
            .spawn(move || {
                tray::win_tray::run_tray_loop(tray_mgr, Some(tray_rt), port, false);
            })
            .ok();
    }

    if start_tray_only || cfg.minimize_to_tray_on_start {
        if let Some(ref m) = mgr {
            m.start_all_enabled();
        }
        std::thread::park();
        return;
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([996.0, 704.0])
            .with_min_inner_size([940.0, 650.0])
            .with_title(WINDOW_TITLE)
            .with_icon(icon::app_icon_data()),
        ..Default::default()
    };

    let rt_handle = rt.handle().clone();
    let mgr_for_gui = mgr.clone();
    let _ = eframe::run_native(
        &format!("Hls2Udp v{}", APP_VERSION),
        options,
        Box::new(move |cc| Ok(Box::new(Hls2UdpGuiApp::new(cc, mgr_for_gui, rt_handle, cfg)))),
    );

    if let Some(ref m) = mgr {
        m.stop_all();
    }
}
