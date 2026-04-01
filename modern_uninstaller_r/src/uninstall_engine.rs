use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sysinfo::{ProcessesToUpdate, Signal, System};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::Foundation::CloseHandle;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, KEY_WRITE};

use crate::model::InstallerInfo;
use crate::resources;
use crate::util::{normalize_path, shortcut_paths};

const UNINSTALL_REGISTRY_ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
pub struct LockingProcessInfo {
    pub pid: u32,
    pub name: String,
}

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
    pub main_file: String,
    pub is_64: bool,
}

#[derive(Clone, Debug, Default)]
struct ExistingInstall {
    installed_path: Option<PathBuf>,
    main_file: Option<String>,
    display_name: Option<String>,
}

pub fn resolve_uninstall_target(info: &InstallerInfo) -> Result<UninstallTarget> {
    let existing = read_existing_install(info);
    let install_path = existing
        .installed_path
        .ok_or_else(|| anyhow::anyhow!("installed program was not found"))?;
    let main_file = existing
        .main_file
        .unwrap_or_else(|| info.can_execute_path.clone());
    let app_name = existing
        .display_name
        .unwrap_or_else(|| info.display_name.clone());

    Ok(UninstallTarget {
        app_name,
        install_path,
        main_file,
        is_64: info.is_64,
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
    report_progress(ProgressState::new(35, "Stopping running application processes"));
    terminate_processes_by_path(
        &target.install_path.join(&target.main_file),
        &mut confirm_terminate,
    )
    .context("failed while terminating target processes, uninstall aborted")?;

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
    let main_file = entry
        .get_value::<String, _>("MainFile")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let display_name = entry
        .get_value::<String, _>("DisplayName")
        .ok()
        .filter(|value| !value.trim().is_empty());

    ExistingInstall {
        installed_path,
        main_file,
        display_name,
    }
}

fn uninstall_entry_name() -> String {
    format!("{{{}}}_ModernInstaller", resources::application_uuid())
}

fn terminate_processes_by_path<C>(executable_path: &Path, confirm_terminate: &mut C) -> Result<()>
where
    C: FnMut(&[LockingProcessInfo]) -> Result<bool>,
{
    let processes_to_terminate = collect_processes_by_executable_path(executable_path);
    if !processes_to_terminate.is_empty() && !confirm_terminate(&processes_to_terminate)? {
        bail!("uninstall cancelled");
    }

    let target = normalize_path(executable_path);
    let current_pid = std::process::id();
    for _ in 0..10 {
        let mut system = System::new_all();
        system.refresh_processes(ProcessesToUpdate::All, true);

        let mut matched_any = false;
        for process in system.processes().values() {
            let pid = process.pid().as_u32();
            if pid == 0 || pid == current_pid {
                continue;
            }
            let Some(exe) = process.exe() else {
                continue;
            };
            if normalize_path(exe) != target {
                continue;
            }
            matched_any = true;
            let killed = process
                .kill_with(Signal::Kill)
                .or_else(|| Some(process.kill()))
                .unwrap_or(false);
            if !killed {
                let _ = kill_by_pid_fallback(pid);
            }
        }

        if !matched_any {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }

    bail!("failed to terminate target process")
}

fn collect_processes_by_executable_path(executable_path: &Path) -> Vec<LockingProcessInfo> {
    let target = normalize_path(executable_path);
    if target.is_empty() {
        return Vec::new();
    }

    let current_pid = std::process::id();
    let mut system = System::new_all();
    system.refresh_processes(ProcessesToUpdate::All, true);

    let mut infos = Vec::new();
    let mut seen_pids = HashSet::new();
    for process in system.processes().values() {
        let pid = process.pid().as_u32();
        if pid == 0 || pid == current_pid || !seen_pids.insert(pid) {
            continue;
        }
        let Some(exe) = process.exe() else {
            continue;
        };
        if normalize_path(exe) != target {
            continue;
        }

        infos.push(LockingProcessInfo {
            pid,
            name: process.name().to_string_lossy().to_string(),
        });
    }

    infos.sort_by_key(|info| info.pid);
    infos
}

#[cfg(windows)]
fn kill_by_pid_fallback(pid: u32) -> bool {
    if pid == 0 || pid == std::process::id() {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return false;
    }

    let terminated = unsafe { TerminateProcess(handle, 1) != 0 };
    unsafe {
        CloseHandle(handle);
    }
    terminated
}

#[cfg(not(windows))]
fn kill_by_pid_fallback(_pid: u32) -> bool {
    false
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
