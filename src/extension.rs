use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    api::{ApiClient, ExtensionDevVersion, ExtensionUploadInit},
    config::Config,
    engine,
    platform::{self, TargetSpec},
    util,
};

const ANDROID_NDK_VERSION: &str = "28.1.13356709";
const ANDROID_API_LEVEL: &str = "21";

#[derive(Debug, Deserialize)]
struct ExtensionProjectJson {
    platforms: Option<Vec<String>>,
}

pub struct ExtensionBuildOptions {
    pub upload: bool,
    pub platform: Option<String>,
    pub engine_tag: Option<String>,
    pub upload_version: Option<String>,
    pub with_engine: Option<PathBuf>,
    pub debug: bool,
    pub force: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
    pub library: String,
    pub lib_sha256: String,
    pub lib_size: i64,
    pub platform: String,
    pub arch: String,
}

#[derive(Debug)]
struct BuiltExtension {
    name: String,
    version: String,
    target: TargetSpec,
    lib_path: PathBuf,
    lib_sha256: String,
    lib_size: i64,
    package_path: PathBuf,
    package_sha256: String,
    package_size: i64,
}

struct UploadVersionPlan {
    version: String,
    repo_dirty: bool,
    dev_key: Option<String>,
}

struct EngineUploadMetadata {
    repo_commit: String,
    engine_commit: String,
    godot_version: String,
    godot_version_short: String,
}

pub fn build(opts: ExtensionBuildOptions) -> Result<()> {
    let cfg = if opts.upload {
        let cfg = Config::load()?;
        cfg.verify_access_token()?;
        Some(cfg)
    } else {
        None
    };

    let should_upload = opts.upload;
    let engine_tag = opts.engine_tag.clone();
    let upload_version = opts.upload_version;
    let force = opts.force;
    let platform = opts.platform;
    let with_engine = opts.with_engine;
    let debug = opts.debug;

    let ext_dir = std::env::current_dir()?;
    let (name, cargo_version) = cargo_package(&ext_dir)?;
    let version_plan = resolve_upload_version(
        cfg.as_ref(),
        should_upload,
        engine_tag.as_deref(),
        upload_version,
        &ext_dir,
        &name,
        cargo_version,
    )?;
    let version = version_plan.version.clone();
    validate_upload_version(&version)?;
    let profile = if debug { "debug" } else { "release" };
    let targets = resolve_targets(platform.as_deref())?;
    let editor = if with_engine.is_some() {
        engine::resolve_editor(with_engine.as_deref())?
    } else if let Some(tag) = engine_tag.as_deref() {
        engine::resolve_editor_for_tag(tag)?
    } else {
        engine::resolve_editor(None)?
    };
    let mut built = Vec::new();
    for target in targets {
        let lib_path = build_one(&ext_dir, &name, &target, profile, &editor)?;
        let package_path = package_extension(&ext_dir, &name, &version, &target, &lib_path)?;
        built.push(BuiltExtension {
            name: name.clone(),
            version: version.clone(),
            target,
            lib_sha256: util::sha256_file(&lib_path)?,
            lib_size: util::file_size(&lib_path)?,
            package_sha256: util::sha256_file(&package_path)?,
            package_size: util::file_size(&package_path)?,
            lib_path,
            package_path,
        });
    }

    for item in &built {
        println!(
            "{} {}:{} -> {}",
            item.name,
            item.target.platform,
            item.target.arch,
            item.package_path.display()
        );
        println!("  lib: {}", item.lib_path.display());
    }
    if should_upload {
        upload(
            &built,
            force,
            engine_tag.as_deref(),
            version_plan.repo_dirty,
            version_plan.dev_key.as_deref(),
        )?;
    }
    Ok(())
}

