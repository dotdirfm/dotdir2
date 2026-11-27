use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    env, fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Platform {
    Macos,
    Windows,
    Linux,
}

impl Platform {
    fn native() -> Result<Self> {
        match env::consts::OS {
            "macos" => Ok(Self::Macos),
            "windows" => Ok(Self::Windows),
            "linux" => Ok(Self::Linux),
            other => bail!("DotDir packaging is unavailable on {other}"),
        }
    }

    fn directory(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Linux => "linux",
        }
    }

    fn artifact(self) -> &'static str {
        match self {
            Self::Macos => "DotDir Native.app",
            Self::Windows => "dotdir.exe",
            Self::Linux => "dotdir",
        }
    }
}

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let mode = args.next();
    let mode = mode.as_deref().map(|mode| mode.to_string_lossy());
    ensure!(
        matches!(mode.as_deref(), None | Some("debug" | "release")) && args.next().is_none(),
        "Usage: cargo dist [debug|release]"
    );
    let platform = Platform::native()?;
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let executable = build(workspace, mode.as_deref() != Some("debug"))?;

    let dist = workspace.join("dist");
    fs::create_dir_all(&dist)?;
    let staging = tempfile::Builder::new()
        .prefix(".dotdir-")
        .tempdir_in(&dist)?;
    let package = staging.path().join("package");
    stage_package(platform, workspace, &executable, &package)?;
    if platform == Platform::Macos {
        let status = Command::new("codesign")
            .args(["--force", "--deep", "--sign", "-"])
            .arg(package.join(platform.artifact()))
            .status()
            .context("Could not run codesign")?;
        ensure!(status.success(), "Could not sign the macOS bundle");
    }

    let destination = dist.join(platform.directory());
    publish_package(staging, &destination)?;
    println!("{}", destination.join(platform.artifact()).display());
    Ok(())
}

fn build(workspace: &Path, release: bool) -> Result<PathBuf> {
    let mut cargo = Command::new(env::var_os("CARGO").unwrap_or_else(|| env!("CARGO").into()));
    cargo.current_dir(workspace).args([
        "build",
        "--locked",
        "--package",
        "dotdir",
        "--bin",
        "dotdir",
        "--message-format=json-render-diagnostics",
    ]);
    if release {
        cargo.arg("--release");
    }
    let mut child = cargo
        .stdout(Stdio::piped())
        .spawn()
        .context("Could not start Cargo")?;
    let mut executable = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            println!("{line}");
            continue;
        };
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == "dotdir"
            && let Some(path) = message["executable"].as_str()
        {
            executable = Some(PathBuf::from(path));
        }
    }
    ensure!(child.wait()?.success(), "DotDir build failed");
    executable.context("Cargo did not report a DotDir executable")
}

fn stage_package(
    platform: Platform,
    workspace: &Path,
    executable: &Path,
    package: &Path,
) -> Result<()> {
    let artifact = package.join(platform.artifact());
    let binary = if platform == Platform::Macos {
        let contents = artifact.join("Contents");
        let resources = contents.join("Resources");
        fs::create_dir_all(&resources)?;
        fs::copy(
            workspace.join("assets/icon.icns"),
            resources.join("icon.icns"),
        )?;
        let plist = include_str!("Info.plist").replace("@VERSION@", env!("CARGO_PKG_VERSION"));
        fs::write(contents.join("Info.plist"), plist)?;
        contents.join("MacOS/dotdir")
    } else {
        artifact
    };

    fs::create_dir_all(binary.parent().unwrap())?;
    fs::copy(executable, &binary)
        .with_context(|| format!("Could not stage {}", binary.display()))?;
    Ok(())
}

fn publish_package(staging: TempDir, destination: &Path) -> Result<()> {
    let previous = staging.path().join("previous");
    let replacing = destination.exists();
    if replacing {
        ensure!(
            destination.symlink_metadata()?.is_dir(),
            "Expected a package directory at {}",
            destination.display()
        );
        fs::rename(destination, &previous).with_context(|| {
            format!("Could not replace the package at {}", destination.display())
        })?;
    }
    if let Err(error) = fs::rename(staging.path().join("package"), destination) {
        if replacing && let Err(restore) = fs::rename(&previous, destination) {
            let saved = staging.keep().join("previous");
            bail!(
                "Could not publish the package: {error}. Could not restore the previous package: {restore}. It is at {}",
                saved.display()
            );
        }
        return Err(error).context("Could not publish the DotDir package");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_each_platform_without_extension_sidecars() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let binary = temp.path().join("fake-binary");
        fs::write(&binary, b"binary")?;
        for (platform, executable) in [
            (Platform::Macos, "DotDir Native.app/Contents/MacOS/dotdir"),
            (Platform::Windows, "dotdir.exe"),
            (Platform::Linux, "dotdir"),
        ] {
            let package = temp.path().join(platform.directory());
            stage_package(platform, workspace, &binary, &package)?;
            assert_eq!(fs::read(package.join(executable))?, b"binary");
            assert!(!package.join("extensions").exists());
        }
        let contents = temp.path().join("macos/DotDir Native.app/Contents");
        assert!(contents.join("Resources/icon.icns").is_file());
        assert!(!contents.join("Resources/extensions").exists());
        let plist = fs::read_to_string(contents.join("Info.plist"))?;
        assert!(plist.contains("<key>CFBundleExecutable</key><string>dotdir</string>"));
        assert!(plist.contains(env!("CARGO_PKG_VERSION")));
        Ok(())
    }
}
