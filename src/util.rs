use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::FileOptions};

pub fn run_command(cmd: &mut Command) -> Result<()> {
    let label = format!("{cmd:?}");
    let status = cmd.status().with_context(|| format!("spawn {label}"))?;
    if !status.success() {
        bail!("command failed ({status}): {label}");
    }
    Ok(())
}

pub fn output_command(cmd: &mut Command) -> Result<String> {
    let label = format!("{cmd:?}");
    let output = cmd.output().with_context(|| format!("spawn {label}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command failed ({}): {label}\n{stderr}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn file_size(path: &Path) -> Result<i64> {
    Ok(fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len() as i64)
}

pub fn ensure_clean_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
    }
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    Ok(())
}

pub fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::copy(src, dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            copy_file(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn zip_paths(output: &Path, base: &Path, paths: &[PathBuf]) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(output).with_context(|| format!("create {}", output.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);
    for path in paths {
        if path.is_dir() {
            for entry in WalkDir::new(path) {
                let entry = entry?;
                if entry.file_type().is_file() {
                    add_zip_file(&mut zip, base, entry.path(), options)?;
                }
            }
        } else if path.is_file() {
            add_zip_file(&mut zip, base, path, options)?;
        }
    }
    zip.finish()?;
    Ok(())
}

fn add_zip_file<W: Write + io::Seek>(
    zip: &mut ZipWriter<W>,
    base: &Path,
    path: &Path,
    options: FileOptions,
) -> Result<()> {
    let rel = path
        .strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    zip.start_file(rel, options)?;
    let mut file = fs::File::open(path)?;
    io::copy(&mut file, zip)?;
    Ok(())
}

pub fn unzip_to(zip_path: &Path, dst: &Path) -> Result<()> {
    let file = fs::File::open(zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive = ZipArchive::new(file)?;
    fs::create_dir_all(dst)?;
    archive.extract(dst)?;
    Ok(())
}

pub fn tar_zst(output: &Path, entries: &[(PathBuf, PathBuf)]) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(output)?;
    let encoder = zstd::Encoder::new(file, 19)?;
    let mut builder = tar::Builder::new(encoder.auto_finish());
    for (src, name) in entries {
        if src.is_file() {
            builder.append_path_with_name(src, name)?;
        } else if src.is_dir() {
            builder.append_dir_all(name, src)?;
        } else {
            return Err(anyhow!("tar entry does not exist: {}", src.display()));
        }
    }
    builder.finish()?;
    Ok(())
}

pub fn untar_zst(archive: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    let file = fs::File::open(archive)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(dst)?;
    Ok(())
}

pub fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
