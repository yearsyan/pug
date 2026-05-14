use anyhow::{Context, Result, bail};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use walkdir::WalkDir;

use crate::{
    api::{ApiClient, DownloadablePackageUploadInit, EngineArtifact, ExportUploadInit},
    config::Config,
    engine,
    extension::{self, PackageMetadata},
    platform, util,
};

fn default_project_version() -> String {
    "0.1.0".to_string()
}

const NUGET_CONFIG_FILE: &str = "NuGet.Config";
const NUGET_GODOT_SOURCE: &str = "godot-local";
const NUGET_ORG_SOURCE: &str = "nuget.org";
const NUGET_ORG_URL: &str = "https://api.nuget.org/v3/index.json";
const PROJECT_FILE: &str = "project.pug.json";
const PROJECT_OVERWRITE_FILE: &str = "project-overwrite.pug.json";
const LOCAL_EXTENSION_PREFIX: &str = "local://";

fn deserialize_project_version<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(version) => Ok(version),
        Value::Number(number) => Ok(number.to_string()),
        Value::Null => Ok(default_project_version()),
        other => Err(serde::de::Error::custom(format!(
            "project version must be a string, got {other}"
        ))),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectConfig {
    #[serde(default)]
    name: String,
    #[serde(
        default = "default_project_version",
        deserialize_with = "deserialize_project_version"
    )]
    version: String,
    engine: ProjectEngine,
    #[serde(default)]
    platforms: ProjectPlatforms,
    #[serde(default)]
    extensions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    packs: BTreeMap<String, ProjectPackConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    export: Option<ProjectExportConfig>,
    #[serde(default, skip_serializing_if = "ProjectNugetConfig::is_empty")]
    nuget: ProjectNugetConfig,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectEngine {
    tag: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectNugetConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    sources: BTreeMap<String, String>,
}

