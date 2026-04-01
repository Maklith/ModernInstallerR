use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;
use font_subset::Font;

const SOURCE_FONT_REL: &str = "installer_assets/HarmonyOS_Sans_SC_Regular.ttf";
const PACKAGE_SOURCE_DIR_REL: &str = "installer_assets";
const STANDALONE_UNINSTALLER_MANIFEST_REL: &str = "modern_uninstaller_r/Cargo.toml";
const GENERATED_FONT_NAME: &str = "HarmonyOS_Sans_SC_Subset.ttf";
const GENERATED_EMBEDDED_PACKAGES_RS_NAME: &str = "embedded_packages.rs";
const GENERATED_UNINSTALLER_GZ_NAME: &str = "ModernInstaller.Uninstaller.exe.gz";
const GENERATED_TEXT_NAME: &str = "font_chars.txt";

fn main() {
    if env::var("CARGO_CFG_WINDOWS").is_ok() {
        embed_resource::compile("resources.rc", embed_resource::NONE);
    }
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("missing OUT_DIR"));

    let source_font = manifest_dir.join(SOURCE_FONT_REL);
    let package_source_dir = manifest_dir.join(PACKAGE_SOURCE_DIR_REL);
    let standalone_uninstaller_manifest = manifest_dir.join(STANDALONE_UNINSTALLER_MANIFEST_REL);
    let generated_font = out_dir.join(GENERATED_FONT_NAME);
    let generated_embedded_packages_rs = out_dir.join(GENERATED_EMBEDDED_PACKAGES_RS_NAME);
    let generated_uninstaller_gz = out_dir.join(GENERATED_UNINSTALLER_GZ_NAME);
    let generated_text = out_dir.join(GENERATED_TEXT_NAME);

    println!("cargo:rerun-if-changed={}", source_font.display());
    println!(
        "cargo:rerun-if-changed={}",
        standalone_uninstaller_manifest.display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("installer_assets/info.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir
            .join("installer_assets/Agreement.txt")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir
            .join("installer_assets/extra_chars.txt")
            .display()
    );
    print_rerun_for_dir(&manifest_dir.join("src"));
    print_rerun_for_dir(&manifest_dir.join("modern_uninstaller_r/src"));

    if !source_font.exists() {
        panic!("source font not found: {}", source_font.display());
    }

    let chars = collect_chars(&manifest_dir).expect("failed to collect characters for font subset");
    fs::write(&generated_text, chars.as_bytes()).expect("failed to write character list");

    let generated_ok = generate_subset_font_with_rust(&source_font, &chars, &generated_font)
        .unwrap_or_else(|error| {
            println!("cargo:warning=font subsetting failed: {error}");
            false
        });

    if generated_ok {
        println!("cargo:warning=font subset generated with pure Rust (font-subset)");
    } else {
        fs::copy(&source_font, &generated_font).expect("failed to fallback to full font");
        println!("cargo:warning=using full Harmony font because Rust subsetting failed");
    }

    let packages = collect_app_packages(&package_source_dir)
        .expect("failed to scan installer_assets for archive packages");
    if packages.is_empty() {
        panic!(
            "missing app package: expected at least one archive in installer_assets (*.zip/*.tar/*.tar.gz/*.tgz)"
        );
    }
    for package in &packages {
        println!("cargo:rerun-if-changed={}", package.source_path.display());
    }

    let generated_packages =
        gzip_packages(&packages, &out_dir).expect("failed to gzip embedded app packages");
    write_embedded_packages_rs(&generated_embedded_packages_rs, &generated_packages)
        .expect("failed to write embedded packages metadata");
    for package in &generated_packages {
        println!(
            "cargo:warning=embedded package {} ({}) compressed {} -> {} bytes",
            package.file_name,
            package.kind.as_str(),
            package.source_len,
            package.gz_len
        );
    }

    let uninstaller_exe = build_standalone_uninstaller(&manifest_dir)
        .expect("failed to build standalone uninstaller crate");
    let uninstaller_stats = gzip_file(&uninstaller_exe, &generated_uninstaller_gz)
        .expect("failed to gzip embedded uninstaller payload");
    println!(
        "cargo:warning=embedded uninstaller compressed {} -> {} bytes",
        uninstaller_stats.source_len, uninstaller_stats.gz_len
    );
}

