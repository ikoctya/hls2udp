use crate::config::{ChannelConfig, APP_VERSION};
use crate::logger;
use crate::ts::{extract_pcr_anchors, MpegTsAnalyzer, TS_PACKET_SIZE, UDP_PAYLOAD_SIZE};
use aes::Aes128;
use cbc::Decryptor;
use cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_QUEUE_SEGMENTS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelTelemetry {
    pub id: String,
    pub name: String,
    pub is_running: bool,
    pub input_alive: bool,
    pub output_alive: bool,
    pub using_backup: bool,
    pub aes_encrypted: bool,
    pub input_bitrates_mbps: f64,
    pub output_bitrates_mbps: f64,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub total_segments_in: u64,
    pub total_packets_out: u64,
    pub watchdog_resets: u64,
    pub cc_errors: u64,
    pub active_pids: usize,
    pub buffer_depth: usize,
    pub uptime_seconds: u64,
    pub output_udp_url: String,
    pub status_text: String,
}

struct QueuedSegment {
    data: Vec<u8>,
    duration_sec: f64,
    reset_tail: bool,
}

struct SegmentQueue {
    items: VecDeque<QueuedSegment>,
    session_ended: bool,
}

#[derive(Clone)]
struct HlsKeyInfo {
    method: String,
    key_uri: Option<String>,
    explicit_iv: Option<[u8; 16]>,
}

struct HlsSegmentItem {
    url: String,
    duration_sec: f64,
    sequence: u64,
    key: Option<HlsKeyInfo>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct ChannelShared {
    pub cfg: Mutex<ChannelConfig>,
    pub running: AtomicBool,
    pub stop_requested: AtomicBool,
    pub watchdog_reset: AtomicBool,
    pub using_backup: AtomicBool,
    pub aes_encrypted: AtomicBool,

    pub window_bytes_in: AtomicU64,
    pub window_bytes_out: AtomicU64,
    pub total_bytes_in: AtomicU64,
    pub total_bytes_out: AtomicU64,
    pub total_segments_in: AtomicU64,
    pub total_packets_out: AtomicU64,
    pub watchdog_resets: AtomicU64,
    pub cc_errors: AtomicU64,
    pub active_pids: AtomicUsize,
    pub buffer_depth: AtomicUsize,

    pub stream_start_ms: AtomicI64,
    pub last_media_input_ms: AtomicI64,
    pub last_output_ms: AtomicI64,
    pub last_watchdog_action_ms: AtomicI64,
    pub last_udp_err_log_ms: AtomicI64,

    pub in_mbps_x1000: AtomicU64,
    pub out_mbps_x1000: AtomicU64,
    pub status_text: Mutex<String>,

    queue: Mutex<SegmentQueue>,
    queue_cv: Condvar,
}

impl ChannelShared {
    pub fn new(cfg: ChannelConfig) -> Self {
        Self {
            cfg: Mutex::new(cfg),
            running: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            watchdog_reset: AtomicBool::new(false),
            using_backup: AtomicBool::new(false),
            aes_encrypted: AtomicBool::new(false),
            window_bytes_in: AtomicU64::new(0),
            window_bytes_out: AtomicU64::new(0),
            total_bytes_in: AtomicU64::new(0),
            total_bytes_out: AtomicU64::new(0),
            total_segments_in: AtomicU64::new(0),
            total_packets_out: AtomicU64::new(0),
            watchdog_resets: AtomicU64::new(0),
            cc_errors: AtomicU64::new(0),
            active_pids: AtomicUsize::new(0),
            buffer_depth: AtomicUsize::new(0),
            stream_start_ms: AtomicI64::new(0),
            last_media_input_ms: AtomicI64::new(0),
            last_output_ms: AtomicI64::new(0),
            last_watchdog_action_ms: AtomicI64::new(0),
            last_udp_err_log_ms: AtomicI64::new(0),
            in_mbps_x1000: AtomicU64::new(0),
            out_mbps_x1000: AtomicU64::new(0),
            status_text: Mutex::new("Остановлен".to_string()),
            queue: Mutex::new(SegmentQueue {
                items: VecDeque::with_capacity(MAX_QUEUE_SEGMENTS + 2),
                session_ended: false,
            }),
            queue_cv: Condvar::new(),
        }
    }

    pub fn set_status(&self, s: impl Into<String>) {
        if let Ok(mut g) = self.status_text.lock() {
            *g = s.into();
        }
    }

    pub fn clear_queue(&self, session_ended: bool) {
        if let Ok(mut q) = self.queue.lock() {
            q.items.clear();
            q.session_ended = session_ended;
            self.buffer_depth.store(0, Ordering::Relaxed);
            self.queue_cv.notify_all();
        }
    }

    pub fn queue_len(&self) -> usize {
        self.queue.lock().map(|q| q.items.len()).unwrap_or(0)
    }

    pub fn telemetry(&self) -> ChannelTelemetry {
        let cfg = self.cfg.lock().map(|g| g.clone()).unwrap_or_default();
        let now = now_ms();
        let running = self.running.load(Ordering::Relaxed);
        let last_in = self.last_media_input_ms.load(Ordering::Relaxed);
        let last_out = self.last_output_ms.load(Ordering::Relaxed);
        let start = self.stream_start_ms.load(Ordering::Relaxed);
        let wd_win_ms = (cfg.watchdog_timeout_sec.max(3) * 1000).min(8000) as i64;

        let input_alive = running && last_in > 0 && (now - last_in) < wd_win_ms;
        let output_alive = running && last_out > 0 && (now - last_out) < 5000;
        let uptime_seconds = if running && start > 0 && now > start {
            ((now - start) / 1000) as u64
        } else {
            0
        };

        let status = self
            .status_text
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| "—".to_string());

