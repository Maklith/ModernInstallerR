use std::collections::HashSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use sysinfo::{ProcessesToUpdate, Signal, System};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA};
#[cfg(windows)]
use windows_sys::Win32::System::RestartManager::{
    CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmStartSession,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, KEY_WRITE};

use crate::model::InstallerInfo;
use crate::resources;
use crate::util::{normalize_path, shortcut_paths};

const UNINSTALL_REGISTRY_ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

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
    pub is_64: bool,
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
    let target_directories = collect_install_target_directories(info, &install_path)?;

    Ok(UninstallTarget {
        app_name,
        install_path,
        is_64: info.is_64,
        target_directories,
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
    report_progress(ProgressState::new(10, "正在准备卸载"));
    report_progress(ProgressState::new(35, "正在结束正在运行的应用进程"));
    terminate_processes_for_install_targets(&target.target_directories, &mut confirm_terminate)
        .context("failed while terminating target processes, uninstall aborted")?;

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

fn terminate_processes_for_install_targets<C>(
    target_directories: &[PathBuf],
    confirm_terminate: &mut C,
) -> Result<()>
where
    C: FnMut(&[LockingProcessInfo]) -> Result<bool>,
{
    let normalized_target_dirs = target_directories
        .iter()
        .map(|directory| normalize_path(directory))
        .collect::<Vec<_>>();
    let current_pid = std::process::id();
    let mut locked_files = find_locked_files_in_directories(target_directories)?;
    if locked_files.is_empty() {
        return Ok(());
    }

    let initial_locking_pids = find_locking_process_ids(&locked_files).unwrap_or_default();
    let processes_to_terminate =
        collect_locking_process_infos(target_directories, &initial_locking_pids);
    if !processes_to_terminate.is_empty() && !confirm_terminate(&processes_to_terminate)? {
        bail!("uninstall cancelled");
    }

    for attempt in 0..10 {
        let locking_pids = find_locking_process_ids(&locked_files).unwrap_or_default();
        let locking_pid_set = locking_pids.iter().copied().collect::<HashSet<u32>>();
        let mut system = System::new_all();
        system.refresh_processes(ProcessesToUpdate::All, true);

        let mut handled_locking_pids = HashSet::new();
        for process in system.processes().values() {
            let pid = process.pid().as_u32();
            if pid == 0 || pid == current_pid {
                continue;
            }
            let in_locking_pid_set = locking_pid_set.contains(&pid);
            let in_target_directory = process.exe().is_some_and(|exe| {
                let normalized_exe = normalize_path(exe);
                normalized_target_dirs
                    .iter()
                    .any(|target_dir| path_in_directory(&normalized_exe, target_dir))
            });
            if !in_locking_pid_set && !in_target_directory {
                continue;
            }
            if in_locking_pid_set {
                handled_locking_pids.insert(pid);
            }
            let killed = process
                .kill_with(Signal::Kill)
                .or_else(|| Some(process.kill()))
                .unwrap_or(false);
            if !killed {
                let _ = kill_by_pid_fallback(pid);
            }
        }

        for pid in locking_pid_set {
            if pid == 0 || pid == current_pid || handled_locking_pids.contains(&pid) {
                continue;
            }
            let _ = kill_by_pid_fallback(pid);
        }

        thread::sleep(Duration::from_millis(800));
        locked_files = locked_files
            .into_iter()
            .filter(|path| path.is_file() && is_file_locked(path))
            .collect();
        if locked_files.is_empty() {
            return Ok(());
        }
        if attempt % 3 == 2 {
            locked_files = find_locked_files_in_directories(target_directories)?;
            if locked_files.is_empty() {
                return Ok(());
            }
        }
    }

    let remaining_locked_files = find_locked_files_in_directories(target_directories)?;
    let locking_pids = find_locking_process_ids(&remaining_locked_files).unwrap_or_default();
    let process_summary = if locking_pids.is_empty() {
        String::new()
    } else {
        format!(
            "; locking process ids: {}",
            locking_pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let example_files = remaining_locked_files
        .iter()
        .take(3)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "failed to terminate processes locking install target files: {} file(s) still locked{}{}",
        remaining_locked_files.len(),
        if example_files.is_empty() {
            String::new()
        } else {
            format!(" (e.g. {example_files})")
        },
        process_summary,
    )
}

fn collect_install_target_directories(
    info: &InstallerInfo,
    install_path: &Path,
) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::with_capacity(info.install_packages.len() + 1);
    let mut seen_directories = HashSet::new();
    let install_dir = install_path.to_path_buf();
    seen_directories.insert(normalize_path(&install_dir));
    directories.push(install_dir);

    for rule in &info.install_packages {
        let target_dir = resolve_package_target(&rule.target, install_path, info)
            .with_context(|| format!("invalid target for package {}", rule.package))?;
        if seen_directories.insert(normalize_path(&target_dir)) {
            directories.push(target_dir);
        }
    }
    Ok(directories)
}

fn resolve_package_target(
    raw_target: &str,
    install_path: &Path,
    info: &InstallerInfo,
) -> Result<PathBuf> {
    let raw_target = raw_target.trim();
    if raw_target.is_empty() {
        bail!("target path template is empty");
    }
    let install_dir = install_path.to_string_lossy().to_string();
    let mut resolved = raw_target.to_owned();
    replace_placeholder_case_insensitive(&mut resolved, "{InstallDir}", &install_dir);
    replace_placeholder_case_insensitive(&mut resolved, "{InstallPath}", &install_dir);
    replace_placeholder_case_insensitive(&mut resolved, "{DisplayName}", &info.display_name);
    replace_env_placeholder(&mut resolved, "{LocalUserData}", "LOCALAPPDATA")?;
    replace_env_placeholder(&mut resolved, "{LocalAppData}", "LOCALAPPDATA")?;
    replace_env_placeholder(&mut resolved, "%LOCALAPPDATA%", "LOCALAPPDATA")?;
    replace_env_placeholder(&mut resolved, "{AppData}", "APPDATA")?;
    replace_env_placeholder(&mut resolved, "{RoamingAppData}", "APPDATA")?;
    replace_env_placeholder(&mut resolved, "%APPDATA%", "APPDATA")?;
    replace_env_placeholder(&mut resolved, "{ProgramData}", "ProgramData")?;
    replace_env_placeholder(&mut resolved, "%ProgramData%", "ProgramData")?;
    replace_env_placeholder(&mut resolved, "{ProgramFiles}", "ProgramFiles")?;
    replace_env_placeholder(&mut resolved, "%ProgramFiles%", "ProgramFiles")?;
    replace_env_placeholder(&mut resolved, "{ProgramFilesX86}", "ProgramFiles(x86)")?;
    replace_env_placeholder(&mut resolved, "%ProgramFiles(x86)%", "ProgramFiles(x86)")?;
    replace_env_placeholder(&mut resolved, "{UserProfile}", "USERPROFILE")?;
    replace_env_placeholder(&mut resolved, "%USERPROFILE%", "USERPROFILE")?;
    replace_placeholder_case_insensitive(
        &mut resolved,
        "{Temp}",
        &env::temp_dir().to_string_lossy(),
    );
    if has_unresolved_brace_placeholder(&resolved) {
        bail!("unknown placeholder in target path: {raw_target}");
    }
    let mut target_path = PathBuf::from(resolved.trim());
    if target_path.as_os_str().is_empty() {
        bail!("resolved target path is empty");
    }
    if !target_path.is_absolute() {
        target_path = install_path.join(target_path);
    }
    Ok(target_path)
}

fn replace_env_placeholder(target: &mut String, placeholder: &str, env_name: &str) -> Result<()> {
    if !target
        .to_ascii_lowercase()
        .contains(&placeholder.to_ascii_lowercase())
    {
        return Ok(());
    }
    let Some(value) = env::var_os(env_name) else {
        bail!("placeholder {placeholder} requires environment variable {env_name}");
    };
    replace_placeholder_case_insensitive(
        target,
        placeholder,
        &PathBuf::from(value).to_string_lossy(),
    );
    Ok(())
}

fn replace_placeholder_case_insensitive(target: &mut String, placeholder: &str, replacement: &str) {
    let placeholder_lower = placeholder.to_ascii_lowercase();
    let mut remaining = target.as_str();
    let mut output = String::with_capacity(target.len().max(replacement.len()));
    loop {
        let lower_remaining = remaining.to_ascii_lowercase();
        let Some(index) = lower_remaining.find(&placeholder_lower) else {
            output.push_str(remaining);
            break;
        };
        output.push_str(&remaining[..index]);
        output.push_str(replacement);
        remaining = &remaining[index + placeholder.len()..];
    }
    *target = output;
}

fn has_unresolved_brace_placeholder(input: &str) -> bool {
    let mut opened = false;
    for ch in input.chars() {
        if ch == '{' {
            opened = true;
        } else if ch == '}' && opened {
            return true;
        }
    }
    false
}

fn find_locked_files_in_directories(directories: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let current_exe = normalize_path(&env::current_exe().unwrap_or_default());
    let mut locked_files = Vec::new();
    let mut seen_paths = HashSet::new();
    for directory in directories {
        collect_locked_files_recursively(directory, &current_exe, &mut locked_files)?;
    }
    locked_files.retain(|path| seen_paths.insert(normalize_path(path)));
    locked_files.sort();
    Ok(locked_files)
}

fn collect_locked_files_recursively(
    directory: &Path,
    current_exe: &str,
    locked_files: &mut Vec<PathBuf>,
) -> Result<()> {
    if !directory.exists() || !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("failed to read directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_locked_files_recursively(&path, current_exe, locked_files)?;
        } else if file_type.is_file()
            && normalize_path(&path) != current_exe
            && is_file_locked(&path)
        {
            locked_files.push(path);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_file_locked(path: &Path) -> bool {
    let open_result = OpenOptions::new()
        .access_mode(0x8000_0000 | 0x4000_0000 | 0x0001_0000)
        .share_mode(0)
        .open(path);
    match open_result {
        Ok(_) => false,
        Err(error) => matches!(error.raw_os_error(), Some(32) | Some(33)),
    }
}

#[cfg(not(windows))]
fn is_file_locked(_path: &Path) -> bool {
    false
}

#[cfg(windows)]
fn find_locking_process_ids(locked_files: &[PathBuf]) -> Result<Vec<u32>> {
    if locked_files.is_empty() {
        return Ok(Vec::new());
    }
    let mut session_handle = 0u32;
    let mut session_key = [0u16; (CCH_RM_SESSION_KEY as usize) + 1];
    let start_status = unsafe { RmStartSession(&mut session_handle, 0, session_key.as_mut_ptr()) };
    if start_status != 0 {
        bail!("RmStartSession failed with code {start_status}");
    }
    let result = (|| -> Result<Vec<u32>> {
        let wide_paths = locked_files
            .iter()
            .map(|path| {
                path.as_os_str()
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect::<Vec<u16>>()
            })
            .collect::<Vec<_>>();
        let path_ptrs = wide_paths
            .iter()
            .map(|path| path.as_ptr())
            .collect::<Vec<_>>();
        let register_status = unsafe {
            RmRegisterResources(
                session_handle,
                path_ptrs.len() as u32,
                path_ptrs.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            )
        };
        if register_status != 0 {
            bail!("RmRegisterResources failed with code {register_status}");
        }
        let mut process_info_needed = 0u32;
        let mut process_info_count = 0u32;
        let mut reboot_reasons = 0u32;
        let first_get_status = unsafe {
            RmGetList(
                session_handle,
                &mut process_info_needed,
                &mut process_info_count,
                std::ptr::null_mut(),
                &mut reboot_reasons,
            )
        };
        if first_get_status == 0 {
            return Ok(Vec::new());
        }
        if first_get_status != ERROR_MORE_DATA {
            bail!("RmGetList failed with code {first_get_status}");
        }
        let mut process_infos =
            vec![unsafe { std::mem::zeroed::<RM_PROCESS_INFO>() }; process_info_needed as usize];
        process_info_count = process_info_needed;
        let second_get_status = unsafe {
            RmGetList(
                session_handle,
                &mut process_info_needed,
                &mut process_info_count,
                process_infos.as_mut_ptr(),
                &mut reboot_reasons,
            )
        };
        if second_get_status != 0 {
            bail!("RmGetList(second call) failed with code {second_get_status}");
        }
        process_infos.truncate(process_info_count as usize);
        let mut process_ids = process_infos
            .into_iter()
            .map(|info| info.Process.dwProcessId)
            .filter(|pid| *pid != 0)
            .collect::<Vec<_>>();
        process_ids.sort_unstable();
        process_ids.dedup();
        Ok(process_ids)
    })();
    let _ = unsafe { RmEndSession(session_handle) };
    result
}

#[cfg(not(windows))]
fn find_locking_process_ids(_locked_files: &[PathBuf]) -> Result<Vec<u32>> {
    Ok(Vec::new())
}

fn collect_locking_process_infos(
    target_directories: &[PathBuf],
    locking_pids: &[u32],
) -> Vec<LockingProcessInfo> {
    let normalized_target_dirs = target_directories
        .iter()
        .map(|directory| normalize_path(directory))
        .collect::<Vec<_>>();
    let locking_pid_set = locking_pids.iter().copied().collect::<HashSet<u32>>();
    let current_pid = std::process::id();
    let mut system = System::new_all();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let mut infos = Vec::new();
    let mut seen_pids = HashSet::new();
    for process in system.processes().values() {
        let pid = process.pid().as_u32();
        if pid == 0 || pid == current_pid {
            continue;
        }
        let in_locking_pid_set = locking_pid_set.contains(&pid);
        let in_target_directory = process.exe().is_some_and(|exe| {
            let normalized_exe = normalize_path(exe);
            normalized_target_dirs
                .iter()
                .any(|target_dir| path_in_directory(&normalized_exe, target_dir))
        });
        let should_include = if locking_pid_set.is_empty() {
            in_target_directory
        } else {
            in_locking_pid_set || in_target_directory
        };
        if !should_include || !seen_pids.insert(pid) {
            continue;
        }
        infos.push(LockingProcessInfo {
            pid,
            name: process.name().to_string_lossy().to_string(),
        });
    }
    for pid in locking_pids {
        if *pid != 0 && *pid != current_pid && seen_pids.insert(*pid) {
            infos.push(LockingProcessInfo {
                pid: *pid,
                name: "Unknown".to_string(),
            });
        }
    }
    infos.sort_by_key(|info| info.pid);
    infos
}

fn path_in_directory(path: &str, directory: &str) -> bool {
    if path == directory {
        return true;
    }
    let Some(rest) = path.strip_prefix(directory) else {
        return false;
    };
    directory.ends_with('\\')
        || directory.ends_with('/')
        || rest.starts_with('\\')
        || rest.starts_with('/')
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
