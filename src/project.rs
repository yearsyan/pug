use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    api::{ApiClient, EngineArtifact},
    config::Config,
    engine,
    extension::PackageMetadata,
    platform, util,
};

#[derive(Debug, Serialize, Deserialize)]
struct ProjectConfig {
    #[serde(default)]
    name: String,
    version: u32,
    engine: ProjectEngine,
    platforms: Vec<String>,
    #[serde(default)]
    extensions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    export: Option<ProjectExportConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectEngine {
    tag: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectExportConfig {
    script_encryption_key: Option<String>,
}

pub struct ProjectExportOptions {
    pub platform: Option<String>,
    pub android: bool,
    pub ios: bool,
    pub debug: bool,
    pub release: bool,
    pub with_engine: Option<PathBuf>,
}

pub fn init(engine_tag: Option<String>, platforms: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = cwd.join("project.pug.json");
    if path.exists() {
        bail!(
            "project.pug.json already exists in this directory.\n       remove it manually if you want to re-initialize."
        );
    }
    let cfg = Config::load()?;
    let api = ApiClient::from_config(&cfg)?;
    let tag = match engine_tag {
        Some(tag) => tag,
        None if !cfg.engine.current.is_empty() => cfg.engine.current.clone(),
        None => {
            let project_name = engine::resolve_project_name()?;
            let tags = api.engine_tags_for_project(&project_name)?;
            tags.tags
                .first()
                .map(|t| t.tag.clone())
                .context("no engine tag available; pass --engine-tag")?
        }
    };
    let platforms = platforms
        .map(|p| platform::parse_platform_list(&p))
        .unwrap_or_else(|| vec![platform::host_platform().unwrap_or("macos").to_string()]);
    let project = ProjectConfig {
        name: engine::resolve_project_name().unwrap_or_default(),
        version: 1,
        engine: ProjectEngine { tag },
        platforms,
        extensions: BTreeMap::new(),
        export: None,
    };
    util::write_json(&path, &project)?;
    update_gitignore(&cwd)?;
    println!("{}", path.display());
    Ok(())
}

pub fn install(package: Option<&str>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    if let Some(package) = package {
        return install_one(&cwd, package);
    }

    let project = read_project(&cwd)?;
    let packages = locked_extension_packages(&project);
    if packages.is_empty() {
        println!("no extensions listed in project.pug.json");
        return Ok(());
    }
    for package in packages {
        install_one(&cwd, &package)?;
    }
    Ok(())
}

fn install_one(cwd: &Path, package: &str) -> Result<()> {
    let mut project = read_project(&cwd)?;
    let (name, requested_version) = parse_package(package)?;
    let cfg = Config::load()?;
    let api = ApiClient::from_config(&cfg)?;
    let project_name = if project.name.trim().is_empty() {
        engine::resolve_project_name()?
    } else {
        project.name.clone()
    };
    let project_engine_commit = api
        .engine_tags_for_project(&project_name)?
        .tags
        .into_iter()
        .find(|tag| tag.tag == project.engine.tag)
        .map(|tag| tag.engine_commit);
    let mut installed_version = None;
    let mut template_text = None;

    for platform_name in &project.platforms {
        for arch in platform::default_arches(platform_name)? {
            let target = platform::spec(platform_name, arch)?;
            let resolved = api.resolve_extension(
                &project_name,
                &name,
                requested_version.as_deref(),
                &target.platform,
                &target.arch,
            )?;
            if resolved.name != name
                || resolved.platform != target.platform
                || resolved.arch != target.arch
            {
                bail!(
                    "pannel resolved unexpected artifact: {} {}:{}",
                    resolved.name,
                    resolved.platform,
                    resolved.arch
                );
            }
            if let Some(existing) = &installed_version
                && existing != &resolved.version
            {
                bail!(
                    "resolved inconsistent versions for {name}: {existing} and {}",
                    resolved.version
                );
            }
            installed_version = Some(resolved.version.clone());

            let tmp = tempfile::tempdir()?;
            let package_path = tmp.path().join("package.tar.zst");
            api.download_to(&resolved.download_url, &package_path)?;
            let sha = util::sha256_file(&package_path)?;
            if sha != resolved.package_sha256 {
                bail!(
                    "package sha mismatch for {name} {}:{}: got {sha} want {}",
                    target.platform,
                    target.arch,
                    resolved.package_sha256
                );
            }
            let package_size = util::file_size(&package_path)?;
            if package_size != resolved.package_size {
                bail!(
                    "package size mismatch for {name} {}:{}: got {package_size} want {}",
                    target.platform,
                    target.arch,
                    resolved.package_size
                );
            }
            let unpack = tmp.path().join("unpack");
            util::untar_zst(&package_path, &unpack)?;
            let metadata = read_metadata(&unpack)?;
            if metadata.lib_sha256 != resolved.lib_sha256 || metadata.lib_size != resolved.lib_size
            {
                bail!("metadata does not match pannel resolve response for {name}");
            }
            let lib_src = unpack.join(&metadata.library);
            let lib_sha = util::sha256_file(&lib_src)?;
            if lib_sha != resolved.lib_sha256 {
                bail!(
                    "library sha mismatch: got {lib_sha} want {}",
                    resolved.lib_sha256
                );
            }
            let lib_dst = cwd
                .join("bin")
                .join(&name)
                .join(&target.platform)
                .join(&target.arch)
                .join(&metadata.library);
            util::copy_file(&lib_src, &lib_dst)?;

            let tmpl = unpack.join(format!("{name}.gdextension.tmpl"));
            if tmpl.is_file() {
                template_text = Some(fs::read_to_string(&tmpl)?);
                util::copy_file(
                    &tmpl,
                    &cwd.join("bin")
                        .join(&name)
                        .join(format!("{name}.gdextension.tmpl")),
                )?;
            }
            if let Some(expected_commit) = &project_engine_commit
                && resolved.engine_commit != *expected_commit
            {
                eprintln!(
                    "warning: extension {}@{} was built from engine commit {}, project engine tag {} points to {}",
                    name,
                    resolved.version,
                    resolved.engine_commit,
                    project.engine.tag,
                    expected_commit
                );
            }
        }
    }

    let version = installed_version.context("no platform artifacts installed")?;
    project.extensions.insert(name.clone(), version);
    write_project(&cwd, &project)?;
    let template_text = template_text.unwrap_or_else(default_template);
    rewrite_manifest(&cwd, &name, &project, &template_text)?;
    update_extension_list(&cwd, &name)?;
    update_gitignore(&cwd)?;
    println!("installed {package}");
    Ok(())
}

fn locked_extension_packages(project: &ProjectConfig) -> Vec<String> {
    project
        .extensions
        .iter()
        .map(|(name, version)| format!("{name}@{version}"))
        .collect()
}

pub fn export_project(opts: ProjectExportOptions) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let project = read_project(&cwd)?;
    let target_platform = resolve_export_platform(&opts)?;
    let mode = ExportMode::from_options(&opts)?;

    if target_platform == "ios" && platform::host_platform()? != "macos" {
        bail!("iOS export requires macOS");
    }
    if target_platform != platform::host_platform()?
        && target_platform != "android"
        && target_platform != "ios"
    {
        bail!("cross-platform export is only supported for Android and iOS");
    }

    let preset = find_export_preset(&cwd, &target_platform)?;
    let editor = engine::resolve_editor(opts.with_engine.as_deref())?;
    let templates = download_export_templates(&cwd, &project, &target_platform, mode)?;

    let presets_path = cwd.join("export_presets.cfg");
    let original_presets = fs::read_to_string(&presets_path)
        .with_context(|| format!("read {}", presets_path.display()))?;
    let mut next_presets = original_presets.clone();
    if let Some(template) = &templates.custom_template {
        next_presets = upsert_preset_option(
            &next_presets,
            preset.index,
            mode.custom_template_key(),
            &quoted_godot_path(template),
        )?;
    }
    if let Some(android_source) = &templates.android_source_template {
        next_presets = upsert_preset_option(
            &next_presets,
            preset.index,
            "gradle_build/android_source_template",
            &quoted_godot_path(android_source),
        )?;
    }
    if preset_encryption_enabled(&original_presets, preset.index) {
        let key = export_encryption_key(&project).with_context(|| {
            "export preset enables encryption; set project.pug.json export.script_encryption_key or SCRIPT_AES256_ENCRYPTION_KEY"
        })?;
        write_export_credentials(&cwd, count_presets(&original_presets), &key)?;
    }

    if next_presets != original_presets {
        fs::write(&presets_path, &next_presets)
            .with_context(|| format!("write {}", presets_path.display()))?;
    }

    let export_result = run_godot_export(
        &editor,
        &cwd,
        &preset,
        &target_platform,
        mode,
        templates.android_source_template.is_some(),
    );
    if next_presets != original_presets {
        fs::write(&presets_path, &original_presets)
            .with_context(|| format!("restore {}", presets_path.display()))?;
    }
    export_result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExportMode {
    Debug,
    Release,
}

impl ExportMode {
    fn from_options(opts: &ProjectExportOptions) -> Result<Self> {
        if opts.debug && opts.release {
            bail!("choose only one of --debug or --release");
        }
        Ok(if opts.debug {
            Self::Debug
        } else {
            Self::Release
        })
    }

    fn template_kind(self) -> &'static str {
        match self {
            Self::Debug => "template_debug",
            Self::Release => "template_release",
        }
    }

    fn export_flag(self) -> &'static str {
        match self {
            Self::Debug => "--export-debug",
            Self::Release => "--export-release",
        }
    }

