use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub fn is_windows_64bit_os() -> bool {
    if cfg!(target_pointer_width = "64") {
        return true;
    }
    env::var_os("PROCESSOR_ARCHITEW6432").is_some()
}

pub fn default_install_dir(app_name: &str) -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("C:\\Users\\Public\\AppData\\Local"));
    base.join("Programs").join(app_name)
}

pub fn path_has_any_content(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut iter| iter.next().is_some())
        .unwrap_or(false)
}

pub fn normalize_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_ascii_lowercase()
}

pub fn shortcut_paths(app_name: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if let Ok(app_data) = env::var("APPDATA") {
        result.push(
            PathBuf::from(app_data)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join(format!("{app_name}.lnk")),
        );
    }
    if let Some(desktop) = dirs::desktop_dir() {
        result.push(desktop.join(format!("{app_name}.lnk")));
    }
    result
}

pub fn escape_ps_single_quote(input: &str) -> String {
    input.replace('\'', "''")
}
