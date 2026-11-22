use crate::config::{list_nics, AppConfig, ChannelConfig, NicInfo, APP_VERSION, WINDOW_TITLE};
use crate::logger;
use crate::manager::{send_telegram_sync, BroadcastManager, ManagerTelemetry};
use crate::service;
use crate::worker::ChannelTelemetry;
use eframe::egui;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const BG_DARK: egui::Color32 = egui::Color32::from_rgb(11, 17, 32);
const CARD_BG: egui::Color32 = egui::Color32::from_rgb(23, 33, 52);
const INPUT_BG: egui::Color32 = egui::Color32::from_rgb(15, 23, 42);
const BORDER_COL: egui::Color32 = egui::Color32::from_rgb(51, 65, 85);
const TEXT_MAIN: egui::Color32 = egui::Color32::from_rgb(248, 250, 252);
const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(148, 163, 184);
const COL_GREEN: egui::Color32 = egui::Color32::from_rgb(16, 185, 129);
const COL_YELLOW: egui::Color32 = egui::Color32::from_rgb(245, 158, 11);
const COL_RED: egui::Color32 = egui::Color32::from_rgb(239, 68, 68);
const COL_BLUE: egui::Color32 = egui::Color32::from_rgb(56, 189, 248);
const COL_ACCENT: egui::Color32 = egui::Color32::from_rgb(37, 99, 235);

struct RemotePollState {
    tel: Option<ManagerTelemetry>,
    logs: VecDeque<String>,
    pending_banner: Option<(String, bool)>,
}

pub struct Hls2UdpGuiApp {
    mgr: Option<Arc<BroadcastManager>>,
    rt: tokio::runtime::Handle,
    http_client: reqwest::Client,
    cfg: AppConfig,
    nics: Vec<NicInfo>,
    selected_idx: usize,
    active_tab: usize,
    service_installed: bool,
    status_banner: Option<(String, bool, Instant)>,

    hist_in: VecDeque<f64>,
    hist_out: VecDeque<f64>,
    last_spark_ch: String,
    last_spark_sample: Instant,

    log_lines: Vec<String>,
    last_log_id: u64,
    remote_state: Arc<Mutex<RemotePollState>>,
    hwnd_registered: bool,
}