    fn custom_template_key(self) -> &'static str {
        match self {
            Self::Debug => "custom_template/debug",
            Self::Release => "custom_template/release",
        }
    }
}

#[derive(Debug)]
struct ExportPreset {
    index: usize,
    name: String,
    platform: String,
    export_path: PathBuf,
}

#[derive(Debug, Default)]
struct ExportTemplates {
    custom_template: Option<PathBuf>,
    android_source_template: Option<PathBuf>,
}

fn read_project(cwd: &Path) -> Result<ProjectConfig> {
    let path = cwd.join("project.pug.json");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn write_project(cwd: &Path, project: &ProjectConfig) -> Result<()> {
    util::write_json(&cwd.join("project.pug.json"), project)
}

fn parse_package(value: &str) -> Result<(String, Option<String>)> {
    if let Some((name, version)) = value.rsplit_once('@')
        && !name.is_empty()
        && !version.is_empty()
    {
        return Ok((name.to_string(), Some(version.to_string())));
    }
    Ok((value.to_string(), None))
}

fn resolve_export_platform(opts: &ProjectExportOptions) -> Result<String> {
    let explicit_count =
        usize::from(opts.platform.is_some()) + usize::from(opts.android) + usize::from(opts.ios);
    if explicit_count > 1 {
        bail!("choose only one export target");
    }
    if opts.android {
        return Ok("android".to_string());
    }
    if opts.ios {
        return Ok("ios".to_string());
    }
    Ok(opts
        .platform
        .as_deref()
        .map(platform::normalize_platform)
        .unwrap_or_else(|| platform::host_platform().unwrap_or("windows").to_string()))
}

fn preset_platform_name(platform_name: &str) -> Result<&'static str> {
    Ok(match platform_name {
        "windows" => "Windows Desktop",
        "macos" => "macOS",
        "linux" => "Linux/X11",
        "android" => "Android",
        "ios" => "iOS",
        other => bail!("unsupported export platform: {other}"),
    })
}