        ChannelTelemetry {
            id: cfg.id,
            name: cfg.name,
            is_running: running,
            input_alive,
            output_alive,
            using_backup: self.using_backup.load(Ordering::Relaxed),
            aes_encrypted: self.aes_encrypted.load(Ordering::Relaxed),
            input_bitrates_mbps: if running {
                self.in_mbps_x1000.load(Ordering::Relaxed) as f64 / 1000.0
            } else {
                0.0
            },
            output_bitrates_mbps: if running {
                self.out_mbps_x1000.load(Ordering::Relaxed) as f64 / 1000.0
            } else {
                0.0
            },
            total_bytes_in: self.total_bytes_in.load(Ordering::Relaxed),
            total_bytes_out: self.total_bytes_out.load(Ordering::Relaxed),
            total_segments_in: self.total_segments_in.load(Ordering::Relaxed),
            total_packets_out: self.total_packets_out.load(Ordering::Relaxed),
            watchdog_resets: self.watchdog_resets.load(Ordering::Relaxed),
            cc_errors: self.cc_errors.load(Ordering::Relaxed),
            active_pids: self.active_pids.load(Ordering::Relaxed),
            buffer_depth: self.buffer_depth.load(Ordering::Relaxed),
            uptime_seconds,
            output_udp_url: cfg.output_udp_url,
            status_text: status,
        }
    }
}

pub struct ChannelWorker {
    pub shared: Arc<ChannelShared>,
    task_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    was_alive_for_alert: AtomicBool,
}

impl ChannelWorker {
    pub fn new(cfg: ChannelConfig) -> Self {
        Self {
            shared: Arc::new(ChannelShared::new(cfg)),
            task_handle: Mutex::new(None),
            was_alive_for_alert: AtomicBool::new(false),
        }
    }

    pub fn update_config(&self, cfg: ChannelConfig) {
        if let Ok(mut g) = self.shared.cfg.lock() {
            *g = cfg;
        }
    }

    pub fn start(&self, rt: &tokio::runtime::Handle) {
        self.stop();

        let sh = self.shared.clone();
        sh.stop_requested.store(false, Ordering::SeqCst);
        sh.watchdog_reset.store(false, Ordering::SeqCst);
        sh.using_backup.store(false, Ordering::SeqCst);
        sh.aes_encrypted.store(false, Ordering::SeqCst);
        sh.cc_errors.store(0, Ordering::Relaxed);
        sh.active_pids.store(0, Ordering::Relaxed);
        sh.clear_queue(false);
        sh.running.store(true, Ordering::SeqCst);
        sh.set_status("Запуск...");

        let now = now_ms();
        sh.stream_start_ms.store(now, Ordering::Relaxed);
        sh.last_watchdog_action_ms.store(now, Ordering::Relaxed);

        let handle = rt.spawn(async move {
            worker_loop(sh).await;
        });
        if let Ok(mut g) = self.task_handle.lock() {
            *g = Some(handle);
        }
    }

    pub fn stop(&self) {
        let sh = &self.shared;
        sh.stop_requested.store(true, Ordering::SeqCst);
        sh.running.store(false, Ordering::SeqCst);
        sh.clear_queue(true);
        if let Ok(mut g) = self.task_handle.lock() {
            if let Some(h) = g.take() {
                h.abort();
            }
        }
        sh.in_mbps_x1000.store(0, Ordering::Relaxed);
        sh.out_mbps_x1000.store(0, Ordering::Relaxed);
        sh.stream_start_ms.store(0, Ordering::Relaxed);
        sh.set_status("Остановлен");
    }

    pub fn sample_metrics_and_watchdog(&self, elapsed_sec: f64) -> Option<(String, String, bool)> {
        let sh = &self.shared;
        let b_in = sh.window_bytes_in.swap(0, Ordering::Relaxed);
        let b_out = sh.window_bytes_out.swap(0, Ordering::Relaxed);

        let in_mbps = (b_in as f64 * 8.0) / (elapsed_sec * 1_000_000.0);
        let out_mbps = (b_out as f64 * 8.0) / (elapsed_sec * 1_000_000.0);
        sh.in_mbps_x1000.store((in_mbps * 1000.0) as u64, Ordering::Relaxed);
        sh.out_mbps_x1000.store((out_mbps * 1000.0) as u64, Ordering::Relaxed);

        if !sh.running.load(Ordering::Relaxed) || sh.stop_requested.load(Ordering::Relaxed) {
            return None;
        }

        let cfg = sh.cfg.lock().map(|g| g.clone()).unwrap_or_default();
        let wd_sec = cfg.watchdog_timeout_sec.max(3);
        let now = now_ms();
        let last_media = sh.last_media_input_ms.load(Ordering::Relaxed);
        let last_act = sh.last_watchdog_action_ms.load(Ordering::Relaxed);
        let ref_ms = last_media.max(last_act);

        let alive_win = (wd_sec * 1000).min(8000) as i64;
        let is_alive = last_media > 0 && (now - last_media) < alive_win;

        if is_alive && !self.was_alive_for_alert.swap(true, Ordering::Relaxed) {
            if sh.watchdog_resets.load(Ordering::Relaxed) > 0 {
                let mode = if sh.using_backup.load(Ordering::Relaxed) { "BACKUP" } else { "PRIMARY" };
                return Some((
                    format!("up_{}", cfg.id),
                    format!("Канал '{}' ВОССТАНОВЛЕН ({}, {:.2} Mbps)", cfg.name, mode, in_mbps),
                    false,
                ));
            }
        }

        if ref_ms <= 0 {
            return None;
        }

        let silence_sec = (now - ref_ms) as f64 / 1000.0;
        if silence_sec >= wd_sec as f64 {
            sh.last_watchdog_action_ms.store(now, Ordering::Relaxed);
            let resets = sh.watchdog_resets.fetch_add(1, Ordering::Relaxed) + 1;
            self.was_alive_for_alert.store(false, Ordering::Relaxed);

            let has_backup = !cfg.backup_hls_url.trim().is_empty();
            let alert_msg = if has_backup {
                let prev = sh.using_backup.fetch_xor(true, Ordering::SeqCst);
                let target = if !prev { "BACKUP" } else { "PRIMARY" };
                sh.set_status(format!("Failover -> {} (WD #{})", target, resets));
                logger::warn(format!(
                    "[{}] WATCHDOG #{} ({:.1} с без .ts): Автопереключение на {}!",
                    cfg.name, resets, silence_sec, target
                ));
                (
                    format!("fail_{}", cfg.id),
                    format!("Канал '{}': Watchdog #{} ({:.0} с без входа). Переключение на {}!", cfg.name, resets, silence_sec, target),
                    false,
                )
            } else {
                sh.set_status(format!("АВАРИЯ: Watchdog #{} ({:.0} с)", resets, silence_sec));
                logger::warn(format!(
                    "[{}] ЖЕСТКИЙ WATCHDOG #{}: Нет данных {:.1} с (порог {} с). Перезапуск сокетов!",
                    cfg.name, resets, silence_sec, wd_sec
                ));
                (
                    format!("down_{}", cfg.id),
                    format!("Канал '{}': АВАРИЯ ВХОДА (Watchdog #{}, нет данных {:.0} с). Переподключение...", cfg.name, resets, silence_sec),
                    false,
                )
            };

            sh.watchdog_reset.store(true, Ordering::SeqCst);
            sh.queue_cv.notify_all();
            return Some(alert_msg);
        }

        None
    }
}