impl Hls2UdpGuiApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        mgr: Option<Arc<BroadcastManager>>,
        rt: tokio::runtime::Handle,
        cfg: AppConfig,
    ) -> Self {
        Self::setup_style(&cc.egui_ctx);
        let service_installed = service::is_service_installed();
        let remote_state = Arc::new(Mutex::new(RemotePollState {
            tel: None,
            logs: VecDeque::with_capacity(260),
            pending_banner: None,
        }));

        if mgr.is_none() {
            let st = remote_state.clone();
            let port = cfg.http_monitor_port;
            let egui_ctx = cc.egui_ctx.clone();
            rt.spawn(async move {
                let client = match reqwest::Client::builder()
                    .timeout(std::time::Duration::from_millis(400))
                    .build()
                {
                    Ok(c) => c,
                    Err(_) => return,
                };
                let host = std::net::Ipv4Addr::LOCALHOST;
                let status_url = format!("http://{}:{}/status", host, port);
                let mut last_id: u64 = 0;
                loop {
                    let tel = match client.get(&status_url).send().await {
                        Ok(r) => r
                            .text()
                            .await
                            .ok()
                            .and_then(|t| serde_json::from_str::<ManagerTelemetry>(&t).ok()),
                        Err(_) => None,
                    };
                    let logs_url = format!("http://{}:{}/api/logs?since={}", host, port, last_id);
                    let new_logs = match client.get(&logs_url).send().await {
                        Ok(r) => r
                            .text()
                            .await
                            .ok()
                            .and_then(|t| serde_json::from_str::<Vec<logger::LogEntry>>(&t).ok()),
                        Err(_) => None,
                    };
                    if let Ok(mut g) = st.lock() {
                        if let Some(t) = tel {
                            g.tel = Some(t);
                        }
                        if let Some(ls) = new_logs {
                            for l in ls {
                                last_id = last_id.max(l.id);
                                g.logs.push_back(l.text);
                            }
                            while g.logs.len() > 250 {
                                g.logs.pop_front();
                            }
                        }
                    }
                    egui_ctx.request_repaint();
                    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
                }
            });
        }

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            mgr,
            rt,
            http_client,
            cfg,
            nics: list_nics(),
            selected_idx: 0,
            active_tab: 0,
            service_installed,
            status_banner: None,
            hist_in: VecDeque::with_capacity(64),
            hist_out: VecDeque::with_capacity(64),
            last_spark_ch: String::new(),
            last_spark_sample: Instant::now(),
            log_lines: Vec::new(),
            last_log_id: 0,
            remote_state,
            hwnd_registered: false,
        }
    }

    fn setup_style(ctx: &egui::Context) {
        #[cfg(windows)]
        {
            if let Some(windir) = std::env::var_os("WINDIR") {
                let font_path = std::path::PathBuf::from(windir).join("Fonts").join("segoeui.ttf");
                if let Ok(font_bytes) = std::fs::read(font_path) {
                    let mut fonts = egui::FontDefinitions::default();
                    fonts
                        .font_data
                        .insert("segoeui".to_owned(), egui::FontData::from_owned(font_bytes));
                    if let Some(vec) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                        vec.insert(0, "segoeui".to_owned());
                    }
                    ctx.set_fonts(fonts);
                }
            }
        }

        let mut visuals = egui::Visuals::dark();
        visuals.window_fill = BG_DARK;
        visuals.panel_fill = BG_DARK;
        visuals.extreme_bg_color = INPUT_BG;
        visuals.faint_bg_color = CARD_BG;
        visuals.widgets.noninteractive.bg_fill = CARD_BG;
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, BORDER_COL);
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, TEXT_MAIN);
        visuals.widgets.inactive.bg_fill = INPUT_BG;
        visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, BORDER_COL);
        visuals.widgets.inactive.rounding = egui::Rounding::same(5.0_f32);
        visuals.widgets.hovered.rounding = egui::Rounding::same(5.0_f32);
        visuals.widgets.active.rounding = egui::Rounding::same(5.0_f32);
        visuals.selection.bg_fill = COL_ACCENT;
        ctx.set_visuals(visuals);
    }

    fn set_banner(&mut self, msg: impl Into<String>, is_err: bool) {
        self.status_banner = Some((msg.into(), is_err, Instant::now()));
    }

    fn save_config(&mut self) -> bool {
        self.cfg.sanitize();
        if let Err(e) = self.cfg.validate() {
            self.set_banner(e, true);
            return false;
        }
        if let Err(e) = self.cfg.save() {
            self.set_banner(format!("Ошибка сохранения: {}", e), true);
            return false;
        }
        if let Some(ref mgr) = self.mgr {
            mgr.reload_config(self.cfg.clone(), false);
        } else {
            self.send_remote_post("/api/config", serde_json::to_string(&self.cfg).ok());
        }
        logger::info(format!("Конфигурация v{} сохранена.", APP_VERSION));
        self.set_banner("Конфигурация сохранена", false);
        true
    }

    fn send_remote_post(&self, path: &str, body: Option<String>) {
        let port = self.cfg.http_monitor_port;
        let host = std::net::Ipv4Addr::LOCALHOST;
        let url = format!("http://{}:{}{}", host, port, path);
        let client = self.http_client.clone();
        self.rt.spawn(async move {
            let req = client.post(&url);
            let req = if let Some(b) = body {
                req.header("Content-Type", "application/json").body(b)
            } else {
                req
            };
            let _ = req.send().await;
        });
    }

    fn poll_telemetry(&mut self) -> ManagerTelemetry {
        if let Ok(mut g) = self.remote_state.try_lock() {
            if let Some((msg, is_err)) = g.pending_banner.take() {
                self.status_banner = Some((msg, is_err, Instant::now()));
            }
            if self.mgr.is_none() {
                let tel_out = g.tel.clone();
                while let Some(line) = g.logs.pop_front() {
                    self.log_lines.push(line);
                }
                if self.log_lines.len() > 250 {
                    let drain = self.log_lines.len() - 250;
                    self.log_lines.drain(..drain);
                }
                return tel_out.unwrap_or(ManagerTelemetry {
                    version: APP_VERSION.to_string(),
                    running_as_service: true,
                    cpu_process: 0.0,
                    cpu_system: 0.0,
                    last_log_id: 0,
                    http_port: self.cfg.http_monitor_port,
                    channels: Vec::new(),
                });
            }
        }

        if let Some(ref mgr) = self.mgr {
            let new_logs = logger::get_since(self.last_log_id, 60);
            for l in new_logs {
                self.last_log_id = self.last_log_id.max(l.id);
                self.log_lines.push(l.text);
            }
            if self.log_lines.len() > 250 {
                let drain = self.log_lines.len() - 250;
                self.log_lines.drain(..drain);
            }
            return mgr.telemetry();
        }

        ManagerTelemetry {
            version: APP_VERSION.to_string(),
            running_as_service: true,
            cpu_process: 0.0,
            cpu_system: 0.0,
            last_log_id: 0,
            http_port: self.cfg.http_monitor_port,
            channels: Vec::new(),
        }
    }

    fn open_web_ui(&self) {
        #[cfg(windows)]
        crate::tray::win_tray::open_dashboard(self.cfg.http_monitor_port);
    }
}