fn find_export_preset(cwd: &Path, platform_name: &str) -> Result<ExportPreset> {
    let wanted = preset_platform_name(platform_name)?;
    let path = cwd.join("export_presets.cfg");
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_export_presets(&text)
        .into_iter()
        .find(|preset| preset.name == wanted || preset.platform == wanted)
        .with_context(|| format!("no export preset found for platform {platform_name} ({wanted})"))
}

fn parse_export_presets(text: &str) -> Vec<ExportPreset> {
    #[derive(Default)]
    struct PartialPreset {
        name: Option<String>,
        platform: Option<String>,
        export_path: Option<PathBuf>,
    }

    let mut current = None;
    let mut presets = BTreeMap::<usize, PartialPreset>::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(index) = preset_section_index(trimmed) {
            current = Some(index);
            presets.entry(index).or_default();
            continue;
        }
        if trimmed.starts_with("[preset.") && trimmed.ends_with(".options]") {
            current = None;
            continue;
        }
        let Some(index) = current else {
            continue;
        };
        let Some((key, value)) = split_assignment(trimmed) else {
            continue;
        };
        let preset = presets.entry(index).or_default();
        match key {
            "name" => preset.name = Some(unquote(value).to_string()),
            "platform" => preset.platform = Some(unquote(value).to_string()),
            "export_path" => preset.export_path = Some(PathBuf::from(unquote(value))),
            _ => {}
        }
    }

    presets
        .into_iter()
        .filter_map(|(index, preset)| {
            Some(ExportPreset {
                index,
                name: preset.name?,
                platform: preset.platform?,
                export_path: preset.export_path?,
            })
        })
        .collect()
}

