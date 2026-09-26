use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, KEY_WRITE};

pub use crate::file_locks::LockingProcessInfo;
use crate::file_locks::terminate_processes_locking_directories;
use crate::model::InstallerInfo;
use crate::path_template::{
    remove_configured_directories, resolve_target_path, resolve_uninstall_directory,
};
use crate::resources;
use crate::util::{normalize_path, shortcut_paths};

const UNINSTALL_REGISTRY_ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

#[derive(Clone, Debug)]
pub struct ProgressState {
    pub percent: u8,
    pub detail: String,
}

impl ProgressState {
    fn new(percent: u8, detail: impl Into<String>) -> Self {
        Self {
            percent,
            detail: detail.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UninstallTarget {
    pub app_name: String,
    pub install_path: PathBuf,
    pub is_64: bool,
    pub uninstall_directories: Vec<PathBuf>,
    target_directories: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default)]
struct ExistingInstall {
    installed_path: Option<PathBuf>,
    display_name: Option<String>,
}

pub fn resolve_uninstall_target(info: &InstallerInfo) -> Result<UninstallTarget> {
    let existing = read_existing_install(info);
    let install_path = existing
        .installed_path
        .ok_or_else(|| anyhow::anyhow!("installed program was not found"))?;
    let app_name = existing
        .display_name
        .unwrap_or_else(|| info.display_name.clone());
    let (target_directories, uninstall_directories) =
        collect_target_directories(info, &install_path)?;

    Ok(UninstallTarget {
        app_name,
        install_path,
        is_64: info.is_64,
        uninstall_directories,
        target_directories,
    })
}

fn collect_target_directories(
    info: &InstallerInfo,
    install_path: &Path,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut target_directories =
        Vec::with_capacity(1 + info.install_packages.len() + info.uninstall_directories.len());
    let mut seen_targets = HashSet::new();
    seen_targets.insert(normalize_path(install_path));
    target_directories.push(install_path.to_path_buf());

    for rule in &info.install_packages {
        let path = resolve_target_path(&rule.target, install_path, &info.display_name)
            .with_context(|| format!("invalid target for package {}", rule.package))?;
        if seen_targets.insert(normalize_path(&path)) {
            target_directories.push(path);
        }
    }

    let mut uninstall_directories = Vec::with_capacity(info.uninstall_directories.len());
    let mut seen_uninstall = HashSet::new();
    for rule in &info.uninstall_directories {
        let path = resolve_uninstall_directory(&rule.target, install_path, &info.display_name)
            .with_context(|| format!("invalid UninstallDirectories target: {}", rule.target))?;
        let normalized = normalize_path(&path);
        if seen_targets.insert(normalized.clone()) {
            target_directories.push(path.clone());
        }
        if seen_uninstall.insert(normalized) {
            uninstall_directories.push(path);
        }
    }

    Ok((target_directories, uninstall_directories))
}

pub fn run_uninstall<F, C>(
    target: &UninstallTarget,
    mut report_progress: F,
    mut confirm_terminate: C,
) -> Result<()>
where
    F: FnMut(ProgressState),
    C: FnMut(&[LockingProcessInfo]) -> Result<bool>,
{
    report_progress(ProgressState::new(10, "正在准备卸载"));
    report_progress(ProgressState::new(35, "正在检查占用并结束相关进程"));
    terminate_processes_locking_directories(
        &target.target_directories,
        true,
        &mut confirm_terminate,
    )
    .context("failed while terminating target processes, uninstall aborted")?;

    report_progress(ProgressState::new(60, "正在删除配置的卸载目录"));
    remove_configured_directories(&target.uninstall_directories)
        .context("failed while deleting configured directories, uninstall aborted")?;
    report_progress(ProgressState::new(70, "正在删除安装文件"));
    remove_install_directory(&target.install_path)
        .context("failed while deleting installed files, uninstall aborted")?;

    report_progress(ProgressState::new(90, "正在清理注册表和快捷方式"));
    delete_registry_values(target.is_64)
        .context("failed while deleting uninstall registry entry, uninstall aborted")?;
    remove_shortcuts(&target.app_name)
        .context("failed while deleting shortcuts, uninstall nearly completed")?;

    report_progress(ProgressState::new(100, "卸载完成"));
    Ok(())
}

fn read_existing_install(info: &InstallerInfo) -> ExistingInstall {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(root) =
        hklm.open_subkey_with_flags(UNINSTALL_REGISTRY_ROOT, registry_read_flags(info.is_64))
    else {
        return ExistingInstall::default();
    };
    let Ok(entry) =
        root.open_subkey_with_flags(uninstall_entry_name(), registry_read_flags(info.is_64))
    else {
        return ExistingInstall::default();
    };

    let installed_path = entry
        .get_value::<String, _>("Path")
        .ok()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    let display_name = entry
        .get_value::<String, _>("DisplayName")
        .ok()
        .filter(|value| !value.trim().is_empty());

    ExistingInstall {
        installed_path,
        display_name,
    }
}

fn uninstall_entry_name() -> String {
    format!("{{{}}}_ModernInstaller", resources::application_uuid())
}

fn remove_install_directory(install_path: &Path) -> Result<()> {
    if !install_path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(install_path)?;
    Ok(())
}

fn remove_shortcuts(app_name: &str) -> Result<()> {
    for shortcut in shortcut_paths(app_name) {
        if shortcut.exists() {
            fs::remove_file(shortcut)?;
        }
    }
    Ok(())
}

fn delete_registry_values(is_64_target: bool) -> Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let root =
        hklm.open_subkey_with_flags(UNINSTALL_REGISTRY_ROOT, registry_write_flags(is_64_target))?;
    let _ = root.delete_subkey_all(uninstall_entry_name());
    Ok(())
}

fn registry_view_flag(is_64_target: bool) -> u32 {
    if is_64_target {
        KEY_WOW64_64KEY
    } else {
        KEY_WOW64_32KEY
    }
}

fn registry_read_flags(is_64_target: bool) -> u32 {
    KEY_READ | registry_view_flag(is_64_target)
}

fn registry_write_flags(is_64_target: bool) -> u32 {
    KEY_WRITE | registry_view_flag(is_64_target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_uninstall_directories_join_lock_scan_once() {
        let info: InstallerInfo = serde_json::from_str(
            r#"{
                "DisplayName": "Kitopia",
                "Is64": true,
                "InstallPackages": [
                    { "Package": "app.zip", "Target": "{InstallDir}" },
                    { "Package": "plugins.zip", "Target": "plugins" }
                ],
                "UninstallDirectories": [
                    { "Target": "plugins" },
                    { "Target": "data" }
                ]
            }"#,
        )
        .unwrap();
        let install_path = std::env::temp_dir().join("Kitopia");

        let (target_directories, uninstall_directories) =
            collect_target_directories(&info, &install_path).unwrap();

        assert_eq!(
            target_directories,
            vec![
                install_path.clone(),
                install_path.join("plugins"),
                install_path.join("data"),
            ]
        );
        assert_eq!(
            uninstall_directories,
            vec![install_path.join("plugins"), install_path.join("data")]
        );
    }
}