fn build_standalone_uninstaller(manifest_dir: &Path) -> io::Result<PathBuf> {
    let target = env::var("TARGET").expect("missing TARGET");
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_owned());
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let uninstaller_manifest = manifest_dir.join(STANDALONE_UNINSTALLER_MANIFEST_REL);
    if !uninstaller_manifest.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "standalone uninstaller manifest not found: {}",
                uninstaller_manifest.display()
            ),
        ));
    }

    let uninstaller_target_dir = manifest_dir.join("target").join("standalone-uninstaller");
    let mut command = Command::new(cargo);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(&uninstaller_manifest)
        .arg("--target")
        .arg(&target)
        .arg("--target-dir")
        .arg(&uninstaller_target_dir);
    if profile == "release" {
        command.arg("--release");
    }
    command.env_remove("CARGO_MAKEFLAGS");

    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "failed to build standalone uninstaller crate, status: {status}"
        )));
    }

    let executable_name = if target.contains("windows") {
        "modern_uninstaller_r.exe"
    } else {
        "modern_uninstaller_r"
    };
    let executable = uninstaller_target_dir
        .join(target)
        .join(profile)
        .join(executable_name);

    if !executable.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "standalone uninstaller binary not found after build: {}",
                executable.display()
            ),
        ));
    }
    Ok(executable)
}

fn print_rerun_for_dir(dir: &Path) {
    if !dir.exists() {
        return;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let entries = match fs::read_dir(&next) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if is_text_file(&path) {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

fn collect_chars(manifest_dir: &Path) -> io::Result<String> {
    let mut set = BTreeSet::new();
    add_ascii_chars(&mut set);
    add_chars(&mut set, " !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~");

    collect_from_dir(&manifest_dir.join("src"), &mut set)?;
    collect_from_file(&manifest_dir.join("Cargo.toml"), &mut set)?;
    collect_from_file(&manifest_dir.join("installer_assets/info.json"), &mut set)?;
    collect_from_file(
        &manifest_dir.join("installer_assets/Agreement.txt"),
        &mut set,
    )?;
    collect_from_file(
        &manifest_dir.join("installer_assets/extra_chars.txt"),
        &mut set,
    )?;

    Ok(set.into_iter().collect())
}

fn collect_from_dir(dir: &Path, set: &mut BTreeSet<char>) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in fs::read_dir(next)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if is_text_file(&path) {
                collect_from_file(&path, set)?;
            }
        }
    }
    Ok(())
}

fn collect_from_file(path: &Path, set: &mut BTreeSet<char>) -> io::Result<()> {
    if !path.exists() || !is_text_file(path) {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    add_chars(set, &content);
    Ok(())
}

fn add_ascii_chars(set: &mut BTreeSet<char>) {
    for code in 0x20u8..=0x7Eu8 {
        set.insert(code as char);
    }
    set.insert('\n');
    set.insert('\r');
    set.insert('\t');
}

fn add_chars(set: &mut BTreeSet<char>, text: &str) {
    for ch in text.chars() {
        if ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t' {
            continue;
        }
        set.insert(ch);
    }
}

fn is_text_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str).map(|s| s.to_ascii_lowercase()),
        Some(ext)
            if matches!(
                ext.as_str(),
                "rs" | "toml" | "json" | "txt" | "md" | "ps1" | "yaml" | "yml"
            )
    )
}

