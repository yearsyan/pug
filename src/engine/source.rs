use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::util;

use super::{
    model::ProjectJson,
    toolchain::{normalize_external_path, path_str, python_command},
};

pub fn find_repo_root() -> Result<PathBuf> {
    find_repo_root_from(&std::env::current_dir()?)
}

pub(super) fn find_repo_root_from(cwd: &Path) -> Result<PathBuf> {
    let root = util::output_command(
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "--show-toplevel"]),
    )
    .with_context(|| format!("resolve git root from {}", cwd.display()))?;
    let root = normalize_external_path(
        PathBuf::from(root)
            .canonicalize()
            .with_context(|| format!("resolve git root path from {}", cwd.display()))?,
    );

    if !root.join("project.json").is_file()
        || !root.join("patches").is_dir()
        || !root.join("modules").is_dir()
    {
        bail!(
            "git root {} is not a godot_custom overlay repo; expected project.json, patches/, and modules/",
            root.display()
        );
    }
    Ok(root)
}

pub(super) fn read_project_json(repo: &Path) -> Result<ProjectJson> {
    let path = repo.join("project.json");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

pub(super) fn resolve_godot_source(repo: &Path, arg: Option<&Path>) -> Result<PathBuf> {
    let cache = repo.join(".repocache");
    if let Some(path) = arg {
        let path = normalize_external_path(
            path.canonicalize()
                .with_context(|| format!("resolve {}", path.display()))?,
        );
        fs::write(&cache, path.to_string_lossy().as_bytes())?;
        return Ok(path);
    }
    let cached = fs::read_to_string(&cache)
        .with_context(|| "no Godot source specified and .repocache is missing")?;
    let path = normalize_external_path(PathBuf::from(cached.trim()));
    if !path.join("SConstruct").is_file() {
        bail!("SConstruct not found at {}", path.display());
    }
    Ok(path)
}

pub(super) fn godot_tag(project: &ProjectJson) -> Option<String> {
    project
        .tag
        .clone()
        .or_else(|| project.godot.as_ref().and_then(|g| g.tag.clone()))
}

pub(super) fn force_restore_godot_source(
    repo: &Path,
    godot_src: &Path,
    tag: Option<&str>,
) -> Result<()> {
    if let Some(tag) = tag.filter(|v| !v.is_empty()) {
        eprintln!("pug: checking out Godot tag {tag}");
        let target = Command::new("git")
            .args([
                "-C",
                path_str(godot_src),
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/tags/{tag}^{{commit}}"),
            ])
            .output()?;
        if !target.status.success() {
            eprintln!("pug: fetching missing Godot tag {tag}");
            util::run_command(Command::new("git").args([
                "-C",
                path_str(godot_src),
                "fetch",
                "origin",
                "tag",
                tag,
            ]))?;
        }
        let target_commit = util::output_command(Command::new("git").args([
            "-C",
            path_str(godot_src),
            "rev-parse",
            &format!("refs/tags/{tag}^{{commit}}"),
        ]))?;
        util::run_command(Command::new("git").args([
            "-C",
            path_str(godot_src),
            "checkout",
            "--detach",
            "--force",
            tag,
        ]))?;
        util::run_command(Command::new("git").args([
            "-C",
            path_str(godot_src),
            "reset",
            "--hard",
            &target_commit,
        ]))?;
    } else {
        eprintln!("pug: resetting Godot source to HEAD");
        util::run_command(Command::new("git").args([
            "-C",
            path_str(godot_src),
            "reset",
            "--hard",
            "HEAD",
        ]))?;
    }
    clean_patch_added_files(repo, godot_src)?;
    ensure_clean_godot_source(godot_src, "after forced restore")?;
    Ok(())
}

fn ensure_clean_godot_source(godot_src: &Path, context: &str) -> Result<()> {
    let status = util::output_command(Command::new("git").args([
        "-C",
        path_str(godot_src),
        "status",
        "--porcelain",
        "--untracked-files=no",
    ]))?;
    if !status.trim().is_empty() {
        bail!("Godot source has tracked local changes {context}:\n{status}");
    }
    Ok(())
}

fn clean_patch_added_files(repo: &Path, godot_src: &Path) -> Result<()> {
    let added = patch_added_paths(repo)?;
    if added.is_empty() {
        return Ok(());
    }
    eprintln!(
        "pug: cleaning {} untracked file path(s) created by patches",
        added.len()
    );
    for chunk in added.chunks(64) {
        let mut cmd = Command::new("git");
        cmd.args(["-C", path_str(godot_src), "clean", "-f", "--"]);
        cmd.args(chunk);
        util::run_command(&mut cmd)?;
    }
    Ok(())
}

fn patch_added_paths(repo: &Path) -> Result<Vec<String>> {
    let patches = repo.join("patches");
    if !patches.is_dir() {
        return Ok(Vec::new());
    }

    let mut paths = BTreeSet::new();
    for entry in fs::read_dir(&patches)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let diff = entry.path().join("patch.diff");
        if !diff.is_file() {
            continue;
        }
        let mut touched = BTreeSet::new();
        collect_patch_paths(&fs::read_to_string(&diff)?, &mut touched, &mut paths);
    }
    Ok(paths.into_iter().collect())
}