impl ProjectNugetConfig {
    fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProjectPackKind {
    Downloadable,
    Internal,
}

impl Default for ProjectPackKind {
    fn default() -> Self {
        Self::Downloadable
    }
}

impl ProjectPackKind {
    fn name(self) -> &'static str {
        match self {
            Self::Downloadable => "downloadable",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProjectPackEncryptType {
    Project,
    None,
    Random,
}

impl Default for ProjectPackEncryptType {
    fn default() -> Self {
        Self::Project
    }
}

impl ProjectPackEncryptType {
    fn name(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::None => "none",
            Self::Random => "random",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectPackConfig {
    path: PathBuf,
    #[serde(default, rename = "type")]
    kind: ProjectPackKind,
    #[serde(default)]
    encrypt_type: ProjectPackEncryptType,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProjectExportConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypt: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypt_pck: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypt_directory: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encryption_include_filters: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encryption_exclude_filters: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    script_encryption_key: Option<String>,
    #[serde(default, skip_serializing)]
    windows: Option<ProjectPlatformConfig>,
    #[serde(default, skip_serializing)]
    macos: Option<ProjectPlatformConfig>,
    #[serde(default, skip_serializing)]
    linux: Option<ProjectPlatformConfig>,
    #[serde(default, skip_serializing)]
    ios: Option<ProjectPlatformConfig>,
    #[serde(default, skip_serializing)]
    android: Option<ProjectPlatformConfig>,
}

#[derive(Debug, Clone, Default)]
struct ProjectPlatforms(Vec<ProjectPlatformEntry>);

#[derive(Debug, Clone)]
struct ProjectPlatformEntry {
    name: String,
    config: ProjectPlatformConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectPlatformConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    export_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    architecture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bundle_identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    embed_pck: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    architectures: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min_sdk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_sdk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gradle_build_directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    debug_keystore: Option<AndroidKeystoreConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    release_keystore: Option<AndroidKeystoreConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    internet_permission: Option<bool>,
}

impl ProjectPlatformConfig {
    fn merge_missing(&mut self, other: ProjectPlatformConfig) {
        macro_rules! fill {
            ($field:ident) => {
                if self.$field.is_none() {
                    self.$field = other.$field;
                }
            };
        }

        fill!(export_path);
        fill!(architecture);
        fill!(bundle_identifier);
        fill!(embed_pck);
        fill!(name);
        fill!(package);
        fill!(signed);
        fill!(architectures);
        fill!(version_code);
        fill!(version_name);
        fill!(min_sdk);
        fill!(target_sdk);
        fill!(gradle_build_directory);
        fill!(debug_keystore);
        fill!(release_keystore);
        fill!(internet_permission);
    }
}

impl ProjectPlatforms {
    fn from_specs<I, S>(specs: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut platforms = Self::default();
        for spec in specs {
            platforms
                .push_spec(spec.as_ref())
                .map_err(|err| anyhow::anyhow!(err))?;
        }
        Ok(platforms)
    }

    #[cfg(test)]
    fn from_configs<I, S>(configs: I) -> Result<Self>
    where
        I: IntoIterator<Item = (S, ProjectPlatformConfig)>,
        S: AsRef<str>,
    {
        let mut platforms = Self::default();
        for (name, config) in configs {
            platforms
                .merge_config(name.as_ref(), config)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
        Ok(platforms)
    }

    fn push_spec(&mut self, raw: &str) -> std::result::Result<(), String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(());
        }
        let (platform_name, arch) = raw
            .split_once(':')
            .map(|(platform_name, arch)| (platform_name, Some(arch)))
            .unwrap_or((raw, None));
        let platform_name = normalize_project_platform_name(platform_name)?;
        let mut config = ProjectPlatformConfig::default();
        if let Some(arch) = arch.map(str::trim).filter(|arch| !arch.is_empty()) {
            if platform_name == "android" {
                config.architectures = Some(vec![arch.to_string()]);
            } else {
                config.architecture = Some(arch.to_string());
            }
        }
        self.merge_config(&platform_name, config)
    }

    fn merge_config(
        &mut self,
        raw_name: &str,
        config: ProjectPlatformConfig,
    ) -> std::result::Result<(), String> {
        let name = normalize_project_platform_name(raw_name)?;
        if let Some(entry) = self.0.iter_mut().find(|entry| entry.name == name) {
            entry.config.merge_missing(config);
        } else {
            self.0.push(ProjectPlatformEntry { name, config });
        }
        Ok(())
    }

    fn get(&self, platform_name: &str) -> Option<&ProjectPlatformConfig> {
        self.0
            .iter()
            .find(|entry| entry.name == platform_name)
            .map(|entry| &entry.config)
    }

    fn names(&self) -> Vec<String> {
        self.0.iter().map(|entry| entry.name.clone()).collect()
    }
}

fn normalize_project_platform_name(raw_name: &str) -> std::result::Result<String, String> {
    let name = platform::normalize_platform(raw_name.trim());
    if name.is_empty() {
        return Err("project.pug.json platforms contains an empty platform key".to_string());
    }
    Ok(name)
}

impl Serialize for ProjectPlatforms {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for entry in &self.0 {
            map.serialize_entry(&entry.name, &entry.config)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ProjectPlatforms {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ProjectPlatformsVisitor;

        impl<'de> de::Visitor<'de> for ProjectPlatformsVisitor {
            type Value = ProjectPlatforms;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a platform config object or legacy platform string list")
            }

            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut platforms = ProjectPlatforms::default();
                while let Some(item) = seq.next_element::<String>()? {
                    platforms.push_spec(&item).map_err(de::Error::custom)?;
                }
                Ok(platforms)
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut platforms = ProjectPlatforms::default();
                while let Some(name) = map.next_key::<String>()? {
                    let config = map
                        .next_value::<Option<ProjectPlatformConfig>>()?
                        .unwrap_or_default();
                    platforms
                        .merge_config(&name, config)
                        .map_err(de::Error::custom)?;
                }
                Ok(platforms)
            }
        }

        deserializer.deserialize_any(ProjectPlatformsVisitor)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AndroidKeystoreConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password_env: Option<String>,
}

pub struct ProjectExportOptions {
    pub platform: Option<String>,
    pub android: bool,
    pub ios: bool,
    pub debug: bool,
    pub release: bool,
    pub upload: bool,
    pub no_remote_sign: bool,
    pub with_engine: Option<PathBuf>,
}

pub fn init(engine_tag: Option<String>, platforms: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = cwd.join(PROJECT_FILE);
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
    let platforms = ProjectPlatforms::from_specs(platforms)?;
    let project = ProjectConfig {
        name: engine::resolve_project_name().unwrap_or_default(),
        version: default_project_version(),
        engine: ProjectEngine { tag },
        platforms,
        extensions: BTreeMap::new(),
        packs: BTreeMap::new(),
        export: None,
        nuget: ProjectNugetConfig::default(),
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

    let mut project = read_project(&cwd)?;
    if project.extensions.is_empty() {
        sync_nuget_config(&cwd, &project, None)?;
        sync_export_presets(&cwd, &project, None)?;
        println!("no extensions listed in project.pug.json");
        return Ok(());
    }
    let platforms = export_platforms(&project)?;
    let mut base_project = read_project_base(&cwd)?;
    let mut base_changed = false;
    let mut local_editor = None;
    for (name, value) in project.extensions.clone() {
        if is_local_extension_ref(&value) {
            let editor = match &local_editor {
                Some(editor) => editor,
                None => {
                    local_editor = Some(engine::resolve_editor(None)?);
                    local_editor.as_ref().unwrap()
                }
            };
            install_local_extension(&cwd, &project, &name, &value, &platforms, editor, false)?;
        } else {
            let requested_version = nonempty_version(&value);
            let installed =
                install_remote_extension(&cwd, &project, &name, requested_version.as_deref())?;
            finalize_extension_install(
                &cwd,
                &name,
                &project,
                &installed.template_text,
                &platforms,
            )?;
            if base_project
                .extensions
                .get(&name)
                .is_some_and(|value| !is_local_extension_ref(value) && value != &installed.version)
            {
                base_project
                    .extensions
                    .insert(name.clone(), installed.version.clone());
                base_changed = true;
            }
            println!("installed {name}@{}", installed.version);
        }
    }
    if base_changed {
        write_project(&cwd, &base_project)?;
        project = read_project(&cwd)?;
    }
    update_gitignore(&cwd)?;
    sync_nuget_config(&cwd, &project, None)?;
    sync_export_presets(&cwd, &project, None)?;
    Ok(())
}

pub fn pack_add(name: &str, path: &Path, internal: bool, encrypt_type: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut project = read_project_base(&cwd)?;
    let name = normalize_pack_name(name)?;
    let path = normalize_pack_path(path)?;
    let encrypt_type = parse_pack_encrypt_type(encrypt_type)?;
    let kind = if internal {
        ProjectPackKind::Internal
    } else {
        ProjectPackKind::Downloadable
    };
    project.packs.insert(
        name.clone(),
        ProjectPackConfig {
            path: PathBuf::from(path),
            kind,
            encrypt_type,
        },
    );
    write_project(&cwd, &project)?;
    let project = read_project(&cwd)?;
    sync_export_presets(&cwd, &project, None)?;
    println!(
        "registered pack {name} ({}) in project.pug.json",
        kind.name()
    );
    Ok(())
}

fn install_one(cwd: &Path, package: &str) -> Result<()> {
    let mut project = read_project_base(cwd)?;
    let (name, requested_version) = parse_package(package)?;
    if let Some(local_ref) = requested_version
        .as_deref()
        .filter(|value| is_local_extension_ref(value))
    {
        project
            .extensions
            .insert(name.clone(), local_ref.to_string());
        write_project(cwd, &project)?;
        let project = read_project(cwd)?;
        let platforms = export_platforms(&project)?;
        let editor = engine::resolve_editor(None)?;
        install_local_extension(cwd, &project, &name, local_ref, &platforms, &editor, false)?;
        update_gitignore(cwd)?;
        sync_nuget_config(cwd, &project, None)?;
        sync_export_presets(cwd, &project, None)?;
        return Ok(());
    }

    let effective_project = read_project(cwd)?;
    let installed =
        install_remote_extension(cwd, &effective_project, &name, requested_version.as_deref())?;
    project
        .extensions
        .insert(name.clone(), installed.version.clone());
    write_project(cwd, &project)?;
    let project = read_project(cwd)?;
    let platforms = export_platforms(&project)?;
    finalize_extension_install(cwd, &name, &project, &installed.template_text, &platforms)?;
    update_gitignore(cwd)?;
    sync_nuget_config(cwd, &project, None)?;
    sync_export_presets(cwd, &project, None)?;
    println!("installed {package}");
    Ok(())
}

struct InstalledExtension {
    version: String,
    template_text: String,
}

fn install_remote_extension(
    cwd: &Path,
    project: &ProjectConfig,
    name: &str,
    requested_version: Option<&str>,
) -> Result<InstalledExtension> {
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

    for platform_name in export_platforms(project)? {
        for arch in extension_arches_for_platform(project, &platform_name)? {
            let target = platform::spec(&platform_name, &arch)?;
            let resolved = api.resolve_extension(
                &project_name,
                &name,
                requested_version,
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
    let template_text = template_text.unwrap_or_else(default_template);
    Ok(InstalledExtension {
        version,
        template_text,
    })
}

fn install_local_extension(
    cwd: &Path,
    project: &ProjectConfig,
    name: &str,
    value: &str,
    platforms: &[String],
    editor: &Path,
    debug: bool,
) -> Result<()> {
    let ext_dir = resolve_local_extension_dir(cwd, value)?;
    let targets = extension_targets_for_platforms(project, platforms)?;
    let built = extension::build_local(&ext_dir, targets, editor, debug)
        .with_context(|| format!("build local extension {name} from {}", ext_dir.display()))?;
    if built.is_empty() {
        bail!("local extension {name} did not produce any artifacts");
    }
    if let Some(actual) = built
        .iter()
        .map(|item| item.name.as_str())
        .find(|actual| *actual != name)
    {
        bail!(
            "local extension dependency {name} points to package {}, expected Cargo.toml package.name to match",
            actual
        );
    }

    let mut template_text = None;
    for item in &built {
        let tmp = tempfile::tempdir()?;
        let unpack = tmp.path().join("unpack");
        util::untar_zst(&item.package_path, &unpack)?;
        let metadata = read_metadata(&unpack)?;
        if metadata.name != name
            || metadata.platform != item.target.platform
            || metadata.arch != item.target.arch
        {
            bail!(
                "local extension package metadata mismatch for {name}: {} {}:{}",
                metadata.name,
                metadata.platform,
                metadata.arch
            );
        }
        let lib_src = unpack.join(&metadata.library);
        let lib_sha = util::sha256_file(&lib_src)?;
        if lib_sha != metadata.lib_sha256 {
            bail!(
                "local extension library sha mismatch for {name}: got {lib_sha} want {}",
                metadata.lib_sha256
            );
        }
        let lib_size = util::file_size(&lib_src)?;
        if lib_size != metadata.lib_size {
            bail!(
                "local extension library size mismatch for {name}: got {lib_size} want {}",
                metadata.lib_size
            );
        }
        let lib_dst = cwd
            .join("bin")
            .join(name)
            .join(&item.target.platform)
            .join(&item.target.arch)
            .join(&metadata.library);
        util::copy_file(&lib_src, &lib_dst)?;

        let tmpl = unpack.join(format!("{name}.gdextension.tmpl"));
        if tmpl.is_file() {
            template_text = Some(fs::read_to_string(&tmpl)?);
            util::copy_file(
                &tmpl,
                &cwd.join("bin")
                    .join(name)
                    .join(format!("{name}.gdextension.tmpl")),
            )?;
        }
    }

    let template_text = template_text.unwrap_or_else(default_template);
    finalize_extension_install(cwd, name, project, &template_text, platforms)?;
    println!("installed {name}@{value}");
    Ok(())
}

fn finalize_extension_install(
    cwd: &Path,
    name: &str,
    project: &ProjectConfig,
    template_text: &str,
    platforms: &[String],
) -> Result<()> {
    rewrite_manifest_for_platforms(cwd, name, project, template_text, platforms)?;
    update_extension_list(cwd, name)?;
    Ok(())
}

fn resolve_local_extension_dir(cwd: &Path, value: &str) -> Result<PathBuf> {
    let value = value.trim();
    let rest = value
        .strip_prefix(LOCAL_EXTENSION_PREFIX)
        .context("local extension dependency must start with local://")?;
    if rest.trim().is_empty() {
        bail!("local extension path must not be empty");
    }
    let path = PathBuf::from(rest);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    path.canonicalize()
        .with_context(|| format!("resolve local extension path {}", path.display()))
}

fn is_local_extension_ref(value: &str) -> bool {
    value.trim().starts_with(LOCAL_EXTENSION_PREFIX)
}

fn nonempty_version(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn extension_targets_for_platforms(
    project: &ProjectConfig,
    platforms: &[String],
) -> Result<Vec<platform::TargetSpec>> {
    let capable = platform::host_capable_platforms()?;
    let mut targets = Vec::new();
    for platform_name in platforms {
        if !capable.iter().any(|capable| capable == platform_name) {
            bail!(
                "local extension platform {platform_name} is not buildable on this host; supported here: {}",
                capable.join(",")
            );
        }
        for arch in extension_arches_for_platform(project, platform_name)? {
            targets.push(platform::spec(platform_name, &arch)?);
        }
    }
    Ok(targets)
}

fn sync_local_extensions_for_export(
    cwd: &Path,
    project: &ProjectConfig,
    target_platform: &str,
    editor: &Path,
    mode: ExportMode,
) -> Result<()> {
    let platforms = vec![target_platform.to_string()];
    for (name, value) in &project.extensions {
        if is_local_extension_ref(value) {
            install_local_extension(
                cwd,
                project,
                name,
                value,
                &platforms,
                editor,
                mode == ExportMode::Debug,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
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
    let target_platforms = resolve_export_platforms(&opts)?;
    let mode = ExportMode::from_options(&opts)?;

    for target_platform in target_platforms {
        export_project_target(&cwd, &project, &opts, &target_platform, mode)?;
    }
    Ok(())
}

fn export_project_target(
    cwd: &Path,
    project: &ProjectConfig,
    opts: &ProjectExportOptions,
    target_platform: &str,
    mode: ExportMode,
) -> Result<()> {
    if target_platform == "ios" && platform::host_platform()? != "macos" {
        bail!("iOS export requires macOS");
    }
    if opts.upload && !matches!(target_platform, "android" | "windows") {
        bail!("export artifact upload is currently supported for android and windows");
    }
    if !export_platforms(project)?
        .iter()
        .any(|platform_name| platform_name == target_platform)
    {
        bail!("project.pug.json platforms does not include export target {target_platform}");
    }

    let editor = engine::resolve_editor(opts.with_engine.as_deref())?;
    sync_local_extensions_for_export(cwd, project, target_platform, &editor, mode)?;
    sync_nuget_config(cwd, project, Some(&editor))?;
    let templates = resolve_export_templates(
        cwd,
        project,
        target_platform,
        mode,
        opts.with_engine.is_some(),
    )?;
    let cfg = Config::load()?;
    let api = ApiClient::from_config(&cfg)?;
    let project_name = if project.name.trim().is_empty() {
        engine::resolve_project_name()?
    } else {
        project.name.clone()
    };
    let remote_sign = if opts.no_remote_sign {
        RemoteSignEnv::disabled()
    } else {
        let token = api
            .editor_token(&project_name)
            .with_context(|| format!("request editor signing token for project {project_name}"))?;
        RemoteSignEnv::enabled(cfg.base_url(), project_name.clone(), token.editor_token)
    };

    let android_keystore =
        prepare_android_keystore_override(cwd, project, target_platform, mode, &editor)?;
    let template_override = ExportTemplateOverride {
        platform: target_platform,
        mode,
        templates: &templates,
        android_keystore: android_keystore.as_ref(),
    };
    let presets = sync_export_presets(cwd, project, Some(&template_override))?;
    let target_preset_name = preset_platform_name(target_platform)?;
    let preset = presets
        .iter()
        .find(|preset| preset.platform == target_preset_name)
        .with_context(|| {
            format!(
                "project.pug.json platforms does not include export target {}",
                target_platform
            )
        })?;
    write_export_credentials(cwd, &presets)?;

    run_godot_export(
        &editor,
        cwd,
        &preset,
        target_platform,
        mode,
        templates.android_source_template.is_some(),
        &remote_sign,
    )?;
    export_pack_presets(&editor, cwd, project, &presets, target_platform)?;

    if opts.upload {
        upload_downloadable_packs(
            &api,
            cwd,
            project,
            &project_name,
            target_platform,
            mode,
            &presets,
        )?;
        let export_path = resolve_export_path(cwd, &preset.export_path);
        upload_export_artifact(
            &api,
            cwd,
            project,
            &project_name,
            target_platform,
            mode,
            &export_path,
            opts.no_remote_sign,
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct RemoteSignEnv {
    base_url: Option<String>,
    project_name: Option<String>,
    editor_token: Option<String>,
    no_remote_sign: bool,
}

impl RemoteSignEnv {
    fn enabled(base_url: String, project_name: String, editor_token: String) -> Self {
        Self {
            base_url: Some(base_url),
            project_name: Some(project_name),
            editor_token: Some(editor_token),
            no_remote_sign: false,
        }
    }

    fn disabled() -> Self {
        Self {
            base_url: None,
            project_name: None,
            editor_token: None,
            no_remote_sign: true,
        }
    }
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

    fn name(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct GeneratedAndroidKeystoreMetadata {
    alias: String,
    password: String,
}

fn prepare_android_keystore_override(
    cwd: &Path,
    project: &ProjectConfig,
    target_platform: &str,
    mode: ExportMode,
    editor: &Path,
) -> Result<Option<GeneratedAndroidKeystore>> {
    if target_platform != "android" {
        return Ok(None);
    }
    let Some(config) = android_export_config(project) else {
        return Ok(None);
    };
    if !config.signed.unwrap_or(false) {
        return Ok(None);
    }
    let configured_keystore = android_mode_keystore_config(config, mode);
    if !keystore_path(configured_keystore).trim().is_empty() {
        return Ok(None);
    }

    let android_env = ensure_android_environment(editor)?;
    let keystore =
        ensure_generated_android_keystore(cwd, mode, configured_keystore, &android_env.java_home)?;
    eprintln!(
        "pug: using generated temporary Android {} keystore {}",
        mode.name(),
        keystore.path.display()
    );
    Ok(Some(keystore))
}

fn android_mode_keystore_config(
    config: &ProjectPlatformConfig,
    mode: ExportMode,
) -> Option<&AndroidKeystoreConfig> {
    match mode {
        ExportMode::Debug => config.debug_keystore.as_ref(),
        ExportMode::Release => config.release_keystore.as_ref(),
    }
}

fn ensure_generated_android_keystore(
    cwd: &Path,
    mode: ExportMode,
    configured: Option<&AndroidKeystoreConfig>,
    java_home: &Path,
) -> Result<GeneratedAndroidKeystore> {
    let dir = cwd.join(".godot").join("pug").join("android_keystores");
    let keystore_path = dir.join(format!("temporary-{}.keystore", mode.name()));
    let metadata_path = dir.join(format!("temporary-{}.json", mode.name()));
    let configured_alias = optional_keystore_alias(configured);
    let configured_password = optional_keystore_password(configured);
    let has_config_override = configured_alias.is_some() || configured_password.is_some();

    if !has_config_override && keystore_path.is_file() {
        if let Some(metadata) = read_generated_android_keystore_metadata(&metadata_path)? {
            return Ok(GeneratedAndroidKeystore {
                mode,
                path: keystore_path,
                alias: metadata.alias,
                password: metadata.password,
            });
        }
    }

    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    if keystore_path.exists() {
        fs::remove_file(&keystore_path)
            .with_context(|| format!("remove {}", keystore_path.display()))?;
    }
    let metadata = GeneratedAndroidKeystoreMetadata {
        alias: configured_alias.unwrap_or_else(|| format!("pug-{}", mode.name())),
        password: configured_password.unwrap_or_else(generate_android_keystore_password),
    };
    generate_android_keystore(
        java_home,
        &keystore_path,
        &metadata.alias,
        &metadata.password,
    )?;
    util::write_json(&metadata_path, &metadata)?;
    Ok(GeneratedAndroidKeystore {
        mode,
        path: keystore_path,
        alias: metadata.alias,
        password: metadata.password,
    })
}

fn read_generated_android_keystore_metadata(
    path: &Path,
) -> Result<Option<GeneratedAndroidKeystoreMetadata>> {
    if !path.is_file() {
        return Ok(None);
    }
    let metadata: GeneratedAndroidKeystoreMetadata = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    if metadata.alias.trim().is_empty() || metadata.password.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(metadata))
}

fn generate_android_keystore(
    java_home: &Path,
    path: &Path,
    alias: &str,
    password: &str,
) -> Result<()> {
    let keytool = java_tool_path(java_home, "keytool");
    let mut cmd = Command::new(&keytool);
    cmd.args(["-genkeypair", "-v", "-keystore"])
        .arg(path)
        .args([
            "-storepass",
            password,
            "-keypass",
            password,
            "-alias",
            alias,
            "-keyalg",
            "RSA",
            "-keysize",
            "2048",
            "-validity",
            "10000",
            "-dname",
            "CN=pug temporary Android export,O=pug,C=US",
            "-noprompt",
        ]);
    let output = cmd
        .output()
        .with_context(|| format!("spawn {}", keytool.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "keytool failed ({}) while generating temporary Android keystore {}\n{}",
            output.status,
            path.display(),
            stderr
        );
    }
    Ok(())
}

fn generate_android_keystore_password() -> String {
    let mut rng = rand::rng();
    (0..32)
        .map(|_| char::from(rng.sample(Alphanumeric)))
        .collect()
}

#[derive(Debug)]
struct ExportPreset {
    index: usize,
    name: String,
    platform: String,
    export_path: PathBuf,
    kind: ExportPresetKind,
    credential_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExportPresetKind {
    App,
    Pack {
        pack_name: String,
        pack_kind: ProjectPackKind,
        encrypt_type: ProjectPackEncryptType,
    },
}

impl ExportPreset {
    fn is_pack_for(&self, platform_name: &str, pack_kind: ProjectPackKind) -> bool {
        self.platform == preset_platform_name(platform_name).unwrap_or("")
            && matches!(
                &self.kind,
                ExportPresetKind::Pack { pack_kind: kind, .. } if *kind == pack_kind
            )
    }
}

#[derive(Debug, Default)]
struct ExportTemplates {
    custom_template: Option<PathBuf>,
    android_source_template: Option<PathBuf>,
}

struct GeneratedAndroidKeystore {
    mode: ExportMode,
    path: PathBuf,
    alias: String,
    password: String,
}

struct ExportTemplateOverride<'a> {
    platform: &'a str,
    mode: ExportMode,
    templates: &'a ExportTemplates,
    android_keystore: Option<&'a GeneratedAndroidKeystore>,
}

fn read_project(cwd: &Path) -> Result<ProjectConfig> {
    let mut value = read_project_value(&cwd.join(PROJECT_FILE))?;
    let overwrite_path = cwd.join(PROJECT_OVERWRITE_FILE);
    if overwrite_path.is_file() {
        let overwrite = read_project_value(&overwrite_path)?;
        merge_json_value(&mut value, overwrite);
    }
    let mut project: ProjectConfig = serde_json::from_value(value)
        .with_context(|| format!("parse {}", cwd.join(PROJECT_FILE).display()))?;
    migrate_legacy_export_platforms(&mut project)?;
    Ok(project)
}

fn read_project_base(cwd: &Path) -> Result<ProjectConfig> {
    let mut project: ProjectConfig =
        serde_json::from_value(read_project_value(&cwd.join(PROJECT_FILE))?)
            .with_context(|| format!("parse {}", cwd.join(PROJECT_FILE).display()))?;
    migrate_legacy_export_platforms(&mut project)?;
    Ok(project)
}

fn write_project(cwd: &Path, project: &ProjectConfig) -> Result<()> {
    util::write_json(&cwd.join(PROJECT_FILE), project)
}

fn read_project_value(path: &Path) -> Result<Value> {
    let value: Value = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    if !value.is_object() {
        bail!("{} must contain a JSON object", path.display());
    }
    Ok(value)
}

fn merge_json_value(base: &mut Value, overwrite: Value) {
    match (base, overwrite) {
        (Value::Object(base), Value::Object(overwrite)) => {
            for (key, value) in overwrite {
                match base.get_mut(&key) {
                    Some(existing) => merge_json_value(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overwrite) => {
            *base = overwrite;
        }
    }
}

fn migrate_legacy_export_platforms(project: &mut ProjectConfig) -> Result<()> {
    let Some(export) = project.export.as_mut() else {
        return Ok(());
    };
    for (platform_name, config) in [
        ("windows", export.windows.take()),
        ("macos", export.macos.take()),
        ("linux", export.linux.take()),
        ("ios", export.ios.take()),
        ("android", export.android.take()),
    ] {
        if let Some(config) = config
            && project.platforms.get(platform_name).is_some()
        {
            project
                .platforms
                .merge_config(platform_name, config)
                .map_err(|err| anyhow::anyhow!(err))?;
        }
    }
    Ok(())
}

fn sync_nuget_config(
    cwd: &Path,
    project: &ProjectConfig,
    resolved_editor: Option<&Path>,
) -> Result<()> {
    if !has_csproj(cwd)? {
        return Ok(());
    }

    let editor = match resolved_editor {
        Some(path) => path.to_path_buf(),
        None => engine::resolve_editor(None)?,
    };
    let source_dir = godot_nuget_source_dir(&editor)?;
    let text = render_nuget_config(project, &source_dir)?;
    let path = cwd.join(NUGET_CONFIG_FILE);
    fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    update_gitignore_entries(cwd, &[NUGET_CONFIG_FILE])?;
    warn_if_generated_file_not_ignored(cwd, NUGET_CONFIG_FILE);
    Ok(())
}

fn has_csproj(cwd: &Path) -> Result<bool> {
    for entry in fs::read_dir(cwd).with_context(|| format!("read {}", cwd.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("csproj"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn godot_nuget_source_dir(editor: &Path) -> Result<PathBuf> {
    let editor_dir = editor
        .parent()
        .with_context(|| format!("resolve editor directory for {}", editor.display()))?;
    let path = make_absolute_path(&editor_dir.join("GodotSharp").join("Tools").join("nupkgs"))?;
    if !path.is_dir() {
        bail!(
            "C# project detected, but the resolved Godot editor does not provide NuGet packages at {}",
            path.display()
        );
    }
    Ok(path)
}

fn make_absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn render_nuget_config(project: &ProjectConfig, godot_source_dir: &Path) -> Result<String> {
    let mut text = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<configuration>\n  <packageSources>\n    <clear />\n",
    );
    text.push_str(&format!(
        "    <add key=\"{}\" value=\"{}\" />\n",
        xml_escape(NUGET_GODOT_SOURCE),
        xml_escape(&nuget_path_value(godot_source_dir))
    ));

    let mut keys = BTreeSet::from([
        NUGET_GODOT_SOURCE.to_ascii_lowercase(),
        NUGET_ORG_SOURCE.to_ascii_lowercase(),
    ]);
    for (key, value) in &project.nuget.sources {
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            bail!("project.pug.json nuget source key must not be empty");
        }
        if value.is_empty() {
            bail!("project.pug.json nuget source {key} must not be empty");
        }
        let normalized = key.to_ascii_lowercase();
        if !keys.insert(normalized) {
            bail!("project.pug.json nuget source {key} conflicts with a generated source");
        }
        text.push_str(&format!(
            "    <add key=\"{}\" value=\"{}\" />\n",
            xml_escape(key),
            xml_escape(value)
        ));
    }

    text.push_str(&format!(
        "    <add key=\"{}\" value=\"{}\" />\n",
        xml_escape(NUGET_ORG_SOURCE),
        xml_escape(NUGET_ORG_URL)
    ));
    text.push_str("  </packageSources>\n</configuration>\n");
    Ok(text)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn nuget_path_value(path: &Path) -> String {
    let value = path.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        value.replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        value
    }
}

fn sync_export_presets(
    cwd: &Path,
    project: &ProjectConfig,
    template_override: Option<&ExportTemplateOverride<'_>>,
) -> Result<Vec<ExportPreset>> {
    let (text, presets) = render_export_presets(cwd, project, template_override)?;
    let path = cwd.join("export_presets.cfg");
    fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    update_gitignore(cwd)?;
    Ok(presets)
}

fn render_export_presets(
    cwd: &Path,
    project: &ProjectConfig,
    template_override: Option<&ExportTemplateOverride<'_>>,
) -> Result<(String, Vec<ExportPreset>)> {
    let platforms = export_platforms(project)?;
    let display_name = export_display_name(project);
    let encrypt_pck = export_encrypt_pck(project);
    let encrypt_directory = export_encrypt_directory(project);
    let include_filters = export_config(project)
        .and_then(|export| export.encryption_include_filters.as_deref())
        .unwrap_or("*");
    let exclude_filters = export_config(project)
        .and_then(|export| export.encryption_exclude_filters.as_deref())
        .unwrap_or(".godot/extension_list.cfg,*.gdextension,.godot_custom/signatures.csv");

    let mut text = String::from(
        "; This file is generated by pug from project.pug.json.\n; Manual changes will be overwritten by `pug project install` and `pug project export`.\n\n",
    );
    let mut presets = Vec::new();
    for platform_name in platforms.iter() {
        validate_platform_pack_config(project, platform_name)?;
        let index = presets.len();
        let preset_name = preset_platform_name(platform_name)?;
        let export_path = export_path_for_platform(project, platform_name, &display_name)?;
        let main_exclude_filter = main_pack_exclude_filter(project, platform_name)?;
        text.push_str(&format!("[preset.{index}]\n"));
        text.push_str(&format!("name={}\n", cfg_string(preset_name)));
        text.push_str(&format!("platform={}\n", cfg_string(preset_name)));
        text.push_str(&format!(
            "runnable={}\n",
            cfg_bool(preset_runnable(platform_name))
        ));
        text.push_str("dedicated_server=false\n");
        text.push_str("custom_features=\"\"\n");
        text.push_str("export_filter=\"all_resources\"\n");
        text.push_str("export_files=PackedStringArray()\n");
        text.push_str("include_filter=\"\"\n");
        text.push_str(&format!(
            "exclude_filter={}\n",
            cfg_string(&main_exclude_filter)
        ));
        text.push_str(&format!("export_path={}\n", cfg_path(&export_path)));
        text.push_str("patches=PackedStringArray()\n");
        text.push_str(&format!(
            "encryption_include_filters={}\n",
            cfg_string(include_filters)
        ));
        text.push_str(&format!(
            "encryption_exclude_filters={}\n",
            cfg_string(exclude_filters)
        ));
        text.push_str(&format!("encrypt_pck={}\n", cfg_bool(encrypt_pck)));
        text.push_str(&format!(
            "encrypt_directory={}\n\n",
            cfg_bool(encrypt_directory)
        ));
        text.push_str(&format!("[preset.{index}.options]\n"));
        text.push_str("dotnet/include_debug_symbols=false\n");
        for (key, value) in
            export_options_for_platform(project, platform_name, &display_name, template_override)?
        {
            text.push_str(&format!("{key}={value}\n"));
        }
        text.push('\n');
        presets.push(ExportPreset {
            index,
            name: preset_name.to_string(),
            platform: preset_name.to_string(),
            export_path,
            kind: ExportPresetKind::App,
            credential_key: app_preset_encryption_key(project)?,
        });
    }
    for platform_name in platforms.iter() {
        for (pack_name, pack) in sorted_packs(project) {
            if !pack_needs_separate_pck(platform_name, pack) {
                continue;
            }
            let index = presets.len();
            let preset_name = pack_preset_name(platform_name, pack_name)?;
            let platform_preset_name = preset_platform_name(platform_name)?;
            let export_path = pack_export_path(project, platform_name, pack_name, pack)?;
            let export_files = pack_export_files(cwd, pack_name, pack)?;
            let (encrypt_pck, encrypt_directory, credential_key) =
                pack_encryption_settings(project, pack)?;
            text.push_str(&format!("[preset.{index}]\n"));
            text.push_str(&format!("name={}\n", cfg_string(&preset_name)));
            text.push_str(&format!("platform={}\n", cfg_string(platform_preset_name)));
            text.push_str("runnable=false\n");
            text.push_str("dedicated_server=false\n");
            text.push_str("custom_features=\"\"\n");
            text.push_str("export_filter=\"selected_resources\"\n");
            text.push_str(&format!(
                "export_files={}\n",
                cfg_packed_string_array(&export_files)
            ));
            text.push_str("include_filter=\"\"\n");
            text.push_str("exclude_filter=\"\"\n");
            text.push_str(&format!("export_path={}\n", cfg_path(&export_path)));
            text.push_str("patches=PackedStringArray()\n");
            text.push_str(&format!(
                "encryption_include_filters={}\n",
                cfg_string(include_filters)
            ));
            text.push_str(&format!(
                "encryption_exclude_filters={}\n",
                cfg_string(exclude_filters)
            ));
            text.push_str(&format!("encrypt_pck={}\n", cfg_bool(encrypt_pck)));
            text.push_str(&format!(
                "encrypt_directory={}\n\n",
                cfg_bool(encrypt_directory)
            ));
            text.push_str(&format!("[preset.{index}.options]\n"));
            text.push_str("dotnet/include_debug_symbols=false\n");
            for (key, value) in export_options_for_platform(
                project,
                platform_name,
                &display_name,
                template_override,
            )? {
                text.push_str(&format!("{key}={value}\n"));
            }
            text.push('\n');
            presets.push(ExportPreset {
                index,
                name: preset_name,
                platform: platform_preset_name.to_string(),
                export_path,
                kind: ExportPresetKind::Pack {
                    pack_name: pack_name.clone(),
                    pack_kind: pack.kind,
                    encrypt_type: pack.encrypt_type,
                },
                credential_key,
            });
        }
    }
    Ok((text, presets))
}

fn sorted_packs(project: &ProjectConfig) -> Vec<(&String, &ProjectPackConfig)> {
    project.packs.iter().collect()
}

fn validate_platform_pack_config(project: &ProjectConfig, platform_name: &str) -> Result<()> {
    if platform_name != "android" {
        return Ok(());
    }
    for (name, pack) in &project.packs {
        if pack.kind == ProjectPackKind::Internal
            && pack.encrypt_type != ProjectPackEncryptType::Project
        {
            bail!(
                "android internal pack {name} is merged into assets.pck and must use encrypt_type=project"
            );
        }
    }
    Ok(())
}

fn pack_needs_separate_pck(platform_name: &str, pack: &ProjectPackConfig) -> bool {
    match pack.kind {
        ProjectPackKind::Downloadable => true,
        ProjectPackKind::Internal => platform_name != "android",
    }
}

fn main_pack_exclude_filter(project: &ProjectConfig, platform_name: &str) -> Result<String> {
    let mut filters = Vec::new();
    for (name, pack) in &project.packs {
        let should_exclude = match pack.kind {
            ProjectPackKind::Downloadable => true,
            ProjectPackKind::Internal => platform_name != "android",
        };
        if should_exclude {
            let path = normalize_pack_path(&pack.path)
                .with_context(|| format!("invalid path for pack {name}"))?;
            filters.push(format!("{path}/*"));
            filters.push(format!("{path}/**"));
        }
    }
    Ok(filters.join(","))
}

fn pack_preset_name(platform_name: &str, pack_name: &str) -> Result<String> {
    Ok(format!(
        "Pug Pack {} {}",
        preset_platform_name(platform_name)?,
        pack_name
    ))
}

fn pack_export_path(
    project: &ProjectConfig,
    platform_name: &str,
    pack_name: &str,
    pack: &ProjectPackConfig,
) -> Result<PathBuf> {
    let display_name = export_display_name(project);
    let base_dir = export_config(project)
        .and_then(|export| export.output_dir.clone())
        .unwrap_or_else(|| {
            PathBuf::from("../build").join(format!("{}_export", safe_file_stem(&display_name)))
        });
    let platform_dir = preset_platform_name(platform_name)?.replace(' ', "_");
    let file_name = format!("{}.pck", safe_file_stem(pack_name));
    let path = match pack.kind {
        ProjectPackKind::Downloadable => base_dir
            .join("Downloadable")
            .join(platform_dir)
            .join(file_name),
        ProjectPackKind::Internal => {
            export_path_for_platform(project, platform_name, &display_name)?
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or(base_dir.join(platform_dir))
                .join(file_name)
        }
    };
    Ok(path)
}

fn pack_export_files(cwd: &Path, pack_name: &str, pack: &ProjectPackConfig) -> Result<Vec<String>> {
    let relative = normalize_pack_path(&pack.path)
        .with_context(|| format!("invalid path for pack {pack_name}"))?;
    let root = cwd.join(&relative);
    if !root.is_dir() {
        bail!(
            "pack {pack_name} path is not a directory: {}",
            root.display()
        );
    }
    let mut files = Vec::new();
    for entry in WalkDir::new(&root)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().strip_prefix(cwd).with_context(|| {
            format!(
                "pack {pack_name} file is outside project root: {}",
                entry.path().display()
            )
        })?;
        let path = path
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        files.push(format!("res://{path}"));
    }
    files.sort();
    if files.is_empty() {
        bail!("pack {pack_name} does not contain files");
    }
    Ok(files)
}

fn app_preset_encryption_key(project: &ProjectConfig) -> Result<Option<String>> {
    if export_encryption_enabled(project) {
        return export_encryption_key(project).with_context(|| {
            "project.pug.json export enables encryption; set export.script_encryption_key or SCRIPT_AES256_ENCRYPTION_KEY"
        }).map(Some);
    }
    Ok(None)
}

fn pack_encryption_settings(
    project: &ProjectConfig,
    pack: &ProjectPackConfig,
) -> Result<(bool, bool, Option<String>)> {
    match pack.encrypt_type {
        ProjectPackEncryptType::None => Ok((false, false, None)),
        ProjectPackEncryptType::Project => {
            if !export_encryption_enabled(project) {
                return Ok((false, false, None));
            }
            let key = export_encryption_key(project).with_context(|| {
                "pack uses encrypt_type=project but project export encryption key is not configured"
            })?;
            Ok((
                export_encrypt_pck(project),
                export_encrypt_directory(project),
                Some(key),
            ))
        }
        ProjectPackEncryptType::Random => Ok((true, true, Some(generate_pack_encryption_key()))),
    }
}

fn generate_pack_encryption_key() -> String {
    let mut rng = rand::rng();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn export_options_for_platform(
    project: &ProjectConfig,
    platform_name: &str,
    display_name: &str,
    template_override: Option<&ExportTemplateOverride<'_>>,
) -> Result<Vec<(String, String)>> {
    match platform_name {
        "windows" | "linux" | "macos" | "ios" => {
            desktop_export_options(project, platform_name, display_name, template_override)
        }
        "android" => android_export_options(project, display_name, template_override),
        other => bail!("unsupported export platform: {other}"),
    }
}

fn desktop_export_options(
    project: &ProjectConfig,
    platform_name: &str,
    display_name: &str,
    template_override: Option<&ExportTemplateOverride<'_>>,
) -> Result<Vec<(String, String)>> {
    let config = platform_export_config(project, platform_name);
    let mut options = vec![
        (
            "custom_template/debug".to_string(),
            custom_template_value(template_override, platform_name, ExportMode::Debug),
        ),
        (
            "custom_template/release".to_string(),
            custom_template_value(template_override, platform_name, ExportMode::Release),
        ),
    ];
    match platform_name {
        "windows" => options.push((
            "binary_format/embed_pck".to_string(),
            cfg_bool(config.and_then(|config| config.embed_pck).unwrap_or(true)).to_string(),
        )),
        "macos" => {
            let architecture = config
                .and_then(|config| config.architecture.as_deref())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    export_arch_for_platform(project, "macos")
                        .unwrap_or_else(|_| "arm64".to_string())
                });
            options.push((
                "binary_format/architecture".to_string(),
                cfg_string(&architecture),
            ));
            options.push((
                "application/bundle_identifier".to_string(),
                cfg_string(&bundle_identifier(project, platform_name, display_name)),
            ));
        }
        "ios" => {
            options.push((
                "application/bundle_identifier".to_string(),
                cfg_string(&bundle_identifier(project, platform_name, display_name)),
            ));
        }
        "linux" => {}
        other => bail!("unsupported desktop export platform: {other}"),
    }
    Ok(options)
}

fn android_export_options(
    project: &ProjectConfig,
    display_name: &str,
    template_override: Option<&ExportTemplateOverride<'_>>,
) -> Result<Vec<(String, String)>> {
    let config = android_export_config(project);
    let package_name = config
        .and_then(|config| config.package.as_deref())
        .map(str::to_string)
        .unwrap_or_else(|| default_package_name(display_name));
    let android_name = config
        .and_then(|config| config.name.as_deref())
        .unwrap_or(display_name);
    let signed = config.and_then(|config| config.signed).unwrap_or(false);
    let enabled_arches = android_enabled_arches(project)?;
    let debug_keystore = config.and_then(|config| config.debug_keystore.as_ref());
    let release_keystore = config.and_then(|config| config.release_keystore.as_ref());

    Ok(vec![
        ("custom_template/debug".to_string(), cfg_string("")),
        ("custom_template/release".to_string(), cfg_string("")),
        (
            "gradle_build/use_gradle_build".to_string(),
            "true".to_string(),
        ),
        (
            "gradle_build/gradle_build_directory".to_string(),
            cfg_string(
                config
                    .and_then(|config| config.gradle_build_directory.as_deref())
                    .unwrap_or("res://android"),
            ),
        ),
        (
            "gradle_build/android_source_template".to_string(),
            android_source_template_value(template_override),
        ),
        (
            "gradle_build/compress_native_libraries".to_string(),
            "false".to_string(),
        ),
        ("gradle_build/export_format".to_string(), "0".to_string()),
        (
            "gradle_build/min_sdk".to_string(),
            cfg_string(
                config
                    .and_then(|config| config.min_sdk.as_deref())
                    .unwrap_or(""),
            ),
        ),
        (
            "gradle_build/target_sdk".to_string(),
            cfg_string(
                config
                    .and_then(|config| config.target_sdk.as_deref())
                    .unwrap_or(""),
            ),
        ),
        (
            "gradle_build/custom_theme_attributes".to_string(),
            "{}".to_string(),
        ),
        (
            "architectures/armeabi-v7a".to_string(),
            cfg_bool(enabled_arches.contains("armeabi-v7a")).to_string(),
        ),
        (
            "architectures/arm64-v8a".to_string(),
            cfg_bool(enabled_arches.contains("arm64-v8a")).to_string(),
        ),
        (
            "architectures/x86".to_string(),
            cfg_bool(enabled_arches.contains("x86")).to_string(),
        ),
        (
            "architectures/x86_64".to_string(),
            cfg_bool(enabled_arches.contains("x86_64")).to_string(),
        ),
        (
            "keystore/debug".to_string(),
            cfg_string(if signed {
                keystore_path_value(debug_keystore, template_override, ExportMode::Debug)
            } else {
                String::new()
            }),
        ),
        (
            "keystore/debug_user".to_string(),
            cfg_string(if signed {
                keystore_alias_value(debug_keystore, template_override, ExportMode::Debug)
            } else {
                String::new()
            }),
        ),
        (
            "keystore/debug_password".to_string(),
            cfg_string(if signed {
                keystore_password_value(debug_keystore, template_override, ExportMode::Debug)
            } else {
                String::new()
            }),
        ),
        (
            "keystore/release".to_string(),
            cfg_string(if signed {
                keystore_path_value(release_keystore, template_override, ExportMode::Release)
            } else {
                String::new()
            }),
        ),
        (
            "keystore/release_user".to_string(),
            cfg_string(if signed {
                keystore_alias_value(release_keystore, template_override, ExportMode::Release)
            } else {
                String::new()
            }),
        ),
        (
            "keystore/release_password".to_string(),
            cfg_string(if signed {
                keystore_password_value(release_keystore, template_override, ExportMode::Release)
            } else {
                String::new()
            }),
        ),
        (
            "version/code".to_string(),
            config
                .and_then(|config| config.version_code)
                .unwrap_or(1)
                .to_string(),
        ),
        (
            "version/name".to_string(),
            cfg_string(
                config
                    .and_then(|config| config.version_name.as_deref())
                    .unwrap_or(project.version.as_str()),
            ),
        ),
        ("package/unique_name".to_string(), cfg_string(&package_name)),
        ("package/name".to_string(), cfg_string(android_name)),
        ("package/signed".to_string(), cfg_bool(signed).to_string()),
        ("package/app_category".to_string(), "2".to_string()),
        (
            "package/retain_data_on_uninstall".to_string(),
            "false".to_string(),
        ),
        (
            "package/exclude_from_recents".to_string(),
            "false".to_string(),
        ),
        (
            "package/show_in_android_tv".to_string(),
            "false".to_string(),
        ),
        (
            "package/show_in_app_library".to_string(),
            "true".to_string(),
        ),
        (
            "package/show_as_launcher_app".to_string(),
            "false".to_string(),
        ),
        ("launcher_icons/main_192x192".to_string(), cfg_string("")),
        (
            "launcher_icons/adaptive_foreground_432x432".to_string(),
            cfg_string(""),
        ),
        (
            "launcher_icons/adaptive_background_432x432".to_string(),
            cfg_string(""),
        ),
        (
            "launcher_icons/adaptive_monochrome_432x432".to_string(),
            cfg_string(""),
        ),
        ("graphics/opengl_debug".to_string(), "false".to_string()),
        ("shader_baker/enabled".to_string(), "false".to_string()),
        ("xr_features/xr_mode".to_string(), "0".to_string()),
        ("gesture/swipe_to_dismiss".to_string(), "false".to_string()),
        ("screen/immersive_mode".to_string(), "true".to_string()),
        ("screen/edge_to_edge".to_string(), "false".to_string()),
        ("screen/support_small".to_string(), "true".to_string()),
        ("screen/support_normal".to_string(), "true".to_string()),
        ("screen/support_large".to_string(), "true".to_string()),
        ("screen/support_xlarge".to_string(), "true".to_string()),
        (
            "screen/background_color".to_string(),
            "Color(0, 0, 0, 1)".to_string(),
        ),
        ("user_data_backup/allow".to_string(), "false".to_string()),
        ("command_line/extra_args".to_string(), cfg_string("")),
        ("apk_expansion/enable".to_string(), "false".to_string()),
        ("apk_expansion/SALT".to_string(), cfg_string("")),
        ("apk_expansion/public_key".to_string(), cfg_string("")),
        (
            "permissions/custom_permissions".to_string(),
            "PackedStringArray()".to_string(),
        ),
        (
            "permissions/internet".to_string(),
            cfg_bool(
                config
                    .and_then(|config| config.internet_permission)
                    .unwrap_or(true),
            )
            .to_string(),
        ),
    ])
}

fn custom_template_value(
    template_override: Option<&ExportTemplateOverride<'_>>,
    platform_name: &str,
    mode: ExportMode,
) -> String {
    template_override
        .filter(|template_override| {
            template_override.platform == platform_name && template_override.mode == mode
        })
        .and_then(|template_override| template_override.templates.custom_template.as_ref())
        .map(|path| cfg_path(path))
        .unwrap_or_else(|| cfg_string(""))
}

fn android_source_template_value(template_override: Option<&ExportTemplateOverride<'_>>) -> String {
    template_override
        .filter(|template_override| template_override.platform == "android")
        .and_then(|template_override| template_override.templates.android_source_template.as_ref())
        .map(|path| cfg_path(path))
        .unwrap_or_else(|| cfg_string(""))
}

fn export_platforms(project: &ProjectConfig) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for platform_name in project.platforms.names() {
        preset_platform_name(&platform_name)?;
        if seen.insert(platform_name.clone()) {
            out.push(platform_name);
        }
    }
    if out.is_empty() {
        bail!("project.pug.json platforms must list at least one export platform");
    }
    Ok(out)
}

fn export_display_name(project: &ProjectConfig) -> String {
    export_config(project)
        .and_then(|export| export.name.as_deref())
        .filter(|name| !name.trim().is_empty())
        .or_else(|| (!project.name.trim().is_empty()).then_some(project.name.as_str()))
        .unwrap_or("Game")
        .trim()
        .to_string()
}

fn export_config(project: &ProjectConfig) -> Option<&ProjectExportConfig> {
    project.export.as_ref()
}

fn platform_export_config<'a>(
    project: &'a ProjectConfig,
    platform_name: &str,
) -> Option<&'a ProjectPlatformConfig> {
    project.platforms.get(platform_name).or_else(|| {
        let export = export_config(project)?;
        match platform_name {
            "windows" => export.windows.as_ref(),
            "macos" => export.macos.as_ref(),
            "linux" => export.linux.as_ref(),
            "ios" => export.ios.as_ref(),
            _ => None,
        }
    })
}

fn android_export_config(project: &ProjectConfig) -> Option<&ProjectPlatformConfig> {
    project
        .platforms
        .get("android")
        .or_else(|| export_config(project).and_then(|export| export.android.as_ref()))
}

fn export_path_for_platform(
    project: &ProjectConfig,
    platform_name: &str,
    display_name: &str,
) -> Result<PathBuf> {
    if let Some(path) = match platform_name {
        "android" => android_export_config(project).and_then(|config| config.export_path.clone()),
        _ => platform_export_config(project, platform_name)
            .and_then(|config| config.export_path.clone()),
    } {
        return Ok(path);
    }

    let output_dir = export_config(project)
        .and_then(|export| export.output_dir.clone())
        .unwrap_or_else(|| {
            PathBuf::from("../build").join(format!("{}_export", safe_file_stem(display_name)))
        });
    Ok(output_dir
        .join(default_platform_output_dir(platform_name)?)
        .join(default_export_filename(
            project,
            platform_name,
            display_name,
        )?))
}

fn default_platform_output_dir(platform_name: &str) -> Result<&'static str> {
    Ok(match platform_name {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        "android" => "Android",
        "ios" => "iOS",
        other => bail!("unsupported export platform: {other}"),
    })
}

fn default_export_filename(
    project: &ProjectConfig,
    platform_name: &str,
    display_name: &str,
) -> Result<String> {
    let stem = safe_file_stem(display_name);
    Ok(match platform_name {
        "windows" => format!("{stem}.exe"),
        "macos" => format!("{stem}.app"),
        "linux" => format!("{stem}.{}", export_arch_for_platform(project, "linux")?),
        "android" => format!("{stem}.apk"),
        "ios" => format!("{stem}.ipa"),
        other => bail!("unsupported export platform: {other}"),
    })
}

fn preset_runnable(platform_name: &str) -> bool {
    matches!(platform_name, "windows" | "android")
}

fn export_encrypt_pck(project: &ProjectConfig) -> bool {
    export_config(project)
        .map(|export| {
            export
                .encrypt_pck
                .or(export.encrypt)
                .unwrap_or_else(|| export.script_encryption_key.is_some())
        })
        .unwrap_or(false)
}

fn export_encrypt_directory(project: &ProjectConfig) -> bool {
    export_config(project)
        .map(|export| {
            export
                .encrypt_directory
                .or(export.encrypt)
                .unwrap_or_else(|| export.script_encryption_key.is_some())
        })
        .unwrap_or(false)
}

fn export_encryption_enabled(project: &ProjectConfig) -> bool {
    export_encrypt_pck(project) || export_encrypt_directory(project)
}

fn bundle_identifier(project: &ProjectConfig, platform_name: &str, display_name: &str) -> String {
    platform_export_config(project, platform_name)
        .and_then(|config| config.bundle_identifier.as_deref())
        .map(str::to_string)
        .or_else(|| {
            android_export_config(project)
                .and_then(|config| config.package.as_deref())
                .map(str::to_string)
        })
        .unwrap_or_else(|| default_package_name(display_name))
}

fn android_enabled_arches(project: &ProjectConfig) -> Result<BTreeSet<String>> {
    let arches = android_export_config(project)
        .and_then(|config| config.architectures.clone())
        .unwrap_or_else(|| {
            platform::default_arches("android")
                .unwrap_or_else(|_| vec!["arm64-v8a"])
                .into_iter()
                .map(str::to_string)
                .collect()
        });
    let mut out = BTreeSet::new();
    for arch in arches {
        out.insert(platform::spec("android", &arch)?.arch);
    }
    Ok(out)
}

fn extension_arches_for_platform(
    project: &ProjectConfig,
    platform_name: &str,
) -> Result<Vec<String>> {
    let mut out = BTreeSet::new();
    if let Some(config) = platform_export_config(project, platform_name) {
        if let Some(architectures) = &config.architectures {
            for arch in architectures {
                out.insert(platform::spec(platform_name, arch)?.arch);
            }
        } else if let Some(arch) = config.architecture.as_deref() {
            out.insert(platform::spec(platform_name, arch)?.arch);
        }
    }
    if out.is_empty() {
        for arch in platform::default_arches(platform_name)? {
            out.insert(platform::spec(platform_name, arch)?.arch);
        }
    }
    Ok(out.into_iter().collect())
}

fn keystore_path(config: Option<&AndroidKeystoreConfig>) -> &str {
    config
        .and_then(|config| config.path.as_deref())
        .unwrap_or("")
}

fn optional_keystore_alias(config: Option<&AndroidKeystoreConfig>) -> Option<String> {
    config
        .and_then(|config| config.alias.as_deref())
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
        .map(str::to_string)
}

fn optional_keystore_password(config: Option<&AndroidKeystoreConfig>) -> Option<String> {
    config
        .and_then(|config| {
            config.password.as_deref().map(str::to_string).or_else(|| {
                config
                    .password_env
                    .as_deref()
                    .and_then(|name| env::var(name).ok())
            })
        })
        .map(|password| password.trim().to_string())
        .filter(|password| !password.is_empty())
}

fn generated_android_keystore<'a>(
    template_override: Option<&'a ExportTemplateOverride<'a>>,
    mode: ExportMode,
) -> Option<&'a GeneratedAndroidKeystore> {
    template_override
        .and_then(|template_override| template_override.android_keystore)
        .filter(|keystore| keystore.mode == mode)
}

fn keystore_path_value(
    config: Option<&AndroidKeystoreConfig>,
    template_override: Option<&ExportTemplateOverride<'_>>,
    mode: ExportMode,
) -> String {
    let path = keystore_path(config).trim();
    if !path.is_empty() {
        return path.to_string();
    }
    generated_android_keystore(template_override, mode)
        .map(|keystore| godot_path(&keystore.path))
        .unwrap_or_default()
}

fn keystore_alias_value(
    config: Option<&AndroidKeystoreConfig>,
    template_override: Option<&ExportTemplateOverride<'_>>,
    mode: ExportMode,
) -> String {
    optional_keystore_alias(config)
        .or_else(|| {
            generated_android_keystore(template_override, mode)
                .map(|keystore| keystore.alias.clone())
        })
        .unwrap_or_default()
}

fn keystore_password_value(
    config: Option<&AndroidKeystoreConfig>,
    template_override: Option<&ExportTemplateOverride<'_>>,
    mode: ExportMode,
) -> String {
    optional_keystore_password(config)
        .or_else(|| {
            generated_android_keystore(template_override, mode)
                .map(|keystore| keystore.password.clone())
        })
        .unwrap_or_default()
}

fn default_package_name(display_name: &str) -> String {
    format!("com.example.{}", package_component(display_name))
}

fn package_component(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '_' {
            out.push(ch);
        }
    }
    if out.is_empty() {
        return "game".to_string();
    }
    if out.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        out.insert_str(0, "app");
    }
    out
}

fn safe_file_stem(value: &str) -> String {
    let stem = value
        .trim()
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' => '_',
            _ => ch,
        })
        .collect::<String>();
    if stem.is_empty() {
        "Game".to_string()
    } else {
        stem
    }
}

fn normalize_pack_name(value: &str) -> Result<String> {
    let name = value.trim();
    if name.is_empty() {
        bail!("pack name is required");
    }
    if name.contains('/') || name.contains('\\') {
        bail!("pack name must not contain path separators");
    }
    Ok(name.to_string())
}

fn normalize_pack_path(path: &Path) -> Result<String> {
    if path.is_absolute() {
        bail!("pack path must be relative to the Godot project root");
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => {
                let part = part
                    .to_str()
                    .with_context(|| format!("pack path is not valid UTF-8: {}", path.display()))?;
                if part.is_empty() {
                    continue;
                }
                parts.push(part.to_string());
            }
            std::path::Component::CurDir => {}
            _ => bail!("pack path must stay inside the Godot project root"),
        }
    }
    if parts.is_empty() {
        bail!("pack path is required");
    }
    Ok(parts.join("/"))
}

fn parse_pack_encrypt_type(value: &str) -> Result<ProjectPackEncryptType> {
    match value.trim() {
        "project" => Ok(ProjectPackEncryptType::Project),
        "none" => Ok(ProjectPackEncryptType::None),
        "random" => Ok(ProjectPackEncryptType::Random),
        other => bail!("unsupported pack encrypt_type: {other}"),
    }
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

fn resolve_export_platforms(opts: &ProjectExportOptions) -> Result<Vec<String>> {
    let explicit_count =
        usize::from(opts.platform.is_some()) + usize::from(opts.android) + usize::from(opts.ios);
    if explicit_count > 1 {
        bail!("choose only one export target source");
    }
    if opts.android {
        return Ok(vec!["android".to_string()]);
    }
    if opts.ios {
        return Ok(vec!["ios".to_string()]);
    }
    let raw = opts
        .platform
        .as_deref()
        .map(platform::parse_platform_list)
        .unwrap_or_else(|| vec![platform::host_platform().unwrap_or("windows").to_string()]);
    let mut seen = BTreeSet::new();
    let mut platforms = Vec::new();
    for platform_name in raw {
        let platform_name = platform::normalize_platform(&platform_name);
        preset_platform_name(&platform_name)?;
        if seen.insert(platform_name.clone()) {
            platforms.push(platform_name);
        }
    }
    if platforms.is_empty() {
        bail!("choose at least one export platform");
    }
    Ok(platforms)
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

fn read_metadata(dir: &Path) -> Result<PackageMetadata> {
    Ok(serde_json::from_slice(&fs::read(
        dir.join("metadata.json"),
    )?)?)
}

fn resolve_export_templates(
    cwd: &Path,
    project: &ProjectConfig,
    platform_name: &str,
    mode: ExportMode,
    prefer_build_output: bool,
) -> Result<ExportTemplates> {
    if prefer_build_output {
        match local_export_templates(project, platform_name, mode) {
            Ok(templates) => return Ok(templates),
            Err(err) => {
                eprintln!(
                    "pug: local export templates unavailable ({err}); trying installed templates"
                );
            }
        }
    }

    if let Some(templates) = installed_export_templates(cwd, project, platform_name, mode)? {
        return Ok(templates);
    }

    if is_local_engine_tag(&project.engine.tag) {
        bail!(
            "local engine tag {} has no installed export template for {} {}; rebuild with `pug engine build --install --template-platform {}`",
            project.engine.tag,
            platform_name,
            mode.template_kind(),
            platform_name
        );
    }

    download_export_templates(cwd, project, platform_name, mode)
}

fn installed_export_templates(
    cwd: &Path,
    project: &ProjectConfig,
    platform_name: &str,
    mode: ExportMode,
) -> Result<Option<ExportTemplates>> {
    let cfg = Config::load()?;
    let install_root = cfg.install_dir()?.join(&project.engine.tag);
    let cache_root = cwd
        .join(".godot")
        .join("pug")
        .join("export_templates")
        .join(&project.engine.tag);

    if matches!(platform_name, "windows" | "macos" | "linux") {
        let arch = export_arch_for_platform(project, platform_name)?;
        let Some(dir) = unpack_installed_template_artifact(
            &install_root,
            &cache_root,
            platform_name,
            mode.template_kind(),
            &arch,
        )?
        else {
            return Ok(None);
        };
        let template = find_template_file(&dir, mode.template_kind())?;
        return Ok(Some(ExportTemplates {
            custom_template: Some(template),
            android_source_template: None,
        }));
    }

    let Some(dir) = unpack_installed_template_artifact(
        &install_root,
        &cache_root,
        platform_name,
        "template_bundle",
        "bundle",
    )?
    else {
        return Ok(None);
    };
    match platform_name {
        "android" => Ok(Some(ExportTemplates {
            custom_template: None,
            android_source_template: Some(find_named_file(&dir, "android_source.zip")?),
        })),
        "ios" => Ok(Some(ExportTemplates {
            custom_template: Some(find_named_file(&dir, "godot_ios.zip")?),
            android_source_template: None,
        })),
        other => bail!("unsupported export platform: {other}"),
    }
}

fn unpack_installed_template_artifact(
    install_root: &Path,
    cache_root: &Path,
    platform_name: &str,
    kind: &str,
    arch: &str,
) -> Result<Option<PathBuf>> {
    let zip = installed_template_artifact_zip(install_root, platform_name, kind, arch);
    if !zip.is_file() {
        return Ok(None);
    }
    let unpack = cache_root
        .join(platform_name)
        .join(kind)
        .join(arch)
        .join("unpack");
    util::ensure_clean_dir(&unpack)?;
    util::unzip_to(&zip, &unpack)?;
    eprintln!(
        "pug: using installed {} {} template {}",
        platform_name,
        kind,
        zip.display()
    );
    Ok(Some(unpack))
}

fn installed_template_artifact_zip(
    install_root: &Path,
    platform_name: &str,
    kind: &str,
    arch: &str,
) -> PathBuf {
    install_root
        .join("export_templates")
        .join(platform_name)
        .join(kind)
        .join(arch)
        .join("artifact.zip")
}

fn is_local_engine_tag(tag: &str) -> bool {
    tag.starts_with("local-")
}

fn local_export_templates(
    project: &ProjectConfig,
    platform_name: &str,
    mode: ExportMode,
) -> Result<ExportTemplates> {
    let repo_root = engine::find_repo_root()?;
    match platform_name {
        "android" => {
            let template = repo_root
                .join("build")
                .join("android")
                .join("export_templates")
                .join("android_source.zip");
            if !template.is_file() {
                bail!(
                    "local Android source template not found at {}",
                    template.display()
                );
            }
            eprintln!(
                "pug: using local Android source template {}",
                template.display()
            );
            Ok(ExportTemplates {
                custom_template: None,
                android_source_template: Some(template),
            })
        }
        "ios" => {
            let template = repo_root
                .join("build")
                .join("ios")
                .join("export_templates")
                .join("godot_ios.zip");
            if !template.is_file() {
                bail!("local iOS template not found at {}", template.display());
            }
            eprintln!("pug: using local iOS template {}", template.display());
            Ok(ExportTemplates {
                custom_template: Some(template),
                android_source_template: None,
            })
        }
        "macos" | "windows" | "linux" => {
            let arch = export_arch_for_platform(project, platform_name)?;
            let target = platform::spec(platform_name, &arch)?;
            let dir = repo_root
                .join("build")
                .join(target.godot_platform)
                .join("export_templates");
            let template = find_template_file(&dir, mode.template_kind())?;
            eprintln!(
                "pug: using local {} template {}",
                platform_name,
                template.display()
            );
            Ok(ExportTemplates {
                custom_template: Some(template),
                android_source_template: None,
            })
        }
        other => bail!("unsupported export platform: {other}"),
    }
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
        let arch = export_arch_for_platform(project, platform_name)?;
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

fn export_arch_for_platform(project: &ProjectConfig, platform_name: &str) -> Result<String> {
    if let Some(arch) = platform_export_config(project, platform_name)
        .and_then(|config| config.architecture.as_deref())
        .filter(|arch| !arch.trim().is_empty())
    {
        return Ok(platform::spec(platform_name, arch)?.arch);
    }
    default_export_arch(platform_name)
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

#[cfg(test)]
fn rewrite_manifest(
    cwd: &Path,
    name: &str,
    project: &ProjectConfig,
    template_text: &str,
) -> Result<()> {
    let platforms = export_platforms(project)?;
    rewrite_manifest_for_platforms(cwd, name, project, template_text, &platforms)
}

fn rewrite_manifest_for_platforms(
    cwd: &Path,
    name: &str,
    project: &ProjectConfig,
    template_text: &str,
    platforms: &[String],
) -> Result<()> {
    let mut text = strip_libraries(template_text);
    text.push_str("\n[libraries]\n");
    for platform_name in platforms {
        for arch in extension_arches_for_platform(project, platform_name)? {
            let target = platform::spec(platform_name, &arch)?;
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

fn cfg_path(path: &Path) -> String {
    cfg_string(godot_path(path))
}

fn cfg_string(value: impl AsRef<str>) -> String {
    format!(
        "\"{}\"",
        value.as_ref().replace('\\', "\\\\").replace('"', "\\\"")
    )
}

fn cfg_packed_string_array(values: &[String]) -> String {
    let items = values.iter().map(cfg_string).collect::<Vec<_>>().join(", ");
    format!("PackedStringArray({items})")
}

fn cfg_bool(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn godot_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\\\"")
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

fn write_export_credentials(cwd: &Path, presets: &[ExportPreset]) -> Result<()> {
    let path = cwd.join(".godot").join("export_credentials.cfg");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for preset in presets {
        let Some(key) = preset.credential_key.as_deref() else {
            continue;
        };
        text.push_str(&format!(
            "[preset.{}]\nscript_encryption_key=\"{key}\"\n\n",
            preset.index
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
    remote_sign: &RemoteSignEnv,
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
    let integrity_status_path = cwd
        .join(".godot")
        .join("pug")
        .join("integrity_export_status.json");
    if let Some(parent) = integrity_status_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if integrity_status_path.exists() {
        fs::remove_file(&integrity_status_path).with_context(|| {
            format!(
                "remove stale integrity status file {}",
                integrity_status_path.display()
            )
        })?;
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
    if remote_sign.no_remote_sign {
        cmd.env("GODOT_CUSTOM_INTEGRITY_NO_REMOTE_SIGN", "1");
    } else {
        cmd.env("GODOT_CUSTOM_INTEGRITY_STATUS_PATH", &integrity_status_path);
        if let Some(base_url) = &remote_sign.base_url {
            cmd.env("PANNEL_BASE_URL", base_url);
        }
        if let Some(project_name) = &remote_sign.project_name {
            cmd.env("GODOT_CUSTOM_INTEGRITY_PROJECT_NAME", project_name);
        }
        if let Some(editor_token) = &remote_sign.editor_token {
            cmd.env("GODOT_CUSTOM_INTEGRITY_EDITOR_TOKEN", editor_token);
        }
    }
    util::run_command(&mut cmd)?;
    if !remote_sign.no_remote_sign {
        validate_remote_sign_status(&integrity_status_path)?;
    }
    println!("exported {} -> {}", preset.name, export_path.display());
    Ok(())
}

fn export_pack_presets(
    editor: &Path,
    cwd: &Path,
    project: &ProjectConfig,
    presets: &[ExportPreset],
    platform_name: &str,
) -> Result<()> {
    for preset in presets
        .iter()
        .filter(|preset| matches!(preset.kind, ExportPresetKind::Pack { .. }))
        .filter(|preset| preset.platform == preset_platform_name(platform_name).unwrap_or(""))
    {
        run_godot_pack_export(editor, cwd, project, preset)?;
        if let Some(key) = random_pack_key(preset) {
            let key_path = resolve_export_path(cwd, &preset.export_path).with_extension("pck.key");
            fs::write(&key_path, format!("{key}\n"))
                .with_context(|| format!("write {}", key_path.display()))?;
            println!("wrote pack key {}", key_path.display());
        }
    }
    Ok(())
}

fn random_pack_key(preset: &ExportPreset) -> Option<&str> {
    match &preset.kind {
        ExportPresetKind::Pack {
            encrypt_type: ProjectPackEncryptType::Random,
            ..
        } => preset.credential_key.as_deref(),
        _ => None,
    }
}

fn run_godot_pack_export(
    editor: &Path,
    cwd: &Path,
    project: &ProjectConfig,
    preset: &ExportPreset,
) -> Result<()> {
    let ExportPresetKind::Pack { pack_name, .. } = &preset.kind else {
        bail!("preset {} is not a pack preset", preset.name);
    };
    let pack = project
        .packs
        .get(pack_name)
        .with_context(|| format!("pack {pack_name} not found in project.pug.json"))?;
    let export_path = resolve_export_path(cwd, &preset.export_path);
    if let Some(parent) = export_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let files = pack_export_file_entries(cwd, pack_name, pack)?;
    let key = preset
        .credential_key
        .as_deref()
        .unwrap_or("0000000000000000000000000000000000000000000000000000000000000000");
    let encrypt_files = preset.credential_key.is_some();
    let encrypt_directory = match &preset.kind {
        ExportPresetKind::Pack {
            encrypt_type: ProjectPackEncryptType::None,
            ..
        } => false,
        ExportPresetKind::Pack {
            encrypt_type: ProjectPackEncryptType::Random,
            ..
        } => true,
        ExportPresetKind::Pack {
            encrypt_type: ProjectPackEncryptType::Project,
            ..
        } => export_encrypt_directory(project),
        ExportPresetKind::App => false,
    };
    let temp_dir = tempfile::Builder::new()
        .prefix("pug-pack-export-")
        .tempdir_in(cwd.join(".godot").join("pug"))
        .context("create temporary pack export directory")?;
    let script_path = temp_dir.path().join("export_pack.gd");
    let manifest_path = temp_dir.path().join("manifest.json");
    fs::write(&script_path, pack_exporter_script())
        .with_context(|| format!("write {}", script_path.display()))?;
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "output": export_path.to_string_lossy(),
            "key": key,
            "encrypt_files": encrypt_files,
            "encrypt_directory": encrypt_directory,
            "files": files,
        }))?,
    )
    .with_context(|| format!("write {}", manifest_path.display()))?;
    let mut cmd = Command::new(editor);
    cmd.args(["--headless", "--path"])
        .arg(cwd)
        .arg("--script")
        .arg(&script_path)
        .env("PUG_PACK_MANIFEST", &manifest_path);
    util::run_command(&mut cmd)?;
    println!("exported pack {} -> {}", preset.name, export_path.display());
    Ok(())
}

fn pack_export_file_entries(
    cwd: &Path,
    pack_name: &str,
    pack: &ProjectPackConfig,
) -> Result<Vec<serde_json::Value>> {
    pack_export_files(cwd, pack_name, pack)?
        .into_iter()
        .map(|target| {
            let relative = target.trim_start_matches("res://");
            Ok(serde_json::json!({
                "target": target,
                "source": cwd.join(relative).to_string_lossy(),
            }))
        })
        .collect()
}

fn pack_exporter_script() -> &'static str {
    r#"extends SceneTree

func _init():
	var manifest_path := OS.get_environment("PUG_PACK_MANIFEST")
	if manifest_path.is_empty():
		_fail("PUG_PACK_MANIFEST is not set")
		return
	var text := FileAccess.get_file_as_string(manifest_path)
	var manifest = JSON.parse_string(text)
	if typeof(manifest) != TYPE_DICTIONARY:
		_fail("invalid pack manifest")
		return
	var packer := PCKPacker.new()
	var err := packer.pck_start(manifest["output"], 32, manifest["key"], manifest["encrypt_directory"])
	if err != OK:
		_fail("pck_start failed: %s" % err)
		return
	for file in manifest["files"]:
		err = packer.add_file(file["target"], file["source"], manifest["encrypt_files"])
		if err != OK:
			_fail("add_file failed for %s: %s" % [file["target"], err])
			return
	err = packer.flush(true)
	if err != OK:
		_fail("flush failed: %s" % err)
		return
	quit(0)

func _fail(message: String):
	push_error(message)
	quit(1)
"#
}

#[derive(Debug, Deserialize)]
struct IntegrityExportStatus {
    status: String,
    #[serde(default)]
    message: String,
}

fn validate_remote_sign_status(path: &Path) -> Result<()> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("read integrity export status {}", path.display()))?;
    let status: IntegrityExportStatus = serde_json::from_str(&text)
        .with_context(|| format!("parse integrity export status {}", path.display()))?;
    match status.status.as_str() {
        "signed" | "not_needed" => Ok(()),
        "bypassed" => bail!(
            "remote integrity signing was bypassed by the editor: {}",
            status.message
        ),
        "failed" => bail!("remote integrity signing failed: {}", status.message),
        other => bail!(
            "remote integrity signing returned unexpected status {other}: {}",
            status.message
        ),
    }
}

fn resolve_export_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn upload_export_artifact(
    api: &ApiClient,
    cwd: &Path,
    project: &ProjectConfig,
    project_name: &str,
    platform_name: &str,
    mode: ExportMode,
    export_path: &Path,
    no_remote_sign: bool,
) -> Result<()> {
    let (package_path, package_type, metadata) =
        package_export_artifact(project, platform_name, mode, export_path, no_remote_sign)?;
    let sha = util::sha256_file(&package_path)?;
    let size = util::file_size(&package_path)?;
    let repo_commit = git_head(cwd).unwrap_or_default();
    let export_path_text = export_path.to_string_lossy().to_string();
    let init = api.export_upload_init(&ExportUploadInit {
        project_name,
        version: &project.version,
        platform: platform_name,
        mode: mode.name(),
        package_type,
        package_sha256: &sha,
        package_size: size,
        engine_tag: &project.engine.tag,
        repo_commit: &repo_commit,
        export_path: &export_path_text,
        metadata,
    })?;
    api.put_file(&init, &package_path)?;
    let complete = api.export_upload_complete(&init.upload_id)?;
    println!(
        "uploaded export {} {} -> status={} key={}",
        platform_name,
        mode.name(),
        complete.status,
        init.s3_key
    );
    Ok(())
}

fn upload_downloadable_packs(
    api: &ApiClient,
    cwd: &Path,
    project: &ProjectConfig,
    project_name: &str,
    platform_name: &str,
    mode: ExportMode,
    presets: &[ExportPreset],
) -> Result<()> {
    for preset in presets
        .iter()
        .filter(|preset| preset.is_pack_for(platform_name, ProjectPackKind::Downloadable))
    {
        let ExportPresetKind::Pack {
            pack_name,
            encrypt_type,
            ..
        } = &preset.kind
        else {
            continue;
        };
        let package_path = resolve_export_path(cwd, &preset.export_path);
        if !package_path.is_file() {
            bail!(
                "downloadable pack artifact not found: {}",
                package_path.display()
            );
        }
        let sha = util::sha256_file(&package_path)?;
        let size = util::file_size(&package_path)?;
        let repo_commit = git_head(cwd).unwrap_or_default();
        let pack_path = project
            .packs
            .get(pack_name)
            .map(|pack| normalize_pack_path(&pack.path))
            .transpose()?
            .unwrap_or_default();
        let metadata = serde_json::json!({
            "export_path": package_path.to_string_lossy(),
            "pack_kind": ProjectPackKind::Downloadable.name(),
        });
        let init = api.downloadable_package_upload_init(&DownloadablePackageUploadInit {
            project_name,
            name: pack_name,
            version: &project.version,
            platform: platform_name,
            mode: mode.name(),
            pack_path: &pack_path,
            package_sha256: &sha,
            package_size: size,
            engine_tag: &project.engine.tag,
            repo_commit: &repo_commit,
            encrypt_type: encrypt_type.name(),
            encryption_key: preset.credential_key.as_deref(),
            metadata,
        })?;
        api.put_file(&init, &package_path)?;
        let complete = api.downloadable_package_upload_complete(&init.upload_id)?;
        println!(
            "uploaded downloadable pack {} {} {} -> status={} key={}",
            pack_name,
            platform_name,
            mode.name(),
            complete.status,
            init.s3_key
        );
    }
    Ok(())
}

fn package_export_artifact(
    project: &ProjectConfig,
    platform_name: &str,
    mode: ExportMode,
    export_path: &Path,
    no_remote_sign: bool,
) -> Result<(PathBuf, &'static str, serde_json::Value)> {
    let integrity_mode = if no_remote_sign {
        "no_remote_sign"
    } else {
        "remote_sign"
    };
    match platform_name {
        "android" => {
            if !export_path.is_file() {
                bail!(
                    "Android export artifact not found: {}",
                    export_path.display()
                );
            }
            let files = apk_native_library_metadata(export_path)?;
            let apk_signed = android_export_signed(project);
            let metadata = serde_json::json!({
                "integrity_mode": integrity_mode,
                "apk_signed": apk_signed,
                "apk_signing": if apk_signed { "local" } else { "unsigned" },
                "files": files,
            });
            Ok((export_path.to_path_buf(), "apk", metadata))
        }
        "windows" => {
            let dir = export_path.parent().with_context(|| {
                format!(
                    "cannot resolve export directory for {}",
                    export_path.display()
                )
            })?;
            if !dir.is_dir() {
                bail!("Windows export directory not found: {}", dir.display());
            }
            let (_temp_file, zip_path) = tempfile::Builder::new()
                .prefix(&format!("pug-{platform_name}-{}-", mode.name()))
                .suffix(".zip")
                .tempfile()?
                .keep()
                .map_err(|err| err.error)
                .context("persist temporary Windows export package")?;
            drop(_temp_file);
            util::zip_paths(&zip_path, dir, &[dir.to_path_buf()])?;
            let files = windows_binary_metadata(dir)?;
            let metadata = serde_json::json!({
                "integrity_mode": integrity_mode,
                "files": files,
            });
            Ok((zip_path, "zip", metadata))
        }
        other => bail!("export artifact upload is not supported for {other}"),
    }
}

fn android_export_signed(project: &ProjectConfig) -> bool {
    android_export_config(project)
        .and_then(|config| config.signed)
        .unwrap_or(false)
}

fn apk_native_library_metadata(apk_path: &Path) -> Result<Vec<serde_json::Value>> {
    let file = fs::File::open(apk_path).with_context(|| format!("open {}", apk_path.display()))?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut out = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let name = entry.name().replace('\\', "/");
        if !name.starts_with("lib/") || !name.ends_with(".so") {
            continue;
        }
        let mut hasher = Sha256::new();
        let mut size = 0_u64;
        let mut buf = [0_u8; 64 * 1024];
        loop {
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            hasher.update(&buf[..n]);
        }
        out.push(serde_json::json!({
            "path": name,
            "kind": "so",
            "sha256": format!("{:x}", hasher.finalize()),
            "size": size,
        }));
    }
    Ok(out)
}

fn windows_binary_metadata(dir: &Path) -> Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(dir) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let kind = if file_name.ends_with(".exe") {
            "exe"
        } else if file_name.ends_with(".dll") {
            "dll"
        } else {
            continue;
        };
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push(serde_json::json!({
            "path": rel,
            "kind": kind,
            "sha256": util::sha256_file(path)?,
            "size": util::file_size(path)?,
        }));
    }
    Ok(out)
}

fn git_head(cwd: &Path) -> Result<String> {
    util::output_command(
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .arg("rev-parse")
            .arg("HEAD"),
    )
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
    for tool in ["java", "keytool"] {
        let path = java_tool_path(&java_home, tool);
        if !path.is_file() {
            bail!("Java SDK is incomplete, missing {}", path.display());
        }
    }

    sync_android_editor_settings(editor, &sdk, &java_home)?;
    Ok(AndroidEnvironment { sdk, java_home })
}

fn java_tool_path(java_home: &Path, tool: &str) -> PathBuf {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    java_home.join("bin").join(format!("{tool}{suffix}"))
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
    update_gitignore_entries(
        cwd,
        &[
            "bin/",
            "export_presets.cfg",
            ".godot/pug/",
            PROJECT_OVERWRITE_FILE,
        ],
    )
}

fn update_gitignore_entries(cwd: &Path, entries: &[&str]) -> Result<()> {
    let path = cwd.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut missing = Vec::new();
    for entry in entries {
        if !existing.lines().any(|line| line.trim() == *entry) {
            missing.push(*entry);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut next = existing;
    if !next.ends_with('\n') && !next.is_empty() {
        next.push('\n');
    }
    if !next.lines().any(|line| line.trim() == "# pug") {
        next.push_str("\n# pug\n");
    }
    for entry in missing {
        next.push_str(entry);
        next.push('\n');
    }
    fs::write(path, next)?;
    Ok(())
}

fn warn_if_generated_file_not_ignored(cwd: &Path, path: &str) {
    if !git_command_success(cwd, &["rev-parse", "--is-inside-work-tree"]).unwrap_or(false) {
        return;
    }
    if git_command_success(cwd, &["ls-files", "--error-unmatch", "--", path]).unwrap_or(false) {
        eprintln!(
            "warning: {path} is generated by pug but is tracked by git; run `git rm --cached {path}`"
        );
        return;
    }
    if !git_command_success(cwd, &["check-ignore", "--quiet", "--", path]).unwrap_or(false) {
        eprintln!(
            "warning: {path} is generated by pug but is not ignored by git; add it to .gitignore"
        );
    }
}

fn git_command_success(cwd: &Path, args: &[&str]) -> Option<bool> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|status| status.success())
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

    fn test_platforms(specs: &[&str]) -> ProjectPlatforms {
        ProjectPlatforms::from_specs(specs.iter().copied()).unwrap()
    }

    fn test_platform_configs(configs: Vec<(&str, ProjectPlatformConfig)>) -> ProjectPlatforms {
        ProjectPlatforms::from_configs(configs).unwrap()
    }

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
    fn project_version_accepts_string_and_legacy_number() {
        let string_project: ProjectConfig = serde_json::from_str(
            r#"{"version":"1.2.3","engine":{"tag":"test"},"platforms":["android"]}"#,
        )
        .unwrap();
        let legacy_project: ProjectConfig = serde_json::from_str(
            r#"{"version":1,"engine":{"tag":"test"},"platforms":["android"]}"#,
        )
        .unwrap();

        assert_eq!(string_project.version, "1.2.3");
        assert_eq!(legacy_project.version, "1");
    }

    #[test]
    fn project_platforms_accept_config_object_and_legacy_list() {
        let object_project: ProjectConfig = serde_json::from_str(
            r#"{"engine":{"tag":"test"},"platforms":{"android":{"architectures":["arm64"]},"macos":{}}}"#,
        )
        .unwrap();
        let legacy_project: ProjectConfig =
            serde_json::from_str(r#"{"engine":{"tag":"test"},"platforms":["android","macos"]}"#)
                .unwrap();

        assert_eq!(
            export_platforms(&object_project).unwrap(),
            vec!["android".to_string(), "macos".to_string()]
        );
        assert_eq!(
            export_platforms(&legacy_project).unwrap(),
            vec!["android".to_string(), "macos".to_string()]
        );
        assert_eq!(
            android_enabled_arches(&object_project).unwrap(),
            BTreeSet::from(["arm64-v8a".to_string()])
        );
    }

    #[test]
    fn read_project_merges_overwrite_file_recursively() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(PROJECT_FILE),
            r#"{
                "name":"demo",
                "engine":{"tag":"stable"},
                "platforms":["android"],
                "extensions":{"netcode":"1.0.0","rust_demo":"0.2.4"}
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join(PROJECT_OVERWRITE_FILE),
            r#"{
                "engine":{"tag":"local-20260514-test"},
                "extensions":{"rust_demo":"local://../extensions/rust_demo"}
            }"#,
        )
        .unwrap();

        let project = read_project(dir.path()).unwrap();

        assert_eq!(project.engine.tag, "local-20260514-test");
        assert_eq!(
            project.extensions.get("rust_demo").unwrap(),
            "local://../extensions/rust_demo"
        );
        assert_eq!(project.extensions.get("netcode").unwrap(), "1.0.0");
    }

    #[test]
    fn update_gitignore_includes_project_overwrite() {
        let dir = tempfile::tempdir().unwrap();

        update_gitignore(dir.path()).unwrap();

        let text = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(text.lines().any(|line| line == PROJECT_OVERWRITE_FILE));
    }

    #[test]
    fn local_extension_ref_resolves_relative_to_project() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("game");
        let ext_dir = dir.path().join("extensions/rust_demo");
        fs::create_dir_all(&project_dir).unwrap();
        fs::create_dir_all(&ext_dir).unwrap();

        let resolved =
            resolve_local_extension_dir(&project_dir, "local://../extensions/rust_demo").unwrap();

        assert_eq!(resolved, ext_dir.canonicalize().unwrap());
    }

    #[test]
    fn installed_template_artifact_unpacks_from_engine_install_dir() {
        let dir = tempfile::tempdir().unwrap();
        let install_root = dir.path().join("engine");
        let cache_root = dir
            .path()
            .join("project/.godot/pug/export_templates/local-test");
        let source = dir.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let template = source.join("godot.windows.template_release.x86_64.mono.exe");
        fs::write(&template, b"template").unwrap();

        let zip =
            installed_template_artifact_zip(&install_root, "windows", "template_release", "x86_64");
        util::zip_paths(&zip, &source, std::slice::from_ref(&template)).unwrap();

        let unpack = unpack_installed_template_artifact(
            &install_root,
            &cache_root,
            "windows",
            "template_release",
            "x86_64",
        )
        .unwrap()
        .unwrap();

        let found = find_template_file(&unpack, "template_release").unwrap();
        assert_eq!(
            found.file_name().and_then(|name| name.to_str()),
            Some("godot.windows.template_release.x86_64.mono.exe")
        );
    }

    #[test]
    fn legacy_export_platform_config_migrates_into_platforms() {
        let mut project: ProjectConfig = serde_json::from_str(
            r#"{"engine":{"tag":"test"},"platforms":["android"],"export":{"android":{"package":"com.example.demo","signed":false}}}"#,
        )
        .unwrap();

        migrate_legacy_export_platforms(&mut project).unwrap();

        let android = android_export_config(&project).unwrap();
        assert_eq!(android.package.as_deref(), Some("com.example.demo"));
        assert_eq!(android.signed, Some(false));
        assert!(project.export.as_ref().unwrap().android.is_none());
    }

    #[test]
    fn export_platforms_accept_comma_list() {
        let opts = ProjectExportOptions {
            platform: Some("windows,android".to_string()),
            android: false,
            ios: false,
            debug: false,
            release: true,
            upload: false,
            no_remote_sign: false,
            with_engine: None,
        };

        assert_eq!(
            resolve_export_platforms(&opts).unwrap(),
            vec!["windows".to_string(), "android".to_string()]
        );
    }

    #[test]
    fn sync_nuget_config_generates_config_for_csproj_projects() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("demo.csproj"), "<Project />\n").unwrap();
        fs::write(dir.path().join(".gitignore"), "# existing\n").unwrap();

        let editor_dir = dir.path().join("engine");
        fs::create_dir_all(editor_dir.join("GodotSharp/Tools/nupkgs")).unwrap();
        let editor = editor_dir.join("godot.windows.editor.x86_64.mono.exe");
        fs::write(&editor, "editor").unwrap();

        let mut sources = BTreeMap::new();
        sources.insert(
            "private-feed".to_string(),
            "https://packages.example.test/index.json".to_string(),
        );
        let project = ProjectConfig {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platforms(&["windows"]),
            extensions: BTreeMap::new(),
            packs: BTreeMap::new(),
            export: None,
            nuget: ProjectNugetConfig { sources },
        };

        sync_nuget_config(dir.path(), &project, Some(&editor)).unwrap();

        let config = fs::read_to_string(dir.path().join(NUGET_CONFIG_FILE)).unwrap();
        assert!(config.contains("<clear />"));
        assert!(config.contains("key=\"godot-local\""));
        assert!(config.contains("GodotSharp"));
        assert!(config.contains("key=\"private-feed\""));
        assert!(config.contains("key=\"nuget.org\""));

        let gitignore = fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gitignore.lines().any(|line| line == NUGET_CONFIG_FILE));
    }

    #[test]
    fn sync_nuget_config_skips_projects_without_csproj() {
        let dir = tempfile::tempdir().unwrap();
        let project = ProjectConfig {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platforms(&["windows"]),
            extensions: BTreeMap::new(),
            packs: BTreeMap::new(),
            export: None,
            nuget: ProjectNugetConfig::default(),
        };

        sync_nuget_config(dir.path(), &project, Some(Path::new("missing-editor"))).unwrap();

        assert!(!dir.path().join(NUGET_CONFIG_FILE).exists());
    }

    #[test]
    fn render_nuget_config_rejects_generated_source_conflicts() {
        let mut sources = BTreeMap::new();
        sources.insert("godot-local".to_string(), "/tmp/source".to_string());
        let project = ProjectConfig {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platforms(&["windows"]),
            extensions: BTreeMap::new(),
            packs: BTreeMap::new(),
            export: None,
            nuget: ProjectNugetConfig { sources },
        };

        let err = render_nuget_config(&project, Path::new("/tmp/nupkgs")).unwrap_err();

        assert!(
            err.to_string()
                .contains("conflicts with a generated source")
        );
    }

    #[test]
    fn locked_extension_packages_use_exact_versions() {
        let mut extensions = BTreeMap::new();
        extensions.insert("rust_demo".to_string(), "0.2.0".to_string());
        extensions.insert("netcode".to_string(), "1.0.0".to_string());
        let project = ProjectConfig {
            name: "test_project".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platforms(&["windows"]),
            extensions,
            packs: BTreeMap::new(),
            export: None,
            nuget: ProjectNugetConfig::default(),
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
    fn managed_android_preset_uses_project_json_signing_config() {
        let project = ProjectConfig {
            name: "porjectK2".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platform_configs(vec![(
                "android",
                ProjectPlatformConfig {
                    package: Some("com.example.demo".to_string()),
                    signed: Some(false),
                    architectures: Some(vec!["arm64".to_string()]),
                    ..Default::default()
                },
            )]),
            extensions: BTreeMap::new(),
            packs: BTreeMap::new(),
            export: Some(ProjectExportConfig {
                name: Some("Demo".to_string()),
                output_dir: Some(PathBuf::from("../build/demo_export")),
                encrypt: Some(true),
                script_encryption_key: Some("0123456789abcdef".to_string()),
                ..Default::default()
            }),
            nuget: ProjectNugetConfig::default(),
        };

        let (text, presets) = render_export_presets(Path::new("."), &project, None).unwrap();

        assert_eq!(presets.len(), 1);
        assert_eq!(
            presets[0].export_path,
            PathBuf::from("../build/demo_export/Android/Demo.apk")
        );
        assert!(text.contains("export_path=\"../build/demo_export/Android/Demo.apk\""));
        assert!(text.contains("encrypt_pck=true"));
        assert!(text.contains("encrypt_directory=true"));
        assert!(text.contains("architectures/arm64-v8a=true"));
        assert!(text.contains("version/name=\"1.0.0\""));
        assert!(text.contains("package/unique_name=\"com.example.demo\""));
        assert!(text.contains("package/signed=false"));
        assert!(text.contains("keystore/release=\"\""));
    }

    #[test]
    fn export_presets_disable_dotnet_debug_symbols() {
        let project = ProjectConfig {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platform_configs(vec![("windows", ProjectPlatformConfig::default())]),
            extensions: BTreeMap::new(),
            packs: BTreeMap::new(),
            export: Some(ProjectExportConfig {
                name: Some("Demo".to_string()),
                ..Default::default()
            }),
            nuget: ProjectNugetConfig::default(),
        };

        let (text, _) = render_export_presets(Path::new("."), &project, None).unwrap();

        assert!(text.contains("dotnet/include_debug_symbols=false"));
    }

    #[test]
    fn managed_android_preset_injects_export_template_override() {
        let project = ProjectConfig {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platform_configs(vec![(
                "android",
                ProjectPlatformConfig {
                    signed: Some(false),
                    ..Default::default()
                },
            )]),
            extensions: BTreeMap::new(),
            packs: BTreeMap::new(),
            export: Some(ProjectExportConfig {
                name: Some("Demo".to_string()),
                ..Default::default()
            }),
            nuget: ProjectNugetConfig::default(),
        };
        let templates = ExportTemplates {
            custom_template: None,
            android_source_template: Some(PathBuf::from("/tmp/android_source.zip")),
        };
        let template_override = ExportTemplateOverride {
            platform: "android",
            mode: ExportMode::Release,
            templates: &templates,
            android_keystore: None,
        };

        let (text, _) =
            render_export_presets(Path::new("."), &project, Some(&template_override)).unwrap();

        assert!(text.contains("gradle_build/android_source_template=\"/tmp/android_source.zip\""));
    }

    #[test]
    fn managed_android_preset_uses_generated_keystore_override() {
        let project = ProjectConfig {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platform_configs(vec![(
                "android",
                ProjectPlatformConfig {
                    signed: Some(true),
                    ..Default::default()
                },
            )]),
            extensions: BTreeMap::new(),
            packs: BTreeMap::new(),
            export: Some(ProjectExportConfig {
                name: Some("Demo".to_string()),
                ..Default::default()
            }),
            nuget: ProjectNugetConfig::default(),
        };
        let templates = ExportTemplates {
            custom_template: None,
            android_source_template: None,
        };
        let keystore = GeneratedAndroidKeystore {
            mode: ExportMode::Release,
            path: PathBuf::from(".godot/pug/android_keystores/temporary-release.keystore"),
            alias: "pug-release".to_string(),
            password: "temporary-password".to_string(),
        };
        let template_override = ExportTemplateOverride {
            platform: "android",
            mode: ExportMode::Release,
            templates: &templates,
            android_keystore: Some(&keystore),
        };

        let (text, _) =
            render_export_presets(Path::new("."), &project, Some(&template_override)).unwrap();

        assert!(text.contains("package/signed=true"));
        assert!(text.contains(
            "keystore/release=\".godot/pug/android_keystores/temporary-release.keystore\""
        ));
        assert!(text.contains("keystore/release_user=\"pug-release\""));
        assert!(text.contains("keystore/release_password=\"temporary-password\""));
        assert!(text.contains("keystore/debug=\"\""));
    }

    #[test]
    fn pack_presets_export_downloadable_and_non_android_internal() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("packs/dl")).unwrap();
        fs::create_dir_all(dir.path().join("packs/internal")).unwrap();
        fs::write(dir.path().join("packs/dl/level.tscn"), "level").unwrap();
        fs::write(dir.path().join("packs/internal/shared.tres"), "shared").unwrap();

        let mut packs = BTreeMap::new();
        packs.insert(
            "dl".to_string(),
            ProjectPackConfig {
                path: PathBuf::from("packs/dl"),
                kind: ProjectPackKind::Downloadable,
                encrypt_type: ProjectPackEncryptType::Random,
            },
        );
        packs.insert(
            "internal".to_string(),
            ProjectPackConfig {
                path: PathBuf::from("packs/internal"),
                kind: ProjectPackKind::Internal,
                encrypt_type: ProjectPackEncryptType::Project,
            },
        );
        let project = ProjectConfig {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platforms(&["android", "windows"]),
            extensions: BTreeMap::new(),
            packs,
            export: Some(ProjectExportConfig {
                name: Some("Demo".to_string()),
                output_dir: Some(PathBuf::from("../build/demo_export")),
                encrypt: Some(true),
                script_encryption_key: Some(
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
                ),
                ..Default::default()
            }),
            nuget: ProjectNugetConfig::default(),
        };

        let (text, presets) = render_export_presets(dir.path(), &project, None).unwrap();

        assert!(text.contains("name=\"Pug Pack Android dl\""));
        assert!(!text.contains("name=\"Pug Pack Android internal\""));
        assert!(text.contains("name=\"Pug Pack Windows Desktop dl\""));
        assert!(text.contains("name=\"Pug Pack Windows Desktop internal\""));
        assert!(text.contains("export_filter=\"selected_resources\""));
        assert!(text.contains("res://packs/dl/level.tscn"));
        assert!(text.contains("res://packs/internal/shared.tres"));
        assert!(text.contains("exclude_filter=\"packs/dl/*,packs/dl/**\""));
        assert!(text.contains(
            "exclude_filter=\"packs/dl/*,packs/dl/**,packs/internal/*,packs/internal/**\""
        ));

        let random = presets
            .iter()
            .find(|preset| preset.name == "Pug Pack Android dl")
            .unwrap();
        assert_eq!(random.credential_key.as_ref().unwrap().len(), 64);
    }

    #[test]
    fn android_internal_pack_rejects_non_project_encryption() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("packs/internal")).unwrap();
        fs::write(dir.path().join("packs/internal/shared.tres"), "shared").unwrap();
        let mut packs = BTreeMap::new();
        packs.insert(
            "internal".to_string(),
            ProjectPackConfig {
                path: PathBuf::from("packs/internal"),
                kind: ProjectPackKind::Internal,
                encrypt_type: ProjectPackEncryptType::Random,
            },
        );
        let project = ProjectConfig {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platforms(&["android"]),
            extensions: BTreeMap::new(),
            packs,
            export: None,
            nuget: ProjectNugetConfig::default(),
        };

        let err = render_export_presets(dir.path(), &project, None).unwrap_err();
        assert!(err.to_string().contains("must use encrypt_type=project"));
    }

    #[test]
    fn rewrite_manifest_omits_integrity_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let lib_dir = dir.path().join("bin/rust_demo/windows/x86_64");
        fs::create_dir_all(&lib_dir).unwrap();
        fs::write(lib_dir.join("rust_demo.dll"), "demo dll").unwrap();

        let project = ProjectConfig {
            name: "test_project".to_string(),
            version: "1.0.0".to_string(),
            engine: ProjectEngine {
                tag: "test".to_string(),
            },
            platforms: test_platforms(&["windows"]),
            extensions: BTreeMap::new(),
            packs: BTreeMap::new(),
            export: None,
            nuget: ProjectNugetConfig::default(),
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
