use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::lock::lock_file;
use crate::state::{GuestArch, ImageConfig, atomic_write, sha256_file};

const UBUNTU_RELEASES_BASE_URL: &str = "https://cloud-images.ubuntu.com/releases";

#[derive(Debug, Clone)]
pub struct CachedImage {
    pub config: ImageConfig,
    pub local_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedImageMetadata {
    url: String,
    sha256_url: String,
    filename: String,
    sha256: String,
}

pub async fn ensure_image(
    client: &Client,
    images_root: &Path,
    release: &str,
    arch: GuestArch,
) -> Result<CachedImage> {
    ensure_image_with_base_url(client, images_root, release, arch, UBUNTU_RELEASES_BASE_URL).await
}

pub async fn ensure_image_with_base_url(
    client: &Client,
    images_root: &Path,
    release: &str,
    arch: GuestArch,
    base_url: &str,
) -> Result<CachedImage> {
    let image_config = image_config_for(base_url, release, arch, None);
    let image_dir = images_root.join(release).join(arch.to_string());
    tokio::fs::create_dir_all(&image_dir).await?;
    let _lock = lock_file(image_dir.join(".lock")).await?;
    let local_path = image_dir.join(&image_config.filename);
    let metadata_path = image_dir.join("image.json");

    if let Some(metadata) = read_metadata(&metadata_path).await?
        && local_path.is_file()
        && metadata.url == image_config.url
        && metadata.sha256_url == image_config.sha256_url
        && metadata.filename == image_config.filename
    {
        return Ok(CachedImage {
            config: ImageConfig {
                sha256: metadata.sha256,
                ..image_config
            },
            local_path,
        });
    }

    let sha256s = client
        .get(&image_config.sha256_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let expected_sha = parse_sha256s(&sha256s, &image_config.filename)?;
    let image_config = image_config_for(base_url, release, arch, Some(expected_sha.clone()));

    if !local_path.is_file() {
        download_image(client, &image_config.url, &local_path).await?;
    }
    let actual_sha = sha256_file(&local_path).await?;
    if actual_sha != expected_sha {
        bail!(
            "checksum mismatch for {}: expected {}, got {}",
            local_path.display(),
            expected_sha,
            actual_sha
        );
    }

    let metadata = CachedImageMetadata {
        url: image_config.url.clone(),
        sha256_url: image_config.sha256_url.clone(),
        filename: image_config.filename.clone(),
        sha256: expected_sha,
    };
    atomic_write(&metadata_path, &serde_json::to_vec_pretty(&metadata)?).await?;

    Ok(CachedImage {
        config: image_config,
        local_path,
    })
}

pub fn image_config_for(
    base_url: &str,
    release: &str,
    arch: GuestArch,
    sha256: Option<String>,
) -> ImageConfig {
    let filename = format!(
        "ubuntu-{release}-server-cloudimg-{}.img",
        arch.ubuntu_arch()
    );
    let base = format!("{}/{release}/release", base_url.trim_end_matches('/'));
    ImageConfig {
        release: release.to_string(),
        arch,
        url: format!("{base}/{filename}"),
        sha256_url: format!("{base}/SHA256SUMS"),
        filename,
        sha256: sha256.unwrap_or_default(),
    }
}

async fn download_image(client: &Client, url: &str, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("download");
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut progress = DownloadProgress::new(url, path, response.content_length());
    while let Some(chunk) = response.chunk().await? {
        file.write_all(&chunk).await?;
        progress.advance(chunk.len() as u64);
    }
    file.flush().await?;
    drop(file);
    progress.finish();
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

struct DownloadProgress {
    label: String,
    total_bytes: Option<u64>,
    downloaded_bytes: u64,
    started_at: Instant,
    last_render_at: Instant,
    is_terminal: bool,
}

impl DownloadProgress {
    fn new(url: &str, path: &Path, total_bytes: Option<u64>) -> Self {
        let label = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| url.to_string());
        let is_terminal = std::io::stderr().is_terminal();
        let now = Instant::now();
        let progress = Self {
            label,
            total_bytes,
            downloaded_bytes: 0,
            started_at: now,
            last_render_at: now,
            is_terminal,
        };
        progress.render(false);
        progress
    }

    fn advance(&mut self, bytes: u64) {
        self.downloaded_bytes += bytes;
        if !self.is_terminal {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_render_at) >= Duration::from_millis(125) {
            self.last_render_at = now;
            self.render(false);
        }
    }

    fn finish(&mut self) {
        self.render(true);
    }

    fn render(&self, finished: bool) {
        if !self.is_terminal {
            return;
        }
        let elapsed = self.started_at.elapsed().as_secs_f64().max(0.001);
        let throughput = self.downloaded_bytes as f64 / elapsed;
        let downloaded = format_bytes(self.downloaded_bytes);
        let speed = format!("{}/s", format_bytes(throughput as u64));

        let line = match self.total_bytes {
            Some(total_bytes) if total_bytes > 0 => {
                let total = format_bytes(total_bytes);
                let percent =
                    (self.downloaded_bytes as f64 / total_bytes as f64 * 100.0).min(100.0);
                if finished {
                    format!(
                        "Downloaded {}: 100% ({downloaded}/{total}) at {speed}",
                        self.label
                    )
                } else {
                    format!(
                        "Downloading {}: {:>5.1}% ({downloaded}/{total}) at {speed}",
                        self.label, percent
                    )
                }
            }
            _ => {
                if finished {
                    format!("Downloaded {}: {downloaded} at {speed}", self.label)
                } else {
                    format!("Downloading {}: {downloaded} at {speed}", self.label)
                }
            }
        };

        let mut stderr = std::io::stderr().lock();
        {
            let _ = write!(stderr, "\r\x1b[2K{line}");
            if finished {
                let _ = writeln!(stderr);
            }
        }
        let _ = stderr.flush();
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

async fn read_metadata(path: &Path) -> Result<Option<CachedImageMetadata>> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(Some(
            serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn parse_sha256s(payload: &str, filename: &str) -> Result<String> {
    for line in payload.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((sha, name)) = trimmed.split_once(' ') else {
            continue;
        };
        let name = name.trim().trim_start_matches('*');
        if name == filename {
            return Ok(sha.to_string());
        }
    }
    Err(anyhow!("missing checksum entry for {filename}"))
}

#[cfg(test)]
mod tests {
    use sha2::Digest;
    use tempfile::tempdir;

    use super::{ensure_image_with_base_url, format_bytes, image_config_for, parse_sha256s};
    use crate::state::GuestArch;

    #[test]
    fn constructs_expected_image_url() {
        let image = image_config_for(
            "https://example.invalid/releases",
            "24.04",
            GuestArch::Arm64,
            None,
        );
        assert_eq!(
            image.url,
            "https://example.invalid/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img"
        );
    }

    #[test]
    fn parses_sha256_manifest_line() {
        let manifest = "abc123 *ubuntu-24.04-server-cloudimg-arm64.img\n";
        assert_eq!(
            parse_sha256s(manifest, "ubuntu-24.04-server-cloudimg-arm64.img").expect("sha"),
            "abc123"
        );
    }

    #[test]
    fn formats_bytes_for_progress_output() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }

    #[tokio::test]
    async fn downloads_and_caches_image() {
        let dir = tempdir().expect("tempdir");
        let mut server = mockito::Server::new_async().await;
        let image_bytes = b"fake-qcow".to_vec();
        let sha = format!("{:x}", sha2::Sha256::digest(&image_bytes));

        let _sha_mock = server
            .mock("GET", "/24.04/release/SHA256SUMS")
            .with_status(200)
            .with_body(format!("{sha} *ubuntu-24.04-server-cloudimg-amd64.img\n"))
            .create_async()
            .await;
        let _image_mock = server
            .mock(
                "GET",
                "/24.04/release/ubuntu-24.04-server-cloudimg-amd64.img",
            )
            .with_status(200)
            .with_body(image_bytes)
            .create_async()
            .await;

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        let cached = ensure_image_with_base_url(
            &client,
            dir.path(),
            "24.04",
            GuestArch::Amd64,
            &server.url(),
        )
        .await
        .expect("cache image");
        assert!(cached.local_path.is_file());
        assert_eq!(cached.config.sha256, sha);
    }
}
