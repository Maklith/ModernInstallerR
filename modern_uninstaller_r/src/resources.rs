use anyhow::{Context, Result};
use eframe::egui::IconData;

use crate::model::InstallerInfo;

const INFO_JSON: &str = include_str!("../../installer_assets/info.json");
const APPLICATION_UUID: &str = include_str!("../../installer_assets/ApplicationUUID");
const APP_LOGO_PNG: &[u8] = include_bytes!("../../installer_assets/Icon.png");
const UNINSTALLER_ICON_PNG: &[u8] = include_bytes!("../../installer_assets/IconUninstall.png");

pub fn installer_info() -> Result<InstallerInfo> {
    serde_json::from_str(INFO_JSON).context("failed to parse installer info.json")
}

pub fn application_uuid() -> &'static str {
    APPLICATION_UUID.trim()
}

pub fn app_logo_data() -> Result<IconData> {
    eframe::icon_data::from_png_bytes(APP_LOGO_PNG).context("failed to decode app logo png")
}

pub fn uninstaller_icon_data() -> Result<IconData> {
    eframe::icon_data::from_png_bytes(UNINSTALLER_ICON_PNG)
        .context("failed to decode uninstaller icon png")
}