pub fn list(remote_only: bool) -> Result<()> {
    if !remote_only {
        println!(
            "local extension listing is project-scoped; use pug project install/list in a project"
        );
    }
    let cfg = Config::load()?;
    let api = ApiClient::from_config(&cfg)?;
    let project_name = engine::resolve_project_name()?;
    let response = api.extensions(&project_name)?;
    println!("remote:");
    for item in response.extensions {
        println!(
            "  {} latest={} versions={}",
            item.name,
            item.latest,
            item.versions.join(",")
        );
    }
    Ok(())
}

fn resolve_upload_version(
    cfg: Option<&Config>,
    upload: bool,
    engine_tag: Option<&str>,
    explicit_version: Option<String>,
    ext_dir: &Path,
    name: &str,
    cargo_version: String,
) -> Result<UploadVersionPlan> {
    if let Some(version) = explicit_version {
        validate_upload_version(&version)?;
        return Ok(UploadVersionPlan {
            version,
            repo_dirty: false,
            dev_key: None,
        });
    }

    validate_upload_version(&cargo_version)?;
    let Some(cfg) = cfg else {
        return Ok(UploadVersionPlan {
            version: cargo_version,
            repo_dirty: false,
            dev_key: None,
        });
    };
    if !upload || !cfg.uses_login_session_auth() {
        return Ok(UploadVersionPlan {
            version: cargo_version,
            repo_dirty: false,
            dev_key: None,
        });
    }

    let repo_dirty = match git_root_from(ext_dir) {
        Ok(git_root) => engine::git_worktree_dirty(&git_root)?,
        Err(_) => false,
    };
    if !repo_dirty {
        return Ok(UploadVersionPlan {
            version: cargo_version,
            repo_dirty: false,
            dev_key: None,
        });
    }

    let api = ApiClient::from_config(cfg)?;
    let project_name = engine::resolve_project_name()?;
    let engine_meta = resolve_engine_upload_metadata(&api, &project_name, engine_tag)?;
    let dev_key = engine::make_dev_key();
    let response = api.extension_dev_version(&ExtensionDevVersion {
        project_name: &project_name,
        name,
        base_version: &cargo_version,
        repo_commit: &engine_meta.repo_commit,
        repo_dirty,
        dev_key: &dev_key,
        engine_commit: &engine_meta.engine_commit,
        godot_version: &engine_meta.godot_version,
        godot_version_short: &engine_meta.godot_version_short,
    })?;
    println!(
        "pug: dirty login-session extension; using dev upload version {} from engine tag {}",
        response.version, response.engine_tag
    );
    Ok(UploadVersionPlan {
        version: response.version,
        repo_dirty,
        dev_key: Some(response.dev_key),
    })
}

fn resolve_engine_upload_metadata(
    api: &ApiClient,
    project_name: &str,
    engine_tag: Option<&str>,
) -> Result<EngineUploadMetadata> {
    if let Some(tag) = engine_tag {
        let tags = api.engine_tags_for_project(project_name)?;
        let tag = tags
            .tags
            .into_iter()
            .find(|item| item.tag == tag)
            .with_context(|| {
                format!("engine tag {tag} not found in pannel project {project_name}")
            })?;
        return Ok(EngineUploadMetadata {
            repo_commit: tag.repo_commit,
            engine_commit: tag.engine_commit,
            godot_version: tag.godot_version,
            godot_version_short: tag.godot_version_short,
        });
    }

    let repo = engine::find_repo_root()?;
    let godot_src = fs::read_to_string(repo.join(".repocache")).context("read .repocache")?;
    let godot_src = PathBuf::from(godot_src.trim());
    let (godot_version, godot_version_short) = engine::godot_version(&godot_src)?;
    Ok(EngineUploadMetadata {
        repo_commit: engine::git_head(&repo)?,
        engine_commit: engine::git_head(&godot_src)?,
        godot_version,
        godot_version_short,
    })
}

fn resolve_targets(value: Option<&str>) -> Result<Vec<TargetSpec>> {
    resolve_targets_from_dir(value, &std::env::current_dir()?)
}

