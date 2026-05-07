use anyhow::{Context, Result, anyhow, bail};
use inquire::Select;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    api::{ApiClient, EngineArtifact},
    config::{Config, expand_tilde},
    platform, util,
};

mod artifacts;
mod binaries;
mod model;
mod source;
mod targets;
mod toolchain;

pub use artifacts::{git_head, godot_version};
use artifacts::{package_engine_artifacts, upload_engine_artifacts};
use binaries::{choose_preferred_binary, find_matching_binary, matching_binaries};
#[cfg(test)]
use model::{ArchSection, ProjectJson, TemplateTarget};
use model::{BuildContext, editor_output_dir};
pub use source::find_repo_root;
#[cfg(test)]
use source::find_repo_root_from;
use source::{
    apply_patches, force_restore_godot_source, godot_tag, prepare_splash, read_project_json,
    resolve_godot_source, restore_splash, revert_patches,
};
#[cfg(test)]
use targets::default_template_arches;
use targets::{grouped_template_archs, resolve_template_targets, validate_targets};
#[cfg(test)]
use toolchain::normalize_external_path;
use toolchain::{ensure_android_swappy, find_scons, path_str, python_command};

pub use toolchain::{android_ndk_host, android_sdk};

const NO_LOG_CPPDEFINE: &str = "GODOT_CUSTOM_NO_LOG";

pub struct EngineBuildOptions {
    pub upload: bool,
    pub template_platforms: Option<String>,
    pub godot_source: Option<PathBuf>,
    pub skip_patches: bool,
    pub no_restore: bool,
    pub no_log: bool,
    pub force: bool,
    pub scons_args: Vec<String>,
}

pub fn build(opts: EngineBuildOptions) -> Result<()> {
    if opts.upload {
        Config::load()?.verify_access_token()?;
    }

    let mut ctx = prepare_context(&opts)?;
    if opts.no_log {
        add_cppdefine(&mut ctx.scons_args, NO_LOG_CPPDEFINE);
    }
    validate_targets(&ctx)?;

    eprintln!("pug: restoring Godot source at {}", ctx.godot_src.display());
    force_restore_godot_source(
        &ctx.repo_root,
        &ctx.godot_src,
        godot_tag(&ctx.project).as_deref(),
    )?;
    let mut applied = Vec::new();
    let mut splash_restore = None;
    let keep_modifications = opts.no_restore;

    let result = (|| -> Result<()> {
        if !opts.skip_patches {
            applied = apply_patches(&ctx.repo_root, &ctx.godot_src, &ctx.project)?;
        }
        splash_restore = prepare_splash(&ctx.repo_root, &ctx.godot_src, &ctx.project)?;

        build_editor(&ctx)?;
        build_templates(&ctx)?;

        if opts.upload {
            let artifacts = package_engine_artifacts(&ctx)?;
            upload_engine_artifacts(&ctx, &artifacts, opts.force)?;
        }
        Ok(())
    })();

    if !keep_modifications {
        if let Some(restore) = splash_restore {
            restore_splash(restore)?;
        }
        revert_patches(&ctx.godot_src, &applied)?;
    } else {
        eprintln!("pug: keeping patches and splash changes because --no-restore was set");
    }
    result
}

pub fn list(remote_only: bool) -> Result<()> {
    let cfg = Config::load()?;
    let project_name = resolve_project_name()?;
    if !remote_only {
        let local = installed_engine_tags(&cfg)?;
        if !local.is_empty() {
            println!("local:");
            for tag in local {
                let marker = if cfg.engine.current == tag { "*" } else { " " };
                println!("{marker} {tag}");
            }
        }
    }
    let api = ApiClient::from_config(&cfg)?;
    let tags = api.engine_tags_for_project(&project_name)?;
    println!("remote:");
    for tag in tags.tags {
        let marker = if cfg.engine.current == tag.tag { "*" } else { " " };
        println!(
            "{marker} {}  godot={} short={} repo={} engine={}",
            tag.tag, tag.godot_version, tag.godot_version_short, tag.repo_commit, tag.engine_commit
        );
    }
    Ok(())
}