async fn worker_loop(sh: Arc<ChannelShared>) {
    let cfg = sh.cfg.lock().map(|g| g.clone()).unwrap_or_default();
    logger::info(format!(
        "[{}] Старт канала: Вход [{}] {} -> Выход [{}] {} (TTL={}, DSCP={}, WD={} с, Буфер={} сегм)",
        cfg.name, cfg.input_nic_ip, cfg.primary_hls_url, cfg.output_nic_ip, cfg.output_udp_url,
        cfg.multicast_ttl, cfg.dscp_value, cfg.watchdog_timeout_sec, cfg.prebuffer_segments
    ));

    let mut backoff_ms: u64 = 1000;
    while !sh.stop_requested.load(Ordering::SeqCst) {
        sh.watchdog_reset.store(false, Ordering::SeqCst);
        sh.last_watchdog_action_ms.store(now_ms(), Ordering::Relaxed);
        let segs_before = sh.total_segments_in.load(Ordering::Relaxed);

        let run_res = run_native_pipeline(sh.clone()).await;
        if sh.total_segments_in.load(Ordering::Relaxed) > segs_before {
            backoff_ms = 1000;
        }
        if let Err(e) = run_res {
            if !sh.stop_requested.load(Ordering::SeqCst) {
                let name = sh.cfg.lock().map(|g| g.name.clone()).unwrap_or_default();
                sh.set_status("Ошибка (переподключение...)");
                logger::error(format!("[{}] Сбой сессии: {} (повтор через {} мс)", name, e, backoff_ms));
            }
        }

        if !sh.stop_requested.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(5000);
        }
    }
}

async fn run_native_pipeline(sh: Arc<ChannelShared>) -> Result<(), String> {
    let cfg = sh.cfg.lock().map(|g| g.clone()).unwrap_or_default();
    let input_bind_ip = parse_bind_ip(&cfg.input_nic_ip);
    let output_bind_ip = parse_bind_ip(&cfg.output_nic_ip);
    let (udp_target_ip, udp_target_port, query_ttl) =
        parse_udp_url(&cfg.output_udp_url, cfg.multicast_ttl)?;

    sh.clear_queue(false);

    let socket = create_udp_socket(
        &cfg.name,
        output_bind_ip,
        udp_target_ip,
        udp_target_port,
        query_ttl,
        cfg.dscp_value,
    )?;
    let remote_addr = SocketAddr::V4(SocketAddrV4::new(udp_target_ip, udp_target_port));

    let pacer_sh = sh.clone();
    let pacer_handle = std::thread::Builder::new()
        .name(format!("Hls2Udp-Pace-{}", cfg.id))
        .spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_udp_pacer_loop(pacer_sh.clone(), socket, remote_addr);
            }));
            if let Err(panic_info) = res {
                let name = pacer_sh.cfg.lock().map(|g| g.name.clone()).unwrap_or_default();
                logger::write_crash(&format!("[{}] Паника в потоке отправки UDP: {:?}", name, panic_info));
                pacer_sh.watchdog_reset.store(true, Ordering::SeqCst);
            }
        })
        .map_err(|e| format!("Не удалось запустить поток отправки UDP: {}", e))?;

    let fetch_res = run_hls_fetcher_loop(sh.clone(), input_bind_ip).await;

    sh.clear_queue(true);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = pacer_handle.join();
    })
    .await;

    fetch_res
}

