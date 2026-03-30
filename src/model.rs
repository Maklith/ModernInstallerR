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
    #[serde(rename = "InstallDependencies", alias = "Dependencies", default)]
    pub install_dependencies: Vec<InstallDependencyRule>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InstallPackageRule {
    #[serde(rename = "Package", alias = "Archive", alias = "File")]
    pub package: String,
    #[serde(rename = "Target", alias = "InstallTo", alias = "Destination")]
    pub target: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InstallDependencyRule {
    #[serde(rename = "Name", alias = "Dependency", alias = "DisplayName")]
    pub name: String,
    #[serde(rename = "Url", alias = "DownloadUrl")]
    pub url: String,
    #[serde(rename = "InstallArgs", alias = "Args", default)]
    pub install_args: Vec<String>,
    #[serde(rename = "FileName", alias = "OutputFile", default)]
    pub file_name: String,
    #[serde(rename = "SkipIfExists", alias = "CheckPath", default)]
    pub skip_if_exists: String,
    #[serde(rename = "RuntimeName", alias = "DotnetRuntimeName", default)]
    pub runtime_name: String,
    #[serde(
        rename = "RuntimeVersionPrefix",
        alias = "DotnetRuntimeVersionPrefix",
        default
    )]
    pub runtime_version_prefix: String,
}

impl InstallerInfo {
    pub fn install_version(&self) -> Option<LooseVersion> {
        LooseVersion::parse(&self.display_version)
    }
}
