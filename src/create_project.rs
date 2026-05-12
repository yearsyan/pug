use anyhow::{Context, Result, bail};
use inquire::Text;
use serde::Serialize;
use std::{fs, path::Path, process::Command};

use crate::util;

const DEFAULT_GODOT_TAG: &str = "4.6.2-stable";
const DEFAULT_PLATFORMS: [&str; 3] = ["macos:arm64", "android:arm64", "windows:x86_64"];
const APP_EXPORT_WORKFLOW: &str = ".gitea/workflows/pug-app-export.yml";
const GITATTRIBUTES: &str = ".gitattributes";
const DEFAULT_GITATTRIBUTES: &str = r#"# Prefer LF everywhere, including on Windows checkouts. Git normalizes text files
# and only keeps CRLF for formats where Windows tooling still expects it.
* text=auto eol=lf

# Scripts and config files should stay LF.
*.sh text eol=lf
*.ps1 text eol=lf
*.psm1 text eol=lf
*.psd1 text eol=lf
*.yml text eol=lf
*.yaml text eol=lf
*.json text eol=lf
patches/**/*.diff text eol=lf

# Windows batch files are the main CRLF exception.
*.bat text eol=crlf
*.cmd text eol=crlf

# Keep binary assets out of line-ending normalization.
*.png binary
*.jpg binary
*.jpeg binary
*.gif binary
*.ico binary
*.zip binary
*.gz binary
*.tar binary
*.7z binary
*.apk binary
*.aar binary
*.keystore binary
"#;

#[derive(Debug, Serialize)]
struct OverlayProjectJson<'a> {
    name: &'a str,
    tag: &'a str,
    platforms: Vec<&'a str>,
    modules: OverlayModulesJson,
}