fn create_udp_socket(
    ch_name: &str,
    output_bind_ip: Ipv4Addr,
    udp_target_ip: Ipv4Addr,
    udp_target_port: u16,
    ttl: u32,
    dscp: u32,
) -> Result<Socket, String> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| format!("Socket::new: {}", e))?;

    let _ = socket.set_reuse_address(true);
    let _ = socket.set_send_buffer_size(8 * 1024 * 1024);

    #[cfg(windows)]
    unsafe {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            setsockopt, IPPROTO_IP, IP_TOS, SIO_UDP_CONNRESET, SOCKET,
        };
        #[link(name = "ws2_32")]
        extern "system" {
            fn WSAIoctl(
                s: SOCKET,
                dwIoControlCode: u32,
                lpvInBuffer: *const core::ffi::c_void,
                cbInBuffer: u32,
                lpvOutBuffer: *mut core::ffi::c_void,
                cbOutBuffer: u32,
                lpcbBytesReturned: *mut u32,
                lpOverlapped: *mut core::ffi::c_void,
                lpCompletionRoutine: *const core::ffi::c_void,
            ) -> i32;
        }
        let raw = socket.as_raw_socket() as SOCKET;

        // Отключаем SIO_UDP_CONNRESET, чтобы ICMP Port Unreachable на 127.0.0.1 не вызывал ошибку 10054
        let mut disable: u32 = 0;
        let mut bytes_returned: u32 = 0;
        WSAIoctl(
            raw,
            SIO_UDP_CONNRESET,
            &mut disable as *mut u32 as *const _,
            std::mem::size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
            std::ptr::null(),
        );

        if dscp > 0 && dscp <= 63 {
            let tos: i32 = (dscp << 2) as i32;
            setsockopt(
                raw,
                IPPROTO_IP,
                IP_TOS,
                &tos as *const i32 as *const u8,
                std::mem::size_of::<i32>() as i32,
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = dscp;
    }

    let is_multicast = udp_target_ip.is_multicast();
    let is_target_loopback = udp_target_ip.is_loopback();
    let is_loopback_nic = output_bind_ip.is_loopback();

    // На Windows отправка Multicast с сокета, привязанного к 127.0.0.1, вызывает WSAENETUNREACH (10051).
    // Для локального Multicast привязываемся к 0.0.0.0 с TTL=0 и MulticastLoopback=true.
    let effective_bind_ip = if is_target_loopback {
        Ipv4Addr::LOCALHOST
    } else if is_multicast && is_loopback_nic {
        Ipv4Addr::UNSPECIFIED
    } else {
        output_bind_ip
    };

    socket
        .bind(&SocketAddr::V4(SocketAddrV4::new(effective_bind_ip, 0)).into())
        .map_err(|e| format!("Bind({}): {}", effective_bind_ip, e))?;

    if is_multicast {
        let _ = socket.set_multicast_loop_v4(true);
        if is_loopback_nic {
            if socket.set_multicast_ttl_v4(0).is_ok() {
                logger::info(format!("[{}] Локальный Multicast (только этот ПК): Bind=0.0.0.0, TTL=0, Loopback=ON.", ch_name));
            } else {
                let _ = socket.set_multicast_ttl_v4(1);
                logger::info(format!("[{}] Локальный Multicast (только этот ПК): Bind=0.0.0.0, TTL=1, Loopback=ON.", ch_name));
            }
        } else {
            let _ = socket.set_multicast_ttl_v4(ttl);
            if !effective_bind_ip.is_unspecified() {
                let _ = socket.set_multicast_if_v4(&effective_bind_ip);
            } else {
                logger::warn(format!("[{}] Выходной NIC не задан (0.0.0.0) — рекомендуется явно выбрать локальный IPv4 адаптер.", ch_name));
            }
        }
    } else if is_target_loopback {
        logger::info(format!("[{}] Локальное вещание Unicast на {}:{} (только этот ПК).", ch_name, udp_target_ip, udp_target_port));
    } else if is_loopback_nic {
        logger::warn(format!("[{}] Выбран NIC 127.0.0.1 для внешнего адреса {} — для localhost укажите udp://127.0.0.1:5000.", ch_name, udp_target_ip));
    }

    Ok(socket)
}