fn resolve_targets_from_dir(value: Option<&str>, ext_dir: &Path) -> Result<Vec<TargetSpec>> {
    let explicit = value.is_some();
    let selectors = if let Some(value) = value {
        platform::parse_platform_list(value)
    } else {
        default_extension_platforms(ext_dir)?
    };
    let targets = target_specs_from_selectors(selectors, explicit)?;
    if targets.is_empty() {
        bail!("no buildable extension platforms found for this host");
    }
    Ok(targets)
}

fn default_extension_platforms(ext_dir: &Path) -> Result<Vec<String>> {
    if let Ok(git_root) = git_root_from(ext_dir) {
        let project_path = git_root.join("project.json");
        if project_path.is_file() {
            let project: ExtensionProjectJson = serde_json::from_str(
                &fs::read_to_string(&project_path)
                    .with_context(|| format!("read {}", project_path.display()))?,
            )
            .with_context(|| format!("parse {}", project_path.display()))?;
            if let Some(platforms) = project.platforms {
                return Ok(platforms
                    .into_iter()
                    .map(|p| platform::normalize_platform(&p))
                    .collect());
            }
        }
    }
    Ok(vec!["all".to_string()])
}

fn git_root_from(cwd: &Path) -> Result<PathBuf> {
    let root = util::output_command(
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(["rev-parse", "--show-toplevel"]),
    )
    .with_context(|| format!("resolve git root from {}", cwd.display()))?;
    PathBuf::from(root)
        .canonicalize()
        .with_context(|| format!("resolve git root path from {}", cwd.display()))
}

fn target_specs_from_selectors(selectors: Vec<String>, explicit: bool) -> Result<Vec<TargetSpec>> {
    let capable_order = platform::host_capable_platforms()?;
    let capable: BTreeSet<_> = capable_order.iter().copied().map(str::to_string).collect();
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();

    let add_target =
        |targets: &mut Vec<TargetSpec>, seen: &mut BTreeSet<String>, spec: TargetSpec| {
            let key = format!("{}:{}", spec.platform, spec.arch);
            if seen.insert(key) {
                targets.push(spec);
            }
        };

    if selectors.iter().any(|item| item == "all") {
        for platform in capable_order {
            for arch in platform::default_arches(platform)? {
                add_target(&mut targets, &mut seen, platform::spec(platform, arch)?);
            }
        }
        return Ok(targets);
    }

    for selector in selectors {
        let platform_name = selector
            .split_once(':')
            .map(|(platform, _)| platform::normalize_platform(platform))
            .unwrap_or_else(|| platform::normalize_platform(&selector));
        if !capable.contains(&platform_name) {
            if explicit {
                bail!(
                    "platform {platform_name} is not buildable on this host; supported here: {}",
                    capable_order.join(",")
                );
            }
            continue;
        }

        if selector.contains(':') {
            add_target(&mut targets, &mut seen, platform::parse_target(&selector)?);
        } else {
            for arch in platform::default_arches(&platform_name)? {
                add_target(
                    &mut targets,
                    &mut seen,
                    platform::spec(&platform_name, arch)?,
                );
            }
        }
    }
    Ok(targets)
}

fn cargo_package(ext_dir: &Path) -> Result<(String, String)> {
    let manifest = ext_dir.join("Cargo.toml");
    let value: toml::Value = toml::from_str(
        &fs::read_to_string(&manifest).with_context(|| format!("read {}", manifest.display()))?,
    )?;
    let package = value
        .get("package")
        .and_then(toml::Value::as_table)
        .context("Cargo.toml missing [package]")?;
    let name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .context("Cargo.toml missing package.name")?
        .to_string();
    let version = package
        .get("version")
        .and_then(toml::Value::as_str)
        .context("Cargo.toml missing package.version")?
        .to_string();
    Ok((name, version))
}

fn validate_upload_version(version: &str) -> Result<()> {
    if version.trim().is_empty() || version.contains('/') {
        bail!("valid upload version is required");
    }
    Ok(())
}

