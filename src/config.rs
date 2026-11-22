use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;

pub const APP_VERSION: &str = "6.0";
pub const WINDOW_TITLE: &str = "Hls2Udp v6.0 (Rust) — Шлюз HLS -> UDP/Multicast";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub input_nic_ip: String,
    pub primary_hls_url: String,
    pub backup_hls_url: String,
    pub output_nic_ip: String,
    pub output_udp_url: String,
    pub multicast_ttl: u32,
    pub dscp_value: u32,
    pub watchdog_timeout_sec: u64,
    pub prebuffer_segments: usize,
    pub custom_headers: String,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            id: "ch1".to_string(),
            name: "Канал 1".to_string(),
            enabled: true,
            input_nic_ip: "0.0.0.0".to_string(),
            primary_hls_url: "http://localhost:8080/live/stream.m3u8".to_string(),
            backup_hls_url: String::new(),
            output_nic_ip: "0.0.0.0".to_string(),
            output_udp_url: "udp://239.0.0.68:5000".to_string(),
            multicast_ttl: 16,
            dscp_value: 34,
            watchdog_timeout_sec: 12,
            prebuffer_segments: 4,
            custom_headers: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub http_monitor_enabled: bool,
    pub http_monitor_port: u16,
    pub telegram_enabled: bool,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub minimize_to_tray_on_start: bool,
    pub channels: Vec<ChannelConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            http_monitor_enabled: true,
            http_monitor_port: 8088,
            telegram_enabled: false,
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            minimize_to_tray_on_start: false,
            channels: vec![ChannelConfig::default()],
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NicInfo {
    pub ip: String,
    pub label: String,
}

pub fn get_data_dir() -> PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("data")))
        .unwrap_or_else(|| PathBuf::from("data"));
    if !dir.exists() {
        let _ = fs::create_dir_all(&dir);
    }
    dir
}

pub fn get_config_path() -> PathBuf {
    // Сохраняем имя файла hls2udp_v5.json для бесшовной миграции каналов из версий 5.x
    get_data_dir().join("hls2udp_v5.json")
}

impl AppConfig {
    pub fn load() -> Self {
        let path = get_config_path();
        let bak_path = path.with_extension("json.bak");

        for candidate in [&path, &bak_path] {
            if candidate.exists() {
                if let Ok(raw) = fs::read_to_string(candidate) {
                    if let Ok(mut cfg) = serde_json::from_str::<AppConfig>(&raw) {
                        if cfg.channels.is_empty() {
                            cfg.channels.push(ChannelConfig::default());
                        }
                        cfg.sanitize();
                        return cfg;
                    }
                }
            }
        }

        let cfg = AppConfig::default();
        let _ = cfg.save();
        cfg
    }

    pub fn sanitize(&mut self) {
        if self.http_monitor_port < 80 {
            self.http_monitor_port = 8088;
        }
        for (i, ch) in self.channels.iter_mut().enumerate() {
            if ch.id.trim().is_empty() {
                ch.id = format!("ch{}", i + 1);
            }
            if ch.name.trim().is_empty() {
                ch.name = format!("Канал {}", i + 1);
            }
            ch.multicast_ttl = ch.multicast_ttl.clamp(1, 255);
            ch.dscp_value = ch.dscp_value.min(63);
            ch.watchdog_timeout_sec = ch.watchdog_timeout_sec.clamp(3, 300);
            ch.prebuffer_segments = ch.prebuffer_segments.clamp(1, 15);
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for ch in &self.channels {
            let pri = ch.primary_hls_url.trim();
            if !pri.starts_with("http://") && !pri.starts_with("https://") {
                return Err(format!(
                    "Канал '{}': Primary HLS URL должен начинаться с http:// или https://",
                    ch.name
                ));
            }
            let bkp = ch.backup_hls_url.trim();
            if !bkp.is_empty() && !bkp.starts_with("http://") && !bkp.starts_with("https://") {
                return Err(format!(
                    "Канал '{}': Backup HLS URL должен начинаться с http:// или https://",
                    ch.name
                ));
            }
            if crate::worker::parse_udp_url(&ch.output_udp_url, ch.multicast_ttl).is_err() {
                return Err(format!(
                    "Канал '{}': некорректный адрес UDP '{}'. Пример: udp://239.0.0.68:5000",
                    ch.name, ch.output_udp_url
                ));
            }
        }
        Ok(())
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = get_data_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .map_err(|e| format!("Ошибка создания папки {:?}: {}", dir, e))?;
        }
        let path = get_config_path();
        let tmp_path = path.with_extension("json.tmp");
        let bak_path = path.with_extension("json.bak");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;

        // Атомарная запись: .tmp -> sync_all -> резервная копия .bak -> rename
        {
            let mut f = File::create(&tmp_path)
                .map_err(|e| format!("Не удалось создать {:?}: {}", tmp_path, e))?;
            f.write_all(json.as_bytes())
                .map_err(|e| format!("Ошибка записи {:?}: {}", tmp_path, e))?;
            let _ = f.sync_all();
        }
        if path.exists() {
            let _ = fs::copy(&path, &bak_path);
        }
        if fs::rename(&tmp_path, &path).is_err() {
            fs::write(&path, &json)
                .map_err(|e| format!("Не удалось записать {:?}: {}", path, e))?;
            let _ = fs::remove_file(&tmp_path);
        }
        Ok(())
    }
}

pub fn list_nics() -> Vec<NicInfo> {
    let mut res = vec![
        NicInfo {
            ip: "0.0.0.0".to_string(),
            label: "0.0.0.0 — Все интерфейсы (маршрутизация ОС)".to_string(),
        },
        NicInfo {
            ip: "127.0.0.1".to_string(),
            label: "127.0.0.1 — Localhost (только этот ПК / Loopback)".to_string(),
        },
    ];

    if let Ok(ifaces) = local_ip_address::list_afinet_netifas() {
        for (name, ip) in ifaces {
            if let IpAddr::V4(v4) = ip {
                if v4.is_loopback() {
                    continue;
                }
                let ip_str = v4.to_string();
                if !res.iter().any(|n| n.ip == ip_str) {
                    res.push(NicInfo {
                        ip: ip_str.clone(),
                        label: format!("{} — {}", ip_str, name),
                    });
                }
            }
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_clamps_bounds() {
        let mut cfg = AppConfig::default();
        cfg.http_monitor_port = 10;
        cfg.channels[0].multicast_ttl = 999;
        cfg.channels[0].dscp_value = 99;
        cfg.channels[0].watchdog_timeout_sec = 1;
        cfg.channels[0].prebuffer_segments = 100;
        cfg.sanitize();

        assert_eq!(cfg.http_monitor_port, 8088);
        assert_eq!(cfg.channels[0].multicast_ttl, 255);
        assert_eq!(cfg.channels[0].dscp_value, 63);
        assert_eq!(cfg.channels[0].watchdog_timeout_sec, 3);
        assert_eq!(cfg.channels[0].prebuffer_segments, 15);
    }
}