impl eframe::App for Hls2UdpGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(windows)]
        {
            if !self.hwnd_registered {
                self.hwnd_registered = true;
                let title: Vec<u16> = WINDOW_TITLE
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                unsafe {
                    let hwnd = windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW(
                        std::ptr::null(),
                        title.as_ptr(),
                    );
                    if !hwnd.is_null() {
                        crate::tray::win_tray::set_gui_hwnd(hwnd as isize);
                    }
                }
            }
            if ctx.input(|i| i.viewport().close_requested())
                && !crate::tray::win_tray::ALLOW_EXIT.load(std::sync::atomic::Ordering::SeqCst)
            {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                crate::tray::win_tray::hide_gui_to_tray();
            }
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(450));
        let tel = self.poll_telemetry();

        if self.selected_idx >= self.cfg.channels.len() && !self.cfg.channels.is_empty() {
            self.selected_idx = self.cfg.channels.len() - 1;
        }

        if let Some(ch) = self.cfg.channels.get(self.selected_idx) {
            if self.last_spark_ch != ch.id {
                self.last_spark_ch = ch.id.clone();
                self.hist_in.clear();
                self.hist_out.clear();
            }
            if self.last_spark_sample.elapsed().as_millis() >= 900 {
                self.last_spark_sample = Instant::now();
                let ct = tel.channels.iter().find(|c| c.id == ch.id);
                let in_m = ct.map(|c| c.input_bitrates_mbps).unwrap_or(0.0);
                let out_m = ct.map(|c| c.output_bitrates_mbps).unwrap_or(0.0);
                if self.hist_in.len() >= 60 {
                    self.hist_in.pop_front();
                }
                if self.hist_out.len() >= 60 {
                    self.hist_out.pop_front();
                }
                self.hist_in.push_back(in_m);
                self.hist_out.push_back(out_m);
            }
        }

        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(9, 14, 27))
                    .inner_margin(egui::Margin::symmetric(12.0_f32, 8.0_f32)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "HLS → UDP / MULTICAST GATEWAY  v{} (RUST)",
                            APP_VERSION
                        ))
                        .strong()
                        .size(15.0_f32)
                        .color(TEXT_MAIN),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        #[cfg(windows)]
                        if ui
                            .add(
                                egui::Button::new("В трей")
                                    .fill(egui::Color32::from_rgb(51, 65, 85)),
                            )
                            .clicked()
                        {
                            crate::tray::win_tray::hide_gui_to_tray();
                        }
                        if ui
                            .add(
                                egui::Button::new(format!("Веб-панель : {}", self.cfg.http_monitor_port))
                                    .fill(egui::Color32::from_rgb(30, 41, 59)),
                            )
                            .clicked()
                        {
                            self.open_web_ui();
                        }

                        let (badge_txt, badge_col) = if tel.running_as_service {
                            ("● СЛУЖБА WINDOWS (ДО ЛОГИНА)", COL_GREEN)
                        } else {
                            ("● АВТОНОМНЫЙ РЕЖИМ", COL_BLUE)
                        };
                        ui.label(egui::RichText::new(badge_txt).strong().color(badge_col));

                        if let Some((ref msg, is_err, ts)) = self.status_banner {
                            if ts.elapsed().as_secs() < 5 {
                                ui.label(
                                    egui::RichText::new(msg)
                                        .color(if is_err { COL_RED } else { COL_GREEN }),
                                );
                            }
                        }
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG_DARK).inner_margin(egui::Margin::same(10.0_f32)))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(CARD_BG)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER_COL))
                    .rounding(egui::Rounding::same(6.0_f32))
                    .inner_margin(egui::Margin::same(8.0_f32))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .add(egui::Button::new("+ Канал").fill(COL_ACCENT))
                                .clicked()
                            {
                                let n = self.cfg.channels.len() + 1;
                                let mut c = ChannelConfig::default();
                                c.id = format!(
                                    "ch{}",
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_secs() % 100000)
                                        .unwrap_or(n as u64)
                                );
                                c.name = format!("Канал {}", n);
                                c.output_udp_url = format!("udp://239.0.0.{}:5000", 67 + n);
                                self.cfg.channels.push(c);
                                self.selected_idx = self.cfg.channels.len() - 1;
                                self.save_config();
                            }
                            if ui
                                .add(egui::Button::new("Удалить").fill(egui::Color32::from_rgb(127, 29, 29)))
                                .clicked()
                            {
                                if self.cfg.channels.len() > 1 {
                                    let id = self.cfg.channels[self.selected_idx].id.clone();
                                    if let Some(ref m) = self.mgr {
                                        m.stop_channel(&id);
                                    } else {
                                        self.send_remote_post(&format!("/api/channel/{}/stop", id), None);
                                    }
                                    self.cfg.channels.remove(self.selected_idx);
                                    self.selected_idx = self.selected_idx.min(self.cfg.channels.len() - 1);
                                    self.save_config();
                                } else {
                                    self.set_banner("Должен остаться хотя бы один канал", true);
                                }
                            }

                            ui.separator();

                            if ui
                                .add(egui::Button::new("▶ Старт").fill(COL_GREEN))
                                .clicked()
                            {
                                if self.save_config() {
                                    let id = self.cfg.channels[self.selected_idx].id.clone();
                                    if let Some(ref m) = self.mgr {
                                        m.start_channel(&id);
                                    } else {
                                        self.send_remote_post(&format!("/api/channel/{}/start", id), None);
                                    }
                                }
                            }
                            if ui
                                .add(egui::Button::new("■ Стоп").fill(COL_RED))
                                .clicked()
                            {
                                let id = self.cfg.channels[self.selected_idx].id.clone();
                                if let Some(ref m) = self.mgr {
                                    m.stop_channel(&id);
                                } else {
                                    self.send_remote_post(&format!("/api/channel/{}/stop", id), None);
                                }
                            }
                            if ui
                                .add(egui::Button::new("▶ Все").fill(egui::Color32::from_rgb(5, 150, 105)))
                                .clicked()
                            {
                                if self.save_config() {
                                    if let Some(ref m) = self.mgr {
                                        m.start_all_enabled();
                                    } else {
                                        self.send_remote_post("/api/start_all", None);
                                    }
                                }
                            }
                            if ui
                                .add(egui::Button::new("■ Все").fill(egui::Color32::from_rgb(185, 28, 28)))
                                .clicked()
                            {
                                if let Some(ref m) = self.mgr {
                                    m.stop_all();
                                } else {
                                    self.send_remote_post("/api/stop_all", None);
                                }
                            }

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Обновить NIC").clicked() {
                                    self.nics = list_nics();
                                    self.set_banner("Список сетевых адаптеров обновлён", false);
                                }
                                let svc_lbl = if self.service_installed {
                                    "Удалить службу"
                                } else {
                                    "Установить службу (до логина)"
                                };
                                let svc_col = if self.service_installed {
                                    egui::Color32::from_rgb(180, 83, 9)
                                } else {
                                    egui::Color32::from_rgb(51, 65, 85)
                                };
                                if ui.add(egui::Button::new(svc_lbl).fill(svc_col)).clicked() {
                                    if self.service_installed {
                                        service::uninstall_service();
                                    } else {
                                        self.save_config();
                                        if let Some(ref m) = self.mgr {
                                            m.stop_all();
                                        }
                                        service::install_and_start_service();
                                    }
                                    self.service_installed = service::is_service_installed();
                                }
                            });
                        });

                        ui.add_space(4.0_f32);

                        egui::ScrollArea::vertical().max_height(88.0_f32).show(ui, |ui| {
                            egui::Grid::new("ch_table")
                                .num_columns(9)
                                .spacing([12.0_f32, 4.0_f32])
                                .striped(true)
                                .show(ui, |ui| {
                                    for h in ["Канал", "Выход UDP/MC", "Вход", "Выход", "In Mbps", "Out Mbps", "CC", "Буфер", "Статус"] {
                                        ui.label(egui::RichText::new(h).strong().size(11.0_f32).color(TEXT_MUTED));
                                    }
                                    ui.end_row();

                                    for (idx, ch) in self.cfg.channels.iter().enumerate() {
                                        let ct = tel.channels.iter().find(|c| c.id == ch.id);
                                        let is_sel = idx == self.selected_idx;
                                        if ui.selectable_label(is_sel, &ch.name).clicked() {
                                            self.selected_idx = idx;
                                        }
                                        ui.label(egui::RichText::new(&ch.output_udp_url).monospace().size(11.0_f32));
                                        let (in_s, in_c) = match ct {
                                            Some(c) if c.is_running && c.input_alive => {
                                                if c.using_backup { ("BACKUP", COL_YELLOW) } else { ("OK", COL_GREEN) }
                                            }
                                            Some(c) if c.is_running => ("НЕТ", COL_RED),
                                            _ => ("СТОП", TEXT_MUTED),
                                        };
                                        ui.label(egui::RichText::new(in_s).strong().color(in_c));

                                        let (out_s, out_c) = match ct {
                                            Some(c) if c.is_running && c.output_alive => ("UDP OK", COL_GREEN),
                                            Some(c) if c.is_running => ("WAIT", COL_YELLOW),
                                            _ => ("СТОП", TEXT_MUTED),
                                        };
                                        ui.label(egui::RichText::new(out_s).strong().color(out_c));

                                        ui.label(egui::RichText::new(format!("{:.2}", ct.map(|c| c.input_bitrates_mbps).unwrap_or(0.0))).monospace());
                                        ui.label(egui::RichText::new(format!("{:.2}", ct.map(|c| c.output_bitrates_mbps).unwrap_or(0.0))).monospace());
                                        let cc_err = ct.map(|c| c.cc_errors).unwrap_or(0);
                                        ui.label(egui::RichText::new(cc_err.to_string()).color(if cc_err > 0 { COL_YELLOW } else { TEXT_MUTED }));
                                        ui.label(format!("{}/20", ct.map(|c| c.buffer_depth).unwrap_or(0)));
                                        let st_text = match ct {
                                            Some(c) if c.aes_encrypted => format!("{} [AES]", c.status_text),
                                            Some(c) => c.status_text.clone(),
                                            None => "Остановлен".to_string(),
                                        };
                                        ui.label(st_text);
                                        ui.end_row();
                                    }
                                });
                        });
                    });

                ui.add_space(6.0_f32);

                egui::Frame::none()
                    .fill(CARD_BG)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER_COL))
                    .rounding(egui::Rounding::same(6.0_f32))
                    .inner_margin(egui::Margin::same(8.0_f32))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.active_tab, 0, "Настройки выбранного канала");
                            ui.selectable_value(&mut self.active_tab, 1, "HTTP-мониторинг и Telegram");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(egui::Button::new("Сохранить настройки").fill(COL_ACCENT)).clicked() {
                                    self.save_config();
                                }
                            });
                        });
                        ui.separator();

                        if self.active_tab == 0 {
                            let mut copied_vlc = None;
                            if let Some(ch) = self.cfg.channels.get_mut(self.selected_idx) {
                                ui.horizontal(|ui| {
                                    ui.label("Имя:");
                                    ui.add(egui::TextEdit::singleline(&mut ch.name).desired_width(150.0_f32));
                                    ui.checkbox(&mut ch.enabled, "Автостарт со службой");
                                    ui.label("Буфер (сегм):");
                                    ui.add(egui::DragValue::new(&mut ch.prebuffer_segments).range(1..=15));
                                    ui.label("WD (с):");
                                    ui.add(egui::DragValue::new(&mut ch.watchdog_timeout_sec).range(3..=300));
                                    ui.label("TTL:");
                                    ui.add(egui::DragValue::new(&mut ch.multicast_ttl).range(1..=255));
                                });
                                ui.add_space(3.0_f32);
                                ui.horizontal(|ui| {
                                    ui.label("Входной NIC:");
                                    egui::ComboBox::from_id_salt("in_nic_cb")
                                        .selected_text(nic_label(&self.nics, &ch.input_nic_ip))
                                        .width(330.0_f32)
                                        .show_ui(ui, |ui| {
                                            for n in &self.nics {
                                                ui.selectable_value(&mut ch.input_nic_ip, n.ip.clone(), &n.label);
                                            }
                                        });
                                    ui.label("Выходной NIC:");
                                    egui::ComboBox::from_id_salt("out_nic_cb")
                                        .selected_text(nic_label(&self.nics, &ch.output_nic_ip))
                                        .width(330.0_f32)
                                        .show_ui(ui, |ui| {
                                            for n in &self.nics {
                                                ui.selectable_value(&mut ch.output_nic_ip, n.ip.clone(), &n.label);
                                            }
                                        });
                                });
                                ui.add_space(3.0_f32);
                                ui.horizontal(|ui| {
                                    ui.label("Primary HLS:");
                                    ui.add(egui::TextEdit::singleline(&mut ch.primary_hls_url).desired_width(f32::INFINITY));
                                });
                                ui.add_space(3.0_f32);
                                ui.horizontal(|ui| {
                                    ui.label("Backup HLS:");
                                    ui.add(egui::TextEdit::singleline(&mut ch.backup_hls_url).desired_width(f32::INFINITY));
                                });
                                ui.add_space(3.0_f32);
                                ui.horizontal(|ui| {
                                    ui.label("Выход UDP/MC:");
                                    ui.add(egui::TextEdit::singleline(&mut ch.output_udp_url).desired_width(195.0_f32));
                                    if ui.button("VLC URL").on_hover_text("Скопировать адрес для VLC (udp://@...)").clicked() {
                                        let raw = ch.output_udp_url.trim().trim_start_matches("udp://").trim_start_matches('@');
                                        let vlc_url = format!("udp://@{}", raw);
                                        ctx.copy_text(vlc_url.clone());
                                        copied_vlc = Some(vlc_url);
                                    }
                                    ui.label("DSCP:");
                                    egui::ComboBox::from_id_salt("dscp_cb")
                                        .selected_text(match ch.dscp_value {
                                            32 => "32 — CS4",
                                            34 => "34 — AF41",
                                            46 => "46 — EF",
                                            _ => "0 — Default",
                                        })
                                        .width(105.0_f32)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(&mut ch.dscp_value, 0, "0 — Default");
                                            ui.selectable_value(&mut ch.dscp_value, 32, "32 — CS4");
                                            ui.selectable_value(&mut ch.dscp_value, 34, "34 — AF41");
                                            ui.selectable_value(&mut ch.dscp_value, 46, "46 — EF");
                                        });
                                    ui.label("Заголовки:");
                                    ui.add(egui::TextEdit::singleline(&mut ch.custom_headers).desired_width(f32::INFINITY));
                                });
                            }
                            if let Some(v) = copied_vlc {
                                self.set_banner(format!("Скопировано для VLC: {}", v), false);
                            }
                        } else {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut self.cfg.http_monitor_enabled, "Включить веб-панель и HTTP-мониторинг (/status, /metrics, /health)");
                                ui.label("Порт:");
                                ui.add(egui::DragValue::new(&mut self.cfg.http_monitor_port).range(80..=65534));
                            });
                            ui.add_space(4.0_f32);
                            ui.checkbox(&mut self.cfg.telegram_enabled, "Отправлять оповещения в Telegram при сбоях и восстановлении");
                            ui.add_space(4.0_f32);
                            ui.horizontal(|ui| {
                                ui.label("Bot Token:");
                                ui.add(egui::TextEdit::singleline(&mut self.cfg.telegram_bot_token).desired_width(360.0_f32));
                                ui.label("Chat ID:");
                                ui.add(egui::TextEdit::singleline(&mut self.cfg.telegram_chat_id).desired_width(150.0_f32));
                                if ui.add(egui::Button::new("Тест Telegram").fill(COL_ACCENT)).clicked() {
                                    let tok = self.cfg.telegram_bot_token.trim().to_string();
                                    let cid = self.cfg.telegram_chat_id.trim().to_string();
                                    if tok.is_empty() || cid.is_empty() {
                                        self.set_banner("Укажите Bot Token и Chat ID", true);
                                    } else {
                                        self.set_banner("Отправка теста Telegram...", false);
                                        let st = self.remote_state.clone();
                                        let egui_ctx = ctx.clone();
                                        self.rt.spawn(async move {
                                            let res = send_telegram_sync(
                                                &tok,
                                                &cid,
                                                &format!("[Hls2Udp v{} Rust] Тестовое уведомление — связь работает!", APP_VERSION),
                                            )
                                            .await;
                                            if let Ok(mut g) = st.lock() {
                                                g.pending_banner = Some(match res {
                                                    Ok(()) => ("Тест Telegram отправлен!".to_string(), false),
                                                    Err(e) => (format!("Ошибка Telegram: {}", e), true),
                                                });
                                            }
                                            egui_ctx.request_repaint();
                                        });
                                    }
                                }
                            });
                        }
                    });

                ui.add_space(6.0_f32);

                let sel_ch = self.cfg.channels.get(self.selected_idx);
                let sel_t: ChannelTelemetry = sel_ch
                    .and_then(|c| tel.channels.iter().find(|t| t.id == c.id).cloned())
                    .unwrap_or_else(|| ChannelTelemetry {
                        id: String::new(),
                        name: String::new(),
                        is_running: false,
                        input_alive: false,
                        output_alive: false,
                        using_backup: false,
                        aes_encrypted: false,
                        input_bitrates_mbps: 0.0,
                        output_bitrates_mbps: 0.0,
                        total_bytes_in: 0,
                        total_bytes_out: 0,
                        total_segments_in: 0,
                        total_packets_out: 0,
                        watchdog_resets: 0,
                        cc_errors: 0,
                        active_pids: 0,
                        buffer_depth: 0,
                        uptime_seconds: 0,
                        output_udp_url: String::new(),
                        status_text: "Остановлен".to_string(),
                    });

                ui.columns(4, |cols| {
                    draw_stat_card(
                        &mut cols[0],
                        "ВХОД HLS / TS",
                        if !sel_t.is_running { TEXT_MUTED } else if sel_t.input_alive { if sel_t.using_backup { COL_YELLOW } else { COL_GREEN } } else { COL_RED },
                        fmt_rate(sel_t.input_bitrates_mbps),
                        format!("CC: {} | PID: {} | WD: {}", sel_t.cc_errors, sel_t.active_pids, sel_t.watchdog_resets),
                    );
                    draw_stat_card(
                        &mut cols[1],
                        "ВЫХОД UDP / MULTICAST",
                        if !sel_t.is_running { TEXT_MUTED } else if sel_t.output_alive { COL_GREEN } else { COL_YELLOW },
                        fmt_rate(sel_t.output_bitrates_mbps),
                        format!("UDP: {} | Буфер: {}/20", sel_t.total_packets_out, sel_t.buffer_depth),
                    );
                    draw_stat_card(
                        &mut cols[2],
                        "ПРОЦЕССОР (CPU)",
                        COL_BLUE,
                        format!("Утилита: {:.1}%", tel.cpu_process),
                        format!("Система: {:.1}%", tel.cpu_system),
                    );
                    draw_stat_card(
                        &mut cols[3],
                        "АПТАЙМ / ТРАФИК",
                        TEXT_MAIN,
                        fmt_uptime(sel_t.uptime_seconds),
                        format!("In: {} | Out: {}", fmt_bytes(sel_t.total_bytes_in), fmt_bytes(sel_t.total_bytes_out)),
                    );
                });

                ui.add_space(6.0_f32);
                draw_sparkline(ui, &self.hist_in, &self.hist_out);
                ui.add_space(6.0_f32);

                egui::Frame::none()
                    .fill(CARD_BG)
                    .stroke(egui::Stroke::new(1.0_f32, BORDER_COL))
                    .rounding(egui::Rounding::same(6.0_f32))
                    .inner_margin(egui::Margin::same(6.0_f32))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("ЖУРНАЛ СОБЫТИЙ").strong().size(11.0_f32).color(TEXT_MUTED));
                            ui.label(
                                egui::RichText::new(format!("({})", logger::get_log_file_path().display()))
                                    .size(10.0_f32)
                                    .color(TEXT_MUTED),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Очистить").clicked() {
                                    self.log_lines.clear();
                                }
                                #[cfg(windows)]
                                if ui.button("Файл лога").clicked() {
                                    crate::tray::win_tray::open_log_file();
                                }
                            });
                        });
                        egui::Frame::none()
                            .fill(INPUT_BG)
                            .rounding(egui::Rounding::same(4.0_f32))
                            .inner_margin(egui::Margin::same(4.0_f32))
                            .show(ui, |ui| {
                                let avail_h = ui.available_height().max(90.0_f32);
                                egui::ScrollArea::vertical()
                                    .id_salt("log_scroll")
                                    .max_height(avail_h)
                                    .auto_shrink([false, false])
                                    .stick_to_bottom(true)
                                    .show(ui, |ui| {
                                        if self.log_lines.is_empty() {
                                            ui.label(egui::RichText::new("Ожидание событий...").monospace().size(10.5_f32).color(TEXT_MUTED));
                                        } else {
                                            for line in &self.log_lines {
                                                let col = if line.contains("[ERROR]") {
                                                    egui::Color32::from_rgb(248, 113, 113)
                                                } else if line.contains("[WARN]") {
                                                    egui::Color32::from_rgb(251, 191, 36)
                                                } else {
                                                    egui::Color32::from_rgb(203, 213, 225)
                                                };
                                                ui.label(egui::RichText::new(line).monospace().size(10.5_f32).color(col));
                                            }
                                        }
                                    });
                            });
                    });
            });
    }
}

