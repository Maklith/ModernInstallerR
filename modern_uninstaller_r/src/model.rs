use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct InstallerInfo {
    #[serde(rename = "DisplayName")]
    pub display_name: String,
    #[serde(rename = "CanExecutePath")]
    pub can_execute_path: String,
    #[serde(rename = "Is64")]
    pub is_64: bool,
}
