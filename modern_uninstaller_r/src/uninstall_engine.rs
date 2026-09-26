use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, KEY_WRITE};

pub use crate::file_locks::LockingProcessInfo;
use crate::file_locks::terminate_processes_locking_directories;
use crate::model::InstallerInfo;
use crate::path_template::{remove_configured_directories, resolve_uninstall_directory};
use crate::resources;
use crate::util::{normalize_path, shortcut_paths};

const UNINSTALL_REGISTRY_ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    let uninstall_directories = info
        .uninstall_directories
        .iter()
        .map(|rule| {
            resolve_uninstall_directory(&rule.target, &install_path, &info.display_name)
                .with_context(|| format!("invalid UninstallDirectories target: {}", rule.target))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(UninstallTarget {
        app_name,
        install_path,
        is_64: info.is_64,
        uninstall_directories,
    })
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
    report_progress(ProgressState::new(10, "Preparing uninstall"));
    report_progress(ProgressState::new(
        35,
        "Checking files and stopping locking processes",
    ));
    let mut target_directories = vec![target.install_path.clone()];
    target_directories.extend_from_slice(&target.uninstall_directories);
    terminate_processes_locking_directories(&target_directories, true, &mut confirm_terminate)
        .context("failed while terminating target processes, uninstall aborted")?;

    report_progress(ProgressState::new(60, "Removing configured directories"));
    remove_configured_directories(&target.uninstall_directories)
        .context("failed while deleting configured directories, uninstall aborted")?;
    report_progress(ProgressState::new(70, "Removing installed files"));
    remove_install_directory(&target.install_path)
        .context("failed while deleting installed files, uninstall aborted")?;

    report_progress(ProgressState::new(90, "Cleaning registry and shortcuts"));
    delete_registry_values(target.is_64)
        .context("failed while deleting uninstall registry entry, uninstall aborted")?;
    remove_shortcuts(&target.app_name)
        .context("failed while deleting shortcuts, uninstall nearly completed")?;

    report_progress(ProgressState::new(100, "Uninstall completed"));
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
    let current_exe = env::current_exe().unwrap_or_default();
    let current_norm = normalize_path(&current_exe);
    let install_norm = normalize_path(install_path);

    if !current_norm.is_empty() && current_norm.starts_with(&install_norm) {
        schedule_directory_cleanup(install_path)?;
        return Ok(());
    }

    fs::remove_dir_all(install_path)?;
    Ok(())
}

fn schedule_directory_cleanup(install_path: &Path) -> Result<()> {
    let quoted_path = install_path.to_string_lossy().replace('\"', "\"\"");
    let cmd_script = format!("timeout /t 2 /nobreak >NUL & rmdir /s /q \"{quoted_path}\"");
    let mut command = Command::new("cmd");
    command.args(["/C", &cmd_script]);
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()?;
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