fn build_one(
    ext_dir: &Path,
    name: &str,
    target: &TargetSpec,
    profile: &str,
    editor: &Path,
) -> Result<PathBuf> {
    rustup_target_add(target.rust_target)?;
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(ext_dir.join("Cargo.toml"));
    cmd.env("CARGO_TARGET_DIR", ext_dir.join("target"));
    if target.platform != platform::host_platform()? || target.rust_target != host_rust_target()? {
        cmd.arg("--target").arg(target.rust_target);
    }
    if profile == "release" {
        cmd.arg("--release");
    }

    let mut envs = cargo_env(editor)?;
    if target.platform == "android" {
        envs.extend(android_env(target)?);
    } else if target.platform == "ios" {
        envs.extend(ios_env()?);
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    util::run_command(&mut cmd)?;

    let target_dir = if cmd.get_args().any(|a| a == "--target") {
        ext_dir
            .join("target")
            .join(target.rust_target)
            .join(profile)
    } else {
        ext_dir.join("target").join(profile)
    };
    let lib = target_dir.join(target.lib_name(name));
    if target.platform == "android" {
        strip_android(&lib)?;
    }
    if !lib.is_file() {
        bail!("built library not found: {}", lib.display());
    }
    Ok(lib)
}

fn cargo_env(editor: &Path) -> Result<Vec<(String, String)>> {
    let repo = engine::find_repo_root().ok();
    let home = repo
        .as_ref()
        .map(|r| r.join(".godot_rust_home"))
        .unwrap_or_else(|| std::env::temp_dir().join("pug-godot-rust-home"));
    fs::create_dir_all(&home)?;
    fs::create_dir_all(home.join(".local/share"))?;
    fs::create_dir_all(home.join(".config"))?;
    let editor_command = editor.to_string_lossy().to_string();
    let mut env = vec![
        ("HOME".to_string(), home.to_string_lossy().to_string()),
        default_env_path("CARGO_HOME", ".cargo")?,
        default_env_path("RUSTUP_HOME", ".rustup")?,
        default_isolated_env_path("XDG_DATA_HOME", &home.join(".local/share")),
        default_isolated_env_path("XDG_CONFIG_HOME", &home.join(".config")),
        ("GDRUST_GODOT_BIN".to_string(), editor_command.clone()),
        ("GODOT4_BIN".to_string(), editor_command),
    ];
    if let Some(path) = find_llvm_path() {
        env.push(("LLVM_PATH".to_string(), path.to_string_lossy().to_string()));
        env.push((
            "LIBCLANG_PATH".to_string(),
            path.join("lib").to_string_lossy().to_string(),
        ));
    }
    Ok(env)
}

fn default_env_path(key: &str, fallback_home_child: &str) -> Result<(String, String)> {
    let value = match std::env::var_os(key) {
        Some(value) => PathBuf::from(value),
        None => dirs::home_dir()
            .with_context(|| format!("resolve home directory for {key}"))?
            .join(fallback_home_child),
    };
    Ok((key.to_string(), value.to_string_lossy().to_string()))
}

fn default_isolated_env_path(key: &str, fallback: &Path) -> (String, String) {
    let value = std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf());
    (key.to_string(), value.to_string_lossy().to_string())
}

