use crate::config::{AppConfig, APP_VERSION};
use crate::http;
use crate::logger;
use crate::manager::BroadcastManager;
use std::ffi::OsString;
use std::time::Duration;

pub const SERVICE_NAME: &str = "Hls2UdpService";
pub const DISPLAY_NAME: &str = "HLS to UDP/Multicast Broadcast Gateway v6 (Rust)";

#[cfg(windows)]
pub fn run_windows_service() -> Result<(), String> {
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    static SHUTDOWN_TX: std::sync::OnceLock<std::sync::mpsc::Sender<()>> = std::sync::OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_arguments: Vec<OsString>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = SHUTDOWN_TX.set(tx);

        let status_handle = match service_control_handler::register(SERVICE_NAME, move |control_event| {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    if let Some(sender) = SHUTDOWN_TX.get() {
                        let _ = sender.send(());
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        }) {
            Ok(h) => h,
            Err(e) => {
                logger::error(format!("Service register error: {}", e));
                return;
            }
        };

        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });

        logger::info(format!("=== Служба Windows Hls2UdpService v{} (Rust) запущена ===", APP_VERSION));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let cfg = AppConfig::load();
        let mgr = BroadcastManager::new(cfg.clone(), rt.handle().clone(), true);
        http::spawn_http_server(mgr.clone(), cfg.http_monitor_port, rt.handle());
        mgr.start_all_enabled();

        let _ = rx.recv();
        logger::info(format!("=== Остановка службы Hls2UdpService v{} ===", APP_VERSION));
        mgr.stop_all();

        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });
    }

    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| format!("Service dispatcher error: {}", e))
}

#[cfg(not(windows))]
pub fn run_windows_service() -> Result<(), String> {
    Err("Windows Service only supported on Windows".to_string())
}

pub fn is_service_installed() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        if let Ok(out) = std::process::Command::new("sc.exe")
            .args(["query", SERVICE_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            return out.status.success();
        }
    }
    false
}

pub fn install_and_start_service() -> String {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "Hls2Udp.exe".to_string());
        let bin_path = format!("\"{}\" --service", exe);

        let _ = std::process::Command::new("sc.exe")
            .args(["stop", SERVICE_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let _ = std::process::Command::new("sc.exe")
            .args(["delete", SERVICE_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        std::thread::sleep(Duration::from_millis(600));

        let mut out = String::new();
        for args in [
            vec!["create", SERVICE_NAME, "binPath=", &bin_path, "start=", "auto", "DisplayName=", DISPLAY_NAME],
            vec!["failure", SERVICE_NAME, "reset=", "86400", "actions=", "restart/5000/restart/5000/restart/5000"],
            vec!["start", SERVICE_NAME],
        ] {
            if let Ok(res) = std::process::Command::new("sc.exe")
                .args(&args)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
            {
                out.push_str(&String::from_utf8_lossy(&res.stdout));
                out.push_str(&String::from_utf8_lossy(&res.stderr));
            }
        }
        logger::info(format!("Служба Windows Hls2UdpService v{} установлена и запущена.", APP_VERSION));
        out
    }
    #[cfg(not(windows))]
    {
        "Not Windows".to_string()
    }
}

pub fn uninstall_service() -> String {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let mut out = String::new();
        let _ = std::process::Command::new("sc.exe")
            .args(["stop", SERVICE_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        std::thread::sleep(Duration::from_millis(700));
        if let Ok(res) = std::process::Command::new("sc.exe")
            .args(["delete", SERVICE_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            out.push_str(&String::from_utf8_lossy(&res.stdout));
            out.push_str(&String::from_utf8_lossy(&res.stderr));
        }
        logger::info("Служба Windows Hls2UdpService удалена.");
        out
    }
    #[cfg(not(windows))]
    {
        "Not Windows".to_string()
    }
}
