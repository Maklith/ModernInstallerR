use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Local;
use flate2::read::GzDecoder;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
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
use zip::ZipArchive;

use crate::model::{InstallDependencyRule, InstallerInfo};
use crate::resources::{self, EmbeddedPackage};
use crate::util::{
    default_install_dir_for_arch, escape_ps_single_quote, is_windows_64bit_os, normalize_path,
    path_has_any_content, shortcut_paths,
};
use crate::version::LooseVersion;

const UNINSTALL_REGISTRY_ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
#[cfg(windows)]
const ERROR_SHARING_VIOLATION: i32 = 32;
#[cfg(windows)]
const ERROR_LOCK_VIOLATION: i32 = 33;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const ACCESS_DELETE: u32 = 0x0001_0000;
#[cfg(windows)]
const ACCESS_GENERIC_READ: u32 = 0x8000_0000;
#[cfg(windows)]
const ACCESS_GENERIC_WRITE: u32 = 0x4000_0000;

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

pub fn suggested_install_path(info: &InstallerInfo, existing: &ExistingInstall) -> PathBuf {
    existing
        .installed_path
        .clone()
        .unwrap_or_else(|| default_install_dir_for_arch(&info.display_name, info.is_64))
}

pub fn read_existing_install(info: &InstallerInfo) -> ExistingInstall {
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
        bail!("X86架构无法安装X64程序");
    }
    if install_path.as_os_str().is_empty() {
        bail!("安装路径为空，请选择安装目录");
    }
    if !install_path.has_root() {
        bail!("安装路径错误");
    }
    if install_path.exists() && path_has_any_content(install_path) && !is_update(info, existing) {
        bail!("安装路径不为空，请重新选择");
    }
    if !agreed {
        bail!("请同意用户协议");
    }
    Ok(())
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

pub fn find_locked_files_for_install(info: &InstallerInfo, install_path: &Path) -> Result<Vec<PathBuf>> {
    let target_dirs = collect_install_target_directories(info, install_path)?;
    find_locked_files_in_directories(&target_dirs)
}

pub fn find_lock_preview_for_install(
    info: &InstallerInfo,
    install_path: &Path,
) -> Result<(Vec<PathBuf>, Vec<LockingProcessInfo>)> {
    let target_dirs = collect_install_target_directories(info, install_path)?;
    let locked_files = find_locked_files_in_directories(&target_dirs)?;
    if locked_files.is_empty() {
        return Ok((locked_files, Vec::new()));
    }
    let locking_pids = find_locking_process_ids(&locked_files).unwrap_or_default();
    let locking_processes = collect_locking_process_infos(&target_dirs, &locking_pids);
    Ok((locked_files, locking_processes))
}

pub fn run_install<F>(
    info: &InstallerInfo,
    install_path: &Path,
    mut report_progress: F,
) -> Result<InstallResult>
where
    F: FnMut(ProgressState),
{
    report_progress(ProgressState::new(8, "正在准备安装"));
    report_progress(ProgressState::new(
        20,
        "正在检测并结束相关进程",
    ));
    terminate_processes_for_install_targets(info, install_path)
        .context("中止目标进程时出现错误,安装被中止")?;

    report_progress(ProgressState::new(28, "正在检查在线依赖"));
    install_online_dependencies(info, install_path, &mut report_progress, 28, 55)
        .context("failed to install online dependencies, installation aborted")?;

    report_progress(ProgressState::new(60, "正在解压应用包"));
    extract_configured_packages(info, install_path)
        .context("解压程序时出现错误,安装被中止")?;

    report_progress(ProgressState::new(82, "正在写入安装支持文件"));
    write_install_support_files(install_path).context("创建卸载程序时出现错误,安装被中止")?;

    report_progress(ProgressState::new(
        92,
        "正在写入注册表并创建快捷方式",
    ));
    write_registry_values(info, install_path)
        .and_then(|_| {
            create_or_replace_shortcuts(
                &info.display_name,
                &install_path.join(&info.can_execute_path),
                install_path,
            )
        })
        .context("写入注册表或创建快捷方式时出现错误,安装被中止")?;

    report_progress(ProgressState::new(100, "安装完成"));
    Ok(InstallResult {
        installed_path: install_path.to_path_buf(),
        executable_path: install_path.join(&info.can_execute_path),
    })
}