#[derive(Debug, Serialize)]
struct OverlayModulesJson {
    release_only: Vec<String>,
    disabled: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TemplateCheckout<'a> {
    Branch(&'a str),
    Tag(&'a str),
    Commit(&'a str),
}

pub fn create(
    name: Option<String>,
    template: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
    commit: Option<String>,
) -> Result<()> {
    let name = match name {
        Some(name) => name,
        None => Text::new("Project name:").prompt()?,
    };
    let template = clean_optional(template.as_deref());
    let checkout = template_checkout(branch.as_deref(), tag.as_deref(), commit.as_deref())?;
    if template.is_none() && checkout.is_some() {
        bail!("--branch, --tag, and --commit require --template");
    }
    let cwd = std::env::current_dir()?;
    let project_dir = cwd.join(valid_project_dir_name(&name)?);
    create_at(&project_dir, name.trim(), template, checkout)
}

fn create_at(
    project_dir: &Path,
    project_name: &str,
    template: Option<&str>,
    checkout: Option<TemplateCheckout<'_>>,
) -> Result<()> {
    let project_name = project_name.trim();
    if project_name.is_empty() {
        bail!("project name must not be empty");
    }
    if project_dir.exists() {
        bail!(
            "project directory already exists: {}",
            project_dir.display()
        );
    }

    fs::create_dir_all(project_dir).with_context(|| format!("create {}", project_dir.display()))?;
    let result = create_project_files(project_dir, project_name, template, checkout);
    if result.is_err() {
        let _ = fs::remove_dir_all(project_dir);
    }
    result?;
    println!("{}", project_dir.display());
    Ok(())
}

fn create_project_files(
    project_dir: &Path,
    project_name: &str,
    template: Option<&str>,
    checkout: Option<TemplateCheckout<'_>>,
) -> Result<()> {
    util::run_command(
        Command::new("git")
            .arg("-C")
            .arg(project_dir)
            .args(["init", "-q"]),
    )?;
    create_required_dirs(project_dir)?;
    write_project_json(project_dir, project_name)?;
    write_gitignore(project_dir)?;
    write_gitattributes(project_dir)?;

    if let Some(template) = template {
        copy_template_dirs(project_dir, template, checkout)?;
    }

    Ok(())
}

fn create_required_dirs(project_dir: &Path) -> Result<()> {
    for dir in ["extensions", "modules", "patches"] {
        let path = project_dir.join(dir);
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        fs::write(path.join(".gitkeep"), b"")
            .with_context(|| format!("write {}", path.join(".gitkeep").display()))?;
    }
    Ok(())
}

fn write_project_json(project_dir: &Path, project_name: &str) -> Result<()> {
    let project = OverlayProjectJson {
        name: project_name,
        tag: DEFAULT_GODOT_TAG,
        platforms: DEFAULT_PLATFORMS.to_vec(),
        modules: OverlayModulesJson {
            release_only: Vec::new(),
            disabled: Vec::new(),
        },
    };
    util::write_json(&project_dir.join("project.json"), &project)
}

fn write_gitignore(project_dir: &Path) -> Result<()> {
    let text = "# pug\n.repocache\n.cache/\nbuild/\n\n# system\n.DS_Store\n";
    fs::write(project_dir.join(".gitignore"), text)
        .with_context(|| format!("write {}", project_dir.join(".gitignore").display()))
}

fn write_gitattributes(project_dir: &Path) -> Result<()> {
    fs::write(project_dir.join(GITATTRIBUTES), DEFAULT_GITATTRIBUTES)
        .with_context(|| format!("write {}", project_dir.join(GITATTRIBUTES).display()))
}

fn copy_template_dirs(
    project_dir: &Path,
    template: &str,
    checkout: Option<TemplateCheckout<'_>>,
) -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let clone_dir = tmp.path().join("template");
    clone_template(template, &clone_dir, checkout)?;
    copy_template_dir(&clone_dir, project_dir, "modules")?;
    copy_template_dir(&clone_dir, project_dir, "patches")?;
    copy_template_file(&clone_dir, project_dir, APP_EXPORT_WORKFLOW)?;
    copy_template_file(&clone_dir, project_dir, GITATTRIBUTES)?;
    Ok(())
}

fn clone_template(
    template: &str,
    clone_dir: &Path,
    checkout: Option<TemplateCheckout<'_>>,
) -> Result<()> {
    match checkout {
        Some(TemplateCheckout::Commit(commit)) => {
            util::run_command(Command::new("git").args(["clone", template]).arg(clone_dir))
                .with_context(|| format!("clone template {template}"))?;
            util::run_command(
                Command::new("git")
                    .arg("-C")
                    .arg(clone_dir)
                    .args(["checkout", "--detach", commit]),
            )
            .with_context(|| format!("checkout template commit {commit}"))?;
        }
        Some(TemplateCheckout::Branch(branch)) => {
            util::run_command(
                Command::new("git")
                    .args(["clone", "--depth", "1", "--branch", branch, template])
                    .arg(clone_dir),
            )
            .with_context(|| format!("clone template branch {branch} from {template}"))?;
        }
        Some(TemplateCheckout::Tag(tag)) => {
            util::run_command(
                Command::new("git")
                    .args(["clone", "--depth", "1", "--branch", tag, template])
                    .arg(clone_dir),
            )
            .with_context(|| format!("clone template tag {tag} from {template}"))?;
        }
        None => {
            util::run_command(
                Command::new("git")
                    .args(["clone", "--depth", "1", template])
                    .arg(clone_dir),
            )
            .with_context(|| format!("clone template {template}"))?;
        }
    }
    Ok(())
}

fn copy_template_dir(template_dir: &Path, project_dir: &Path, name: &str) -> Result<bool> {
    let src = template_dir.join(name);
    if !src.is_dir() {
        return Ok(false);
    }
    let dst = project_dir.join(name);
    fs::create_dir_all(&dst).with_context(|| format!("create {}", dst.display()))?;
    util::copy_dir(&src, &dst).with_context(|| format!("copy template {name}/"))?;
    Ok(true)
}

fn copy_template_file(template_dir: &Path, project_dir: &Path, name: &str) -> Result<bool> {
    let src = template_dir.join(name);
    if !src.is_file() {
        return Ok(false);
    }
    let dst = project_dir.join(name);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(&src, &dst).with_context(|| format!("copy template {name}"))?;
    Ok(true)
}

fn valid_project_dir_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        bail!("project name must not be empty");
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        bail!("project name must be a directory name, not a path");
    }
    Ok(name)
}

