use anyhow::{Context, Result};
use eframe::egui::IconData;

use crate::model::InstallerInfo;

const INFO_JSON: &str = include_str!("../installer_assets/info.json");
const AGREEMENT_TEXT: &str = include_str!("../installer_assets/Agreement.txt");
const APPLICATION_UUID: &str = include_str!("../installer_assets/ApplicationUUID");
const APP_PACKAGE_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/App.package.gz"));
const APP_PACKAGE_KIND: &str = include_str!(concat!(env!("OUT_DIR"), "/App.package.kind"));
const EMBEDDED_UNINSTALLER_GZ: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/ModernInstaller.Uninstaller.exe.gz"
));
const APP_LOGO_PNG: &[u8] = include_bytes!("../installer_assets/Icon.png");
const INSTALLER_ICON_PNG: &[u8] = include_bytes!("../installer_assets/IconPack.png");
const UNINSTALLER_ICON_PNG: &[u8] = include_bytes!("../installer_assets/IconUninstall.png");

pub fn installer_info() -> Result<InstallerInfo> {
    serde_json::from_str(INFO_JSON).context("failed to parse installer info.json")
}

pub fn app_package_gz() -> &'static [u8] {
    APP_PACKAGE_GZ
}

pub fn app_package_kind() -> &'static str {
    APP_PACKAGE_KIND.trim()
}

pub fn agreement_text() -> &'static str {
    AGREEMENT_TEXT
}

pub fn application_uuid() -> &'static str {
    APPLICATION_UUID.trim()
}

pub fn embedded_info_json() -> &'static [u8] {
    INFO_JSON.as_bytes()
}

pub fn embedded_uninstaller_gz() -> &'static [u8] {
    EMBEDDED_UNINSTALLER_GZ
}

pub fn app_logo_data() -> Result<IconData> {
    eframe::icon_data::from_png_bytes(APP_LOGO_PNG).context("failed to decode app logo png")
}

pub fn installer_icon_data() -> Result<IconData> {
    eframe::icon_data::from_png_bytes(INSTALLER_ICON_PNG)
        .context("failed to decode installer icon png")
}

pub fn uninstaller_icon_data() -> Result<IconData> {
    eframe::icon_data::from_png_bytes(UNINSTALLER_ICON_PNG)
        .context("failed to decode uninstaller icon png")
}
