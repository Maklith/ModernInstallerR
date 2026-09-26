use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

pub(crate) fn resolve_target_path(
    raw_target: &str,
    install_path: &Path,
    display_name: &str,
) -> Result<PathBuf> {
    let raw_target = raw_target.trim();
    if raw_target.is_empty() {
        bail!("target path template is empty");
    }

    let install_dir = install_path.to_string_lossy().to_string();
    let mut resolved = raw_target.to_owned();

    replace_placeholder_case_insensitive(&mut resolved, "{InstallDir}", &install_dir);
    replace_placeholder_case_insensitive(&mut resolved, "{InstallPath}", &install_dir);
    replace_placeholder_case_insensitive(&mut resolved, "{DisplayName}", display_name);

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

pub(crate) fn resolve_uninstall_directory(
    raw_target: &str,
    install_path: &Path,
    display_name: &str,
) -> Result<PathBuf> {
    let path = resolve_target_path(raw_target, install_path, display_name)?;
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
        || install_path.starts_with(&path)
        || [
            "LOCALAPPDATA",
            "APPDATA",
            "ProgramData",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "USERPROFILE",
        ]
        .iter()
        .filter_map(env::var_os)
        .any(|root| path == PathBuf::from(root))
        || path == env::temp_dir()
        || path
            .components()
            .filter(|part| matches!(part, Component::Normal(_)))
            .count()
            < 2
    {
        bail!("unsafe UninstallDirectories target: {}", path.display());
    }
    Ok(path)
}

pub(crate) fn remove_configured_directories(directories: &[PathBuf]) -> Result<()> {
    for path in directories {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
            }
        };
        if !metadata.file_type().is_dir() {
            bail!(
                "UninstallDirectories target is not a directory: {}",
                path.display()
            );
        }
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove configured directory {}", path.display()))?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn uninstall_target_rejects_parent_traversal_and_root() {
        let install_dir = env::temp_dir().join("Kitopia");
        assert!(resolve_uninstall_directory("../Other", &install_dir, "Kitopia").is_err());
        let root = install_dir
            .ancestors()
            .last()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(resolve_uninstall_directory(&root, &install_dir, "Kitopia").is_err());
    }

    #[test]
    fn uninstall_target_resolves_relative_path() {
        let install_dir = env::temp_dir().join("Kitopia");
        let path = resolve_uninstall_directory("plugins", &install_dir, "Kitopia").unwrap();
        assert_eq!(path, install_dir.join("plugins"));
    }

    #[test]
    fn uninstall_target_rejects_environment_root() {
        let Some(local_app_data) = env::var_os("LOCALAPPDATA") else {
            return;
        };
        let install_dir = env::temp_dir().join("Kitopia");
        assert!(resolve_uninstall_directory("{LocalUserData}", &install_dir, "Kitopia").is_err());
        assert!(resolve_uninstall_directory("{Temp}", &install_dir, "Kitopia").is_err());
        assert!(
            resolve_uninstall_directory(
                &PathBuf::from(local_app_data).to_string_lossy(),
                &install_dir,
                "Kitopia"
            )
            .is_err()
        );
    }

    #[test]
    fn removing_configured_directory_preserves_sibling() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("kitopia-uninstall-{}-{unique}", std::process::id()));
        let target = root.join("plugins");
        let sibling = root.join("settings");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::write(target.join("plugin.txt"), b"plugin").unwrap();

        remove_configured_directories(&[target.clone()]).unwrap();

        assert!(!target.exists());
        assert!(sibling.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