pub fn resolve_uninstall_target(info: &InstallerInfo) -> Result<UninstallTarget> {
    let existing = read_existing_install(info);
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
        is_64: info.is_64,
    })
}

pub fn run_uninstall<F>(target: &UninstallTarget, mut report_progress: F) -> Result<()>
where
    F: FnMut(ProgressState),
{
    report_progress(ProgressState::new(10, "正在准备卸载"));
    report_progress(ProgressState::new(
        35,
        "正在结束正在运行的应用进程",
    ));
    terminate_processes_by_path(&target.install_path.join(&target.main_file))
        .context("中止目标进程时出现错误,卸载被中止")?;

    report_progress(ProgressState::new(70, "正在删除安装文件"));
    remove_install_directory(&target.install_path).context("文件删除时出现错误,卸载被中止")?;

    report_progress(ProgressState::new(
        90,
        "正在清理注册表和快捷方式",
    ));
    delete_registry_values(target.is_64).context("移除安装注册时出现问题,卸载被中止")?;
    remove_shortcuts(&target.app_name).context("移除快捷方式时出现错误,卸载近乎完成,请手动删除快捷方式")?;

    report_progress(ProgressState::new(100, "卸载完成"));
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

fn install_online_dependencies<F>(
    info: &InstallerInfo,
    install_path: &Path,
    report_progress: &mut F,
    stage_start: u8,
    stage_end: u8,
) -> Result<()>
where
    F: FnMut(ProgressState),
{
    if info.install_dependencies.is_empty() {
        report_progress(ProgressState::new(
            stage_end,
            "未配置在线依赖，已跳过",
        ));
        return Ok(());
    }

    let download_root = env::temp_dir().join("ModernInstaller").join("dependencies");
    fs::create_dir_all(&download_root)?;

    let total = info.install_dependencies.len() as u32;
    let total_steps = total.saturating_mul(4);
    for (index, dependency) in info.install_dependencies.iter().enumerate() {
        let dep_name = if dependency.name.trim().is_empty() {
            "Unnamed dependency"
        } else {
            dependency.name.trim()
        };
        let base_step = (index as u32).saturating_mul(4);

        report_progress(ProgressState::new(
            progress_within_stage(stage_start, stage_end, base_step, total_steps),
            format!(
                "依赖 {}/{}：正在检查 {}",
                index + 1,
                total,
                dep_name
            ),
        ));

        if should_skip_dependency_install(dependency, info, install_path)? {
            report_progress(ProgressState::new(
                progress_within_stage(stage_start, stage_end, base_step + 3, total_steps),
                format!(
                    "依赖 {}/{}：{} 已安装，已跳过",
                    index + 1,
                    total,
                    dep_name
                ),
            ));
            continue;
        }

        let download_name = dependency_download_file_name(dependency)?;
        let download_path = download_root.join(download_name);

        report_progress(ProgressState::new(
            progress_within_stage(stage_start, stage_end, base_step + 1, total_steps),
            format!(
                "依赖 {}/{}：正在下载 {}",
                index + 1,
                total,
                dep_name
            ),
        ));
        download_file_with_powershell(&dependency.url, &download_path)
            .with_context(|| format!("failed to download dependency {}", dependency.name))?;

        report_progress(ProgressState::new(
            progress_within_stage(stage_start, stage_end, base_step + 2, total_steps),
            format!(
                "依赖 {}/{}：正在安装 {}",
                index + 1,
                total,
                dep_name
            ),
        ));
        run_dependency_installer(dependency, &download_path)
            .with_context(|| format!("failed to install dependency {}", dependency.name))?;

        report_progress(ProgressState::new(
            progress_within_stage(stage_start, stage_end, base_step + 3, total_steps),
            format!(
                "依赖 {}/{}：{} 安装完成",
                index + 1,
                total,
                dep_name
            ),
        ));
    }

    report_progress(ProgressState::new(stage_end, "在线依赖处理完成"));
    Ok(())
}

fn should_skip_dependency_install(
    dependency: &InstallDependencyRule,
    info: &InstallerInfo,
    install_path: &Path,
) -> Result<bool> {
    if is_dotnet_runtime_installed(dependency)? {
        return Ok(true);
    }

    let check_path = dependency.skip_if_exists.trim();
    if check_path.is_empty() {
        return Ok(false);
    }
    let resolved_path = resolve_package_target(check_path, install_path, info)?;
    if resolved_path.exists() {
        return Ok(true);
    }

    Ok(false)
}

fn progress_within_stage(start: u8, end: u8, step: u32, total_steps: u32) -> u8 {
    if total_steps == 0 || end <= start {
        return end;
    }
    let clamped = step.min(total_steps);
    let span = (end - start) as u32;
    (start as u32 + (span * clamped) / total_steps) as u8
}

fn is_dotnet_runtime_installed(dependency: &InstallDependencyRule) -> Result<bool> {
    let runtime_name = dependency.runtime_name.trim();
    if runtime_name.is_empty() {
        return Ok(false);
    }
    let runtime_version_prefix = dependency.runtime_version_prefix.trim();

    let output = match Command::new("dotnet").arg("--list-runtimes").output() {
        Ok(output) if output.status.success() => output,
        _ => return Ok(false),
    };

    let listing = String::from_utf8_lossy(&output.stdout);
    for line in listing.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        let Some(version) = parts.next() else {
            continue;
        };
        if !name.eq_ignore_ascii_case(runtime_name) {
            continue;
        }
        if runtime_version_prefix.is_empty() || version.starts_with(runtime_version_prefix) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn dependency_download_file_name(dependency: &InstallDependencyRule) -> Result<String> {
    let configured_name = dependency.file_name.trim();
    if !configured_name.is_empty() {
        return Ok(configured_name.to_string());
    }

    let url_no_query = dependency.url.trim().split('?').next().unwrap_or("");
    let inferred = url_no_query.rsplit('/').next().unwrap_or("").trim();
    if inferred.is_empty() {
        bail!(
            "dependency {} missing FileName and Url has no file name",
            dependency.name
        );
    }
    Ok(inferred.to_string())
}

fn download_file_with_powershell(url: &str, output_path: &Path) -> Result<()> {
    let url = url.trim();
    if url.is_empty() {
        bail!("dependency Url is empty");
    }
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let escaped_url = escape_ps_single_quote(url);
    let escaped_output = escape_ps_single_quote(&output_path.to_string_lossy());
    let script = format!(
        "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri '{escaped_url}' -OutFile '{escaped_output}'"
    );
    let mut command = Command::new("powershell");
    command.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ]);
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("powershell download failed")
    }
}