fn find_llvm_path() -> Option<PathBuf> {
    for key in ["LLVM_PATH", "LIBCLANG_PATH"] {
        if let Ok(value) = std::env::var(key) {
            let path = PathBuf::from(value);
            let candidate =
                if key == "LIBCLANG_PATH" && path.file_name().is_some_and(|n| n == "lib") {
                    path.parent().map(Path::to_path_buf).unwrap_or(path)
                } else {
                    path
                };
            if candidate.join("lib/libclang.dylib").is_file()
                || candidate.join("lib/libclang.so").is_file()
            {
                return Some(candidate);
            }
        }
    }
    [
        "/opt/homebrew/opt/llvm",
        "/usr/local/opt/llvm",
        "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.join("lib/libclang.dylib").is_file() || p.join("lib/libclang.so").is_file())
}

fn rustup_target_add(target: &str) -> Result<()> {
    let installed =
        util::output_command(Command::new("rustup").args(["target", "list", "--installed"]))?;
    if !installed.lines().any(|line| line.trim() == target) {
        util::run_command(Command::new("rustup").args(["target", "add", target]))?;
    }
    Ok(())
}

fn android_env(target: &TargetSpec) -> Result<Vec<(String, String)>> {
    let sdk = engine::android_sdk().context("Android SDK not found")?;
    let toolchain = android_ndk_toolchain_bin(&sdk);
    let sysroot = sdk
        .join("ndk")
        .join(ANDROID_NDK_VERSION)
        .join("toolchains/llvm/prebuilt")
        .join(engine::android_ndk_host())
        .join("sysroot");
    let triple = match target.rust_target {
        "aarch64-linux-android" => "aarch64-linux-android",
        other => bail!("unsupported Android target: {other}"),
    };
    let cc = android_ndk_tool(
        &toolchain,
        &format!("{triple}{ANDROID_API_LEVEL}-clang"),
        AndroidNdkToolKind::ClangWrapper,
    );
    let cxx = android_ndk_tool(
        &toolchain,
        &format!("{triple}{ANDROID_API_LEVEL}-clang++"),
        AndroidNdkToolKind::ClangWrapper,
    );
    let mut out = vec![
        ("CC".to_string(), cc.to_string_lossy().to_string()),
        ("CXX".to_string(), cxx.to_string_lossy().to_string()),
        (
            "AR".to_string(),
            android_ndk_tool(&toolchain, "llvm-ar", AndroidNdkToolKind::Binary)
                .to_string_lossy()
                .to_string(),
        ),
        (
            "RANLIB".to_string(),
            android_ndk_tool(&toolchain, "llvm-ranlib", AndroidNdkToolKind::Binary)
                .to_string_lossy()
                .to_string(),
        ),
        (
            format!(
                "BINDGEN_EXTRA_CLANG_ARGS_{}",
                bindgen_env_target(target.rust_target)
            ),
            format!(
                "--target={triple}{ANDROID_API_LEVEL} --sysroot={}",
                sysroot.to_string_lossy()
            ),
        ),
    ];
    out.push((
        format!(
            "CARGO_TARGET_{}_LINKER",
            cargo_env_target(target.rust_target)
        ),
        cc.to_string_lossy().to_string(),
    ));
    Ok(out)
}

fn ios_env() -> Result<Vec<(String, String)>> {
    if std::env::consts::OS != "macos" {
        bail!("iOS extension builds require macOS");
    }
    let sdkroot =
        util::output_command(Command::new("xcrun").args(["--sdk", "iphoneos", "--show-sdk-path"]))?;
    Ok(vec![("SDKROOT".to_string(), sdkroot)])
}

fn strip_android(lib: &Path) -> Result<()> {
    let sdk = engine::android_sdk().context("Android SDK not found")?;
    let toolchain = android_ndk_toolchain_bin(&sdk);
    let strip = android_ndk_tool(&toolchain, "llvm-strip", AndroidNdkToolKind::Binary);
    if strip.is_file() {
        util::run_command(Command::new(strip).arg("--strip-unneeded").arg(lib))?;
    }
    Ok(())
}

fn android_ndk_toolchain_bin(sdk: &Path) -> PathBuf {
    sdk.join("ndk")
        .join(ANDROID_NDK_VERSION)
        .join("toolchains/llvm/prebuilt")
        .join(engine::android_ndk_host())
        .join("bin")
}

#[derive(Clone, Copy)]
enum AndroidNdkToolKind {
    Binary,
    ClangWrapper,
}

fn android_ndk_tool(toolchain: &Path, tool: &str, kind: AndroidNdkToolKind) -> PathBuf {
    let suffix = match (cfg!(windows), kind) {
        (true, AndroidNdkToolKind::Binary) => ".exe",
        (true, AndroidNdkToolKind::ClangWrapper) => ".cmd",
        (false, _) => "",
    };
    toolchain.join(format!("{tool}{suffix}"))
}

fn package_extension(
    ext_dir: &Path,
    name: &str,
    version: &str,
    target: &TargetSpec,
    lib: &Path,
) -> Result<PathBuf> {
    let out_dir = ext_dir.join("target/pug");
    fs::create_dir_all(&out_dir)?;
    let metadata = PackageMetadata {
        name: name.to_string(),
        version: version.to_string(),
        library: target.lib_name(name),
        lib_sha256: util::sha256_file(lib)?,
        lib_size: util::file_size(lib)?,
        platform: target.platform.clone(),
        arch: target.arch.clone(),
    };
    let meta_path = out_dir.join("metadata.json");
    util::write_json(&meta_path, &metadata)?;
    let template_path = write_manifest_template(ext_dir, &out_dir, name)?;
    let package = out_dir.join(format!(
        "{name}-{version}-{}-{}.tar.zst",
        target.platform, target.arch
    ));
    util::tar_zst(
        &package,
        &[
            (lib.to_path_buf(), PathBuf::from(target.lib_name(name))),
            (
                template_path,
                PathBuf::from(format!("{name}.gdextension.tmpl")),
            ),
            (meta_path, PathBuf::from("metadata.json")),
        ],
    )?;
    Ok(package)
}

fn write_manifest_template(ext_dir: &Path, out_dir: &Path, name: &str) -> Result<PathBuf> {
    for candidate in [
        ext_dir.join(format!("{name}.gdextension.tmpl")),
        ext_dir.join(format!("{name}.gdextension")),
        ext_dir.join("extension.gdextension.tmpl"),
        ext_dir.join("extension.gdextension"),
    ] {
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate)?;
            let template = strip_libraries_section(&text);
            let out = out_dir.join(format!("{name}.gdextension.tmpl"));
            fs::write(&out, template)?;
            return Ok(out);
        }
    }
    let out = out_dir.join(format!("{name}.gdextension.tmpl"));
    fs::write(
        &out,
        "[configuration]\nentry_symbol = \"gdext_rust_init\"\ncompatibility_minimum = \"4.1\"\n",
    )?;
    Ok(out)
}

