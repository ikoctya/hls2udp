use eframe::egui::IconData;
use std::sync::Arc;

const ICON_64_RGBA: &[u8] = include_bytes!("../assets/icon_64.rgba");

pub fn app_icon_data() -> Arc<IconData> {
    Arc::new(IconData {
        rgba: ICON_64_RGBA.to_vec(),
        width: 64,
        height: 64,
    })
}
