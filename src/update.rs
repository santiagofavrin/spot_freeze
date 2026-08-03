//! Self-update from the matching GitHub Release asset.
//!
//! HTTP is done **in-process** via [`ureq`] (rustls) — there is no `curl`
//! subprocess, so no console window flashes and there is no runtime
//! dependency on an external tool being installed. The check and the
//! download/extract are pure I/O and are meant to run on a background thread
//! by the platform shell (see each `app.rs`); this module never touches the
//! UI thread. The replacement helper script is generated locally and runs
//! after this process exits, so the running executable is never overwritten
//! in place.
//!
//! Any remaining spawned helpers (`tar` for extraction, the PowerShell/shell
//! replacer) are launched with `CREATE_NO_WINDOW` on Windows so they never
//! pop a console either.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Windows process creation flag that prevents a child console process from
/// flashing a visible console window when spawned from this GUI app.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const RELEASES_API: &str = "https://api.github.com/repos/santiagofavrin/spotfreeze/releases/latest";
/// `User-Agent` header value (GitHub rejects requests without one).
const USER_AGENT: &str = concat!("SpotFreeze/", env!("CARGO_PKG_VERSION"));

/// Result of checking the latest matching GitHub Release asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate,
    Available { version: String },
}

/// Check for an update without downloading it. Network I/O — run off the UI
/// thread. Unauthenticated GitHub API calls are rate-limited (60/hour/IP),
/// which is fine for a user-initiated check.
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
///
/// `progress` is called as the download proceeds with `(bytes_done,
/// total_bytes)`, where `total_bytes` is `Some` only when the server sent a
/// `Content-Length`. It is invoked from the calling (background) thread, so
/// the platform shell can forward it to its UI thread without blocking here.
pub fn stage_latest(mut progress: impl FnMut(u64, Option<u64>)) -> Result<()> {
    let asset_name = asset_name();
    let (_, asset_url) = latest_release(asset_name)?;
    let root = std::env::temp_dir().join(format!("spotfreeze-update-{}", std::process::id()));
    if root.exists() {
        fs::remove_dir_all(&root).context("removing stale update staging directory")?;
    }
    fs::create_dir_all(&root).context("creating update staging directory")?;
    let archive = root.join(asset_name);
    download(&asset_url, &archive, &mut progress)?;
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

/// Fetch the latest release JSON and pull out the tag name and the matching
/// asset's `browser_download_url`.
fn latest_release(name: &str) -> Result<(String, String)> {
    let response = ureq::get(RELEASES_API)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", USER_AGENT)
        .call()
        .context("requesting the latest GitHub Release")?;
    let json: Value = response
        .into_json()
        .context("parsing the GitHub Release response")?;
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

/// Stream `url` into `destination`, calling `progress` per 64 KiB chunk. ureq
/// follows redirects, so the `browser_download_url` → CDN hop is handled.
fn download(
    url: &str,
    destination: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    let response = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .context("starting the release download")?;
    let total = response
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok());
    let mut reader = response.into_reader();
    let mut file = fs::File::create(destination)
        .with_context(|| format!("creating {}", destination.display()))?;
    let mut buf = [0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .context("reading the release download")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .context("writing the release download")?;
        done += n as u64;
        progress(done, total);
    }
    file.sync_all().context("flushing the release download")?;
    Ok(())
}

/// Extract the downloaded archive into `destination`. On Windows `tar` (bsdtar,
/// present on Windows 10+) also handles `.zip`; `CREATE_NO_WINDOW` keeps it
/// silent.
fn extract(archive: &Path, destination: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("ditto");
    #[cfg(target_os = "macos")]
    {
        command.args(["-x", "-k"]).arg(archive).arg(destination);
    }
    #[cfg(not(target_os = "macos"))]
    let mut command = {
        let mut c = Command::new("tar");
        c.args(["-xf"]).arg(archive).args(["-C"]).arg(destination);
        c
    };
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let status = command.status().context("extracting the release archive")?;
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

/// Spawn the replacement helper detached. On Windows it runs PowerShell with
/// `CREATE_NO_WINDOW` so no console flashes; the script itself uses
/// `-WindowStyle Hidden` and `-NonInteractive` as belt-and-suspenders.
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
        command.arg(script);
        command.creation_flags(CREATE_NO_WINDOW);
        command
    };
    #[cfg(not(windows))]
    let mut command = Command::new(script);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().context("launching update helper")?;
    Ok(())
}