fn preset_section_index(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("[preset.")?.strip_suffix(']')?;
    if rest.contains('.') {
        return None;
    }
    rest.parse().ok()
}

fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim()))
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"')
}

fn read_metadata(dir: &Path) -> Result<PackageMetadata> {
    Ok(serde_json::from_slice(&fs::read(
        dir.join("metadata.json"),
    )?)?)
}

fn download_export_templates(
    cwd: &Path,
    project: &ProjectConfig,
    platform_name: &str,
    mode: ExportMode,
) -> Result<ExportTemplates> {
    let cfg = Config::load()?;
    let api = ApiClient::from_config(&cfg)?;
    let project_name = if project.name.trim().is_empty() {
        engine::resolve_project_name()?
    } else {
        project.name.clone()
    };
    let response = api.engine_download(&project_name, &project.engine.tag)?;
    let cache_root = cwd
        .join(".godot")
        .join("pug")
        .join("export_templates")
        .join(&response.tag);

    if matches!(platform_name, "windows" | "macos" | "linux") {
        let arch = default_export_arch(platform_name)?;
        let artifact = response
            .artifacts
            .iter()
            .find(|artifact| {
                artifact.platform == platform_name
                    && artifact.kind == mode.template_kind()
                    && artifact.arch == arch
            })
            .with_context(|| {
                format!(
                    "engine {} has no {} template for {}:{}",
                    response.tag,
                    mode.template_kind(),
                    platform_name,
                    arch
                )
            })?;
        let dir = download_engine_artifact(&api, artifact, &cache_root)?;
        let template = find_template_file(&dir, mode.template_kind())?;
        return Ok(ExportTemplates {
            custom_template: Some(template),
            android_source_template: None,
        });
    }

    let artifact = response
        .artifacts
        .iter()
        .find(|artifact| artifact.platform == platform_name && artifact.kind == "template_bundle")
        .with_context(|| {
            format!(
                "engine {} has no template bundle for {}",
                response.tag, platform_name
            )
        })?;
    let dir = download_engine_artifact(&api, artifact, &cache_root)?;
    match platform_name {
        "android" => Ok(ExportTemplates {
            custom_template: None,
            android_source_template: Some(find_named_file(&dir, "android_source.zip")?),
        }),
        "ios" => Ok(ExportTemplates {
            custom_template: Some(find_named_file(&dir, "godot_ios.zip")?),
            android_source_template: None,
        }),
        other => bail!("unsupported export platform: {other}"),
    }
}

