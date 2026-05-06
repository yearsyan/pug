use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeSet,
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use crate::util;

const ANDROID_NDK_VERSION: &str = "28.1.13356709";

pub(super) fn find_scons() -> Result<Vec<String>> {
    let mut tried = Vec::new();
    for candidate in command_candidates("scons") {
        let candidate = normalize_external_path(candidate);
        if !candidate.is_file() {
            continue;
        }
        let command = command_invocation(&candidate);
        tried.push(command.join(" "));
        if command_status_success(&command, &["--version"]) {
            return Ok(command);
        }
    }

    for candidate in python_candidates() {
        let candidate = normalize_external_path(candidate);
        if !candidate.is_file() {
            continue;
        }
        tried.push(format!("{} -m SCons", candidate.display()));
        if command_status_success(
            &[candidate.to_string_lossy().to_string()],
            &["-m", "SCons", "--version"],
        ) {
            return Ok(vec![
                candidate
                    .canonicalize()
                    .map(normalize_external_path)
                    .unwrap_or(candidate)
                    .to_string_lossy()
                    .to_string(),
                "-m".to_string(),
                "SCons".to_string(),
            ]);
        }
    }

    bail!(
        "SCons was not found. Install SCons or set PATH to a working scons executable. Tried: {}",
        tried.join(", ")
    )
}

pub(super) fn ensure_android_toolchain() -> Result<()> {
    let sdk = android_sdk()
        .context("Android SDK not configured. Set ANDROID_HOME or ANDROID_SDK_ROOT")?;
    let toolchain = sdk
        .join("ndk")
        .join(ANDROID_NDK_VERSION)
        .join("toolchains/llvm/prebuilt")
        .join(android_ndk_host())
        .join("bin");
    for tool in ["clang", "clang++", "llvm-ar", "llvm-ranlib"] {
        let tool_path = android_tool_path(&toolchain, tool);
        if !tool_path.is_file() {
            bail!("Android NDK tool missing: {}", tool_path.display());
        }
    }
    Ok(())
}

fn android_tool_path(toolchain: &Path, tool: &str) -> PathBuf {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    toolchain.join(format!("{tool}{suffix}"))
}

pub fn android_sdk() -> Option<PathBuf> {
    env::var("ANDROID_HOME")
        .or_else(|_| env::var("ANDROID_SDK_ROOT"))
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            dirs::home_dir()
                .map(|h| h.join("Library/Android/sdk"))
                .filter(|p| p.is_dir())
        })
        .or_else(|| {
            dirs::home_dir()
                .map(|h| h.join("Android/Sdk"))
                .filter(|p| p.is_dir())
        })
}

pub fn android_ndk_host() -> &'static str {
    match env::consts::OS {
        "macos" => "darwin-x86_64",
        "linux" => "linux-x86_64",
        "windows" => "windows-x86_64",
        _ => "unknown",
    }
}

pub(super) fn ensure_android_swappy(godot_src: &Path) -> Result<()> {
    let swappy = godot_src.join("thirdparty/swappy-frame-pacing");
    if ["arm64-v8a", "armeabi-v7a", "x86", "x86_64"]
        .iter()
        .all(|arch| swappy.join(arch).join("libswappy_static.a").is_file())
    {
        return Ok(());
    }
    let python = python_command()?;
    util::run_command(
        Command::new(python)
            .arg(godot_src.join("misc/scripts/install_swappy_android.py"))
            .current_dir(godot_src),
    )
}

pub(super) fn path_str(path: &Path) -> &str {
    path.to_str().expect("path is not valid UTF-8")
}

pub(super) fn python_command() -> Result<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut tried = Vec::new();
    for candidate in python_candidates() {
        let candidate = normalize_external_path(candidate);
        let key = if cfg!(windows) {
            candidate.to_string_lossy().to_ascii_lowercase()
        } else {
            candidate.to_string_lossy().to_string()
        };
        if !seen.insert(key) {
            continue;
        }

        tried.push(candidate.display().to_string());
        if !candidate.is_file() {
            continue;
        }
        let Ok(status) = Command::new(&candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        else {
            continue;
        };
        if status.success() {
            return candidate
                .canonicalize()
                .map(normalize_external_path)
                .with_context(|| format!("resolve Python path {}", candidate.display()));
        }
    }

    bail!(
        "usable Python interpreter not found. Set PYTHON3 or PYTHON to a Python executable. Tried: {}",
        tried.join(", ")
    )
}

fn python_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for var in ["PYTHON3", "PYTHON"] {
        if let Some(value) = env::var_os(var) {
            candidates.extend(command_candidates(&value.to_string_lossy()));
        }
    }
    for name in ["python3", "python"] {
        candidates.extend(command_candidates(name));
    }
    candidates
}

fn command_candidates(command: &str) -> Vec<PathBuf> {
    let command = command.trim().trim_matches('"');
    if command.is_empty() {
        return Vec::new();
    }

    let path = PathBuf::from(command);
    if path.is_absolute() || command.contains('/') || command.contains('\\') {
        return executable_variants(path);
    }

    let mut out = Vec::new();
    let Some(paths) = env::var_os("PATH").or_else(|| env::var_os("Path")) else {
        return out;
    };
    for dir in env::split_paths(&paths) {
        out.extend(executable_variants(dir.join(command)));
    }
    out
}

fn executable_variants(path: PathBuf) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        if path.extension().is_some() {
            return vec![path];
        }

        let mut out = Vec::new();
        let pathext = env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut saw_ps1 = false;
        for ext in pathext.split(';').filter(|ext| !ext.is_empty()) {
            saw_ps1 |= ext.eq_ignore_ascii_case(".PS1") || ext.eq_ignore_ascii_case("PS1");
            let suffix = if ext.starts_with('.') {
                ext.to_string()
            } else {
                format!(".{ext}")
            };
            let mut value = path.as_os_str().to_os_string();
            value.push(suffix);
            out.push(PathBuf::from(value));
        }
        if !saw_ps1 {
            let mut value = path.as_os_str().to_os_string();
            value.push(".PS1");
            out.push(PathBuf::from(value));
        }
        out.push(path);
        out
    }

    #[cfg(not(windows))]
    {
        vec![path]
    }
}

fn command_invocation(path: &Path) -> Vec<String> {
    #[cfg(windows)]
    {
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if ext.eq_ignore_ascii_case("ps1") {
            return vec![
                "powershell".to_string(),
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                path.to_string_lossy().to_string(),
            ];
        }
    }

    vec![path.to_string_lossy().to_string()]
}

fn command_status_success(command: &[String], args: &[&str]) -> bool {
    if command.is_empty() {
        return false;
    }
    matches!(
        Command::new(&command[0])
            .args(&command[1..])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
        Ok(status) if status.success()
    )
}

pub(super) fn normalize_external_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix("\\\\?\\") {
            if let Some(unc) = rest.strip_prefix("UNC\\") {
                return PathBuf::from(format!("\\\\{unc}"));
            }
            let bytes = rest.as_bytes();
            if bytes.len() >= 3 && bytes[1] == b':' && bytes[2] == b'\\' {
                return PathBuf::from(rest);
            }
        }
    }
    path
}