fn generate_subset_font_with_rust(
    source_font: &Path,
    chars: &str,
    output_font: &Path,
) -> io::Result<bool> {
    if chars.is_empty() {
        return Ok(false);
    }

    let font_bytes = fs::read(source_font)?;
    let font = Font::opentype(&font_bytes).map_err(|error| io::Error::other(error.to_string()))?;

    let mut retained_chars = BTreeSet::new();
    for ch in chars.chars() {
        retained_chars.insert(ch);
    }
    retained_chars.insert(' ');

    let subset = font
        .subset(&retained_chars)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let subset_bytes = subset.to_opentype();
    if subset_bytes.is_empty() {
        return Ok(false);
    }

    fs::write(output_font, subset_bytes)?;
    Ok(true)
}

#[derive(Clone)]
struct PackageSource {
    source_path: PathBuf,
    file_name: String,
    kind: AppPackageKind,
}

struct GeneratedPackage {
    file_name: String,
    kind: AppPackageKind,
    source_len: usize,
    gz_len: usize,
    generated_gz_name: String,
}

fn collect_app_packages(package_dir: &Path) -> io::Result<Vec<PackageSource>> {
    if !package_dir.exists() {
        return Ok(Vec::new());
    }

    let mut packages = Vec::new();
    for entry in fs::read_dir(package_dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        let file_name = file_name.to_owned();
        let Some(kind) = detect_archive_kind(&file_name) else {
            continue;
        };
        packages.push(PackageSource {
            source_path: path,
            file_name,
            kind,
        });
    }

    packages.sort_by(|left, right| {
        left.file_name
            .to_ascii_lowercase()
            .cmp(&right.file_name.to_ascii_lowercase())
    });
    Ok(packages)
}

fn detect_archive_kind(file_name: &str) -> Option<AppPackageKind> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return Some(AppPackageKind::TarGz);
    }
    if lower.ends_with(".zip") {
        return Some(AppPackageKind::Zip);
    }
    if lower.ends_with(".tar") {
        return Some(AppPackageKind::Tar);
    }
    None
}

fn gzip_packages(packages: &[PackageSource], out_dir: &Path) -> io::Result<Vec<GeneratedPackage>> {
    let mut generated = Vec::with_capacity(packages.len());
    for (index, package) in packages.iter().enumerate() {
        let generated_gz_name = format!("Package.{index}.gz");
        let generated_gz_path = out_dir.join(&generated_gz_name);
        let stats = gzip_file(&package.source_path, &generated_gz_path)?;
        generated.push(GeneratedPackage {
            file_name: package.file_name.clone(),
            kind: package.kind,
            source_len: stats.source_len,
            gz_len: stats.gz_len,
            generated_gz_name,
        });
    }
    Ok(generated)
}

fn write_embedded_packages_rs(output_path: &Path, packages: &[GeneratedPackage]) -> io::Result<()> {
    let mut source = String::from("pub static EMBEDDED_PACKAGES: &[EmbeddedPackage] = &[\n");
    for package in packages {
        writeln!(
            source,
            "    EmbeddedPackage {{ file_name: {:?}, kind: {:?}, gzip_bytes: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{}\")) }},",
            package.file_name,
            package.kind.as_str(),
            package.generated_gz_name
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
    }
    source.push_str("];\n");
    fs::write(output_path, source)?;
    Ok(())
}

struct GzipStats {
    source_len: usize,
    gz_len: usize,
}

fn gzip_file(source_path: &Path, output_path: &Path) -> io::Result<GzipStats> {
    let source = fs::read(source_path)?;
    let mut encoder = GzEncoder::new(Vec::with_capacity(source.len() / 2), Compression::best());
    encoder.write_all(&source)?;
    let compressed = encoder.finish()?;
    fs::write(output_path, &compressed)?;
    Ok(GzipStats {
        source_len: source.len(),
        gz_len: compressed.len(),
    })
}

#[derive(Copy, Clone)]
enum AppPackageKind {
    Zip,
    Tar,
    TarGz,
}

impl AppPackageKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
        }
    }
}