fn default_export_arch(platform_name: &str) -> Result<String> {
    if platform_name == platform::host_platform()? {
        return Ok(platform::host_arch().to_string());
    }
    platform::default_arches(platform_name)?
        .into_iter()
        .next()
        .map(str::to_string)
        .with_context(|| format!("no default arch for {platform_name}"))
}

fn download_engine_artifact(
    api: &ApiClient,
    artifact: &EngineArtifact,
    cache_root: &Path,
) -> Result<PathBuf> {
    let arch = if artifact.arch.is_empty() {
        "bundle"
    } else {
        artifact.arch.as_str()
    };
    let dir = cache_root
        .join(&artifact.platform)
        .join(&artifact.kind)
        .join(arch);
    fs::create_dir_all(&dir)?;
    let zip = dir.join("artifact.zip");
    let has_cached_zip = zip.is_file()
        && util::sha256_file(&zip).ok().as_deref() == Some(artifact.package_sha256.as_str())
        && util::file_size(&zip).ok() == Some(artifact.package_size);
    if !has_cached_zip {
        api.download_to(&artifact.download_url, &zip)?;
        let sha = util::sha256_file(&zip)?;
        if sha != artifact.package_sha256 {
            bail!(
                "downloaded template sha mismatch: got {sha} want {}",
                artifact.package_sha256
            );
        }
        let size = util::file_size(&zip)?;
        if size != artifact.package_size {
            bail!(
                "downloaded template size mismatch: got {size} want {}",
                artifact.package_size
            );
        }
    }

    let unpack = dir.join("unpack");
    util::ensure_clean_dir(&unpack)?;
    util::unzip_to(&zip, &unpack)?;
    Ok(unpack)
}

fn find_template_file(dir: &Path, kind: &str) -> Result<PathBuf> {
    let is_template = |path: &Path| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        name.contains(kind)
            && !name.ends_with(".exp")
            && !name.ends_with(".lib")
            && !name.ends_with(".pdb")
    };
    find_file_by(dir, &|path| {
        is_template(path)
            && !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.to_ascii_lowercase().contains(".console."))
    })
    .or_else(|| find_file_by(dir, &is_template))
    .with_context(|| format!("no {kind} template found under {}", dir.display()))
}

fn find_named_file(dir: &Path, file_name: &str) -> Result<PathBuf> {
    find_file_by(dir, &|path| {
        path.file_name().and_then(|n| n.to_str()) == Some(file_name)
    })
    .with_context(|| format!("no {file_name} found under {}", dir.display()))
}

fn find_file_by(dir: &Path, predicate: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        let file_type = entry.file_type().ok()?;
        if file_type.is_file() && predicate(&path) {
            return Some(path);
        }
        if file_type.is_dir()
            && let Some(found) = find_file_by(&path, predicate)
        {
            return Some(found);
        }
    }
    None
}

fn rewrite_manifest(
    cwd: &Path,
    name: &str,
    project: &ProjectConfig,
    template_text: &str,
) -> Result<()> {
    let mut text = strip_libraries(template_text);
    text.push_str("\n[libraries]\n");
    for platform_name in &project.platforms {
        for arch in platform::default_arches(platform_name)? {
            let target = platform::spec(platform_name, arch)?;
            let lib = find_installed_lib(cwd, name, &target.platform, &target.arch)?;
            let rel = format!(
                "res://bin/{}/{}/{}/{}",
                name,
                target.platform,
                target.arch,
                lib.file_name().unwrap().to_string_lossy()
            );
            for key in target.gdextension_keys() {
                text.push_str(&format!("{key} = \"{rel}\"\n"));
            }
        }
    }
    let path = cwd.join("bin").join(format!("{name}.gdextension"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

fn find_installed_lib(cwd: &Path, name: &str, platform: &str, arch: &str) -> Result<PathBuf> {
    let dir = cwd.join("bin").join(name).join(platform).join(arch);
    for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.ends_with(".so")
                || file_name.ends_with(".dylib")
                || file_name.ends_with(".dll")
            {
                return Ok(entry.path());
            }
        }
    }
    bail!("no installed library in {}", dir.display())
}

fn strip_libraries(text: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[libraries]" {
            skipping = true;
            continue;
        }
        if skipping && trimmed.starts_with('[') && trimmed.ends_with(']') {
            skipping = false;
        }
        if !skipping {
            out.push(line);
        }
    }
    out.join("\n").trim_end().to_string()
}

