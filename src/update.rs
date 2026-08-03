//! Self-update from the matching GitHub Release asset.
//!
//! Downloads and extracts data only; the replacement script is generated
//! locally and runs after this process exits, so the running executable is
//! never overwritten in place.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const RELEASES_API: &str = "https://api.github.com/repos/santiagofavrin/spotfreeze/releases/latest";

/// Result of checking the latest matching GitHub Release asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate,
    Available { version: String },
}

/// Check for an update without downloading it.
pub fn check_latest() -> Result<CheckResult> {
    let (tag, _) = latest_release(asset_name())?;
    let version = tag.strip_prefix('v').unwrap_or(&tag).to_owned();
    if version == env!("CARGO_PKG_VERSION") {
        Ok(CheckResult::UpToDate)
    } else {
        Ok(CheckResult::Available { version })
    }
}

/// Stage the latest platform asset and launch a replacement helper.
pub fn stage_latest() -> Result<()> {
    let asset_name = asset_name();
    let (_, asset_url) = latest_release(asset_name)?;
    let root = std::env::temp_dir().join(format!("spotfreeze-update-{}", std::process::id()));
    if root.exists() {
        fs::remove_dir_all(&root).context("removing stale update staging directory")?;
    }
    fs::create_dir_all(&root).context("creating update staging directory")?;
    let archive = root.join(asset_name);
    download(&asset_url, &archive)?;
    let extracted = root.join("extracted");
    fs::create_dir(&extracted).context("creating update extraction directory")?;
    extract(&archive, &extracted)?;

    let current = std::env::current_exe().context("locating the running executable")?;
    let replacement = replacement_path(&extracted, &current)?;
    let script = write_replacement_script(&root, &replacement, &current)?;
    launch_helper(&script)?;
    Ok(())
}

fn asset_name() -> &'static str {
    #[cfg(windows)]
    {
        "spotfreeze-windows-x64.zip"
    }
    #[cfg(target_os = "linux")]
    {
        "spotfreeze-linux-x64.tar.gz"
    }
    #[cfg(target_os = "macos")]
    {
        "SpotFreeze-macos-arm64.zip"
    }
}

fn latest_release(name: &str) -> Result<(String, String)> {
    let response = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: SpotFreeze",
            RELEASES_API,
        ])
        .output()
        .context("running curl to check GitHub Releases")?;
    if !response.status.success() {
        bail!("GitHub Releases request failed ({})", response.status);
    }
    let json: Value =
        serde_json::from_slice(&response.stdout).context("parsing GitHub Releases response")?;
    let assets = json["assets"]
        .as_array()
        .context("latest release has no asset list")?;
    let url = assets
        .iter()
        .find(|asset| asset["name"].as_str() == Some(name))
        .and_then(|asset| asset["browser_download_url"].as_str())
        .map(str::to_owned)
        .with_context(|| format!("latest release has no {name} asset"))?;
    let tag = json["tag_name"]
        .as_str()
        .context("latest release has no tag name")?
        .to_owned();
    Ok((tag, url))
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(destination)
        .arg(url)
        .status()
        .context("downloading the latest SpotFreeze release")?;
    if !status.success() {
        bail!("release download failed ({status})");
    }
    Ok(())
}

fn extract(archive: &Path, destination: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let status = Command::new("ditto")
        .args(["-x", "-k"])
        .arg(archive)
        .arg(destination)
        .status()
        .context("extracting the macOS release")?;
    #[cfg(not(target_os = "macos"))]
    let status = Command::new("tar")
        .args(["-xf"])
        .arg(archive)
        .args(["-C"])
        .arg(destination)
        .status()
        .context("extracting the release")?;
    if !status.success() {
        bail!("release extraction failed ({status})");
    }
    Ok(())
}

fn replacement_path(extracted: &Path, current: &Path) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        if let Some(app) = current
            .ancestors()
            .find(|path| path.extension().is_some_and(|e| e == "app"))
        {
            let app_name = app.file_name().context("invalid macOS app path")?;
            let replacement = extracted.join(app_name).join("Contents/MacOS/spotfreeze");
            if replacement.is_file() {
                return Ok(replacement);
            }
        }
        bail!("the macOS release does not contain SpotFreeze.app")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let name = current
            .file_name()
            .context("running executable has no filename")?;
        let replacement = extracted.join(name);
        if replacement.is_file() {
            Ok(replacement)
        } else {
            bail!(
                "the latest release does not contain {}",
                name.to_string_lossy()
            )
        }
    }
}

fn write_replacement_script(root: &Path, replacement: &Path, current: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let script = root.join("replace.ps1");
        let content = format!(
            "$ErrorActionPreference = 'Stop'\nwhile (Get-Process -Id {} -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 200 }}\nCopy-Item -LiteralPath '{}' -Destination '{}' -Force\nStart-Process -FilePath '{}'\nRemove-Item -LiteralPath '{}' -Recurse -Force\n",
            std::process::id(),
            replacement.display(),
            current.display(),
            current.display(),
            root.display()
        );
        fs::write(&script, content).context("writing Windows update helper")?;
        Ok(script)
    }
    #[cfg(not(windows))]
    {
        let script = root.join("replace.sh");
        #[cfg(target_os = "macos")]
        let post_install = format!(
            "codesign --force --sign - '{}'\nopen -a '{}'\n",
            current
                .ancestors()
                .find(|path| path.extension().is_some_and(|e| e == "app"))
                .context("running macOS executable is not inside an app bundle")?
                .display(),
            current
                .ancestors()
                .find(|path| path.extension().is_some_and(|e| e == "app"))
                .context("running macOS executable is not inside an app bundle")?
                .display()
        );
        #[cfg(not(target_os = "macos"))]
        let post_install = format!("exec '{}'\n", current.display());
        let content = format!(
            "#!/bin/sh\nwhile kill -0 {} 2>/dev/null; do sleep 0.2; done\ninstall -m 0755 '{}' '{}'\n{}",
            std::process::id(),
            replacement.display(),
            current.display(),
            post_install
        );
        fs::write(&script, content).context("writing update helper")?;
        let status = Command::new("chmod").args(["+x"]).arg(&script).status()?;
        if !status.success() {
            bail!("could not make update helper executable");
        }
        Ok(script)
    }
}

fn launch_helper(script: &Path) -> Result<()> {
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ]);
        command
    };
    #[cfg(not(windows))]
    let mut command = Command::new(script);
    #[cfg(windows)]
    command.arg(script);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().context("launching update helper")?;
    Ok(())
}