fn strip_libraries_section(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_libraries = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[libraries]" {
            in_libraries = true;
            continue;
        }
        if in_libraries && trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_libraries = false;
        }
        if !in_libraries {
            out.push(line);
        }
    }
    let mut text = out.join("\n");
    while text.ends_with("\n\n") {
        text.pop();
    }
    text.push('\n');
    text
}

fn upload(
    items: &[BuiltExtension],
    force: bool,
    engine_tag: Option<&str>,
    repo_dirty: bool,
    dev_key: Option<&str>,
) -> Result<()> {
    let cfg = Config::load()?;
    let api = ApiClient::from_config(&cfg)?;
    let project_name = engine::resolve_project_name()?;
    let engine_meta = resolve_engine_upload_metadata(&api, &project_name, engine_tag)?;
    for item in items {
        let init = api.extension_upload_init(&ExtensionUploadInit {
            project_name: &project_name,
            name: &item.name,
            version: &item.version,
            repo_commit: &engine_meta.repo_commit,
            repo_dirty,
            dev_key,
            engine_commit: &engine_meta.engine_commit,
            godot_version: &engine_meta.godot_version,
            godot_version_short: &engine_meta.godot_version_short,
            platform: &item.target.platform,
            arch: &item.target.arch,
            lib_sha256: &item.lib_sha256,
            lib_size: item.lib_size,
            package_sha256: &item.package_sha256,
            package_size: item.package_size,
            force,
        })?;
        api.put_file(&init, &item.package_path)?;
        let complete = api.extension_upload_complete(&init.upload_id)?;
        println!(
            "uploaded {}@{} {}:{} -> {} status={} key={}",
            item.name,
            item.version,
            item.target.platform,
            item.target.arch,
            complete.engine_tag.as_deref().unwrap_or(""),
            complete.status,
            init.s3_key
        );
    }
    Ok(())
}