fn run_dependency_installer(dependency: &InstallDependencyRule, installer_path: &Path) -> Result<()> {
    let extension = installer_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut command = if extension == "msi" {
        let mut command = Command::new("msiexec");
        command.arg("/i").arg(installer_path);
        if dependency.install_args.is_empty() {
            command.args(["/qn", "/norestart"]);
        } else {
            command.args(&dependency.install_args);
        }
        command
    } else {
        let mut command = Command::new(installer_path);
        command.args(&dependency.install_args);
        command
    };

    if let Some(parent) = installer_path.parent() {
        command.current_dir(parent);
    }

    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("dependency installer exited with status {status}")
    }
}

fn extract_configured_packages(info: &InstallerInfo, install_path: &Path) -> Result<()> {
    fs::create_dir_all(install_path)?;

    if info.install_packages.is_empty() {
        return extract_legacy_default_package(install_path);
    }

    for rule in &info.install_packages {
        let package_name = rule.package.trim();
        if package_name.is_empty() {
            bail!("InstallPackages contains an empty Package value");
        }

        let package = resources::find_embedded_package(package_name).ok_or_else(|| {
            anyhow::anyhow!(
                "embedded package not found: {} (available: {})",
                package_name,
                available_package_names()
            )
        })?;
        let target_dir = resolve_package_target(&rule.target, install_path, info)
            .with_context(|| format!("invalid target for package {}", package.file_name))?;
        extract_embedded_package(package, &target_dir).with_context(|| {
            format!(
                "failed to extract package {} to {}",
                package.file_name,
                target_dir.display()
            )
        })?;
    }

    Ok(())
}

