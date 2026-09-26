use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct InstallerInfo {
    #[serde(rename = "DisplayName")]
    pub display_name: String,
    #[serde(rename = "Is64")]
    pub is_64: bool,
    #[serde(rename = "InstallPackages", alias = "Packages", default)]
    pub install_packages: Vec<InstallPackageRule>,
    #[serde(rename = "UninstallDirectories", default)]
    pub uninstall_directories: Vec<UninstallDirectoryRule>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InstallPackageRule {
    #[serde(rename = "Package", alias = "Archive", alias = "File", default)]
    pub package: String,
    #[serde(rename = "Target", alias = "InstallTo", alias = "Destination")]
    pub target: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UninstallDirectoryRule {
    #[serde(rename = "Target", alias = "InstallTo", alias = "Destination")]
    pub target: String,
}
