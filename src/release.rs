//! Official BeamMP-Server releases from GitHub: listing, picking the right
//! build for this machine, and installing it with checksum verification.
//!
//! Layout: `bin/<tag>/<flavor>/BeamMP-Server`, where the flavor is the asset
//! suffix (`ubuntu.22.04.x86_64`, `debian.12.arm64`, ...). Native Linux and
//! docker may want different flavors of the same tag, so both can coexist.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::model::Installed;
use crate::paths;

const API: &str = "https://api.github.com/repos/BeamMP/BeamMP-Server/releases?per_page=15";
const USER_AGENT: &str = concat!("beamhost/", env!("CARGO_PKG_VERSION"));
pub const BINARY: &str = "BeamMP-Server";

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
    /// `sha256:<hex>` on releases published since GitHub started recording it.
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, flavor: &str) -> Option<&Asset> {
        let wanted = format!("{BINARY}.{flavor}");
        self.assets.iter().find(|a| a.name == wanted)
    }
}

pub fn fetch_releases() -> Result<Vec<Release>> {
    let mut response = ureq::get(API)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("asking GitHub for BeamMP-Server releases")?;
    let text = response
        .body_mut()
        .read_to_string()
        .context("reading the release list")?;
    let releases: Vec<Release> = serde_json::from_str(&text).context("parsing the release list")?;
    Ok(releases.into_iter().filter(|r| !r.draft).collect())
}

/// Newest stable release, falling back to a pre-release only when nothing
/// stable exists.
pub fn latest(releases: &[Release]) -> Option<&Release> {
    releases
        .iter()
        .filter(|r| !r.prerelease)
        .max_by_key(|r| version_key(&r.tag_name))
        .or_else(|| releases.iter().max_by_key(|r| version_key(&r.tag_name)))
}

/// `v3.9.3` → `(3, 9, 3)`; anything unparsable sorts first.
pub fn version_key(tag: &str) -> (u32, u32, u32) {
    let mut parts = tag
        .trim_start_matches(['v', 'V'])
        .split(['.', '-'])
        .map(|p| p.parse::<u32>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

pub fn normalize_tag(tag: &str) -> String {
    let tag = tag.trim();
    if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    }
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x86_64",
    }
}

/// The flavor a container runs. Debian 12 has the oldest glibc of the
/// supported builds, and the runtime image is built on it.
pub fn docker_flavor() -> String {
    format!("debian.12.{}", arch())
}

/// The flavor that runs natively here, or `None` where there is none (macOS).
pub fn host_flavor() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    Some(format!(
        "{}.{}",
        distro_from_os_release(&os_release),
        arch()
    ))
}

/// Map `/etc/os-release` onto one of the distros BeamMP builds for. Ubuntu
/// derivatives are matched by codename; unknown systems get the build with
/// the oldest glibc, which is the one most likely to load.
pub fn distro_from_os_release(text: &str) -> &'static str {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(|v| v.trim_matches('"').to_ascii_lowercase())
            .unwrap_or_default()
    };
    let id = field("ID");
    let version = field("VERSION_ID");
    let codename = field("UBUNTU_CODENAME");
    match (id.as_str(), version.as_str()) {
        ("ubuntu", v) if v.starts_with("24") || v.starts_with("25") => "ubuntu.24.04",
        ("ubuntu", _) => "ubuntu.22.04",
        ("debian", v) if v.starts_with("13") => "debian.13",
        ("debian", v) if v.starts_with("12") => "debian.12",
        _ => match codename.as_str() {
            "noble" | "oracular" | "plucky" => "ubuntu.24.04",
            _ => "ubuntu.22.04",
        },
    }
}

pub fn binary_path(tag: &str, flavor: &str) -> PathBuf {
    paths::bin_dir().join(tag).join(flavor).join(BINARY)
}