fn extract_legacy_default_package(install_path: &Path) -> Result<()> {
    let package = resources::legacy_app_package()
        .or_else(|| resources::embedded_packages().first())
        .ok_or_else(|| anyhow::anyhow!("no embedded archive package found"))?;
    extract_embedded_package(package, install_path).with_context(|| {
        format!(
            "failed to extract default package {} to {}",
            package.file_name,
            install_path.display()
        )
    })
}

fn available_package_names() -> String {
    resources::embedded_packages()
        .iter()
        .map(|package| package.file_name)
        .collect::<Vec<_>>()
        .join(", ")
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
    if !contains_ignore_ascii_case(target, placeholder) {
        return Ok(());
    }
    let Some(value) = env::var_os(env_name) else {
        bail!("placeholder {placeholder} requires environment variable {env_name}");
    };
    let value = PathBuf::from(value).to_string_lossy().to_string();
    replace_placeholder_case_insensitive(target, placeholder, &value);
    Ok(())
}

fn contains_ignore_ascii_case(input: &str, pattern: &str) -> bool {
    input
        .to_ascii_lowercase()
        .contains(&pattern.to_ascii_lowercase())
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
            continue;
        }
        if ch == '}' && opened {
            return true;
        }
    }
    false
}

fn extract_embedded_package(package: &EmbeddedPackage, target_dir: &Path) -> Result<()> {
    fs::create_dir_all(target_dir)?;
    let package_payload = inflate_gzip_bytes(package.gzip_bytes)
        .with_context(|| format!("invalid gzip stream for {}", package.file_name))?;

    match package.kind {
        "zip" => extract_zip_package(target_dir, &package_payload),
        "tar" => extract_tar_package(target_dir, &package_payload),
        "tar.gz" => extract_tar_gz_package(target_dir, &package_payload),
        unknown => bail!(
            "unsupported package kind for {}: {unknown}",
            package.file_name
        ),
    }
}

fn extract_zip_package(target_dir: &Path, package_payload: &[u8]) -> Result<()> {
    let reader = Cursor::new(package_payload);
    let mut archive = ZipArchive::new(reader).context("invalid zip package data")?;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let Some(relative_path) = file.enclosed_name() else {
            continue;
        };
        let output_path = target_dir.join(relative_path);
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

fn extract_tar_package(target_dir: &Path, package_payload: &[u8]) -> Result<()> {
    let reader = Cursor::new(package_payload);
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.unpack_in(target_dir)? {
            bail!("invalid path in tar package");
        }
    }
    Ok(())
}

fn extract_tar_gz_package(target_dir: &Path, package_payload: &[u8]) -> Result<()> {
    let reader = Cursor::new(package_payload);
    let decoder = GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.unpack_in(target_dir)? {
            bail!("invalid path in tar.gz package");
        }
    }
    Ok(())
}

fn write_install_support_files(install_path: &Path) -> Result<()> {
    let uninstaller_bytes = inflate_gzip_bytes(resources::embedded_uninstaller_gz())
        .context("invalid uninstaller gzip stream")?;
    fs::write(
        install_path.join("ModernInstaller.Uninstaller.exe"),
        uninstaller_bytes,
    )?;
    fs::write(
        install_path.join("info.json"),
        resources::embedded_info_json(),
    )?;
    Ok(())
}

fn inflate_gzip_bytes(gzip_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(gzip_bytes);
    let mut output = Vec::new();
    decoder.read_to_end(&mut output)?;
    Ok(output)
}

fn write_registry_values(info: &InstallerInfo, install_path: &Path) -> Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let (root, _) =
        hklm.create_subkey_with_flags(UNINSTALL_REGISTRY_ROOT, registry_write_flags(info.is_64))?;
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
        format!(
            "{},0",
            install_path.join(&info.can_execute_path).to_string_lossy()
        )
    } else {
        info.display_icon.clone()
    };
    entry.set_value("DisplayIcon", &display_icon)?;
    entry.set_value("InstallDate", &Local::now().format("%Y-%m-%d").to_string())?;

    Ok(())
}