async fn run_hls_fetcher_loop(sh: Arc<ChannelShared>, input_bind_ip: Ipv4Addr) -> Result<(), String> {
    let cfg = sh.cfg.lock().map(|g| g.clone()).unwrap_or_default();
    let using_backup = sh.using_backup.load(Ordering::Relaxed);
    let source_root = if using_backup && !cfg.backup_hls_url.trim().is_empty() {
        cfg.backup_hls_url.clone()
    } else {
        cfg.primary_hls_url.clone()
    };

    let mut client_builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(6)
        .pool_idle_timeout(Duration::from_secs(60))
        .tcp_keepalive(Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .tcp_nodelay(true)
        .user_agent(format!("Hls2Udp-Rust/{}", APP_VERSION));

    if !input_bind_ip.is_unspecified() {
        client_builder = client_builder.local_address(IpAddr::V4(input_bind_ip));
    }
    let headers = parse_custom_headers(&cfg.custom_headers);
    if !headers.is_empty() {
        client_builder = client_builder.default_headers(headers);
    }

    let client = client_builder
        .build()
        .map_err(|e| format!("HTTP client error: {}", e))?;

    let mut active_playlist_url = source_root.clone();
    let mut recent_urls: VecDeque<String> = VecDeque::with_capacity(220);
    let mut seen_urls: HashSet<String> = HashSet::with_capacity(220);
    let mut aes_key_cache: HashMap<String, [u8; 16]> = HashMap::new();
    let mut last_media_seq: Option<i64> = None;
    let mut last_new_seg = Instant::now();
    let mut last_primary_probe = Instant::now();
    let mut need_tail_reset = true;

    let wd_timeout = cfg.watchdog_timeout_sec.max(3);
    sh.set_status(if using_backup { "Подключение (BACKUP)..." } else { "Подключение к HLS..." });

    while !sh.stop_requested.load(Ordering::SeqCst) && !sh.watchdog_reset.load(Ordering::SeqCst) {
        if sh.using_backup.load(Ordering::Relaxed)
            && !cfg.primary_hls_url.trim().is_empty()
            && last_primary_probe.elapsed() >= Duration::from_secs(30)
        {
            last_primary_probe = Instant::now();
            if let Ok(resp) = client.get(&cfg.primary_hls_url).timeout(Duration::from_millis(2500)).send().await {
                if let Ok(txt) = resp.text().await {
                    if txt.contains("#EXTM3U") {
                        logger::info(format!("[{}] Основной источник (Primary) снова доступен — возврат с Backup!", cfg.name));
                        sh.using_backup.store(false, Ordering::SeqCst);
                        return Ok(());
                    }
                }
            }
        }

        let (playlist_text, resolved_playlist_url) = match client.get(&active_playlist_url).timeout(Duration::from_secs(5)).send().await {
            Ok(resp) if resp.status().is_success() => {
                let final_url = resp.url().to_string();
                let txt = resp.text().await.unwrap_or_default();
                (txt, final_url)
            }
            Ok(resp) => {
                sh.set_status(format!("HTTP {}", resp.status().as_u16()));
                tokio::time::sleep(Duration::from_millis(600)).await;
                continue;
            }
            Err(_) => {
                sh.set_status("Ошибка загрузки плейлиста");
                tokio::time::sleep(Duration::from_millis(600)).await;
                continue;
            }
        };

        if playlist_text.trim().is_empty() {
            sh.set_status("Пустой плейлист HLS");
            tokio::time::sleep(Duration::from_millis(600)).await;
            continue;
        }

        if let Some(variant) = try_pick_best_variant_playlist(&playlist_text, &resolved_playlist_url) {
            if !variant.eq_ignore_ascii_case(&active_playlist_url) {
                logger::info(format!("[{}] Выбран поток максимального качества: {}", cfg.name, variant));
                active_playlist_url = variant;
                continue;
            }
        }

        let (segments, target_dur_sec, media_seq, has_endlist, has_discontinuity) =
            parse_media_playlist(&playlist_text, &resolved_playlist_url);

        if let (Some(prev_seq), Some(cur_seq)) = (last_media_seq, media_seq) {
            if cur_seq < prev_seq {
                logger::warn(format!("[{}] Сброс MEDIA-SEQUENCE ({} -> {}), очистка истории.", cfg.name, prev_seq, cur_seq));
                seen_urls.clear();
                recent_urls.clear();
                need_tail_reset = true;
            }
        }
        if media_seq.is_some() {
            last_media_seq = media_seq;
        }

        let init_keep = (cfg.prebuffer_segments + 1).clamp(3, MAX_QUEUE_SEGMENTS - 2);
        if seen_urls.is_empty() && segments.len() > init_keep {
            for seg in &segments[..segments.len() - init_keep] {
                remember_segment(&seg.url, &mut seen_urls, &mut recent_urls);
            }
        }

        let mut new_segments = Vec::new();
        for seg in segments {
            if !seen_urls.contains(&seg.url) {
                remember_segment(&seg.url, &mut seen_urls, &mut recent_urls);
                new_segments.push(seg);
            }
        }

        if new_segments.is_empty() {
            if has_endlist {
                seen_urls.clear();
                recent_urls.clear();
                last_media_seq = None;
                active_playlist_url = source_root.clone();
                need_tail_reset = true;
                tokio::time::sleep(Duration::from_millis(600)).await;
                continue;
            }
            if last_new_seg.elapsed().as_secs_f64() >= wd_timeout as f64 {
                return Err(format!("Зависший плейлист HLS (нет новых .ts {:.1} с)", last_new_seg.elapsed().as_secs_f64()));
            }
            let sleep_ms = (target_dur_sec * 100.0).clamp(100.0, 250.0) as u64;
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            continue;
        }

        last_new_seg = Instant::now();

        for seg in new_segments {
            if sh.stop_requested.load(Ordering::SeqCst) || sh.watchdog_reset.load(Ordering::SeqCst) {
                break;
            }
            while sh.queue_len() >= MAX_QUEUE_SEGMENTS
                && !sh.stop_requested.load(Ordering::SeqCst)
                && !sh.watchdog_reset.load(Ordering::SeqCst)
            {
                tokio::time::sleep(Duration::from_millis(35)).await;
            }
            if sh.stop_requested.load(Ordering::SeqCst) || sh.watchdog_reset.load(Ordering::SeqCst) {
                break;
            }

            let mut raw_bytes_opt = None;
            for attempt in 0..2 {
                if sh.stop_requested.load(Ordering::SeqCst) || sh.watchdog_reset.load(Ordering::SeqCst) {
                    break;
                }
                match client.get(&seg.url).timeout(Duration::from_secs(6)).send().await {
                    Ok(r) if r.status().is_success() => {
                        if let Ok(b) = r.bytes().await {
                            if !b.is_empty() {
                                raw_bytes_opt = Some(b.to_vec());
                                break;
                            }
                        }
                    }
                    _ => {}
                }
                if attempt == 0 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
            let raw_bytes = match raw_bytes_opt {
                Some(b) => b,
                None => continue,
            };

            let read_len = raw_bytes.len() as u64;
            sh.window_bytes_in.fetch_add(read_len, Ordering::Relaxed);
            sh.total_bytes_in.fetch_add(read_len, Ordering::Relaxed);
            sh.last_media_input_ms.store(now_ms(), Ordering::Relaxed);

            let payload = if let Some(ref key) = seg.key {
                if key.method.eq_ignore_ascii_case("AES-128") {
                    if let Some(ref key_uri) = key.key_uri {
                        sh.aes_encrypted.store(true, Ordering::Relaxed);
                        match decrypt_aes128_segment(&client, &mut aes_key_cache, &raw_bytes, key_uri, key.explicit_iv, seg.sequence).await {
                            Some(dec) => dec,
                            None => continue,
                        }
                    } else {
                        raw_bytes
                    }
                } else {
                    sh.aes_encrypted.store(false, Ordering::Relaxed);
                    raw_bytes
                }
            } else {
                sh.aes_encrypted.store(false, Ordering::Relaxed);
                raw_bytes
            };

            if payload.is_empty() {
                continue;
            }

            sh.total_segments_in.fetch_add(1, Ordering::Relaxed);
            sh.set_status(if sh.using_backup.load(Ordering::Relaxed) { "BACKUP АКТИВЕН" } else { "Вещание активно" });

            let q_seg = QueuedSegment {
                data: payload,
                duration_sec: if seg.duration_sec > 0.1 { seg.duration_sec } else { target_dur_sec },
                reset_tail: need_tail_reset || has_discontinuity,
            };
            need_tail_reset = false;

            if let Ok(mut q) = sh.queue.lock() {
                while q.items.len() >= MAX_QUEUE_SEGMENTS {
                    q.items.pop_front();
                }
                q.items.push_back(q_seg);
                sh.buffer_depth.store(q.items.len(), Ordering::Relaxed);
                sh.queue_cv.notify_one();
            }
        }
    }

    if sh.watchdog_reset.load(Ordering::SeqCst) && !sh.stop_requested.load(Ordering::SeqCst) {
        return Err("Сброс сессии по сигналу Watchdog.".to_string());
    }
    Ok(())
}

async fn decrypt_aes128_segment(
    client: &reqwest::Client,
    cache: &mut HashMap<String, [u8; 16]>,
    encrypted: &[u8],
    key_uri: &str,
    explicit_iv: Option<[u8; 16]>,
    sequence: u64,
) -> Option<Vec<u8>> {
    let key = if let Some(k) = cache.get(key_uri) {
        *k
    } else {
        let resp = client.get(key_uri).timeout(Duration::from_secs(4)).send().await.ok()?;
        let bytes = resp.bytes().await.ok()?;
        if bytes.len() != 16 {
            logger::warn(format!("Неверный размер AES-128 ключа ({} байт): {}", bytes.len(), key_uri));
            return None;
        }
        let mut k = [0u8; 16];
        k.copy_from_slice(&bytes);
        cache.insert(key_uri.to_string(), k);
        k
    };

    let iv = explicit_iv.unwrap_or_else(|| {
        let mut iv = [0u8; 16];
        iv[8..16].copy_from_slice(&sequence.to_be_bytes());
        iv
    });

    let mut buf = encrypted.to_vec();
    let dec = Decryptor::<Aes128>::new_from_slices(&key, &iv).ok()?;
    let pt = dec.decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?;
    Some(pt.to_vec())
}

fn run_udp_pacer_loop(sh: Arc<ChannelShared>, socket: Socket, remote_addr: SocketAddr) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }

    let mut ts_analyzer = MpegTsAnalyzer::new();
    let mut udp_packet = [0u8; UDP_PAYLOAD_SIZE];
    let mut udp_fill: usize = 0;

    let master_clock = Instant::now();
    let mut clock_anchored = false;
    let mut stream_time_offset_sec = 0.0;
    let mut prev_seg_last_pcr: Option<f64> = None;
    let mut next_synthetic_sec = 0.0;

    let cfg_prebuf = sh.cfg.lock().map(|g| g.prebuffer_segments).unwrap_or(4);
    let target_prebuf = cfg_prebuf.clamp(1, MAX_QUEUE_SEGMENTS - 2);
    let mut need_prebuffer = true;
    let mut empty_wait_streak: u32 = 0;

    while !sh.stop_requested.load(Ordering::SeqCst) && !sh.watchdog_reset.load(Ordering::SeqCst) {
        let mut popped: Option<(QueuedSegment, usize)> = None;
        {
            let mut q = match sh.queue.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            loop {
                if q.session_ended || sh.stop_requested.load(Ordering::SeqCst) || sh.watchdog_reset.load(Ordering::SeqCst) {
                    break;
                }
                if need_prebuffer {
                    if q.items.len() >= target_prebuf {
                        need_prebuffer = false;
                        empty_wait_streak = 0;
                        if let Some(seg) = q.items.pop_front() {
                            let rem = q.items.len();
                            sh.buffer_depth.store(rem, Ordering::Relaxed);
                            popped = Some((seg, rem));
                        }
                        break;
                    } else if !q.items.is_empty() {
                        sh.set_status(format!("Буферизация ({}/{})...", q.items.len(), target_prebuf));
                    }
                } else if let Some(seg) = q.items.pop_front() {
                    empty_wait_streak = 0;
                    let rem = q.items.len();
                    sh.buffer_depth.store(rem, Ordering::Relaxed);
                    popped = Some((seg, rem));
                    break;
                } else {
                    if udp_fill > 0 {
                        send_raw_udp(&sh, &socket, &remote_addr, &udp_packet[..udp_fill]);
                        udp_fill = 0;
                    }
                    empty_wait_streak += 1;
                    // Не останавливаем вещание на 20 секунд; только сбрасываем привязку часов при длительной паузе > 1.5с
                    if empty_wait_streak == 45 {
                        clock_anchored = false;
                        let name = sh.cfg.lock().map(|g| g.name.clone()).unwrap_or_default();
                        logger::warn(format!("[{}] Очередь сегментов пуста (>1.5 с) — ожидание данных от источника...", name));
                    }
                }

                let wait_res = sh.queue_cv.wait_timeout(q, Duration::from_millis(35));
                q = match wait_res {
                    Ok((guard, _)) => guard,
                    Err(_) => return,
                };
            }
        }

        let (seg, remaining) = match popped {
            Some(v) => v,
            None => {
                if sh.queue.lock().map(|q| q.session_ended).unwrap_or(true) {
                    break;
                }
                continue;
            }
        };

        if seg.reset_tail {
            if udp_fill > 0 {
                send_raw_udp(&sh, &socket, &remote_addr, &udp_packet[..udp_fill]);
                udp_fill = 0;
            }
            clock_anchored = false;
        }

        let data = &seg.data;
        let mut start_off = 0;
        while start_off + TS_PACKET_SIZE <= data.len() && data[start_off] != 0x47 {
            start_off += 1;
        }

        if start_off + TS_PACKET_SIZE <= data.len() && data[start_off] == 0x47 {
            let anchors = extract_pcr_anchors(data, start_off);
            let use_pcr = anchors.len() >= 2 && (anchors[anchors.len() - 1].time_sec - anchors[0].time_sec) > 0.05;

            if use_pcr {
                let first_pcr = anchors[0].time_sec;
                let last_pcr = anchors[anchors.len() - 1].time_sec;
                let pcr_span = (last_pcr - first_pcr).max(0.1);
                let now_sec = master_clock.elapsed().as_secs_f64();

                // Разрыв PCR считаем только при реальном скачке > 2.5 с или отмотке назад < -0.25 с
                let pcr_discont = prev_seg_last_pcr
                    .map(|p| (first_pcr - p) > 2.5 || (first_pcr - p) < -0.25)
                    .unwrap_or(false);
                let drift = (first_pcr - stream_time_offset_sec) - now_sec;

                if !clock_anchored || pcr_discont || drift.abs() > 2.5 {
                    stream_time_offset_sec = first_pcr - now_sec;
                    clock_anchored = true;
                } else if remaining >= target_prebuf + 6 {
                    stream_time_offset_sec += pcr_span * 0.030;
                } else if remaining >= target_prebuf + 3 {
                    stream_time_offset_sec += pcr_span * 0.015;
                } else if remaining < target_prebuf.saturating_sub(1).max(1) {
                    stream_time_offset_sec -= pcr_span * 0.015;
                }
                prev_seg_last_pcr = Some(last_pcr);

                sh.set_status(if sh.using_backup.load(Ordering::Relaxed) {
                    "BACKUP (PCR Sync)"
                } else {
                    "Вещание (PCR Sync)"
                });

                let mut anchor_idx = 0;
                let mut off = start_off;
                let last_anchor = anchors[anchors.len() - 1];
                let avg_bytes_per_sec = ((last_anchor.byte_offset - anchors[0].byte_offset) as f64 / pcr_span).max(1000.0);

                while off + TS_PACKET_SIZE <= data.len()
                    && !sh.stop_requested.load(Ordering::SeqCst)
                    && !sh.watchdog_reset.load(Ordering::SeqCst)
                {
                    if data[off] != 0x47 {
                        off += 1;
                        continue;
                    }
                    ts_analyzer.inspect_packet(&data[off..off + TS_PACKET_SIZE]);
                    udp_packet[udp_fill..udp_fill + TS_PACKET_SIZE]
                        .copy_from_slice(&data[off..off + TS_PACKET_SIZE]);
                    udp_fill += TS_PACKET_SIZE;
                    off += TS_PACKET_SIZE;

                    if udp_fill == UDP_PAYLOAD_SIZE {
                        while anchor_idx + 1 < anchors.len() - 1 && off > anchors[anchor_idx + 1].byte_offset {
                            anchor_idx += 1;
                        }
                        let pkt_stream_sec = if off <= last_anchor.byte_offset {
                            let a0 = anchors[anchor_idx];
                            let a1 = anchors[anchor_idx + 1];
                            let span_bytes = (a1.byte_offset as f64 - a0.byte_offset as f64).max(1.0);
                            let frac = ((off as f64 - a0.byte_offset as f64) / span_bytes).clamp(0.0, 1.0);
                            a0.time_sec + frac * (a1.time_sec - a0.time_sec)
                        } else {
                            // Плавная экстраполяция хвоста сегмента после последнего PCR (без скачков и бурстов)
                            let tail_sec = ((off - last_anchor.byte_offset) as f64 / avg_bytes_per_sec).min(2.0);
                            last_anchor.time_sec + tail_sec
                        };
                        let target_wall_sec = pkt_stream_sec - stream_time_offset_sec;

                        wait_until_wall_seconds(&sh, &master_clock, target_wall_sec);
                        send_raw_udp(&sh, &socket, &remote_addr, &udp_packet);
                        udp_fill = 0;
                    }
                }
                next_synthetic_sec = master_clock.elapsed().as_secs_f64();
            } else {
                sh.set_status(if sh.using_backup.load(Ordering::Relaxed) {
                    "BACKUP (EXTINF)"
                } else {
                    "Вещание (EXTINF)"
                });
                let speed_mult = if remaining >= target_prebuf + 4 {
                    1.025
                } else if remaining >= target_prebuf + 2 {
                    1.012
                } else if remaining < target_prebuf.saturating_sub(1).max(1) {
                    0.985
                } else {
                    1.0
                };
                let seg_dur = seg.duration_sec.max(0.1) / speed_mult;
                let now_sec = master_clock.elapsed().as_secs_f64();
                if !clock_anchored || (next_synthetic_sec - now_sec).abs() > 2.5 {
                    next_synthetic_sec = now_sec;
                    clock_anchored = true;
                }
                let total_dgrams = ((data.len() - start_off) / UDP_PAYLOAD_SIZE).max(1);
                let sec_per_dgram = seg_dur / total_dgrams as f64;
                let mut off = start_off;

                while off + TS_PACKET_SIZE <= data.len()
                    && !sh.stop_requested.load(Ordering::SeqCst)
                    && !sh.watchdog_reset.load(Ordering::SeqCst)
                {
                    if data[off] != 0x47 {
                        off += 1;
                        continue;
                    }
                    ts_analyzer.inspect_packet(&data[off..off + TS_PACKET_SIZE]);
                    udp_packet[udp_fill..udp_fill + TS_PACKET_SIZE]
                        .copy_from_slice(&data[off..off + TS_PACKET_SIZE]);
                    udp_fill += TS_PACKET_SIZE;
                    off += TS_PACKET_SIZE;

                    if udp_fill == UDP_PAYLOAD_SIZE {
                        next_synthetic_sec += sec_per_dgram;
                        wait_until_wall_seconds(&sh, &master_clock, next_synthetic_sec);
                        send_raw_udp(&sh, &socket, &remote_addr, &udp_packet);
                        udp_fill = 0;
                    }
                }
            }

            sh.cc_errors.store(ts_analyzer.cc_errors(), Ordering::Relaxed);
            sh.active_pids.store(ts_analyzer.active_pids(), Ordering::Relaxed);
        }
    }
}

