use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

use crate::platform;

use super::{
    model::{BuildContext, ProjectJson, TemplateTarget},
    toolchain::ensure_android_toolchain,
};

pub(super) fn resolve_template_targets(
    project: &ProjectJson,
    override_template_platforms: Option<&str>,
) -> Result<Vec<TemplateTarget>> {
    let explicit = override_template_platforms.is_some();
    let requested = if let Some(value) = override_template_platforms {
        value
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .collect()
    } else if let Some(items) = &project.platforms {
        items.clone()
    } else {
        platform::host_capable_platforms()?
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    filter_host_capable_template_targets(project, requested, explicit)
}

fn filter_host_capable_template_targets(
    project: &ProjectJson,
    requested: Vec<String>,
    explicit: bool,
) -> Result<Vec<TemplateTarget>> {
    let capable_order = platform::host_capable_platforms()?;
    let capable: BTreeSet<_> = capable_order.iter().copied().map(str::to_string).collect();
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    for raw in requested {
        if raw == "all" {
            for platform_name in capable_order.iter().copied() {
                for arch in default_template_arches(project, platform_name)? {
                    push_template_target(&mut targets, &mut seen, platform_name, &arch)?;
                }
            }
            continue;
        }

        let (platform_raw, arch_raw) = raw
            .split_once(':')
            .map(|(platform_name, arch)| (platform_name.trim(), Some(arch.trim())))
            .unwrap_or((raw.trim(), None));
        let item = platform::normalize_platform(platform_raw);
        if capable.contains(&item) {
            if let Some(arch) = arch_raw {
                push_template_target(&mut targets, &mut seen, &item, arch)?;
            } else {
                for arch in default_template_arches(project, &item)? {
                    push_template_target(&mut targets, &mut seen, &item, &arch)?;
                }
            }
        } else if explicit {
            bail!(
                "platform {item} is not buildable on this host; supported here: {}",
                capable_order.join(",")
            );
        }
    }
    if targets.is_empty() {
        bail!(
            "no buildable template platforms for this host; supported here: {}",
            capable_order.join(",")
        );
    }
    Ok(targets)
}

fn push_template_target(
    targets: &mut Vec<TemplateTarget>,
    seen: &mut BTreeSet<String>,
    platform_name: &str,
    arch: &str,
) -> Result<()> {
    let platform_name = platform::normalize_platform(platform_name);
    let arch = normalize_template_arch(&platform_name, arch)?;
    let key = format!("{platform_name}:{arch}");
    if seen.insert(key) {
        let godot_platform = if platform_name == "linux" {
            "linuxbsd".to_string()
        } else {
            platform_name.clone()
        };
        targets.push(TemplateTarget {
            platform: platform_name,
            godot_platform,
            arch,
        });
    }
    Ok(())
}

pub(super) fn default_template_arches(
    project: &ProjectJson,
    platform_name: &str,
) -> Result<Vec<String>> {
    let platform_name = platform::normalize_platform(platform_name);
    let raw = match platform_name.as_str() {
        "android" => project
            .android
            .as_ref()
            .and_then(|s| s.archs.clone())
            .unwrap_or_else(|| vec!["arm64".to_string()]),
        "ios" => project
            .ios
            .as_ref()
            .and_then(|s| s.archs.clone())
            .unwrap_or_else(|| vec!["arm64".to_string()]),
        _ => vec![platform::host_arch().to_string()],
    };
    raw.into_iter()
        .map(|arch| normalize_template_arch(&platform_name, &arch))
        .collect()
}

fn normalize_template_arch(platform_name: &str, arch: &str) -> Result<String> {
    let arch = arch.trim();
    if arch.is_empty() {
        bail!("empty arch for template platform {platform_name}");
    }
    Ok(match platform_name {
        "android" => match arch {
            "arm64-v8a" => "arm64".to_string(),
            "armeabi-v7a" => "arm32".to_string(),
            "x86" => "x86_32".to_string(),
            other => other.to_string(),
        },
        _ => arch.to_string(),
    })
}

pub(super) fn grouped_template_archs(targets: &[TemplateTarget]) -> BTreeMap<String, Vec<String>> {
    let mut grouped = BTreeMap::<String, Vec<String>>::new();
    for target in targets {
        let archs = grouped.entry(target.platform.clone()).or_default();
        if !archs.contains(&target.arch) {
            archs.push(target.arch.clone());
        }
    }
    grouped
}

pub(super) fn validate_targets(ctx: &BuildContext) -> Result<()> {
    let mut checked_android = false;
    for target in &ctx.template_targets {
        if target.platform == "android" && !checked_android {
            ensure_android_toolchain()?;
            checked_android = true;
        }
    }
    Ok(())
}
