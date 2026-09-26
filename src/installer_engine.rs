use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::Local;
use flate2::read::GzDecoder;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, KEY_WRITE};
use zip::ZipArchive;

pub use crate::file_locks::{LockingProcessInfo, find_locked_files_in_directory};
use crate::file_locks::{
    collect_locking_process_infos, find_locked_files_in_directories, find_locking_process_ids,
    terminate_processes_locking_directories,
};
use crate::model::{InstallDependencyRule, InstallerInfo};
use crate::path_template::{
    remove_configured_directories, resolve_target_path, resolve_uninstall_directory,
};
use crate::resources::{self, EmbeddedPackage};
use crate::util::{
    default_install_dir_for_arch, escape_ps_single_quote, is_windows_64bit_os, normalize_path,
    path_has_any_content, shortcut_paths,
};
use crate::version::LooseVersion;

const UNINSTALL_REGISTRY_ROOT: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

pub fn find_locked_files_for_install(
    info: &InstallerInfo,
    install_path: &Path,
) -> Result<Vec<PathBuf>> {
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

pub fn run_install<F, C>(
    info: &InstallerInfo,
    install_path: &Path,
    create_shortcuts: bool,
    mut report_progress: F,
    mut confirm_terminate: C,
) -> Result<InstallResult>
where
    F: FnMut(ProgressState),
    C: FnMut(&[LockingProcessInfo]) -> Result<bool>,
{
    report_progress(ProgressState::new(8, "正在准备安装"));
    report_progress(ProgressState::new(20, "正在检测并结束相关进程"));
    let target_directories = collect_install_target_directories(info, install_path)?;
    terminate_processes_locking_directories(&target_directories, false, &mut confirm_terminate)
        .context("中止目标进程时出现错误,安装被中止")?;

    report_progress(ProgressState::new(28, "正在检查在线依赖"));
    install_online_dependencies(info, install_path, &mut report_progress, 28, 55)
        .context("failed to install online dependencies, installation aborted")?;

    report_progress(ProgressState::new(60, "正在解压应用包"));
    extract_configured_packages(info, install_path).context("解压程序时出现错误,安装被中止")?;

    report_progress(ProgressState::new(82, "正在写入安装支持文件"));
    write_install_support_files(install_path).context("创建卸载程序时出现错误,安装被中止")?;

    report_progress(ProgressState::new(92, "正在写入注册表并创建快捷方式"));
    write_registry_values(info, install_path).context("写入注册表时出现错误,安装被中止")?;
    if create_shortcuts {
        create_or_replace_shortcuts(
            &info.display_name,
            &install_path.join(&info.can_execute_path),
            install_path,
        )
        .context("创建快捷方式时出现错误,安装被中止")?;
    }

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
    report_progress(ProgressState::new(10, "正在准备卸载"));
    report_progress(ProgressState::new(35, "正在检查文件占用并结束相关进程"));
    let mut target_directories = vec![target.install_path.clone()];
    target_directories.extend_from_slice(&target.uninstall_directories);
    terminate_processes_locking_directories(&target_directories, true, &mut confirm_terminate)
        .context("中止目标进程时出现错误,卸载被中止")?;

    report_progress(ProgressState::new(60, "正在删除配置的目录"));
    remove_configured_directories(&target.uninstall_directories)
        .context("删除配置的目录时出现错误,卸载被中止")?;
    report_progress(ProgressState::new(70, "正在删除安装文件"));
    remove_install_directory(&target.install_path).context("文件删除时出现错误,卸载被中止")?;

    report_progress(ProgressState::new(90, "正在清理注册表和快捷方式"));
    delete_registry_values(target.is_64).context("移除安装注册时出现问题,卸载被中止")?;
    remove_shortcuts(&target.app_name)
        .context("移除快捷方式时出现错误,卸载近乎完成,请手动删除快捷方式")?;

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
        report_progress(ProgressState::new(stage_end, "未配置在线依赖，已跳过"));
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
            format!("依赖 {}/{}：正在检查 {}", index + 1, total, dep_name),
        ));

        if should_skip_dependency_install(dependency, info, install_path)? {
            report_progress(ProgressState::new(
                progress_within_stage(stage_start, stage_end, base_step + 3, total_steps),
                format!("依赖 {}/{}：{} 已安装，已跳过", index + 1, total, dep_name),
            ));
            continue;
        }

        let download_name = dependency_download_file_name(dependency)?;
        let download_path = download_root.join(download_name);

        report_progress(ProgressState::new(
            progress_within_stage(stage_start, stage_end, base_step + 1, total_steps),
            format!("依赖 {}/{}：正在下载 {}", index + 1, total, dep_name),
        ));
        download_file_with_powershell(&dependency.url, &download_path)
            .with_context(|| format!("failed to download dependency {}", dependency.name))?;

        report_progress(ProgressState::new(
            progress_within_stage(stage_start, stage_end, base_step + 2, total_steps),
            format!("依赖 {}/{}：正在安装 {}", index + 1, total, dep_name),
        ));
        run_dependency_installer(dependency, &download_path)
            .with_context(|| format!("failed to install dependency {}", dependency.name))?;

        report_progress(ProgressState::new(
            progress_within_stage(stage_start, stage_end, base_step + 3, total_steps),
            format!("依赖 {}/{}：{} 安装完成", index + 1, total, dep_name),
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
    let resolved_path = resolve_target_path(check_path, install_path, &info.display_name)?;
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

fn run_dependency_installer(
    dependency: &InstallDependencyRule,
    installer_path: &Path,
) -> Result<()> {
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
        let target_dir = resolve_target_path(&rule.target, install_path, &info.display_name)
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

fn collect_install_target_directories(
    info: &InstallerInfo,
    install_path: &Path,
) -> Result<Vec<PathBuf>> {
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

        let target_dir = resolve_target_path(&rule.target, install_path, &info.display_name)
            .with_context(|| format!("invalid target for package {}", package_name))?;
        if seen_directories.insert(normalize_path(&target_dir)) {
            directories.push(target_dir);
        }
    }

    Ok(directories)
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