fn wait_until_wall_seconds(sh: &ChannelShared, clock: &Instant, mut target_sec: f64) {
    let now_sec = clock.elapsed().as_secs_f64();
    let mut rem_ms = (target_sec - now_sec) * 1000.0;
    if rem_ms <= 0.0 {
        return;
    }
    if rem_ms > 250.0 {
        rem_ms = 250.0;
        target_sec = now_sec + 0.250;
    }
    if rem_ms >= 2.0 {
        let sleep_ms = (rem_ms - 1.2) as u64;
        if sleep_ms > 0 {
            std::thread::sleep(Duration::from_millis(sleep_ms));
        }
    }
    while clock.elapsed().as_secs_f64() < target_sec
        && !sh.stop_requested.load(Ordering::Relaxed)
        && !sh.watchdog_reset.load(Ordering::Relaxed)
    {
        std::hint::spin_loop();
    }
}

fn send_raw_udp(sh: &ChannelShared, socket: &Socket, remote: &SocketAddr, buf: &[u8]) {
    match socket.send_to(buf, &(*remote).into()) {
        Ok(sent) => {
            let s = sent as u64;
            sh.window_bytes_out.fetch_add(s, Ordering::Relaxed);
            sh.total_bytes_out.fetch_add(s, Ordering::Relaxed);
            sh.total_packets_out.fetch_add(1, Ordering::Relaxed);
            sh.last_output_ms.store(now_ms(), Ordering::Relaxed);
        }
        Err(e) => {
            // Игнорируем временные ошибки Winsock (10035 WOULDBLOCK, 10054 CONNRESET, 10055 NOBUFS)
            let code = e.raw_os_error().unwrap_or(0);
            if code == 10035 || code == 10054 || code == 10055 {
                return;
            }
            sh.set_status(format!("Ошибка UDP ({})", code));
            let now = now_ms();
            let prev = sh.last_udp_err_log_ms.load(Ordering::Relaxed);
            if now - prev >= 5000 {
                sh.last_udp_err_log_ms.store(now, Ordering::Relaxed);
                let name = sh.cfg.lock().map(|g| g.name.clone()).unwrap_or_default();
                logger::error(format!("[{}] Ошибка отправки UDP (OS {}): {}", name, code, e));
            }
        }
    }
}