fn nic_label<'a>(nics: &'a [NicInfo], ip: &'a str) -> &'a str {
    nics.iter()
        .find(|n| n.ip == ip)
        .map(|n| n.label.as_str())
        .unwrap_or(ip)
}

fn draw_stat_card(ui: &mut egui::Ui, title: &str, dot_col: egui::Color32, val: String, sub: String) {
    egui::Frame::none()
        .fill(CARD_BG)
        .stroke(egui::Stroke::new(1.0_f32, BORDER_COL))
        .rounding(egui::Rounding::same(6.0_f32))
        .inner_margin(egui::Margin::same(8.0_f32))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0_f32, 10.0_f32), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.5_f32, dot_col);
                ui.label(egui::RichText::new(title).strong().size(10.5_f32).color(TEXT_MUTED));
            });
            ui.label(egui::RichText::new(val).strong().monospace().size(14.0_f32).color(TEXT_MAIN));
            ui.label(egui::RichText::new(sub).size(10.5_f32).color(TEXT_MUTED));
        });
}

fn draw_sparkline(ui: &mut egui::Ui, hist_in: &VecDeque<f64>, hist_out: &VecDeque<f64>) {
    let desired = egui::vec2(ui.available_width(), 68.0_f32);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, egui::Rounding::same(5.0_f32), INPUT_BG);
    p.rect_stroke(rect, egui::Rounding::same(5.0_f32), egui::Stroke::new(1.0_f32, BORDER_COL));

    let max_v = hist_in
        .iter()
        .chain(hist_out.iter())
        .copied()
        .fold(1.0_f64, f64::max)
        * 1.15;

    let pad_l = 44.0_f32;
    let pad_t = 8.0_f32;
    let pad_b = 6.0_f32;
    let w = (rect.width() - pad_l - 8.0_f32).max(10.0_f32);
    let h = (rect.height() - pad_t - pad_b).max(10.0_f32);

    for s in 0..=2 {
        let y = rect.top() + pad_t + h * (s as f32 / 2.0_f32);
        p.line_segment(
            [egui::pos2(rect.left() + pad_l, y), egui::pos2(rect.right() - 8.0_f32, y)],
            egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(30, 41, 59)),
        );
        let lbl = format!("{:.1}M", max_v * (1.0 - s as f64 / 2.0));
        p.text(
            egui::pos2(rect.left() + 4.0_f32, y - 6.0_f32),
            egui::Align2::LEFT_TOP,
            lbl,
            egui::FontId::monospace(9.5_f32),
            TEXT_MUTED,
        );
    }

    let plot = |arr: &VecDeque<f64>, col: egui::Color32| {
        if arr.len() < 2 {
            return;
        }
        let pts: Vec<egui::Pos2> = arr
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = rect.left() + pad_l + w * (i as f32 / 59.0_f32);
                let norm = (*v / max_v).clamp(0.0, 1.0) as f32;
                let y = rect.top() + pad_t + h * (1.0_f32 - norm);
                egui::pos2(x, y)
            })
            .collect();
        p.add(egui::Shape::line(pts, egui::Stroke::new(2.0_f32, col)));
    };

    plot(hist_in, COL_BLUE);
    plot(hist_out, COL_GREEN);
}

fn fmt_rate(mbps: f64) -> String {
    if mbps >= 1.0 {
        format!("{:.2} Mbps", mbps)
    } else {
        format!("{:.0} kbps", mbps * 1000.0)
    }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.2} GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.1} MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.0} KB", b as f64 / 1024.0)
    } else {
        format!("{} B", b)
    }
}

fn fmt_uptime(sec: u64) -> String {
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    let s = sec % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}
