use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(crate) fn find_matching_binary(dir: &Path, prefix: &str, target: &str) -> Result<PathBuf> {
    if let Some(path) = preferred_exact_binary(dir, prefix, target) {
        return Ok(path);
    }
    let candidates = matching_binaries(dir, prefix)?;
    if let Some(path) = choose_preferred_binary(candidates, prefix, target) {
        return Ok(path);
    }
    bail!("no file matching {prefix} in {}", dir.display())
}

pub(crate) fn matching_binaries(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(prefix) && entry.file_type()?.is_file() && !is_ignored_sidecar(&name) {
            candidates.push(entry.path());
        }
    }
    candidates.sort_by_key(|path| path.file_name().map(|name| name.to_os_string()));
    Ok(candidates)
}

pub(crate) fn choose_preferred_binary(
    candidates: Vec<PathBuf>,
    prefix: &str,
    target: &str,
) -> Option<PathBuf> {
    if target == "editor" {
        for preferred in preferred_editor_names(prefix) {
            if let Some(path) = candidates
                .iter()
                .find(|path| file_name_eq(path, &preferred))
            {
                return Some(path.clone());
            }
        }
    }
    if let Some(path) = candidates.iter().find(|path| !is_console_binary(path)) {
        return Some(path.clone());
    }
    candidates.into_iter().next()
}

fn preferred_exact_binary(dir: &Path, prefix: &str, target: &str) -> Option<PathBuf> {
    if target != "editor" {
        return None;
    }
    for name in preferred_editor_names(prefix) {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn preferred_editor_names(prefix: &str) -> [String; 2] {
    [format!("{prefix}.exe"), prefix.to_string()]
}

fn is_ignored_sidecar(name: &str) -> bool {
    name.ends_with(".exp") || name.ends_with(".lib")
}

fn is_console_binary(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains(".console."))
}

fn file_name_eq(path: &Path, expected: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::{choose_preferred_binary, find_matching_binary};
    use std::path::PathBuf;

    #[test]
    fn prefers_gui_exe_over_console_regardless_of_order() {
        let prefix = "godot.windows.editor.x86_64.mono";
        let selected = choose_preferred_binary(
            vec![
                PathBuf::from("godot.windows.editor.x86_64.mono.console.exe"),
                PathBuf::from("godot.windows.editor.x86_64.mono.exe"),
            ],
            prefix,
            "editor",
        )
        .unwrap();
        assert_eq!(
            selected,
            PathBuf::from("godot.windows.editor.x86_64.mono.exe")
        );
    }

    #[test]
    fn exact_editor_match_does_not_depend_on_directory_order() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = "godot.windows.editor.x86_64.mono";
        std::fs::write(dir.path().join(format!("{prefix}.console.exe")), "").unwrap();
        std::fs::write(dir.path().join(format!("{prefix}.exe")), "").unwrap();

        let selected = find_matching_binary(dir.path(), prefix, "editor").unwrap();
        assert_eq!(
            selected.file_name().unwrap(),
            format!("{prefix}.exe").as_str()
        );
    }

    #[test]
    fn template_prefers_gui_exe_over_console() {
        let prefix = "godot.windows.template_release.x86_64.mono";
        let selected = choose_preferred_binary(
            vec![
                PathBuf::from("godot.windows.template_release.x86_64.mono.console.exe"),
                PathBuf::from("godot.windows.template_release.x86_64.mono.exe"),
            ],
            prefix,
            "template_release",
        )
        .unwrap();
        assert_eq!(
            selected,
            PathBuf::from("godot.windows.template_release.x86_64.mono.exe")
        );
    }
}