fn parse_bind_ip(s: &str) -> Ipv4Addr {
    let token = s.split_whitespace().next().unwrap_or("0.0.0.0").trim();
    token.parse::<Ipv4Addr>().unwrap_or(Ipv4Addr::UNSPECIFIED)
}

pub fn parse_udp_url(udp_url: &str, default_ttl: u32) -> Result<(Ipv4Addr, u16, u32), String> {
    let mut s = udp_url.trim();
    if let Some(rest) = s.strip_prefix("udp://") {
        s = rest;
    }
    if let Some(rest) = s.strip_prefix('@') {
        s = rest;
    }
    let mut ttl = default_ttl;
    if let Some((host_port, query)) = s.split_once('?') {
        s = host_port;
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k.eq_ignore_ascii_case("ttl") {
                    if let Ok(t) = v.parse::<u32>() {
                        ttl = t.clamp(1, 255);
                    }
                }
            }
        }
    }
    if let Some((hp, _)) = s.split_once('/') {
        s = hp;
    }
    let (ip_str, port_str) = s
        .split_once(':')
        .ok_or_else(|| format!("Некорректный UDP URL: {}", udp_url))?;
    let ip: Ipv4Addr = ip_str.trim().parse().map_err(|_| format!("Некорректный IP: {}", ip_str))?;
    let port: u16 = port_str.trim().parse().map_err(|_| format!("Некорректный порт: {}", port_str))?;
    if port == 0 {
        return Err("Порт не может быть 0".to_string());
    }
    Ok((ip, port, ttl))
}

