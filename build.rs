use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    println!("cargo:rerun-if-changed={}", manifest_dir.join("installer_assets/info.json").display());
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

    let generated_ok = generate_subset_font(&source_font, &generated_text, &generated_font)
        .unwrap_or_else(|error| {
            println!("cargo:warning=font subsetting failed: {error}");
            false
        });

    if !generated_ok {
        fs::copy(&source_font, &generated_font).expect("failed to fallback to full font");
        println!("cargo:warning=using full Harmony font because subset generation was unavailable");
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
        "，。！？、；：‘’“”【】（）《》〈〉—…·+-=*/_[]{}()<>|\\\"'`~!@#$%^&,:.;? ",
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

fn generate_subset_font(source_font: &Path, text_file: &Path, output_font: &Path) -> io::Result<bool> {
    let source = source_font.to_string_lossy().to_string();
    let text = text_file.to_string_lossy().to_string();
    let output = output_font.to_string_lossy().to_string();

    let direct_output = Command::new("pyftsubset")
        .arg(&source)
        .arg(format!("--text-file={text}"))
        .arg(format!("--output-file={output}"))
        .arg("--no-hinting")
        .output();
    if let Ok(output_data) = direct_output {
        if output_data.status.success() {
            println!("cargo:warning=font subset generated with pyftsubset");
            return Ok(true);
        }
        let stderr = String::from_utf8_lossy(&output_data.stderr);
        println!("cargo:warning=pyftsubset failed: {}", stderr.trim());
    }

    let mut python_candidates: Vec<(String, Vec<String>)> = Vec::new();
    if let Ok(python_env) = env::var("PYTHON") {
        if !python_env.trim().is_empty() {
            python_candidates.push((python_env, vec![]));
        }
    }
    python_candidates.push(("python".to_owned(), vec![]));
    python_candidates.push(("py".to_owned(), vec!["-3".to_owned()]));

    for (command_name, prefix_args) in python_candidates {
        let mut command = Command::new(&command_name);
        for arg in &prefix_args {
            command.arg(arg);
        }
        let output_data = command
            .arg("-m")
            .arg("fontTools.subset")
            .arg(&source)
            .arg(format!("--text-file={text}"))
            .arg(format!("--output-file={output}"))
            .arg("--no-hinting")
            .output();

        if let Ok(process_output) = output_data {
            if process_output.status.success() {
                println!(
                    "cargo:warning=font subset generated with {} {}",
                    command_name,
                    if prefix_args.is_empty() {
                        "-m fontTools.subset"
                    } else {
                        "-3 -m fontTools.subset"
                    }
                );
                return Ok(true);
            }
            let stderr = String::from_utf8_lossy(&process_output.stderr);
            println!(
                "cargo:warning={} fontTools.subset failed: {}",
                command_name,
                stderr.trim()
            );
        }
    }

    Ok(false)
}