pub fn install(tag: Option<String>, download_only: bool) -> Result<()> {
    let cfg = Config::load()?;
    let project_name = resolve_project_name()?;
    let tag = tag
        .or_else(|| read_project_engine_tag().ok().flatten())
        .or_else(|| (!cfg.engine.current.is_empty()).then(|| cfg.engine.current.clone()))
        .context("no engine tag specified and no project/config current tag found")?;
    let api = ApiClient::from_config(&cfg)?;
    let response = api.engine_download(&project_name, &tag)?;
    let target = choose_editor_artifact(&response.artifacts)?;
    let tmp = tempfile::tempdir()?;
    let zip = tmp.path().join("engine.zip");
    api.download_to(&target.download_url, &zip)?;
    let sha = util::sha256_file(&zip)?;
    if sha != target.package_sha256 {
        bail!(
            "downloaded engine sha mismatch: got {sha} want {}",
            target.package_sha256
        );
    }
    let size = util::file_size(&zip)?;
    if size != target.package_size {
        bail!(
            "downloaded engine size mismatch: got {size} want {}",
            target.package_size
        );
    }
    if download_only {
        println!("{}", zip.display());
        return Ok(());
    }
    let install_dir = cfg.install_dir()?.join(&response.tag);
    util::ensure_clean_dir(&install_dir)?;
    util::unzip_to(&zip, &install_dir)?;
    println!("installed {} -> {}", response.tag, install_dir.display());
    Ok(())
}

pub fn use_tag(tag: Option<&str>) -> Result<()> {
    let mut cfg = Config::load()?;
    let tag = match tag {
        Some(tag) => tag.to_string(),
        None => choose_installed_engine_tag(&cfg)?,
    };
    let install_dir = cfg.install_dir()?.join(&tag);
    if !install_dir.is_dir() {
        bail!("engine tag is not installed locally: {tag}");
    }
    cfg.engine.current = tag;
    cfg.save()?;
    println!("{}", cfg.engine.current);
    Ok(())
}

pub fn current() -> Result<()> {
    let cfg = Config::load()?;
    if cfg.engine.current.is_empty() {
        bail!("no current engine configured");
    }
    println!("{}", cfg.engine.current);
    Ok(())
}

pub fn uninstall(tag: &str) -> Result<()> {
    let cfg = Config::load()?;
    let dir = cfg.install_dir()?.join(tag);
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    println!("removed {}", dir.display());
    Ok(())
}

fn installed_engine_tags(cfg: &Config) -> Result<Vec<String>> {
    let install_dir = cfg.install_dir()?;
    if !install_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut tags = Vec::new();
    for entry in fs::read_dir(&install_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            tags.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    tags.sort();
    Ok(tags)
}

fn choose_installed_engine_tag(cfg: &Config) -> Result<String> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("engine tag is required when not running in an interactive terminal");
    }

    let tags = installed_engine_tags(cfg)?;
    if tags.is_empty() {
        bail!("no local engines installed; run `pug engine install <tag>` first");
    }

    let start = tags
        .iter()
        .position(|tag| tag == &cfg.engine.current)
        .unwrap_or(0);
    Select::new("Select engine", tags)
        .with_starting_cursor(start)
        .prompt()
        .map_err(Into::into)
}

pub fn start(with_engine: Option<&Path>, project: Option<&Path>, args: &[String]) -> Result<()> {
    let editor = resolve_editor(with_engine)?;
    let mut cmd = Command::new(editor);
    if let Some(project) = project {
        cmd.arg("--path").arg(project);
    }
    cmd.args(args);
    util::run_command(&mut cmd)
}

pub fn resolve_editor(with_engine: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = with_engine {
        return existing_file(path);
    }
    if let Some(tag) = read_project_engine_tag()? {
        let cfg = Config::load()?;
        let dir = cfg.install_dir()?.join(&tag);
        if dir.is_dir() {
            if let Some(path) = find_editor_in_dir(&dir)? {
                return Ok(path);
            }
        } else if cfg.extension.auto_fetch_engine {
            install(Some(tag), false)?;
            if let Some(path) = find_editor_in_dir(&dir)? {
                return Ok(path);
            }
        }
    }
    let cfg = Config::load()?;
    if !cfg.engine.current.is_empty() {
        let dir = cfg.install_dir()?.join(&cfg.engine.current);
        if let Some(path) = find_editor_in_dir(&dir)? {
            return Ok(path);
        }
    }
    if !cfg.engine.editor_path.is_empty() {
        return existing_file(&expand_tilde(&cfg.engine.editor_path)?);
    }
    let repo = find_repo_root()?;
    let build_dir = editor_output_dir(
        &repo,
        platform::host_godot_platform()?,
        platform::host_arch(),
    );
    if let Some(path) = find_editor_in_dir(&build_dir)? {
        return Ok(path);
    }
    let legacy_dir = repo.join("build").join(platform::host_godot_platform()?);
    find_editor_in_dir(&legacy_dir)?.ok_or_else(|| anyhow!("no editor binary found"))
}

