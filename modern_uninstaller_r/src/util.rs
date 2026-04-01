use std::env;
use std::path::{Path, PathBuf};

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
