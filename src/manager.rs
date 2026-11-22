use crate::config::{AppConfig, APP_VERSION};
use crate::logger;
use crate::worker::{ChannelTelemetry, ChannelWorker};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerTelemetry {
    pub version: String,
    pub running_as_service: bool,
    pub cpu_process: f32,
    pub cpu_system: f32,
    pub last_log_id: u64,
    pub http_port: u16,
    pub channels: Vec<ChannelTelemetry>,
}

pub struct BroadcastManager {
    pub config: Mutex<AppConfig>,
    workers: Mutex<HashMap<String, Arc<ChannelWorker>>>,
    rt: tokio::runtime::Handle,
    cpu_process_x10: AtomicU32,
    cpu_system_x10: AtomicU32,
    tg_throttle: Mutex<HashMap<String, Instant>>,
    running_as_service: bool,
    shutdown: AtomicBool,
}

impl BroadcastManager {
    pub fn new(cfg: AppConfig, rt: tokio::runtime::Handle, running_as_service: bool) -> Arc<Self> {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }

        let mgr = Arc::new(Self {
            config: Mutex::new(cfg.clone()),
            workers: Mutex::new(HashMap::new()),
            rt: rt.clone(),
            cpu_process_x10: AtomicU32::new(0),
            cpu_system_x10: AtomicU32::new(0),
            tg_throttle: Mutex::new(HashMap::new()),
            running_as_service,
            shutdown: AtomicBool::new(false),
        });

        mgr.sync_workers_from_config(&cfg, false);

        let m_clone = mgr.clone();
        rt.spawn(async move {
            metrics_loop(m_clone).await;
        });

        mgr
    }

    pub fn reload_config(&self, new_cfg: AppConfig, start_enabled: bool) {
        if let Ok(mut g) = self.config.lock() {
            *g = new_cfg.clone();
        }
        self.sync_workers_from_config(&new_cfg, start_enabled);
    }

    fn sync_workers_from_config(&self, cfg: &AppConfig, start_enabled: bool) {
        if let Ok(mut workers) = self.workers.lock() {
            let mut keep = HashSet::new();
            for ch in &cfg.channels {
                keep.insert(ch.id.clone());
                if let Some(w) = workers.get(&ch.id) {
                    w.update_config(ch.clone());
                    if start_enabled && ch.enabled {
                        w.start(&self.rt);
                    }
                } else {
                    let w = Arc::new(ChannelWorker::new(ch.clone()));
                    if start_enabled && ch.enabled {
                        w.start(&self.rt);
                    }
                    workers.insert(ch.id.clone(), w);
                }
            }
            let remove_ids: Vec<String> = workers
                .keys()
                .filter(|k| !keep.contains(*k))
                .cloned()
                .collect();
            for id in remove_ids {
                if let Some(w) = workers.remove(&id) {
                    w.stop();
                }
            }
        }
    }

    pub fn start_all_enabled(&self) {
        let cfg = self.config.lock().map(|g| g.clone()).unwrap_or_default();
        if let Ok(workers) = self.workers.lock() {
            for ch in &cfg.channels {
                if ch.enabled {
                    if let Some(w) = workers.get(&ch.id) {
                        w.start(&self.rt);
                    }
                }
            }
        }
    }

    pub fn stop_all(&self) {
        if let Ok(workers) = self.workers.lock() {
            for w in workers.values() {
                w.stop();
            }
        }
    }

    pub fn start_channel(&self, id: &str) {
        if let Ok(workers) = self.workers.lock() {
            if let Some(w) = workers.get(id) {
                w.start(&self.rt);
            }
        }
    }

    pub fn stop_channel(&self, id: &str) {
        if let Ok(workers) = self.workers.lock() {
            if let Some(w) = workers.get(id) {
                w.stop();
            }
        }
    }

    pub fn telemetry(&self) -> ManagerTelemetry {
        let cfg = self.config.lock().map(|g| g.clone()).unwrap_or_default();
        let mut channels = Vec::new();
        if let Ok(workers) = self.workers.lock() {
            for ch in &cfg.channels {
                if let Some(w) = workers.get(&ch.id) {
                    channels.push(w.shared.telemetry());
                }
            }
        }
        ManagerTelemetry {
            version: APP_VERSION.to_string(),
            running_as_service: self.running_as_service,
            cpu_process: self.cpu_process_x10.load(Ordering::Relaxed) as f32 / 10.0,
            cpu_system: self.cpu_system_x10.load(Ordering::Relaxed) as f32 / 10.0,
            last_log_id: logger::get_last_id(),
            http_port: cfg.http_monitor_port,
            channels,
        }
    }

    pub fn send_telegram_alert(&self, key: &str, msg: &str, bypass_throttle: bool) {
        let cfg = self.config.lock().map(|g| g.clone()).unwrap_or_default();
        if !cfg.telegram_enabled
            || cfg.telegram_bot_token.trim().is_empty()
            || cfg.telegram_chat_id.trim().is_empty()
        {
            return;
        }
        if !bypass_throttle {
            if let Ok(mut map) = self.tg_throttle.lock() {
                if let Some(prev) = map.get(key) {
                    if prev.elapsed() < Duration::from_secs(45) {
                        return;
                    }
                }
                map.insert(key.to_string(), Instant::now());
            }
        }

        let token = cfg.telegram_bot_token.trim().to_string();
        let chat_id = cfg.telegram_chat_id.trim().to_string();
        let text = format!("[Hls2Udp v{}] {}", APP_VERSION, msg);
        self.rt.spawn(async move {
            let _ = send_telegram_sync(&token, &chat_id, &text).await;
        });
    }
}

impl Drop for BroadcastManager {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.stop_all();
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

pub async fn send_telegram_sync(token: &str, chat_id: &str, text: &str) -> Result<(), String> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(&url)
        .form(&[("chat_id", chat_id), ("text", text)])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("Telegram HTTP {}", resp.status()));
    }
    Ok(())
}

async fn metrics_loop(mgr: Arc<BroadcastManager>) {
    let mut sys = System::new_all();
    let pid = Pid::from_u32(std::process::id());
    let mut last_tick = Instant::now();

    while !mgr.shutdown.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if mgr.shutdown.load(Ordering::Relaxed) {
            break;
        }
        let elapsed = last_tick.elapsed().as_secs_f64().max(0.05);
        last_tick = Instant::now();

        let mut alerts = Vec::new();
        if let Ok(workers) = mgr.workers.lock() {
            for w in workers.values() {
                if let Some(al) = w.sample_metrics_and_watchdog(elapsed) {
                    alerts.push(al);
                }
            }
        }
        for (k, m, bypass) in alerts {
            mgr.send_telegram_alert(&k, &m, bypass);
        }

        sys.refresh_cpu_usage();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), false);
        let sys_cpu = sys.global_cpu_usage();
        let proc_cpu = sys
            .process(pid)
            .map(|p| p.cpu_usage() / sys.cpus().len().max(1) as f32)
            .unwrap_or(0.0);

        mgr.cpu_system_x10.store((sys_cpu.clamp(0.0, 100.0) * 10.0) as u32, Ordering::Relaxed);
        mgr.cpu_process_x10.store((proc_cpu.clamp(0.0, 100.0) * 10.0) as u32, Ordering::Relaxed);
    }
}
