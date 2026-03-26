use serde::Deserialize;

use crate::version::LooseVersion;

#[derive(Clone, Debug, Deserialize)]
pub struct InstallerInfo {
    #[serde(rename = "DisplayIcon")]
    pub display_icon: String,
    #[serde(rename = "DisplayName")]
    pub display_name: String,
    #[serde(rename = "DisplayVersion")]
    pub display_version: String,
    #[serde(rename = "Publisher")]
    pub publisher: String,
    #[serde(rename = "CanExecutePath")]
    pub can_execute_path: String,
    #[serde(rename = "Is64")]
    pub is_64: bool,
    #[serde(rename = "InstallPackages", alias = "Packages", default)]
    pub install_packages: Vec<InstallPackageRule>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InstallPackageRule {
    #[serde(rename = "Package", alias = "Archive", alias = "File")]
    pub package: String,
    #[serde(rename = "Target", alias = "InstallTo", alias = "Destination")]
    pub target: String,
}

impl InstallerInfo {
    pub fn install_version(&self) -> Option<LooseVersion> {
        LooseVersion::parse(&self.display_version)
    }
}
