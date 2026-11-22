use crate::config::{list_nics, AppConfig, APP_VERSION};
use crate::logger;
use crate::manager::{send_telegram_sync, BroadcastManager, ManagerTelemetry};
use crate::service;
use serde::Serialize;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const UI_HTML: &str = include_str!("ui.html");

#[derive(Serialize)]
struct ApiState {
    config: AppConfig,
    telemetry: ManagerTelemetry,
    nics: Vec<crate::config::NicInfo>,
    service_installed: bool,
    log_path: String,
    config_path: String,
}

pub fn spawn_http_server(mgr: Arc<BroadcastManager>, port: u16, rt: &tokio::runtime::Handle) {
    rt.spawn(async move {
        let addr = format!("0.0.0.0:{}", port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => {
                logger::info(format!(
                    "Веб-интерфейс и HTTP-мониторинг v{} запущены: http://localhost:{}",
                    APP_VERSION, port
                ));
                l
            }
            Err(e) => {
                logger::warn(format!("Не удалось занять HTTP-порт {}: {}", port, e));
                return;
            }
        };

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let m = mgr.clone();
                    tokio::spawn(async move {
                        let _ = handle_client(stream, m).await;
                    });
                }
                Err(e) => {
                    logger::warn(format!("HTTP accept error: {}", e));
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    });
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Ok(h1), Ok(h2)) = (
                std::str::from_utf8(&bytes[i + 1..i + 2]),
                std::str::from_utf8(&bytes[i + 2..i + 3]),
            ) {
                if let Ok(b) = u8::from_str_radix(&format!("{}{}", h1, h2), 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn handle_client(mut stream: TcpStream, mgr: Arc<BroadcastManager>) -> Result<(), ()> {
    let mut buf = vec![0u8; 65536];
    let n = tokio::time::timeout(std::time::Duration::from_secs(3), stream.read(&mut buf))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    if n == 0 {
        return Ok(());
    }

    let req_str = String::from_utf8_lossy(&buf[..n]);
    let header_end = req_str.find("\r\n\r\n").unwrap_or(req_str.len());
    let head = &req_str[..header_end];
    let mut lines = head.lines();
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let full_path = parts.next().unwrap_or("/");
    let (path, query) = full_path.split_once('?').unwrap_or((full_path, ""));

    let mut content_len: usize = 0;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("Content-Length") {
                content_len = v.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body_bytes = if header_end + 4 <= n {
        buf[header_end + 4..n].to_vec()
    } else {
        Vec::new()
    };
    while body_bytes.len() < content_len && body_bytes.len() < 1024 * 1024 {
        let mut chunk = [0u8; 8192];
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut chunk))
            .await
            .ok()
            .and_then(|res| res.ok())
            .unwrap_or(0);
        if r == 0 {
            break;
        }
        body_bytes.extend_from_slice(&chunk[..r]);
    }
    if body_bytes.len() > content_len && content_len > 0 {
        body_bytes.truncate(content_len);
    }

    let (status_code, content_type, resp_body) = match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => (
            "200 OK",
            "text/html; charset=utf-8",
            UI_HTML.as_bytes().to_vec(),
        ),
        ("GET", "/health") => {
            let msg = serde_json::json!({"status": "ok", "version": APP_VERSION}).to_string();
            ("200 OK", "application/json; charset=utf-8", msg.into_bytes())
        }
        ("GET", "/status") => {
            let tel = mgr.telemetry();
            let json = serde_json::to_vec(&tel).unwrap_or_else(|_| b"{}".to_vec());
            ("200 OK", "application/json; charset=utf-8", json)
        }
        ("GET", "/metrics") => {
            let tel = mgr.telemetry();
            let prom = build_prometheus(&tel);
            ("200 OK", "text/plain; version=0.0.4; charset=utf-8", prom.into_bytes())
        }
        ("GET", "/api/state") => {
            let cfg = mgr.config.lock().map(|g| g.clone()).unwrap_or_default();
            let st = ApiState {
                config: cfg,
                telemetry: mgr.telemetry(),
                nics: list_nics(),
                service_installed: service::is_service_installed(),
                log_path: logger::get_log_file_path().to_string_lossy().to_string(),
                config_path: crate::config::get_config_path().to_string_lossy().to_string(),
            };
            let json = serde_json::to_vec(&st).unwrap_or_else(|_| b"{}".to_vec());
            ("200 OK", "application/json; charset=utf-8", json)
        }
        ("GET", "/api/logs") => {
            let since = query
                .split('&')
                .find_map(|p| p.strip_prefix("since="))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let logs = logger::get_since(since, 80);
            let json = serde_json::to_vec(&logs).unwrap_or_else(|_| b"[]".to_vec());
            ("200 OK", "application/json; charset=utf-8", json)
        }
        ("POST", "/api/config") => match serde_json::from_slice::<AppConfig>(&body_bytes) {
            Ok(mut new_cfg) => {
                new_cfg.sanitize();
                if let Err(ve) = new_cfg.validate() {
                    let msg = serde_json::json!({"ok": false, "error": ve}).to_string();
                    ("400 Bad Request", "application/json; charset=utf-8", msg.into_bytes())
                } else {
                    match new_cfg.save() {
                        Ok(()) => {
                            mgr.reload_config(new_cfg, false);
                            logger::info(format!("Конфигурация v{} сохранена.", APP_VERSION));
                            ("200 OK", "application/json; charset=utf-8", br#"{"ok":true}"#.to_vec())
                        }
                        Err(e) => {
                            let msg = serde_json::json!({"ok": false, "error": e}).to_string();
                            ("500 Internal Server Error", "application/json; charset=utf-8", msg.into_bytes())
                        }
                    }
                }
            }
            Err(e) => {
                let msg = serde_json::json!({"ok": false, "error": e.to_string()}).to_string();
                ("400 Bad Request", "application/json; charset=utf-8", msg.into_bytes())
            }
        },
        ("POST", "/api/start_all") => {
            mgr.start_all_enabled();
            ("200 OK", "application/json; charset=utf-8", br#"{"ok":true}"#.to_vec())
        }
        ("POST", "/api/stop_all") => {
            mgr.stop_all();
            ("200 OK", "application/json; charset=utf-8", br#"{"ok":true}"#.to_vec())
        }
        ("POST", p) if p.starts_with("/api/channel/") && p.ends_with("/start") => {
            let raw_id = p.trim_start_matches("/api/channel/").trim_end_matches("/start");
            let id = percent_decode(raw_id);
            mgr.start_channel(&id);
            ("200 OK", "application/json; charset=utf-8", br#"{"ok":true}"#.to_vec())
        }
        ("POST", p) if p.starts_with("/api/channel/") && p.ends_with("/stop") => {
            let raw_id = p.trim_start_matches("/api/channel/").trim_end_matches("/stop");
            let id = percent_decode(raw_id);
            mgr.stop_channel(&id);
            ("200 OK", "application/json; charset=utf-8", br#"{"ok":true}"#.to_vec())
        }
        ("POST", "/api/telegram_test") => {
            let cfg = mgr.config.lock().map(|g| g.clone()).unwrap_or_default();
            if cfg.telegram_bot_token.trim().is_empty() || cfg.telegram_chat_id.trim().is_empty() {
                (
                    "400 Bad Request",
                    "application/json; charset=utf-8",
                    r#"{"ok":false,"error":"Укажите Bot Token и Chat ID"}"#.as_bytes().to_vec(),
                )
            } else {
                match send_telegram_sync(
                    cfg.telegram_bot_token.trim(),
                    cfg.telegram_chat_id.trim(),
                    &format!("[Hls2Udp v{} Rust] Тестовое уведомление — связь работает!", APP_VERSION),
                )
                .await
                {
                    Ok(()) => ("200 OK", "application/json; charset=utf-8", br#"{"ok":true}"#.to_vec()),
                    Err(e) => {
                        let msg = serde_json::json!({"ok": false, "error": e}).to_string();
                        ("500 Internal Server Error", "application/json; charset=utf-8", msg.into_bytes())
                    }
                }
            }
        }
        ("POST", "/api/service/install") => {
            let out = service::install_and_start_service();
            let msg = serde_json::json!({"ok": true, "output": out}).to_string();
            ("200 OK", "application/json; charset=utf-8", msg.into_bytes())
        }
        ("POST", "/api/service/uninstall") => {
            let out = service::uninstall_service();
            let msg = serde_json::json!({"ok": true, "output": out}).to_string();
            ("200 OK", "application/json; charset=utf-8", msg.into_bytes())
        }
        _ => ("404 Not Found", "text/plain; charset=utf-8", b"Not Found".to_vec()),
    };

    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        status_code,
        content_type,
        resp_body.len()
    );
    let _ = stream.write_all(header.as_bytes()).await;
    let _ = stream.write_all(&resp_body).await;
    Ok(())
}

fn build_prometheus(m: &ManagerTelemetry) -> String {
    let mut s = String::new();
    s.push_str(&format!("hls2udp_cpu_process_percent {:.2}\n", m.cpu_process));
    s.push_str(&format!("hls2udp_cpu_system_percent {:.2}\n", m.cpu_system));
    for c in &m.channels {
        let lbl = format!("id=\"{}\",name=\"{}\"", c.id.replace('"', ""), c.name.replace('"', ""));
        s.push_str(&format!("hls2udp_channel_running{{{}}} {}\n", lbl, if c.is_running { 1 } else { 0 }));
        s.push_str(&format!("hls2udp_channel_input_alive{{{}}} {}\n", lbl, if c.input_alive { 1 } else { 0 }));
        s.push_str(&format!("hls2udp_channel_output_alive{{{}}} {}\n", lbl, if c.output_alive { 1 } else { 0 }));
        s.push_str(&format!("hls2udp_channel_using_backup{{{}}} {}\n", lbl, if c.using_backup { 1 } else { 0 }));
        s.push_str(&format!("hls2udp_channel_bitrate_in_mbps{{{}}} {:.3}\n", lbl, c.input_bitrates_mbps));
        s.push_str(&format!("hls2udp_channel_bitrate_out_mbps{{{}}} {:.3}\n", lbl, c.output_bitrates_mbps));
        s.push_str(&format!("hls2udp_channel_cc_errors{{{}}} {}\n", lbl, c.cc_errors));
        s.push_str(&format!("hls2udp_channel_watchdog_resets{{{}}} {}\n", lbl, c.watchdog_resets));
        s.push_str(&format!("hls2udp_channel_buffer_depth{{{}}} {}\n", lbl, c.buffer_depth));
    }
    s
}