fn delete_registry_values(is_64_target: bool) -> Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let root =
        hklm.open_subkey_with_flags(UNINSTALL_REGISTRY_ROOT, registry_write_flags(is_64_target))?;
    let _ = root.delete_subkey_all(uninstall_entry_name());
    Ok(())
}

fn find_locked_files_in_directories(directories: &[PathBuf]) -> Result<Vec<PathBuf>> {
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

fn collect_install_target_directories(info: &InstallerInfo, install_path: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    let mut seen_directories = HashSet::new();

    let install_dir = install_path.to_path_buf();
    seen_directories.insert(normalize_path(&install_dir));
    directories.push(install_dir);

    if info.install_packages.is_empty() {
        return Ok(directories);
    }

    for rule in &info.install_packages {
        let package_name = rule.package.trim();
        if package_name.is_empty() {
            bail!("InstallPackages contains an empty Package value");
        }

        let target_dir = resolve_package_target(&rule.target, install_path, info)
            .with_context(|| format!("invalid target for package {}", package_name))?;
        if seen_directories.insert(normalize_path(&target_dir)) {
            directories.push(target_dir);
        }
    }

    Ok(directories)
}

fn collect_locked_files_recursively(directory: &Path, locked_files: &mut Vec<PathBuf>) -> Result<()> {
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
        let path_ptrs = wide_paths.iter().map(|path| path.as_ptr()).collect::<Vec<_>>();

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

        let mut process_infos = vec![unsafe { std::mem::zeroed::<RM_PROCESS_INFO>() }; process_info_needed as usize];
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

fn terminate_processes_for_install_targets(info: &InstallerInfo, install_path: &Path) -> Result<()> {
    let target_directories = collect_install_target_directories(info, install_path)?;
    let normalized_target_dirs = target_directories
        .iter()
        .map(|directory| normalize_path(directory))
        .collect::<Vec<_>>();
    let current_pid = std::process::id();
    let mut locked_files = find_locked_files_in_directories(&target_directories)?;
    if locked_files.is_empty() {
        return Ok(());
    }

    for attempt in 0..10 {
        let locking_pids = find_locking_process_ids(&locked_files).unwrap_or_default();
        let locking_pid_set = locking_pids.iter().copied().collect::<HashSet<u32>>();

        let mut system = System::new_all();
        system.refresh_processes(ProcessesToUpdate::All, true);

        let mut matched_any = false;
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
            let should_kill = in_locking_pid_set || in_target_directory;

            if !should_kill {
                continue;
            }

            if in_locking_pid_set {
                handled_locking_pids.insert(pid);
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

        for pid in locking_pid_set {
            if pid == 0 || pid == current_pid || handled_locking_pids.contains(&pid) {
                continue;
            }
            matched_any = true;
            let _ = kill_by_pid_fallback(pid);
        }

        if !matched_any {
            thread::sleep(Duration::from_millis(800));
        } else {
            thread::sleep(Duration::from_millis(800));
        }

        locked_files = locked_files
            .into_iter()
            .filter(|path| path.is_file() && is_file_locked(path))
            .collect();
        if locked_files.is_empty() {
            return Ok(());
        }

        // Avoid repeatedly crawling large target directories on every retry.
        // Re-scan periodically to catch newly-created or newly-locked files.
        if attempt % 3 == 2 {
            locked_files = find_locked_files_in_directories(&target_directories)?;
            if locked_files.is_empty() {
                return Ok(());
            }
        }
    }

    let remaining_locked_files = find_locked_files_in_directories(&target_directories)?;
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
        "failed to terminate processes locking install target files: {} file(s) still locked{}{}",
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

fn terminate_processes_by_path(executable_path: &Path) -> Result<()> {
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
    bail!("failed to terminate target process");
}

fn create_or_replace_shortcuts(
    app_name: &str,
    target_path: &Path,
    install_dir: &Path,
) -> Result<()> {
    remove_shortcuts(app_name)?;
    for shortcut in shortcut_paths(app_name) {
        if let Some(parent) = shortcut.parent() {
            let _ = fs::create_dir_all(parent);
        }
        create_shortcut_with_powershell(target_path, &shortcut, install_dir, "")?;
    }
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

    let mut command = Command::new("powershell");
    command.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ]);
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let status = command.status()?;
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
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn()?;
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
