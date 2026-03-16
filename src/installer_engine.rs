use std::env;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Local;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use sysinfo::{ProcessesToUpdate, Signal, System};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use zip::ZipArchive;

use crate::model::InstallerInfo;
use crate::resources;
use crate::util::{
    default_install_dir, escape_ps_single_quote, is_windows_64bit_os, normalize_path, path_has_any_content,
    shortcut_paths,
};
use crate::version::LooseVersion;

const UNINSTALL_REGISTRY_ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

#[derive(Clone, Debug, Default)]
pub struct ExistingInstall {
    pub installed_version: Option<LooseVersion>,
    pub installed_path: Option<PathBuf>,
    pub main_file: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InstallResult {
    pub installed_path: PathBuf,
    pub executable_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct UninstallTarget {
    pub app_name: String,
    pub install_path: PathBuf,
    pub main_file: String,
}

pub fn suggested_install_path(info: &InstallerInfo, existing: &ExistingInstall) -> PathBuf {
    existing
        .installed_path
        .clone()
        .unwrap_or_else(|| default_install_dir(&info.display_name))
}

pub fn read_existing_install() -> ExistingInstall {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(root) = hkcu.open_subkey_with_flags(UNINSTALL_REGISTRY_ROOT, KEY_READ) else {
        return ExistingInstall::default();
    };
    let Ok(entry) = root.open_subkey_with_flags(uninstall_entry_name(), KEY_READ) else {
        return ExistingInstall::default();
    };

    let version = entry
        .get_value::<String, _>("DisplayVersion")
        .ok()
        .and_then(|value| LooseVersion::parse(&value));
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
        installed_version: version,
        installed_path,
        main_file,
        display_name,
    }
}

pub fn is_update(info: &InstallerInfo, existing: &ExistingInstall) -> bool {
    let Some(existing_version) = existing.installed_version.as_ref() else {
        return false;
    };
    let Some(current_version) = info.install_version() else {
        return false;
    };
    current_version >= *existing_version
}

pub fn validate_install(
    info: &InstallerInfo,
    install_path: &Path,
    agreed: bool,
    existing: &ExistingInstall,
) -> Result<()> {
    if info.is_64 && !is_windows_64bit_os() {
        bail!("X86 架构无法安装 X64 程序");
    }
    if install_path.as_os_str().is_empty() {
        bail!("安装路径为空，请选择安装目录");
    }
    if install_path.exists() && path_has_any_content(install_path) && !is_update(info, existing) {
        bail!("安装路径不为空，请重新选择");
    }
    if !agreed {
        bail!("请同意用户协议");
    }
    Ok(())
}

pub fn run_install<F>(info: &InstallerInfo, install_path: &Path, mut report_progress: F) -> Result<InstallResult>
where
    F: FnMut(u8),
{
    report_progress(20);
    terminate_processes_by_path(&install_path.join(&info.can_execute_path))
        .context("中止目标进程时出现错误, 安装被中止")?;

    report_progress(50);
    extract_app_zip(install_path).context("解压程序时出现错误, 安装被中止")?;

    report_progress(70);
    write_install_support_files(install_path).context("创建卸载程序时出现错误, 安装被中止")?;

    report_progress(90);
    write_registry_values(info, install_path).context("写入注册表时出现错误, 安装被中止")?;
    create_or_replace_shortcuts(
        &info.display_name,
        &install_path.join(&info.can_execute_path),
        install_path,
    )
    .context("创建快捷方式时出现错误, 安装被中止")?;

    report_progress(100);
    Ok(InstallResult {
        installed_path: install_path.to_path_buf(),
        executable_path: install_path.join(&info.can_execute_path),
    })
}

pub fn resolve_uninstall_target(info: &InstallerInfo) -> Result<UninstallTarget> {
    let existing = read_existing_install();
    let install_path = existing
        .installed_path
        .ok_or_else(|| anyhow::anyhow!("安装程序未找到"))?;
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
    })
}

pub fn run_uninstall<F>(target: &UninstallTarget, mut report_progress: F) -> Result<()>
where
    F: FnMut(u8),
{
    report_progress(70);
    terminate_processes_by_path(&target.install_path.join(&target.main_file))
        .context("中止目标进程时出现错误, 卸载被中止")?;

    report_progress(50);
    remove_install_directory(&target.install_path).context("文件删除时出现错误, 卸载被中止")?;

    report_progress(0);
    delete_registry_values().context("移除安装注册时出现问题, 卸载被中止")?;
    remove_shortcuts(&target.app_name);

    Ok(())
}

pub fn launch_application(executable_path: &Path, install_dir: &Path) -> Result<()> {
    Command::new(executable_path)
        .current_dir(install_dir)
        .spawn()
        .with_context(|| format!("failed to launch {}", executable_path.display()))?;
    Ok(())
}

