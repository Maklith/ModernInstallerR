use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct InstallerInfo {
    #[serde(rename = "DisplayName")]
    pub display_name: String,
    #[serde(rename = "Is64")]
    pub is_64: bool,
    #[serde(rename = "InstallPackages", alias = "Packages", default)]
    pub install_packages: Vec<InstallPackageRule>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InstallPackageRule {
    #[serde(rename = "Package", alias = "Archive", alias = "File", default)]
    pub package: String,
    #[serde(rename = "Target", alias = "InstallTo", alias = "Destination")]
    pub target: String,
}