fn cargo_env_target(target: &str) -> String {
    target.to_ascii_uppercase().replace('-', "_")
}

fn bindgen_env_target(target: &str) -> String {
    target.replace('-', "_")
}

fn host_rust_target() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        other => return Err(anyhow!("unsupported host rust target: {other:?}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_libraries_section() {
        let input = "[configuration]\nentry_symbol = \"x\"\n[libraries]\nmacos.debug = \"a\"\n[icons]\nfoo = \"bar\"\n";
        let output = strip_libraries_section(input);
        assert!(output.contains("[configuration]"));
        assert!(output.contains("[icons]"));
        assert!(!output.contains("macos.debug"));
    }

    #[test]
    fn default_targets_use_git_root_project_json() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q"]);
        fs::write(
            repo.path().join("project.json"),
            r#"{"platforms":["windows","macos","android","ios"]}"#,
        )
        .unwrap();
        let ext_dir = repo.path().join("extensions/demo");
        fs::create_dir_all(&ext_dir).unwrap();

        let targets = resolve_targets_from_dir(None, &ext_dir).unwrap();
        let got: Vec<String> = targets
            .iter()
            .map(|target| target.platform.clone())
            .collect();
        let capable: BTreeSet<String> = platform::host_capable_platforms()
            .unwrap()
            .into_iter()
            .map(str::to_string)
            .collect();
        let expected: Vec<String> = ["windows", "macos", "android", "ios"]
            .into_iter()
            .map(platform::normalize_platform)
            .filter(|platform| capable.contains(platform))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn default_targets_without_project_json_use_all_host_capable() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q"]);
        let ext_dir = repo.path().join("standalone");
        fs::create_dir_all(&ext_dir).unwrap();

        let targets = resolve_targets_from_dir(None, &ext_dir).unwrap();
        let got: Vec<(String, String)> = targets
            .iter()
            .map(|target| (target.platform.clone(), target.arch.clone()))
            .collect();
        let mut expected = Vec::new();
        for platform in platform::host_capable_platforms().unwrap() {
            for arch in platform::default_arches(platform).unwrap() {
                let spec = platform::spec(platform, arch).unwrap();
                expected.push((spec.platform, spec.arch));
            }
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn explicit_target_overrides_project_json() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-q"]);
        fs::write(
            repo.path().join("project.json"),
            r#"{"platforms":["android"]}"#,
        )
        .unwrap();
        let ext_dir = repo.path().join("extensions/demo");
        fs::create_dir_all(&ext_dir).unwrap();
        let host = platform::host_platform().unwrap();

        let targets = resolve_targets_from_dir(Some(host), &ext_dir).unwrap();
        assert!(targets.iter().all(|target| target.platform == host));
    }

    #[test]
    fn android_ndk_tools_use_host_executable_suffixes() {
        let toolchain = PathBuf::from("ndk-bin");
        let cc = android_ndk_tool(
            &toolchain,
            "aarch64-linux-android21-clang",
            AndroidNdkToolKind::ClangWrapper,
        );
        let ar = android_ndk_tool(&toolchain, "llvm-ar", AndroidNdkToolKind::Binary);

        if cfg!(windows) {
            assert_eq!(cc, toolchain.join("aarch64-linux-android21-clang.cmd"));
            assert_eq!(ar, toolchain.join("llvm-ar.exe"));
        } else {
            assert_eq!(cc, toolchain.join("aarch64-linux-android21-clang"));
            assert_eq!(ar, toolchain.join("llvm-ar"));
        }
    }

    fn run_git(repo: &Path, args: &[&str]) {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(repo).args(args);
        util::run_command(&mut cmd).unwrap();
    }
}