fn default_template() -> String {
    "[configuration]\nentry_symbol = \"gdext_rust_init\"\ncompatibility_minimum = \"4.1\"\n"
        .to_string()
}

fn update_extension_list(cwd: &Path, name: &str) -> Result<()> {
    let path = cwd.join(".godot").join("extension_list.cfg");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let managed = format!("res://bin/{name}.gdextension");
    let legacy_root = format!("res://{name}.gdextension");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| *line != managed && *line != legacy_root)
        .map(str::to_string)
        .collect();
    lines.push(managed);
    fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

fn upsert_preset_option(text: &str, preset_index: usize, key: &str, value: &str) -> Result<String> {
    let header = format!("[preset.{preset_index}.options]");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = lines
        .iter()
        .position(|line| line.trim() == header)
        .with_context(|| format!("export preset {preset_index} is missing options section"))?;
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            let trimmed = line.trim();
            trimmed.starts_with('[') && trimmed.ends_with(']')
        })
        .map(|offset| start + 1 + offset)
        .unwrap_or(lines.len());
    let prefix = format!("{key}=");
    if let Some(index) =
        (start + 1..end).find(|index| lines[*index].trim_start().starts_with(&prefix))
    {
        lines[index] = format!("{key}={value}");
    } else {
        lines.insert(end, format!("{key}={value}"));
    }
    Ok(lines.join("\n") + "\n")
}

fn quoted_godot_path(path: &Path) -> String {
    format!("\"{}\"", godot_path(path))
}

fn godot_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\\\"")
}

fn preset_encryption_enabled(text: &str, preset_index: usize) -> bool {
    let mut current = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(index) = preset_section_index(trimmed) {
            current = Some(index);
            continue;
        }
        if trimmed.starts_with("[preset.") && !trimmed.ends_with(".options]") {
            current = None;
            continue;
        }
        if current == Some(preset_index)
            && matches!(trimmed, "encrypt_pck=true" | "encrypt_directory=true")
        {
            return true;
        }
    }
    false
}

fn count_presets(text: &str) -> usize {
    text.lines()
        .filter_map(|line| preset_section_index(line.trim()))
        .count()
}

