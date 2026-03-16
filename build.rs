use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use font_subset::Font;

const SOURCE_FONT_REL: &str = "installer_assets/HarmonyOS_Sans_SC_Regular.ttf";
const GENERATED_FONT_NAME: &str = "HarmonyOS_Sans_SC_Subset.ttf";
const GENERATED_TEXT_NAME: &str = "font_chars.txt";

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("missing OUT_DIR"));

    let source_font = manifest_dir.join(SOURCE_FONT_REL);
    let generated_font = out_dir.join(GENERATED_FONT_NAME);
    let generated_text = out_dir.join(GENERATED_TEXT_NAME);

    println!("cargo:rerun-if-changed={}", source_font.display());
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("installer_assets/info.json").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("installer_assets/Agreement.txt").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("installer_assets/extra_chars.txt").display()
    );
    print_rerun_for_dir(&manifest_dir.join("src"));

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
    add_chars(
        &mut set,
        " !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~",
    );

    collect_from_dir(&manifest_dir.join("src"), &mut set)?;
    collect_from_file(&manifest_dir.join("Cargo.toml"), &mut set)?;
    collect_from_file(&manifest_dir.join("installer_assets/info.json"), &mut set)?;
    collect_from_file(&manifest_dir.join("installer_assets/Agreement.txt"), &mut set)?;
    collect_from_file(&manifest_dir.join("installer_assets/extra_chars.txt"), &mut set)?;

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

fn generate_subset_font_with_rust(source_font: &Path, chars: &str, output_font: &Path) -> io::Result<bool> {
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
