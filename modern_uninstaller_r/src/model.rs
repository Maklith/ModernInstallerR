use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct InstallerInfo {
    #[serde(rename = "DisplayName")]
    pub display_name: String,
    #[serde(rename = "Is64")]
    pub is_64: bool,
    #[serde(rename = "UninstallDirectories", default)]
    pub uninstall_directories: Vec<UninstallDirectoryRule>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UninstallDirectoryRule {
    #[serde(rename = "Target", alias = "InstallTo", alias = "Destination")]
    pub target: String,
}
