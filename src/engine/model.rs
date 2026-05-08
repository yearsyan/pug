use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize, Serialize, Default)]
pub(crate) struct ProjectJson {
    pub(crate) name: Option<String>,
    pub(crate) tag: Option<String>,
    pub(crate) godot: Option<GodotSection>,
    pub(crate) platforms: Option<Vec<String>>,
    pub(crate) modules: Option<ModulesSection>,
    pub(crate) encryption: Option<EncryptionSection>,
    pub(crate) signing: Option<SigningSection>,
    pub(crate) android: Option<ArchSection>,
    pub(crate) ios: Option<ArchSection>,
    pub(crate) splash: Option<Value>,
    pub(crate) boot_splash: Option<Value>,
    pub(crate) splash_image: Option<String>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub(crate) struct GodotSection {
    pub(crate) tag: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub(crate) struct ModulesSection {
    pub(crate) disabled: Option<Vec<String>>,
    pub(crate) release_only: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub(crate) struct EncryptionSection {
    pub(crate) key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub(crate) struct SigningSection {
    pub(crate) enabled: Option<bool>,
    pub(crate) manifest_public_key_path: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
pub(crate) struct ArchSection {
    pub(crate) archs: Option<Vec<String>>,
}

#[derive(Debug)]
pub(crate) struct BuildContext {
    pub(crate) repo_root: PathBuf,
    pub(crate) godot_src: PathBuf,
    pub(crate) project: ProjectJson,
    pub(crate) host_godot: &'static str,
    pub(crate) host_api: &'static str,
    pub(crate) host_arch: &'static str,
    pub(crate) template_targets: Vec<TemplateTarget>,
    pub(crate) scons_args: Vec<String>,
    pub(crate) manifest_public_key_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TemplateTarget {
    pub(crate) platform: String,
    pub(crate) godot_platform: String,
    pub(crate) arch: String,
}

#[derive(Debug)]
pub(crate) struct BuiltArtifact {
    pub(crate) platform: String,
    pub(crate) kind: String,
    pub(crate) arch: Option<String>,
    pub(crate) archs: Vec<String>,
    pub(crate) package_path: PathBuf,
    pub(crate) sha256: String,
    pub(crate) size: i64,
}

impl BuildContext {
    pub(crate) fn editor_output_dir(&self) -> PathBuf {
        editor_output_dir(&self.repo_root, self.host_godot, self.host_arch)
    }
}

pub(crate) fn editor_output_dir(repo_root: &Path, godot_platform: &str, arch: &str) -> PathBuf {
    repo_root
        .join("build")
        .join(godot_platform)
        .join("editor")
        .join(arch)
}