fn collect_patch_paths(
    diff: &str,
    touched_paths: &mut BTreeSet<String>,
    added_paths: &mut BTreeSet<String>,
) {
    let mut current_path: Option<String> = None;
    let mut new_file = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            current_path = rest
                .rsplit_once(" b/")
                .map(|(_, path)| path.to_string())
                .or_else(|| rest.strip_prefix("a/").map(|path| path.to_string()));
            if let Some(path) = &current_path {
                touched_paths.insert(path.clone());
            }
            new_file = false;
            continue;
        }
        if line.starts_with("new file mode ") {
            new_file = true;
            continue;
        }
        if new_file {
            if let Some(path) = line.strip_prefix("+++ b/") {
                added_paths.insert(path.to_string());
                new_file = false;
            } else if line.starts_with("@@") {
                if let Some(path) = current_path.take() {
                    added_paths.insert(path);
                }
                new_file = false;
            }
        }
    }
}

pub(super) fn apply_patches(
    repo: &Path,
    godot_src: &Path,
    project: &ProjectJson,
) -> Result<Vec<PathBuf>> {
    let mut applied = Vec::new();
    let patches = repo.join("patches");
    if !patches.is_dir() {
        eprintln!("pug: no patches directory found");
        return Ok(applied);
    }
    eprintln!("pug: applying patches from {}", patches.display());
    let mut dirs = fs::read_dir(&patches)?.collect::<Result<Vec<_>, _>>()?;
    dirs.sort_by_key(|e| e.file_name());
    for entry in dirs {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let diff = entry.path().join("patch.diff");
        if !diff.is_file() {
            continue;
        }
        if !should_apply_patch(&entry.path(), project)? {
            eprintln!("pug: patch skipped {}", patch_label(&entry.path()));
            continue;
        }
        eprintln!("pug: patch apply {}", patch_label(&entry.path()));
        util::run_command(Command::new("git").args([
            "-C",
            path_str(godot_src),
            "apply",
            "--check",
            path_str(&diff),
        ]))?;
        util::run_command(Command::new("git").args([
            "-C",
            path_str(godot_src),
            "apply",
            path_str(&diff),
        ]))?;
        applied.push(diff);
    }
    eprintln!("pug: applied {} patch(es)", applied.len());
    Ok(applied)
}

fn should_apply_patch(patch_dir: &Path, project: &ProjectJson) -> Result<bool> {
    let hook = patch_dir.join("patch.py");
    if !hook.is_file() {
        return Ok(true);
    }
    let script = "import importlib.util,json,sys; p=sys.argv[1]; c=json.load(open(sys.argv[2])); spec=importlib.util.spec_from_file_location('patch_hook', p); m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m); print('1' if (not hasattr(m,'should_apply') or m.should_apply(c)) else '0')";
    let tmp = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(tmp.as_file(), project)?;
    let python = python_command()?;
    let out = util::output_command(
        Command::new(python)
            .arg("-c")
            .arg(script)
            .arg(&hook)
            .arg(tmp.path()),
    )?;
    Ok(out.trim() != "0")
}