fn template_checkout<'a>(
    branch: Option<&'a str>,
    tag: Option<&'a str>,
    commit: Option<&'a str>,
) -> Result<Option<TemplateCheckout<'a>>> {
    let branch = clean_optional(branch);
    let tag = clean_optional(tag);
    let commit = clean_optional(commit);
    let count = [branch, tag, commit]
        .into_iter()
        .filter(|value| value.is_some())
        .count();
    if count > 1 {
        bail!("only one of --branch, --tag, and --commit can be used");
    }
    Ok(branch
        .map(TemplateCheckout::Branch)
        .or_else(|| tag.map(TemplateCheckout::Tag))
        .or_else(|| commit.map(TemplateCheckout::Commit)))
}

fn clean_optional(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn create_project_initializes_overlay_structure() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("demo_overlay");

        create_at(&project_dir, "demo_overlay", None, None).unwrap();

        assert!(project_dir.join(".git").is_dir());
        assert!(project_dir.join("extensions").is_dir());
        assert!(project_dir.join("modules").is_dir());
        assert!(project_dir.join("patches").is_dir());
        assert!(project_dir.join("extensions/.gitkeep").is_file());
        assert!(project_dir.join("modules/.gitkeep").is_file());
        assert!(project_dir.join("patches/.gitkeep").is_file());
        assert!(project_dir.join(".gitattributes").is_file());

        let project: Value =
            serde_json::from_str(&fs::read_to_string(project_dir.join("project.json")).unwrap())
                .unwrap();
        assert_eq!(project["name"], "demo_overlay");
        assert_eq!(project["tag"], DEFAULT_GODOT_TAG);
        assert_eq!(project["platforms"][0], "macos:arm64");
        assert_eq!(
            project["modules"]["release_only"].as_array().unwrap().len(),
            0
        );
    }

    #[test]
    fn project_name_must_not_be_a_path() {
        assert!(valid_project_dir_name("nested/demo").is_err());
    }

    #[test]
    fn copy_template_copies_overlay_and_app_export_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let template = dir.path().join("template");
        let project = dir.path().join("project");
        fs::create_dir_all(template.join("modules/custom")).unwrap();
        fs::create_dir_all(template.join("patches/001-test")).unwrap();
        fs::create_dir_all(template.join(".gitea/workflows")).unwrap();
        fs::write(template.join("modules/custom/SCsub"), "pass\n").unwrap();
        fs::write(template.join("patches/001-test/description.md"), "# test\n").unwrap();
        fs::write(template.join(".gitattributes"), "* text=auto eol=lf\n").unwrap();
        fs::write(
            template.join(".gitea/workflows/pug-app-export.yml"),
            "name: export\n",
        )
        .unwrap();
        fs::write(template.join(".gitea/workflows/other.yml"), "name: other\n").unwrap();
        fs::create_dir_all(project.join("modules")).unwrap();
        fs::create_dir_all(project.join("patches")).unwrap();

        copy_template_dir(&template, &project, "modules").unwrap();
        copy_template_dir(&template, &project, "patches").unwrap();
        copy_template_file(&template, &project, APP_EXPORT_WORKFLOW).unwrap();
        copy_template_file(&template, &project, GITATTRIBUTES).unwrap();

        assert!(project.join("modules/custom/SCsub").is_file());
        assert!(project.join("patches/001-test/description.md").is_file());
        assert!(
            project
                .join(".gitea/workflows/pug-app-export.yml")
                .is_file()
        );
        assert!(!project.join(".gitea/workflows/other.yml").exists());
        assert!(project.join(".gitattributes").is_file());
    }

    #[test]
    fn template_checkout_accepts_only_one_anchor() {
        assert_eq!(
            template_checkout(Some("main"), None, None).unwrap(),
            Some(TemplateCheckout::Branch("main"))
        );
        assert!(template_checkout(Some("main"), Some("v1"), None).is_err());
    }
}