pub fn installed() -> Vec<Installed> {
    let mut out = Vec::new();
    let Ok(tags) = std::fs::read_dir(paths::bin_dir()) else {
        return out;
    };
    for tag in tags.flatten() {
        let Ok(flavors) = std::fs::read_dir(tag.path()) else {
            continue;
        };
        for flavor in flavors.flatten() {
            if let Ok(meta) = std::fs::metadata(flavor.path().join(BINARY)) {
                out.push(Installed {
                    tag: tag.file_name().to_string_lossy().into_owned(),
                    flavor: flavor.file_name().to_string_lossy().into_owned(),
                    bytes: meta.len(),
                });
            }
        }
    }
    out.sort_by_key(|i| std::cmp::Reverse(version_key(&i.tag)));
    out
}

/// Newest installed tag for a flavor.
pub fn newest_installed(flavor: &str) -> Option<String> {
    installed()
        .into_iter()
        .find(|i| i.flavor == flavor)
        .map(|i| i.tag)
}

/// Download `asset` for `tag`/`flavor`, verifying the published sha256 when
/// GitHub has one. `progress` is called with (done, total) as bytes arrive.
pub fn install(
    tag: &str,
    flavor: &str,
    asset: &Asset,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let dest = binary_path(tag, flavor);
    let dir = dest.parent().expect("binary path has a parent");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{BINARY}.part"));

    let response = ureq::get(&asset.browser_download_url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("downloading {}", asset.name))?;
    let total = asset.size;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(&tmp)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        done += n as u64;
        progress(done, total);
    }
    file.sync_all()?;
    drop(file);

    if total > 0 && done != total {
        let _ = std::fs::remove_file(&tmp);
        bail!("download truncated: got {done} of {total} bytes");
    }
    if let Some(expected) = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
    {
        let actual = hex(&hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = std::fs::remove_file(&tmp);
            bail!(
                "checksum mismatch for {}: expected {expected}, got {actual}",
                asset.name
            );
        }
    }
    set_executable(&tmp)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_numerically() {
        assert!(version_key("v3.10.0") > version_key("v3.9.4"));
        assert_eq!(version_key("3.9.3"), (3, 9, 3));
        assert_eq!(normalize_tag("3.9.3"), "v3.9.3");
    }

    #[test]
    fn stable_wins_over_a_newer_prerelease() {
        let release = |tag: &str, pre: bool| Release {
            tag_name: tag.into(),
            prerelease: pre,
            draft: false,
            assets: vec![],
        };
        let list = vec![release("v3.9.4", true), release("v3.9.3", false)];
        assert_eq!(latest(&list).unwrap().tag_name, "v3.9.3");
        let only_pre = vec![release("v4.0.0", true)];
        assert_eq!(latest(&only_pre).unwrap().tag_name, "v4.0.0");
    }

    #[test]
    fn distros_map_onto_published_builds() {
        assert_eq!(
            distro_from_os_release("ID=ubuntu\nVERSION_ID=\"24.04\"\n"),
            "ubuntu.24.04"
        );
        assert_eq!(
            distro_from_os_release("ID=debian\nVERSION_ID=\"12\"\n"),
            "debian.12"
        );
        assert_eq!(
            distro_from_os_release("ID=linuxmint\nUBUNTU_CODENAME=noble\n"),
            "ubuntu.24.04"
        );
        assert_eq!(distro_from_os_release("ID=arch\n"), "ubuntu.22.04");
    }

    #[test]
    fn assets_are_matched_by_exact_flavor() {
        let asset = |name: &str| Asset {
            name: name.into(),
            size: 1,
            browser_download_url: String::new(),
            digest: None,
        };
        let release = Release {
            tag_name: "v3.9.3".into(),
            prerelease: false,
            draft: false,
            assets: vec![
                asset("debuginfo.debian.12.arm64"),
                asset("BeamMP-Server.debian.12.arm64"),
            ],
        };
        assert_eq!(
            release.asset("debian.12.arm64").unwrap().name,
            "BeamMP-Server.debian.12.arm64"
        );
        assert!(release.asset("debian.13.arm64").is_none());
    }
}
