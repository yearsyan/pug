use anyhow::{Result, anyhow, bail};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetSpec {
    pub platform: String,
    pub arch: String,
    pub godot_platform: &'static str,
    pub godot_arch: &'static str,
    pub rust_target: &'static str,
    pub lib_prefix: &'static str,
    pub lib_ext: &'static str,
    pub gd_arch: &'static str,
}

impl TargetSpec {
    pub fn lib_name(&self, name: &str) -> String {
        format!("{}{}.{}", self.lib_prefix, name, self.lib_ext)
    }

    pub fn gdextension_keys(&self) -> Vec<String> {
        let platform = self.platform.as_str();
        if platform == "ios" {
            return vec!["ios.debug".to_string(), "ios.release".to_string()];
        }
        vec![
            format!("{platform}.{}.debug", self.gd_arch),
            format!("{platform}.{}.release", self.gd_arch),
        ]
    }
}

pub fn host_platform() -> Result<&'static str> {
    match std::env::consts::OS {
        "macos" => Ok("macos"),
        "linux" => Ok("linux"),
        "windows" => Ok("windows"),
        other => bail!("unsupported host platform: {other}"),
    }
}

pub fn host_godot_platform() -> Result<&'static str> {
    Ok(match host_platform()? {
        "linux" => "linuxbsd",
        other => other,
    })
}

pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        "x86" => "x86_32",
        other => other,
    }
}

pub fn host_capable_platforms() -> Result<Vec<&'static str>> {
    Ok(match host_platform()? {
        "macos" => vec!["macos", "ios", "android"],
        "linux" => vec!["linux", "android"],
        "windows" => vec!["windows", "android"],
        _ => unreachable!(),
    })
}

pub fn parse_platform_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(normalize_platform)
        .collect()
}

pub fn normalize_platform(value: &str) -> String {
    match value {
        "linuxbsd" => "linux".to_string(),
        other => other.to_string(),
    }
}

pub fn default_arches(platform: &str) -> Result<Vec<&'static str>> {
    Ok(match platform {
        "macos" => vec![host_arch()],
        "windows" => vec!["x86_64"],
        "linux" => vec![host_arch()],
        "android" => vec!["arm64-v8a"],
        "ios" => vec!["arm64"],
        other => bail!("unknown platform: {other}"),
    })
}

pub fn spec(platform: &str, arch: &str) -> Result<TargetSpec> {
    let platform = normalize_platform(platform);
    let item = match (platform.as_str(), arch) {
        ("macos", "arm64") => TargetSpec {
            platform,
            arch: arch.to_string(),
            godot_platform: "macos",
            godot_arch: "arm64",
            rust_target: "aarch64-apple-darwin",
            lib_prefix: "lib",
            lib_ext: "dylib",
            gd_arch: "arm64",
        },
        ("macos", "x86_64") => TargetSpec {
            platform,
            arch: arch.to_string(),
            godot_platform: "macos",
            godot_arch: "x86_64",
            rust_target: "x86_64-apple-darwin",
            lib_prefix: "lib",
            lib_ext: "dylib",
            gd_arch: "x86_64",
        },
        ("windows", "x86_64") => TargetSpec {
            platform,
            arch: arch.to_string(),
            godot_platform: "windows",
            godot_arch: "x86_64",
            rust_target: "x86_64-pc-windows-msvc",
            lib_prefix: "",
            lib_ext: "dll",
            gd_arch: "x86_64",
        },
        ("linux", "x86_64") => TargetSpec {
            platform,
            arch: arch.to_string(),
            godot_platform: "linuxbsd",
            godot_arch: "x86_64",
            rust_target: "x86_64-unknown-linux-gnu",
            lib_prefix: "lib",
            lib_ext: "so",
            gd_arch: "x86_64",
        },
        ("linux", "arm64") => TargetSpec {
            platform,
            arch: arch.to_string(),
            godot_platform: "linuxbsd",
            godot_arch: "arm64",
            rust_target: "aarch64-unknown-linux-gnu",
            lib_prefix: "lib",
            lib_ext: "so",
            gd_arch: "arm64",
        },
        ("android", "arm64-v8a") | ("android", "arm64") => TargetSpec {
            platform,
            arch: "arm64-v8a".to_string(),
            godot_platform: "android",
            godot_arch: "arm64",
            rust_target: "aarch64-linux-android",
            lib_prefix: "lib",
            lib_ext: "so",
            gd_arch: "arm64",
        },
        ("ios", "arm64") => TargetSpec {
            platform,
            arch: arch.to_string(),
            godot_platform: "ios",
            godot_arch: "arm64",
            rust_target: "aarch64-apple-ios",
            lib_prefix: "lib",
            lib_ext: "dylib",
            gd_arch: "arm64",
        },
        _ => return Err(anyhow!("unsupported platform/arch: {platform}:{arch}")),
    };
    Ok(item)
}

pub fn parse_target(value: &str) -> Result<TargetSpec> {
    let (platform, arch) = match value.split_once(':') {
        Some((platform, arch)) => (platform, arch),
        None => {
            let platform = normalize_platform(value);
            let arch = default_arches(&platform)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no default arch for {platform}"))?;
            return spec(&platform, arch);
        }
    };
    spec(platform, arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_arch_normalizes_for_api() {
        let target = spec("android", "arm64").unwrap();
        assert_eq!(target.arch, "arm64-v8a");
        assert_eq!(target.godot_arch, "arm64");
        assert_eq!(target.gd_arch, "arm64");
    }
}