fn prepare_context(opts: &EngineBuildOptions) -> Result<BuildContext> {
    let repo_root = find_repo_root()?;
    let project = read_project_json(&repo_root)?;
    let godot_src = resolve_godot_source(&repo_root, opts.godot_source.as_deref())?;
    let host_api = platform::host_platform()?;
    let host_godot = platform::host_godot_platform()?;
    let host_arch = platform::host_arch();
    let template_targets = resolve_template_targets(&project, opts.template_platforms.as_deref())?;
    Ok(BuildContext {
        repo_root,
        godot_src,
        project,
        host_godot,
        host_api,
        host_arch,
        template_targets,
        scons_args: opts.scons_args.clone(),
    })
}

fn build_editor(ctx: &BuildContext) -> Result<()> {
    run_scons(
        ctx,
        ctx.host_godot,
        ctx.host_arch,
        &[("target", "editor"), ("production", "yes")],
        module_overrides(ctx, "editor"),
    )?;
    build_mono_editor_support(ctx)?;
    copy_binaries(
        &ctx.godot_src,
        &ctx.editor_output_dir(),
        ctx.host_godot,
        "editor",
        ctx.host_arch,
        true,
    )
}

fn build_templates(ctx: &BuildContext) -> Result<()> {
    let grouped = grouped_template_archs(&ctx.template_targets);
    let mut built_bundles = BTreeSet::new();
    for target in &ctx.template_targets {
        match target.platform.as_str() {
            "macos" | "linux" | "windows" => {
                build_desktop_templates(ctx, &target.godot_platform, &target.arch)?;
            }
            "android" => {
                if built_bundles.insert(target.platform.clone()) {
                    build_android_templates(ctx, &grouped["android"])?;
                }
            }
            "ios" => {
                if built_bundles.insert(target.platform.clone()) {
                    build_ios_templates(ctx, &grouped["ios"])?;
                }
            }
            other => bail!("unsupported target: {other}"),
        }
    }
    Ok(())
}

fn build_desktop_templates(ctx: &BuildContext, godot_platform: &str, arch: &str) -> Result<()> {
    run_scons(
        ctx,
        godot_platform,
        arch,
        &[("target", "template_release"), ("production", "yes")],
        module_overrides(ctx, "template_release"),
    )?;
    copy_binaries(
        &ctx.godot_src,
        &ctx.repo_root
            .join("build")
            .join(godot_platform)
            .join("export_templates"),
        godot_platform,
        "template_release",
        arch,
        false,
    )?;
    run_scons(
        ctx,
        godot_platform,
        arch,
        &[("target", "template_debug"), ("dev_mode", "yes")],
        module_overrides(ctx, "template_debug"),
    )?;
    copy_binaries(
        &ctx.godot_src,
        &ctx.repo_root
            .join("build")
            .join(godot_platform)
            .join("export_templates"),
        godot_platform,
        "template_debug",
        arch,
        false,
    )
}

fn build_android_templates(ctx: &BuildContext, archs: &[String]) -> Result<()> {
    ensure_android_swappy(&ctx.godot_src)?;
    for arch in archs {
        run_scons(
            ctx,
            "android",
            arch,
            &[
                ("target", "template_release"),
                ("production", "yes"),
                ("generate_android_binaries", "no"),
            ],
            module_overrides(ctx, "template_release"),
        )?;
    }
    for (idx, arch) in archs.iter().enumerate() {
        let generate = if idx == archs.len() - 1 { "yes" } else { "no" };
        run_scons(
            ctx,
            "android",
            arch,
            &[
                ("target", "template_debug"),
                ("dev_mode", "yes"),
                ("generate_android_binaries", generate),
            ],
            module_overrides(ctx, "template_debug"),
        )?;
    }
    let out = ctx.repo_root.join("build/android/export_templates");
    fs::create_dir_all(&out)?;
    for name in [
        "android_monoDebug.apk",
        "android_monoRelease.apk",
        "android_source.zip",
        "godot-lib.template_debug.aar",
        "godot-lib.template_release.aar",
    ] {
        util::copy_file(&ctx.godot_src.join("bin").join(name), &out.join(name))?;
    }
    Ok(())
}