pub(super) fn revert_patches(godot_src: &Path, applied: &[PathBuf]) -> Result<()> {
    if applied.is_empty() {
        eprintln!("pug: no patches to revert");
        return Ok(());
    }
    eprintln!("pug: reverting {} patch(es)", applied.len());
    for diff in applied.iter().rev() {
        eprintln!(
            "pug: patch revert {}",
            patch_label(diff.parent().unwrap_or(diff))
        );
        let status = Command::new("git")
            .args(["-C", path_str(godot_src), "apply", "-R", path_str(diff)])
            .status();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!(
                    "pug: reverse patch failed for {} ({status}); restoring touched paths",
                    diff.display()
                );
                restore_patch_paths(godot_src, diff)?;
            }
            Err(err) => {
                eprintln!(
                    "pug: reverse patch failed for {} ({err}); restoring touched paths",
                    diff.display()
                );
                restore_patch_paths(godot_src, diff)?;
            }
        }
    }
    Ok(())
}

fn restore_patch_paths(godot_src: &Path, diff: &Path) -> Result<()> {
    let mut touched = BTreeSet::new();
    let mut added = BTreeSet::new();
    collect_patch_paths(&fs::read_to_string(diff)?, &mut touched, &mut added);
    let tracked: Vec<String> = touched.difference(&added).cloned().collect();
    for chunk in tracked.chunks(64) {
        let mut cmd = Command::new("git");
        cmd.args([
            "-C",
            path_str(godot_src),
            "restore",
            "--source",
            "HEAD",
            "--",
        ]);
        cmd.args(chunk);
        util::run_command(&mut cmd)?;
    }
    let added: Vec<String> = added.into_iter().collect();
    for chunk in added.chunks(64) {
        let mut cmd = Command::new("git");
        cmd.args(["-C", path_str(godot_src), "clean", "-f", "--"]);
        cmd.args(chunk);
        util::run_command(&mut cmd)?;
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct SplashRestore {
    target: PathBuf,
    backup: PathBuf,
    generated: PathBuf,
    generated_backup: PathBuf,
    generated_existed: bool,
}

pub(super) fn prepare_splash(
    repo: &Path,
    godot_src: &Path,
    project: &ProjectJson,
) -> Result<Option<SplashRestore>> {
    let Some(splash) = splash_ref(project) else {
        eprintln!("pug: no splash override configured");
        return Ok(None);
    };
    let target = godot_src.join("main/splash.png");
    let generated = godot_src.join("main/splash.gen.h");
    let data = if splash.starts_with("http://") || splash.starts_with("https://") {
        reqwest::blocking::get(&splash)?.bytes()?.to_vec()
    } else {
        let path = if Path::new(&splash).is_absolute() {
            PathBuf::from(&splash)
        } else {
            repo.join(&splash)
        };
        fs::read(&path).with_context(|| format!("read splash {}", path.display()))?
    };
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("splash image must be PNG");
    }
    if fs::read(&target)? == data {
        eprintln!("pug: splash override already matches {}", target.display());
        return Ok(None);
    }
    eprintln!("pug: replacing splash {}", target.display());
    let cache = repo.join(".cache/splash");
    fs::create_dir_all(&cache)?;
    let backup = cache.join(format!("main_splash.{}.png", std::process::id()));
    let generated_backup = cache.join(format!("splash_gen.{}.h", std::process::id()));
    fs::copy(&target, &backup)?;
    let generated_existed = generated.is_file();
    if generated_existed {
        fs::copy(&generated, &generated_backup)?;
        fs::remove_file(&generated)?;
    }
    fs::write(&target, data)?;
    Ok(Some(SplashRestore {
        target,
        backup,
        generated,
        generated_backup,
        generated_existed,
    }))
}

pub(super) fn restore_splash(restore: SplashRestore) -> Result<()> {
    eprintln!("pug: restoring splash {}", restore.target.display());
    fs::copy(&restore.backup, &restore.target)?;
    let _ = fs::remove_file(&restore.backup);
    if restore.generated_existed {
        fs::copy(&restore.generated_backup, &restore.generated)?;
        let _ = fs::remove_file(&restore.generated_backup);
    } else if restore.generated.is_file() {
        fs::remove_file(&restore.generated)?;
    }
    Ok(())
}

fn patch_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn splash_ref(project: &ProjectJson) -> Option<String> {
    fn from_value(value: &Option<Value>) -> Option<String> {
        match value {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Object(map)) => map
                .get("image")
                .or_else(|| map.get("path"))
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        }
    }
    from_value(&project.splash)
        .or_else(|| from_value(&project.boot_splash))
        .or_else(|| project.splash_image.clone())
}
