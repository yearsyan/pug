use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    api::{ApiClient, EngineArtifact as RemoteEngineArtifact, EngineUploadInit},
    config::Config,
    util,
};

use super::{
    binaries::matching_binaries,
    model::{BuildContext, BuiltArtifact},
    targets::grouped_template_archs,
    toolchain::path_str,
};

pub(super) fn package_engine_artifacts(ctx: &BuildContext) -> Result<Vec<BuiltArtifact>> {
    let mut out = Vec::new();
    let package_dir = ctx.repo_root.join(".cache/pug/engine-packages");
    fs::create_dir_all(&package_dir)?;

    let host_dir = ctx.editor_output_dir();
    let editor_zip = package_dir.join(format!("editor-{}-{}.zip", ctx.host_api, ctx.host_arch));
    util::zip_paths(&editor_zip, &host_dir, std::slice::from_ref(&host_dir))?;
    out.push(make_built_artifact(
        ctx.host_api,
        "editor",
        Some(ctx.host_arch),
        Vec::new(),
        editor_zip,
    )?);

    let grouped = grouped_template_archs(&ctx.template_targets);
    let mut bundled = BTreeSet::new();
    for target in &ctx.template_targets {
        match target.platform.as_str() {
            "macos" | "linux" | "windows" => {
                let dir = ctx
                    .repo_root
                    .join("build")
                    .join(&target.godot_platform)
                    .join("export_templates");
                for kind in ["template_debug", "template_release"] {
                    let prefix = format!(
                        "godot.{}.{kind}.{}.mono",
                        target.godot_platform, target.arch
                    );
                    let files = matching_binaries(&dir, &prefix)?;
                    if files.is_empty() {
                        bail!("no file matching {prefix} in {}", dir.display());
                    }
                    let zip =
                        package_dir.join(format!("{}-{kind}-{}.zip", target.platform, target.arch));
                    util::zip_paths(&zip, &dir, &files)?;
                    out.push(make_built_artifact(
                        &target.platform,
                        kind,
                        Some(&target.arch),
                        Vec::new(),
                        zip,
                    )?);
                }
            }
            "android" | "ios" => {
                if !bundled.insert(target.platform.clone()) {
                    continue;
                }
                let dir = ctx
                    .repo_root
                    .join("build")
                    .join(&target.platform)
                    .join("export_templates");
                let zip = package_dir.join(format!("{}-template-bundle.zip", target.platform));
                util::zip_paths(&zip, &dir, std::slice::from_ref(&dir))?;
                let archs = grouped
                    .get(&target.platform)
                    .map(|items| {
                        items
                            .iter()
                            .map(|arch| api_template_arch(&target.platform, arch))
                            .collect()
                    })
                    .unwrap_or_else(Vec::new);
                out.push(make_built_artifact(
                    &target.platform,
                    "template_bundle",
                    None,
                    archs,
                    zip,
                )?);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn api_template_arch(platform: &str, arch: &str) -> String {
    match (platform, arch) {
        ("android", "arm64") => "arm64-v8a".to_string(),
        ("android", "arm32") => "armeabi-v7a".to_string(),
        ("android", "x86_32") => "x86".to_string(),
        _ => arch.to_string(),
    }
}

fn make_built_artifact(
    platform: &str,
    kind: &str,
    arch: Option<&str>,
    archs: Vec<String>,
    package_path: PathBuf,
) -> Result<BuiltArtifact> {
    let sha256 = util::sha256_file(&package_path)?;
    let size = util::file_size(&package_path)?;
    Ok(BuiltArtifact {
        platform: platform.to_string(),
        kind: kind.to_string(),
        arch: arch.map(str::to_string),
        archs,
        package_path,
        sha256,
        size,
    })
}

pub(super) fn upload_engine_artifacts(
    ctx: &BuildContext,
    artifacts: &[BuiltArtifact],
    force: bool,
) -> Result<()> {
    let cfg = Config::load()?;
    let api = ApiClient::from_config(&cfg)?;
    let project_name = ctx
        .project
        .name
        .as_deref()
        .context("project.json missing name; pannel uploads are project-scoped")?;
    let repo_commit = git_head(&ctx.repo_root)?;
    let repo_dirty = git_worktree_dirty(&ctx.repo_root)?;
    let use_dev_tag = cfg.uses_login_session_auth() && repo_dirty;
    let dev_key = use_dev_tag.then(make_dev_key);
    if use_dev_tag {
        println!("pug: dirty login-session build; requesting a unique dev engine tag");
    }
    let engine_commit = git_head(&ctx.godot_src)?;
    let (godot_version, godot_version_short) = godot_version(&ctx.godot_src)?;
    let engine_build_id = std::env::var("PANNEL_ENGINE_BUILD_ID")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|id| *id > 0);
    let existing = if use_dev_tag {
        Vec::new()
    } else {
        existing_engine_artifacts(&api, project_name, &repo_commit, &engine_commit)?
    };
    for artifact in artifacts {
        if let Some(remote) = existing
            .iter()
            .find(|remote| same_engine_artifact_slot(remote, artifact))
        {
            if remote.package_sha256 == artifact.sha256 && remote.package_size == artifact.size {
                println!(
                    "skipped {} {} {:?}: already published",
                    artifact.platform, artifact.kind, artifact.arch
                );
                continue;
            }
            bail!(
                "engine artifact already published with different package: {} {} {:?}",
                artifact.platform,
                artifact.kind,
                artifact.arch
            );
        }
        let init = api.engine_upload_init(&EngineUploadInit {
            project_name,
            engine_build_id,
            repo_commit: &repo_commit,
            repo_dirty,
            dev_key: dev_key.as_deref(),
            engine_commit: &engine_commit,
            godot_version: &godot_version,
            godot_version_short: &godot_version_short,
            platform: &artifact.platform,
            kind: &artifact.kind,
            arch: artifact.arch.as_deref(),
            archs: if artifact.archs.is_empty() {
                None
            } else {
                Some(&artifact.archs)
            },
            package_sha256: &artifact.sha256,
            package_size: artifact.size,
            force,
        })?;
        api.put_file(&init, &artifact.package_path)?;
        let complete = api.engine_upload_complete(&init.upload_id)?;
        println!(
            "uploaded {} {} {:?} -> {} status={} key={}",
            artifact.platform,
            artifact.kind,
            artifact.arch,
            complete.engine_tag.as_deref().unwrap_or(""),
            complete.status,
            init.s3_key
        );
    }
    Ok(())
}

fn existing_engine_artifacts(
    api: &ApiClient,
    project_name: &str,
    repo_commit: &str,
    engine_commit: &str,
) -> Result<Vec<RemoteEngineArtifact>> {
    let tags = api.engine_tags_for_project(project_name)?;
    let Some(tag) = tags
        .tags
        .into_iter()
        .find(|tag| tag.repo_commit == repo_commit && tag.engine_commit == engine_commit)
    else {
        return Ok(Vec::new());
    };
    Ok(api.engine_download(project_name, &tag.tag)?.artifacts)
}

fn same_engine_artifact_slot(remote: &RemoteEngineArtifact, local: &BuiltArtifact) -> bool {
    remote.platform == local.platform
        && remote.kind == local.kind
        && remote.arch == local.arch.as_deref().unwrap_or("")
}

pub fn git_head(repo: &Path) -> Result<String> {
    util::output_command(Command::new("git").args(["-C", path_str(repo), "rev-parse", "HEAD"]))
}

pub fn git_worktree_dirty(repo: &Path) -> Result<bool> {
    let status = util::output_command(Command::new("git").args([
        "-C",
        path_str(repo),
        "status",
        "--porcelain",
        "--untracked-files=all",
    ]))?;
    Ok(!status.trim().is_empty())
}

fn make_dev_key() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

pub fn godot_version(godot_src: &Path) -> Result<(String, String)> {
    let text = fs::read_to_string(godot_src.join("version.py"))?;
    let mut values = BTreeMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            values.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
        }
    }
    let major = values.get("major").context("version.py missing major")?;
    let minor = values.get("minor").context("version.py missing minor")?;
    let patch = values.get("patch").map(String::as_str).unwrap_or("x");
    let status = values.get("status").map(String::as_str).unwrap_or("dev");
    let version = format!("{major}.{minor}.{patch}-{status}");
    let short = format!("{major}{:02}", minor.parse::<u32>()?);
    Ok((version, short))
}

#[cfg(test)]
mod tests {
    use super::{
        api_template_arch, git_worktree_dirty, package_engine_artifacts, same_engine_artifact_slot,
    };
    use crate::{
        api::EngineArtifact as RemoteEngineArtifact,
        engine::model::{BuildContext, BuiltArtifact, ProjectJson, TemplateTarget},
    };
    use std::{fs, path::PathBuf, process::Command};
    use zip::ZipArchive;

    #[test]
    fn android_template_archs_use_api_abi_names() {
        assert_eq!(api_template_arch("android", "arm64"), "arm64-v8a");
        assert_eq!(api_template_arch("android", "arm32"), "armeabi-v7a");
        assert_eq!(api_template_arch("android", "x86_32"), "x86");
    }

    #[test]
    fn non_android_template_archs_are_unchanged() {
        assert_eq!(api_template_arch("ios", "arm64"), "arm64");
        assert_eq!(api_template_arch("macos", "x86_64"), "x86_64");
    }

    #[test]
    fn template_bundle_slot_matches_empty_remote_arch() {
        let remote = remote_artifact("android", "template_bundle", "");
        let local = built_artifact("android", "template_bundle", None);
        assert!(same_engine_artifact_slot(&remote, &local));
    }

    #[test]
    fn single_arch_slot_requires_same_arch() {
        let remote = remote_artifact("windows", "editor", "x86_64");
        let local = built_artifact("windows", "editor", Some("arm64"));
        assert!(!same_engine_artifact_slot(&remote, &local));
    }

    #[test]
    fn git_worktree_dirty_detects_uncommitted_changes() {
        let repo = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("init")
            .status()
            .unwrap();
        if !init.success() {
            return;
        }

        assert!(!git_worktree_dirty(repo.path()).unwrap());
        fs::write(repo.path().join("dirty.txt"), "dirty\n").unwrap();
        assert!(git_worktree_dirty(repo.path()).unwrap());
    }

    #[test]
    fn editor_package_contains_whole_editor_directory() {
        let repo = tempfile::tempdir().unwrap();
        let editor_dir = repo.path().join("build/windows/editor/x86_64");
        fs::create_dir_all(editor_dir.join("GodotSharp")).unwrap();
        fs::write(
            editor_dir.join("godot.windows.editor.x86_64.mono.exe"),
            "gui",
        )
        .unwrap();
        fs::write(
            editor_dir.join("godot.windows.editor.x86_64.mono.console.exe"),
            "console",
        )
        .unwrap();
        fs::write(editor_dir.join("GodotSharp/GodotSharp.dll"), "mono").unwrap();
        let ctx = BuildContext {
            repo_root: repo.path().to_path_buf(),
            godot_src: repo.path().join("godot"),
            project: ProjectJson::default(),
            host_godot: "windows",
            host_api: "windows",
            host_arch: "x86_64",
            template_targets: Vec::new(),
            scons_args: Vec::new(),
            manifest_public_key_path: None,
        };

        let artifacts = package_engine_artifacts(&ctx).unwrap();
        let editor = artifacts
            .iter()
            .find(|artifact| artifact.kind == "editor")
            .unwrap();
        let file = fs::File::open(&editor.package_path).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();
        let mut names = Vec::new();
        for index in 0..zip.len() {
            names.push(zip.by_index(index).unwrap().name().to_string());
        }

        assert!(names.contains(&"godot.windows.editor.x86_64.mono.exe".to_string()));
        assert!(names.contains(&"godot.windows.editor.x86_64.mono.console.exe".to_string()));
        assert!(names.contains(&"GodotSharp/GodotSharp.dll".to_string()));
    }

    #[test]
    fn desktop_template_package_contains_gui_and_console_binaries() {
        let repo = tempfile::tempdir().unwrap();
        let editor_dir = repo.path().join("build/windows/editor/x86_64");
        fs::create_dir_all(&editor_dir).unwrap();
        fs::write(
            editor_dir.join("godot.windows.editor.x86_64.mono.exe"),
            "editor",
        )
        .unwrap();

        let template_dir = repo.path().join("build/windows/export_templates");
        fs::create_dir_all(&template_dir).unwrap();
        for kind in ["template_debug", "template_release"] {
            fs::write(
                template_dir.join(format!("godot.windows.{kind}.x86_64.mono.console.exe")),
                "console",
            )
            .unwrap();
            fs::write(
                template_dir.join(format!("godot.windows.{kind}.x86_64.mono.exe")),
                "gui",
            )
            .unwrap();
        }

        let ctx = BuildContext {
            repo_root: repo.path().to_path_buf(),
            godot_src: repo.path().join("godot"),
            project: ProjectJson::default(),
            host_godot: "windows",
            host_api: "windows",
            host_arch: "x86_64",
            template_targets: vec![TemplateTarget {
                platform: "windows".to_string(),
                godot_platform: "windows".to_string(),
                arch: "x86_64".to_string(),
            }],
            scons_args: Vec::new(),
            manifest_public_key_path: None,
        };

        let artifacts = package_engine_artifacts(&ctx).unwrap();
        let release = artifacts
            .iter()
            .find(|artifact| artifact.kind == "template_release")
            .unwrap();
        let file = fs::File::open(&release.package_path).unwrap();
        let mut zip = ZipArchive::new(file).unwrap();
        let names = (0..zip.len())
            .map(|index| zip.by_index(index).unwrap().name().to_string())
            .collect::<Vec<_>>();

        assert!(names.contains(&"godot.windows.template_release.x86_64.mono.exe".to_string()));
        assert!(
            names.contains(&"godot.windows.template_release.x86_64.mono.console.exe".to_string())
        );
    }

    fn remote_artifact(platform: &str, kind: &str, arch: &str) -> RemoteEngineArtifact {
        RemoteEngineArtifact {
            platform: platform.to_string(),
            kind: kind.to_string(),
            arch: arch.to_string(),
            archs: Vec::new(),
            package_sha256: "0".repeat(64),
            package_size: 1,
            download_url: "https://example.invalid/artifact.zip".to_string(),
        }
    }

    fn built_artifact(platform: &str, kind: &str, arch: Option<&str>) -> BuiltArtifact {
        BuiltArtifact {
            platform: platform.to_string(),
            kind: kind.to_string(),
            arch: arch.map(str::to_string),
            archs: Vec::new(),
            package_path: PathBuf::from("artifact.zip"),
            sha256: "0".repeat(64),
            size: 1,
        }
    }
}