fn build_ios_templates(ctx: &BuildContext, archs: &[String]) -> Result<()> {
    for arch in archs {
        run_scons(
            ctx,
            "ios",
            arch,
            &[("target", "template_release"), ("production", "yes")],
            module_overrides(ctx, "template_release"),
        )?;
    }
    for (idx, arch) in archs.iter().enumerate() {
        let generate = if idx == archs.len() - 1 { "yes" } else { "no" };
        run_scons(
            ctx,
            "ios",
            arch,
            &[("target", "template_debug"), ("generate_bundle", generate)],
            module_overrides(ctx, "template_debug"),
        )?;
    }
    util::copy_file(
        &ctx.godot_src.join("bin/godot_ios.zip"),
        &ctx.repo_root
            .join("build/ios/export_templates/godot_ios.zip"),
    )
}

fn run_scons(
    ctx: &BuildContext,
    platform_name: &str,
    arch: &str,
    kwargs: &[(&str, &str)],
    module_overrides: BTreeMap<String, bool>,
) -> Result<()> {
    let scons = find_scons()?;
    let mut cmd = Command::new(&scons[0]);
    cmd.args(&scons[1..])
        .arg(format!("platform={platform_name}"))
        .arg(format!(
            "custom_modules={}",
            ctx.repo_root.join("modules").display()
        ))
        .arg("module_mono_enabled=yes")
        .arg(format!("arch={arch}"));
    for (key, value) in kwargs {
        cmd.arg(format!("{key}={value}"));
    }
    for (module, enabled) in module_overrides {
        cmd.arg(format!(
            "module_{module}_enabled={}",
            if enabled { "yes" } else { "no" }
        ));
    }
    if let Some(key) = ctx
        .project
        .encryption
        .as_ref()
        .and_then(|e| e.key.as_deref())
        .map(str::trim)
        .filter(|key| !key.is_empty())
    {
        cmd.env("SCRIPT_AES256_ENCRYPTION_KEY", key);
    }
    cmd.args(&ctx.scons_args)
        .arg(format!(
            "-j{}",
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(4)
        ))
        .current_dir(&ctx.godot_src);
    util::run_command(&mut cmd)
}

fn module_overrides(ctx: &BuildContext, target: &str) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    if let Some(modules) = &ctx.project.modules {
        if let Some(disabled) = &modules.disabled {
            for name in disabled {
                out.insert(name.clone(), false);
            }
        }
        if target != "template_release"
            && let Some(release_only) = &modules.release_only
        {
            for name in release_only {
                out.entry(name.clone()).or_insert(false);
            }
        }
    }
    out
}

fn build_mono_editor_support(ctx: &BuildContext) -> Result<()> {
    let editor = find_built_binary(
        &ctx.godot_src.join("bin"),
        ctx.host_godot,
        "editor",
        ctx.host_arch,
    )?;
    let glue_dir = ctx.godot_src.join("modules/mono/glue");
    util::run_command(
        Command::new(&editor)
            .args(["--headless", "--generate-mono-glue", path_str(&glue_dir)])
            .current_dir(&ctx.godot_src),
    )?;
    let script = ctx
        .godot_src
        .join("modules/mono/build_scripts/build_assemblies.py");
    let python = python_command()?;
    util::run_command(
        Command::new(python)
            .arg(script)
            .arg("--godot-output-dir")
            .arg(ctx.godot_src.join("bin"))
            .arg("--godot-platform")
            .arg(ctx.host_godot)
            .arg("--precision")
            .arg("single")
            .current_dir(&ctx.godot_src),
    )
}

fn copy_binaries(
    godot_src: &Path,
    out_dir: &Path,
    platform_name: &str,
    target: &str,
    arch: &str,
    mono_support: bool,
) -> Result<()> {
    fs::create_dir_all(out_dir)?;
    let bin_dir = godot_src.join("bin");
    let prefix = format!("godot.{platform_name}.{target}.{arch}.mono");
    let mut copied = false;
    for entry in fs::read_dir(&bin_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix)
            && entry.file_type()?.is_file()
            && !name.ends_with(".exp")
            && !name.ends_with(".lib")
        {
            util::copy_file(&entry.path(), &out_dir.join(name))?;
            copied = true;
        }
    }
    if !copied {
        bail!(
            "no build outputs found for {prefix} in {}",
            bin_dir.display()
        );
    }
    if mono_support {
        let godotsharp = bin_dir.join("GodotSharp");
        if godotsharp.is_dir() {
            util::copy_dir(&godotsharp, &out_dir.join("GodotSharp"))?;
        }
    }
    Ok(())
}