fn export_encryption_key(project: &ProjectConfig) -> Option<String> {
    project
        .export
        .as_ref()
        .and_then(|export| export.script_encryption_key.as_deref())
        .map(str::to_string)
        .or_else(|| env::var("PUG_SCRIPT_AES256_ENCRYPTION_KEY").ok())
        .or_else(|| env::var("SCRIPT_AES256_ENCRYPTION_KEY").ok())
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

fn write_export_credentials(cwd: &Path, preset_count: usize, key: &str) -> Result<()> {
    let path = cwd.join(".godot").join("export_credentials.cfg");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for index in 0..preset_count {
        text.push_str(&format!(
            "[preset.{index}]\nscript_encryption_key=\"{key}\"\n\n"
        ));
    }
    fs::write(path, text)?;
    Ok(())
}

fn run_godot_export(
    editor: &Path,
    cwd: &Path,
    preset: &ExportPreset,
    platform_name: &str,
    mode: ExportMode,
    install_android_template: bool,
) -> Result<()> {
    let android_env = if platform_name == "android" {
        Some(ensure_android_environment(editor)?)
    } else {
        None
    };
    let export_path = resolve_export_path(cwd, &preset.export_path);
    if let Some(parent) = export_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut cmd = Command::new(editor);
    cmd.args(["--headless", "--path"]).arg(cwd);
    if install_android_template {
        cmd.arg("--install-android-build-template");
    }
    cmd.arg(mode.export_flag())
        .arg(&preset.name)
        .arg(&export_path);
    if let Some(android_env) = android_env {
        cmd.env("ANDROID_HOME", &android_env.sdk);
        cmd.env("ANDROID_SDK_ROOT", &android_env.sdk);
        cmd.env("JAVA_HOME", &android_env.java_home);
    }
    util::run_command(&mut cmd)?;
    println!("exported {} -> {}", preset.name, export_path.display());
    Ok(())
}

fn resolve_export_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

struct AndroidEnvironment {
    sdk: PathBuf,
    java_home: PathBuf,
}

fn ensure_android_environment(editor: &Path) -> Result<AndroidEnvironment> {
    let sdk = env::var_os("ANDROID_HOME")
        .or_else(|| env::var_os("ANDROID_SDK_ROOT"))
        .map(PathBuf::from)
        .or_else(find_default_android_sdk)
        .context("Android SDK not found; set ANDROID_HOME or ANDROID_SDK_ROOT")?;
    if !sdk.is_dir() {
        bail!("Android SDK path does not exist: {}", sdk.display());
    }

    let java_home = env::var_os("JAVA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            find_on_path("java").and_then(|java| java.parent()?.parent().map(Path::to_path_buf))
        })
        .context("Java SDK not found; set JAVA_HOME or put java on PATH")?;
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    for tool in ["java", "keytool"] {
        let path = java_home.join("bin").join(format!("{tool}{suffix}"));
        if !path.is_file() {
            bail!("Java SDK is incomplete, missing {}", path.display());
        }
    }

    sync_android_editor_settings(editor, &sdk, &java_home)?;
    Ok(AndroidEnvironment { sdk, java_home })
}

fn find_default_android_sdk() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local_app_data).join("Android/Sdk"));
    }
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("AppData/Local/Android/Sdk"));
        candidates.push(home.join("Library/Android/sdk"));
        candidates.push(home.join("Android/Sdk"));
    }
    candidates.into_iter().find(|path| path.is_dir())
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let suffix = if cfg!(windows) && !name.ends_with(".exe") {
        ".exe"
    } else {
        ""
    };
    let file_name = format!("{name}{suffix}");
    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(&file_name))
        .find(|path| path.is_file())
}

fn sync_android_editor_settings(editor: &Path, sdk: &Path, java_home: &Path) -> Result<()> {
    let version = editor_settings_version(editor)?;
    let path = editor_settings_path(&version)?;
    let mut text = if path.is_file() {
        fs::read_to_string(&path)?
    } else {
        "[gd_resource type=\"EditorSettings\" format=3]\n\n[resource]\n".to_string()
    };
    text = upsert_editor_setting(text, "export/android/java_sdk_path", java_home);
    text = upsert_editor_setting(text, "export/android/android_sdk_path", sdk);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

fn editor_settings_version(editor: &Path) -> Result<String> {
    let output = util::output_command(Command::new(editor).arg("--version"))?;
    let first = output
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .context("could not parse Godot version")?;
    let mut parts = first.split('.');
    let major = parts.next().context("Godot version missing major")?;
    let minor = parts.next().context("Godot version missing minor")?;
    Ok(format!("{major}.{minor}"))
}

fn editor_settings_path(version: &str) -> Result<PathBuf> {
    let base = if cfg!(windows) {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .context("APPDATA is not set")?
            .join("Godot")
    } else if cfg!(target_os = "macos") {
        dirs::home_dir()
            .context("cannot resolve home directory")?
            .join("Library/Application Support/Godot")
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            })
            .join("godot")
    };
    Ok(base.join(format!("editor_settings-{version}.tres")))
}