fn uninstall_entry_name() -> String {
    format!("{{{}}}_ModernInstaller", resources::application_uuid())
}

fn extract_app_zip(install_path: &Path) -> Result<()> {
    fs::create_dir_all(install_path)?;
    let reader = Cursor::new(resources::app_zip());
    let mut archive = ZipArchive::new(reader).context("invalid app.zip data")?;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let Some(relative_path) = file.enclosed_name() else {
            continue;
        };
        let output_path = install_path.join(relative_path);
        if file.name().ends_with('/') {
            fs::create_dir_all(&output_path)?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file = fs::File::create(&output_path)?;
        std::io::copy(&mut file, &mut out_file)?;
        out_file.flush()?;
    }

    Ok(())
}

fn write_install_support_files(install_path: &Path) -> Result<()> {
    fs::write(
        install_path.join("ModernInstaller.Uninstaller.exe"),
        resources::embedded_uninstaller_exe(),
    )?;
    fs::write(install_path.join("info.json"), resources::embedded_info_json())?;
    Ok(())
}

fn write_registry_values(info: &InstallerInfo, install_path: &Path) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (root, _) = hkcu.create_subkey(UNINSTALL_REGISTRY_ROOT)?;
    let (entry, _) = root.create_subkey(uninstall_entry_name())?;

    entry.set_value("DisplayName", &info.display_name)?;
    entry.set_value("DisplayVersion", &info.display_version)?;
    entry.set_value("Publisher", &info.publisher)?;
    entry.set_value("Path", &install_path.to_string_lossy().to_string())?;
    entry.set_value(
        "UninstallString",
        &install_path
            .join("ModernInstaller.Uninstaller.exe")
            .to_string_lossy()
            .to_string(),
    )?;
    entry.set_value("MainFile", &info.can_execute_path)?;

    let display_icon = if info.display_icon.trim().is_empty() {
        format!("{},0", install_path.join(&info.can_execute_path).to_string_lossy())
    } else {
        info.display_icon.clone()
    };
    entry.set_value("DisplayIcon", &display_icon)?;
    entry.set_value("InstallDate", &Local::now().format("%Y-%m-%d").to_string())?;

    Ok(())
}

fn delete_registry_values() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let root = hkcu.open_subkey_with_flags(UNINSTALL_REGISTRY_ROOT, KEY_WRITE)?;
    let _ = root.delete_subkey_all(uninstall_entry_name());
    Ok(())
}

fn terminate_processes_by_path(executable_path: &Path) -> Result<()> {
    let target = normalize_path(executable_path);
    for _ in 0..10 {
        let mut system = System::new_all();
        system.refresh_processes(ProcessesToUpdate::All, true);

        let mut matched_any = false;
        for process in system.processes().values() {
            let Some(exe) = process.exe() else {
                continue;
            };
            if normalize_path(exe) != target {
                continue;
            }
            matched_any = true;
            if process.kill_with(Signal::Kill).is_none() {
                let _ = process.kill();
            }
        }

        if !matched_any {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    bail!("无法终止目标进程");
}

fn create_or_replace_shortcuts(app_name: &str, target_path: &Path, install_dir: &Path) -> Result<()> {
    remove_shortcuts(app_name);
    for shortcut in shortcut_paths(app_name) {
        if let Some(parent) = shortcut.parent() {
            let _ = fs::create_dir_all(parent);
        }
        create_shortcut_with_powershell(target_path, &shortcut, install_dir, "")?;
    }
    Ok(())
}

fn remove_shortcuts(app_name: &str) {
    for shortcut in shortcut_paths(app_name) {
        if shortcut.exists() {
            let _ = fs::remove_file(shortcut);
        }
    }
}

fn create_shortcut_with_powershell(
    target_path: &Path,
    shortcut_path: &Path,
    working_dir: &Path,
    description: &str,
) -> Result<()> {
    let target = escape_ps_single_quote(&target_path.to_string_lossy());
    let shortcut = escape_ps_single_quote(&shortcut_path.to_string_lossy());
    let workdir = escape_ps_single_quote(&working_dir.to_string_lossy());
    let desc = escape_ps_single_quote(description);

    let script = format!(
        "$w=New-Object -ComObject WScript.Shell;\
         $s=$w.CreateShortcut('{shortcut}');\
         $s.TargetPath='{target}';\
         $s.WorkingDirectory='{workdir}';\
         $s.Description='{desc}';\
         $s.Save()"
    );

    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("powershell create shortcut failed")
    }
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
        command.creation_flags(0x08000000);
    }
    command.spawn()?;
    Ok(())
}