fn find_built_binary(
    bin_dir: &Path,
    platform_name: &str,
    target: &str,
    arch: &str,
) -> Result<PathBuf> {
    let prefix = format!("godot.{platform_name}.{target}.{arch}.mono");
    find_matching_binary(bin_dir, &prefix, target)
}

fn choose_editor_artifact(artifacts: &[EngineArtifact]) -> Result<&EngineArtifact> {
    let host = platform::host_platform()?;
    let arch = platform::host_arch();
    artifacts
        .iter()
        .find(|a| a.kind == "editor" && a.platform == host && a.arch == arch)
        .or_else(|| {
            artifacts
                .iter()
                .find(|a| a.kind == "editor" && a.platform == host)
        })
        .context("no matching editor artifact in engine tag")
}

fn find_editor_in_dir(dir: &Path) -> Result<Option<PathBuf>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let host = platform::host_godot_platform()?;
    let arch = platform::host_arch();
    let prefix = format!("godot.{host}.editor.{arch}.mono");
    let candidates = matching_binaries(dir, &prefix)?;
    Ok(choose_preferred_binary(candidates, &prefix, "editor"))
}

fn read_project_engine_tag() -> Result<Option<String>> {
    let path = std::env::current_dir()?.join("project.pug.json");
    if !path.is_file() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)?;
    Ok(value
        .get("engine")
        .and_then(|e| e.get("tag"))
        .and_then(Value::as_str)
        .map(str::to_string))
}

pub(crate) fn resolve_project_name() -> Result<String> {
    if let Ok(name) = std::env::var("PANNEL_PROJECT_NAME")
        && !name.trim().is_empty()
    {
        return Ok(name.trim().to_string());
    }
    let cwd = std::env::current_dir()?;
    for candidate in [cwd.join("project.json"), cwd.join("project.pug.json")] {
        if candidate.is_file() {
            let value: Value = serde_json::from_slice(&fs::read(&candidate)?)?;
            if let Some(name) = value.get("name").and_then(Value::as_str)
                && !name.trim().is_empty()
            {
                return Ok(name.trim().to_string());
            }
        }
    }
    bail!("project name is required; set project.json name or PANNEL_PROJECT_NAME")
}

fn existing_file(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path.to_path_buf())
    } else {
        bail!("file does not exist: {}", path.display())
    }
}

