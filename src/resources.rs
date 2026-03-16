use anyhow::{Context, Result};

use crate::model::InstallerInfo;

const INFO_JSON: &str = include_str!("../installer_assets/info.json");
const AGREEMENT_TEXT: &str = include_str!("../installer_assets/Agreement.txt");
const APPLICATION_UUID: &str = include_str!("../installer_assets/ApplicationUUID");
const APP_ZIP: &[u8] = include_bytes!("../installer_assets/App.zip");
const EMBEDDED_UNINSTALLER: &[u8] =
    include_bytes!("../installer_assets/ModernInstaller.Uninstaller.exe");

pub fn installer_info() -> Result<InstallerInfo> {
    serde_json::from_str(INFO_JSON).context("failed to parse installer info.json")
}

pub fn app_zip() -> &'static [u8] {
    APP_ZIP
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

pub fn embedded_uninstaller_exe() -> &'static [u8] {
    EMBEDDED_UNINSTALLER
}