fn parse_custom_headers(raw: &str) -> HeaderMap {
    let mut map = HeaderMap::new();
    for part in raw.split(';') {
        if let Some((k, v)) = part.split_once(':') {
            if let (Ok(hn), Ok(hv)) = (
                HeaderName::from_bytes(k.trim().as_bytes()),
                HeaderValue::from_str(v.trim()),
            ) {
                map.insert(hn, hv);
            }
        }
    }
    map
}

fn resolve_url(base_url: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    if let Ok(base) = Url::parse(base_url) {
        if let Ok(joined) = base.join(rel) {
            return joined.to_string();
        }
    }
    rel.to_string()
}

fn try_pick_best_variant_playlist(text: &str, base_url: &str) -> Option<String> {
    if !text.contains("#EXT-X-STREAM-INF") {
        return None;
    }
    let lines: Vec<&str> = text.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    let mut best_bw: i64 = -1;
    let mut best_url: Option<String> = None;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("#EXT-X-STREAM-INF") {
            let mut bw: i64 = 0;
            if let Some(idx) = line.find("BANDWIDTH=") {
                let digits: String = line[idx + 10..].chars().take_while(|c| c.is_ascii_digit()).collect();
                bw = digits.parse::<i64>().unwrap_or(0);
            }
            for j in (i + 1)..lines.len() {
                if !lines[j].starts_with('#') {
                    if bw >= best_bw {
                        best_bw = bw;
                        best_url = Some(resolve_url(base_url, lines[j]));
                    }
                    i = j;
                    break;
                }
            }
        }
        i += 1;
    }
    best_url
}

fn parse_media_playlist(
    text: &str,
    base_url: &str,
) -> (Vec<HlsSegmentItem>, f64, Option<i64>, bool, bool) {
    let mut target_dur = 2.0;
    let mut media_seq: Option<i64> = None;
    let mut cur_seq: u64 = 0;
    let mut has_endlist = false;
    let mut has_discontinuity = false;
    let mut next_dur = 2.0;
    let mut cur_key: Option<HlsKeyInfo> = None;
    let mut list = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            if let Ok(td) = rest.trim().parse::<f64>() {
                if td > 0.1 {
                    target_dur = td;
                }
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            if let Ok(ms) = rest.trim().parse::<i64>() {
                media_seq = Some(ms);
                cur_seq = ms.max(0) as u64;
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            cur_key = Some(parse_key_tag(rest, base_url));
        } else if line.starts_with("#EXT-X-ENDLIST") {
            has_endlist = true;
        } else if line.starts_with("#EXT-X-DISCONTINUITY") {
            has_discontinuity = true;
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let val = rest.split(',').next().unwrap_or("").trim();
            if let Ok(d) = val.parse::<f64>() {
                if d > 0.01 {
                    next_dur = d;
                }
            }
        } else if !line.starts_with('#') {
            list.push(HlsSegmentItem {
                url: resolve_url(base_url, line),
                duration_sec: next_dur,
                sequence: cur_seq,
                key: cur_key.clone(),
            });
            cur_seq += 1;
            next_dur = target_dur;
        }
    }
    (list, target_dur, media_seq, has_endlist, has_discontinuity)
}

fn parse_key_tag(attrs: &str, base_url: &str) -> HlsKeyInfo {
    let mut method = "NONE".to_string();
    let mut key_uri = None;
    let mut explicit_iv = None;

    if let Some(idx) = attrs.find("METHOD=") {
        let rest = &attrs[idx + 7..];
        method = rest.split(',').next().unwrap_or("NONE").trim().to_string();
    }
    if let Some(idx) = attrs.find("URI=\"") {
        let rest = &attrs[idx + 5..];
        if let Some(end) = rest.find('"') {
            key_uri = Some(resolve_url(base_url, &rest[..end]));
        }
    }
    if let Some(idx) = attrs.find("IV=0x") {
        let hex: String = attrs[idx + 5..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if hex.len() == 32 {
            let mut iv = [0u8; 16];
            let mut ok = true;
            for i in 0..16 {
                if let Ok(b) = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
                    iv[i] = b;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok {
                explicit_iv = Some(iv);
            }
        }
    }
    HlsKeyInfo {
        method,
        key_uri,
        explicit_iv,
    }
}

fn remember_segment(url: &str, set: &mut HashSet<String>, q: &mut VecDeque<String>) {
    if set.insert(url.to_string()) {
        q.push_back(url.to_string());
        while q.len() > 200 {
            if let Some(old) = q.pop_front() {
                set.remove(&old);
            }
        }
    }
}