fn add_cppdefine(args: &mut Vec<String>, define: &str) {
    let mut found = false;
    for arg in args.iter_mut() {
        if let Some(rest) = arg.strip_prefix("cppdefines=") {
            found = true;
            if !rest.split_whitespace().any(|v| v == define) {
                arg.push(' ');
                arg.push_str(define);
            }
        }
    }
    if !found {
        args.push(format!("cppdefines={define}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_cppdefine_appends_once() {
        let mut args = vec![
            "debug_symbols=yes".to_string(),
            "cppdefines=FOO".to_string(),
        ];
        add_cppdefine(&mut args, "BAR");
        add_cppdefine(&mut args, "BAR");
        assert_eq!(args[1], "cppdefines=FOO BAR");
    }

    #[test]
    fn parse_godot_version_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("version.py"),
            "major = 4\nminor = 6\npatch = 2\nstatus = \"stable\"\n",
        )
        .unwrap();
        let (full, short) = godot_version(dir.path()).unwrap();
        assert_eq!(full, "4.6.2-stable");
        assert_eq!(short, "406");
    }

    #[test]
    fn project_json_preserves_extra_fields_for_patch_hooks() {
        let project: ProjectJson = serde_json::from_str(r#"{"no_google_play_obb":true}"#).unwrap();
        let value = serde_json::to_value(project).unwrap();
        assert_eq!(value["no_google_play_obb"], Value::Bool(true));
    }

    #[test]
    fn force_restore_keeps_unrelated_untracked_files() {
        let overlay = tempfile::tempdir().unwrap();
        let patch_dir = overlay.path().join("patches/001-test");
        fs::create_dir_all(&patch_dir).unwrap();
        fs::write(
            patch_dir.join("patch.diff"),
            "diff --git a/generated.txt b/generated.txt\n\
new file mode 100644\n\
index 0000000..f2ad6c7\n\
--- /dev/null\n\
+++ b/generated.txt\n\
@@ -0,0 +1 @@\n\
+generated\n",
        )
        .unwrap();

        let godot = tempfile::tempdir().unwrap();
        run_git(godot.path(), &["init", "-q"]);
        fs::write(godot.path().join("tracked.txt"), "clean\n").unwrap();
        run_git(godot.path(), &["add", "tracked.txt"]);
        run_git(
            godot.path(),
            &[
                "-c",
                "user.name=pug-test",
                "-c",
                "user.email=pug-test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );

        fs::write(godot.path().join("tracked.txt"), "dirty\n").unwrap();
        fs::write(godot.path().join("generated.txt"), "generated\n").unwrap();
        fs::write(godot.path().join("notes.txt"), "keep\n").unwrap();

        force_restore_godot_source(overlay.path(), godot.path(), None).unwrap();

        let tracked = fs::read_to_string(godot.path().join("tracked.txt")).unwrap();
        assert!(tracked == "clean\n" || tracked == "clean\r\n");
        assert!(!godot.path().join("generated.txt").exists());
        assert!(godot.path().join("notes.txt").exists());
    }

    #[test]
    fn find_repo_root_uses_git_toplevel() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q"]);
        fs::write(repo.path().join("project.json"), "{}").unwrap();
        fs::create_dir(repo.path().join("patches")).unwrap();
        fs::create_dir(repo.path().join("modules")).unwrap();
        let nested = repo.path().join("tools/pug/work");
        fs::create_dir_all(&nested).unwrap();

        let found = find_repo_root_from(&nested).unwrap();
        assert_eq!(
            found,
            normalize_external_path(repo.path().canonicalize().unwrap())
        );
    }

    #[cfg(windows)]
    #[test]
    fn normalize_external_path_strips_windows_verbatim_prefix() {
        let path = normalize_external_path(PathBuf::from("\\\\?\\C:\\ext\\GODOTEXT\\godot"));
        assert_eq!(path.to_string_lossy(), "C:\\ext\\GODOTEXT\\godot");

        let unc = normalize_external_path(PathBuf::from("\\\\?\\UNC\\server\\share\\godot"));
        assert_eq!(unc.to_string_lossy(), "\\\\server\\share\\godot");
    }

    #[test]
    fn explicit_template_platforms_do_not_add_project_templates() {
        let project = ProjectJson {
            platforms: Some(vec![platform::host_platform().unwrap().to_string()]),
            ..ProjectJson::default()
        };
        let target = platform::host_capable_platforms()
            .unwrap()
            .into_iter()
            .find(|item| *item != platform::host_platform().unwrap())
            .unwrap();

        let targets = resolve_template_targets(&project, Some(target)).unwrap();
        assert_eq!(
            target_pairs(&targets),
            default_template_arches(&project, target)
                .unwrap()
                .into_iter()
                .map(|arch| (platform::normalize_platform(target), arch))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn engine_defaults_to_all_host_capable_when_project_has_no_platforms() {
        let project = ProjectJson::default();
        let targets = resolve_template_targets(&project, None).unwrap();
        let mut expected = Vec::new();
        for platform_name in platform::host_capable_platforms().unwrap() {
            for arch in default_template_arches(&project, platform_name).unwrap() {
                expected.push((platform_name.to_string(), arch));
            }
        }
        assert_eq!(target_pairs(&targets), expected);
    }

    #[test]
    fn project_platforms_accept_platform_arch_pairs() {
        let project = ProjectJson {
            platforms: Some(vec![
                format!("{}:custom_arch", platform::host_platform().unwrap()),
                "android:arm64-v8a".to_string(),
                "android:arm32".to_string(),
            ]),
            ..ProjectJson::default()
        };

        let targets = resolve_template_targets(&project, None).unwrap();
        assert_eq!(
            target_pairs(&targets),
            vec![
                (
                    platform::host_platform().unwrap().to_string(),
                    "custom_arch".to_string()
                ),
                ("android".to_string(), "arm64".to_string()),
                ("android".to_string(), "arm32".to_string()),
            ]
        );
    }

    #[test]
    fn platform_only_android_uses_legacy_arch_section() {
        let project = ProjectJson {
            platforms: Some(vec!["android".to_string()]),
            android: Some(ArchSection {
                archs: Some(vec!["arm64".to_string(), "arm32".to_string()]),
            }),
            ..ProjectJson::default()
        };

        let targets = resolve_template_targets(&project, None).unwrap();
        assert_eq!(
            target_pairs(&targets),
            vec![
                ("android".to_string(), "arm64".to_string()),
                ("android".to_string(), "arm32".to_string()),
            ]
        );
    }

    fn target_pairs(targets: &[TemplateTarget]) -> Vec<(String, String)> {
        targets
            .iter()
            .map(|target| (target.platform.clone(), target.arch.clone()))
            .collect()
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        util::run_command(&mut cmd).unwrap();
    }
}