fn upsert_editor_setting(mut text: String, key: &str, value: &Path) -> String {
    let line = format!("{key} = \"{}\"", godot_path(value));
    let prefix = format!("{key} =");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if let Some(index) = lines
        .iter()
        .position(|existing| existing.trim_start().starts_with(&prefix))
    {
        lines[index] = line;
        return lines.join("\n") + "\n";
    }
    if let Some(index) = lines
        .iter()
        .position(|existing| existing.trim() == "[resource]")
    {
        lines.insert(index + 1, line);
        return lines.join("\n") + "\n";
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n[resource]\n");
    text.push_str(&line);
    text.push('\n');
    text
}

fn update_gitignore(cwd: &Path) -> Result<()> {
    let path = cwd.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == "bin/") {
        return Ok(());
    }
    let mut next = existing;
    if !next.ends_with('\n') && !next.is_empty() {
        next.push('\n');
    }
    next.push_str("\n# pug\nbin/\n");
    fs::write(path, next)?;
    Ok(())
}

#[allow(dead_code)]
fn merge_json_object(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_package_supports_latest_and_version() {
        assert_eq!(
            parse_package("rust_demo").unwrap(),
            ("rust_demo".to_string(), None)
        );
        assert_eq!(
            parse_package("rust_demo@0.1.0").unwrap(),
            ("rust_demo".to_string(), Some("0.1.0".to_string()))
        );
    }

    #[test]
    fn locked_extension_packages_use_exact_versions() {
        let mut extensions = BTreeMap::new();
        extensions.insert("rust_demo".to_string(), "0.2.0".to_string());
        extensions.insert("netcode".to_string(), "1.0.0".to_string());
        let project = ProjectConfig {
            name: "test_project".to_string(),
            version: 1,
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: vec!["windows".to_string()],
            extensions,
            export: None,
        };

        assert_eq!(
            locked_extension_packages(&project),
            vec!["netcode@1.0.0".to_string(), "rust_demo@0.2.0".to_string()]
        );
    }

    #[test]
    fn strips_existing_libraries() {
        let text = "[configuration]\na = 1\n[libraries]\nmacos.debug = \"x\"\n";
        assert!(!strip_libraries(text).contains("macos.debug"));
    }

    #[test]
    fn updates_extension_list_for_managed_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let godot_dir = dir.path().join(".godot");
        fs::create_dir_all(&godot_dir).unwrap();
        fs::write(
            godot_dir.join("extension_list.cfg"),
            "res://other.gdextension\nres://rust_demo.gdextension\n",
        )
        .unwrap();

        update_extension_list(dir.path(), "rust_demo").unwrap();

        let text = fs::read_to_string(godot_dir.join("extension_list.cfg")).unwrap();
        assert!(text.contains("res://other.gdextension\n"));
        assert!(text.contains("res://bin/rust_demo.gdextension\n"));
        assert!(!text.contains("res://rust_demo.gdextension\n"));
    }

    #[test]
    fn rewrite_manifest_omits_integrity_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let lib_dir = dir.path().join("bin/rust_demo/windows/x86_64");
        fs::create_dir_all(&lib_dir).unwrap();
        fs::write(lib_dir.join("rust_demo.dll"), "demo dll").unwrap();

        let project = ProjectConfig {
            name: "test_project".to_string(),
            version: 1,
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: vec!["windows".to_string()],
            extensions: BTreeMap::new(),
            export: None,
        };

        rewrite_manifest(
            dir.path(),
            "rust_demo",
            &project,
            "[configuration]\nentry_symbol = \"gdext_rust_init\"\n",
        )
        .unwrap();

        let text = fs::read_to_string(dir.path().join("bin/rust_demo.gdextension")).unwrap();
        assert!(text.contains("[libraries]\n"));
        assert!(text.contains(
            "windows.x86_64.release = \"res://bin/rust_demo/windows/x86_64/rust_demo.dll\""
        ));
        assert!(!text.contains("[integrity]\n"));
    }
}
