use std::collections::HashSet;
use std::env;
use std::fs;
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

use crate::util::normalize_path;

#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;
#[cfg(windows)]
const ACCESS_DELETE: u32 = 0x0001_0000;
#[cfg(windows)]
const ACCESS_GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
const ACCESS_GENERIC_WRITE: u32 = 0x4000_0000;

#[derive(Clone, Debug)]
pub struct LockingProcessInfo {
    pub pid: u32,
    pub name: String,
}

pub fn find_locked_files_in_directory(directory: &Path) -> Result<Vec<PathBuf>> {
    if !directory.exists() || !directory.is_dir() {
        return Ok(Vec::new());
    }

    let mut locked_files = Vec::new();
    collect_locked_files_recursively(directory, &mut locked_files)?;
    locked_files.sort();
    Ok(locked_files)
}

pub(crate) fn find_locked_files_in_directories(directories: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut locked_files = Vec::new();
    let mut seen_paths = HashSet::new();
    for directory in directories {
        for file_path in find_locked_files_in_directory(directory)? {
            let normalized = normalize_path(&file_path);
            if seen_paths.insert(normalized) {
                locked_files.push(file_path);
            }
        }
    }
    locked_files.sort();
    Ok(locked_files)
}

fn collect_locked_files_recursively(
    directory: &Path,
    locked_files: &mut Vec<PathBuf>,
) -> Result<()> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("failed to read directory {}", directory.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_locked_files_recursively(&path, locked_files)?;
            continue;
        }
        if file_type.is_file() && is_file_locked(&path) {
            locked_files.push(path);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_file_locked(path: &Path) -> bool {
    let open_result = fs::OpenOptions::new()
        .access_mode(ACCESS_GENERIC_READ | ACCESS_GENERIC_WRITE | ACCESS_DELETE)
        .share_mode(0)
        .open(path);
    match open_result {
        Ok(_) => false,
        Err(error) => matches!(
            error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION) | Some(ERROR_LOCK_VIOLATION)
        ),
    }
}

#[cfg(not(windows))]
fn is_file_locked(_path: &Path) -> bool {
    false
}

#[cfg(windows)]
pub(crate) fn find_locking_process_ids(locked_files: &[PathBuf]) -> Result<Vec<u32>> {
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
pub(crate) fn find_locking_process_ids(_locked_files: &[PathBuf]) -> Result<Vec<u32>> {
    Ok(Vec::new())
}

pub(crate) fn collect_locking_process_infos(
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
        if *pid == 0 || *pid == current_pid || !seen_pids.insert(*pid) {
            continue;
        }
        infos.push(LockingProcessInfo {
            pid: *pid,
            name: "Unknown".to_string(),
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

pub(crate) fn terminate_processes_locking_directories<C>(
    target_directories: &[PathBuf],
    exclude_current_exe: bool,
    confirm_terminate: &mut C,
) -> Result<()>
where
    C: FnMut(&[LockingProcessInfo]) -> Result<bool>,
{
    let normalized_target_dirs = target_directories
        .iter()
        .map(|directory| normalize_path(directory))
        .collect::<Vec<_>>();
    let current_exe = if exclude_current_exe {
        Some(normalize_path(&env::current_exe()?))
    } else {
        None
    };
    let scan_locked_files = || -> Result<Vec<PathBuf>> {
        let mut files = find_locked_files_in_directories(target_directories)?;
        if let Some(current_exe) = current_exe.as_ref() {
            files.retain(|path| normalize_path(path) != *current_exe);
        }
        Ok(files)
    };
    let current_pid = std::process::id();
    let mut locked_files = scan_locked_files()?;
    if locked_files.is_empty() {
        return Ok(());
    }

    let initial_locking_pids = find_locking_process_ids(&locked_files).unwrap_or_default();
    let processes_to_terminate =
        collect_locking_process_infos(target_directories, &initial_locking_pids);
    if !processes_to_terminate.is_empty() && !confirm_terminate(&processes_to_terminate)? {
        bail!("operation cancelled");
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
        locked_files.retain(|path| path.is_file() && is_file_locked(path));
        if locked_files.is_empty() {
            return Ok(());
        }

        if attempt % 3 == 2 {
            locked_files = scan_locked_files()?;
            if locked_files.is_empty() {
                return Ok(());
            }
        }
    }

    let remaining_locked_files = scan_locked_files()?;
    let locking_pids = find_locking_process_ids(&remaining_locked_files).unwrap_or_default();
    let example_files = remaining_locked_files
        .iter()
        .take(3)
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let process_summary = if locking_pids.is_empty() {
        String::new()
    } else {
        let mut system = System::new_all();
        system.refresh_processes(ProcessesToUpdate::All, true);
        let names = locking_pids
            .into_iter()
            .take(6)
            .map(|pid| {
                system
                    .processes()
                    .values()
                    .find(|process| process.pid().as_u32() == pid)
                    .map(|process| format!("{}({pid})", process.name().to_string_lossy()))
                    .unwrap_or_else(|| format!("pid {pid}"))
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("; locking processes: {names}")
    };
    bail!(
        "failed to terminate processes locking target files: {} file(s) still locked{}{}",
        remaining_locked_files.len(),
        if example_files.is_empty() {
            String::new()
        } else {
            format!(" (e.g. {example_files})")
        },
        process_summary
    );
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn scans_locked_files_across_target_directories() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("kitopia-lock-scan-{}-{unique}", std::process::id()));
        let install_dir = root.join("app");
        let configured_dir = root.join("plugins");
        fs::create_dir_all(&install_dir).unwrap();
        fs::create_dir_all(&configured_dir).unwrap();
        let locked_path = configured_dir.join("plugin.dll");
        fs::write(&locked_path, b"plugin").unwrap();

        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked_path)
            .unwrap();
        let target_directories = vec![install_dir, configured_dir];
        assert_eq!(
            find_locked_files_in_directories(&target_directories).unwrap(),
            vec![locked_path]
        );

        drop(handle);
        assert!(
            find_locked_files_in_directories(&target_directories)
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
