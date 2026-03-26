use std::io::{Seek, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use fatfs::{FileSystem, FormatVolumeOptions, FsOptions};
use fscommon::BufStream;
use serde_yaml::Value;

use crate::state::sha256_hex;

const CLOUD_CONFIG_HEADER: &str = "#cloud-config\n";
const CIDATA_LABEL: [u8; 11] = *b"CIDATA     ";
const MIN_SEED_BYTES: u64 = 2 * 1024 * 1024;
const SEED_OVERHEAD_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CloudInitRender {
    pub user_data: Vec<u8>,
    pub meta_data: Vec<u8>,
    pub network_config: Option<Vec<u8>>,
    pub user_data_sha256: String,
    pub network_config_sha256: Option<String>,
}

pub async fn render_cloud_init(
    name: &str,
    ssh_public_key: &str,
    user_data_path: Option<&Path>,
    network_config_path: Option<&Path>,
) -> Result<CloudInitRender> {
    let mut user_data = default_user_data(ssh_public_key)?;
    if let Some(path) = user_data_path {
        let override_value = read_cloud_config_yaml(path).await?;
        merge_yaml(&mut user_data, override_value);
    }

    let user_data_payload = format!(
        "{CLOUD_CONFIG_HEADER}{}",
        serde_yaml::to_string(&user_data)?
    );
    let network_config = if let Some(path) = network_config_path {
        Some(tokio::fs::read(path).await?)
    } else {
        None
    };
    let meta_data = format!("instance-id: hardpass-{name}\nlocal-hostname: {name}\n").into_bytes();

    Ok(CloudInitRender {
        user_data_sha256: sha256_hex(user_data_payload.as_bytes()),
        network_config_sha256: network_config.as_deref().map(sha256_hex),
        user_data: user_data_payload.into_bytes(),
        meta_data,
        network_config,
    })
}

pub async fn create_seed_image(path: &Path, render: &CloudInitRender) -> Result<()> {
    let path = path.to_path_buf();
    let render = render.clone();
    tokio::task::spawn_blocking(move || create_seed_image_blocking(&path, &render)).await?
}

fn create_seed_image_blocking(path: &Path, render: &CloudInitRender) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let size = seed_image_size(render);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)?;
    file.set_len(size)?;
    let mut stream = BufStream::new(file);
    fatfs::format_volume(
        &mut stream,
        FormatVolumeOptions::new().volume_label(CIDATA_LABEL),
    )?;
    stream.seek(std::io::SeekFrom::Start(0))?;
    let fs = FileSystem::new(stream, FsOptions::new())?;
    let root = fs.root_dir();
    write_fat_file(&root, "user-data", &render.user_data)?;
    write_fat_file(&root, "meta-data", &render.meta_data)?;
    if let Some(network) = &render.network_config {
        write_fat_file(&root, "network-config", network)?;
    }
    Ok(())
}

fn seed_image_size(render: &CloudInitRender) -> u64 {
    let payload_bytes = render.user_data.len() as u64
        + render.meta_data.len() as u64
        + render
            .network_config
            .as_ref()
            .map_or(0, |network| network.len() as u64);
    MIN_SEED_BYTES.max(payload_bytes.saturating_add(SEED_OVERHEAD_BYTES))
}

fn write_fat_file<T: fatfs::ReadWriteSeek>(
    root: &fatfs::Dir<'_, T>,
    name: &str,
    payload: &[u8],
) -> Result<()> {
    let mut file = root.create_file(name)?;
    file.write_all(payload)?;
    Ok(())
}

fn default_user_data(ssh_public_key: &str) -> Result<Value> {
    let value = serde_yaml::to_value(serde_json::json!({
        "users": [
            "default",
            {
                "name": "ubuntu",
                "ssh_authorized_keys": [ssh_public_key],
                "sudo": "ALL=(ALL) NOPASSWD:ALL"
            }
        ],
        "ssh_pwauth": false,
        "disable_root": true,
        "growpart": {
            "mode": "auto",
            "devices": ["/"]
        },
        "resize_rootfs": true
    }))?;
    Ok(value)
}

async fn read_cloud_config_yaml(path: &Path) -> Result<Value> {
    let payload = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let normalized = if let Some(rest) = payload.strip_prefix(CLOUD_CONFIG_HEADER) {
        rest
    } else {
        payload.as_str()
    };
    let value: Value = serde_yaml::from_str(normalized)
        .with_context(|| format!("parse cloud-init YAML from {}", path.display()))?;
    if matches!(value, Value::Mapping(_)) {
        Ok(value)
    } else {
        bail!("cloud-init user-data override must be a YAML mapping");
    }
}

fn merge_yaml(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Mapping(target_map), Value::Mapping(source_map)) => {
            for (key, source_value) in source_map {
                match target_map.get_mut(&key) {
                    Some(target_value) => merge_yaml(target_value, source_value),
                    None => {
                        target_map.insert(key, source_value);
                    }
                }
            }
        }
        (target_value, source_value) => {
            *target_value = source_value;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use anyhow::{Result, anyhow};
    use fatfs::{FileSystem, FsOptions};
    use fscommon::BufStream;
    use serde_yaml::{Mapping, Value};
    use tempfile::tempdir;

    use super::{CloudInitRender, create_seed_image, render_cloud_init};

    fn yaml_mapping(value: &Value) -> Result<&Mapping> {
        match value {
            Value::Mapping(mapping) => Ok(mapping),
            _ => Err(anyhow!("expected YAML mapping")),
        }
    }

    #[tokio::test]
    async fn renders_default_cloud_init() {
        let render = render_cloud_init("dev", "ssh-ed25519 AAAA hardpass", None, None)
            .await
            .expect("render");
        let yaml = String::from_utf8(render.user_data).expect("utf8");
        assert!(yaml.starts_with("#cloud-config"));
        assert!(yaml.contains("ssh-ed25519 AAAA hardpass"));
    }

    #[tokio::test]
    async fn merge_override_replaces_values() {
        let dir = tempdir().expect("tempdir");
        let user_data_path = dir.path().join("user-data.yaml");
        tokio::fs::write(
            &user_data_path,
            "#cloud-config\nresize_rootfs: false\nhostname: custom\n",
        )
        .await
        .expect("write");
        let render = render_cloud_init(
            "dev",
            "ssh-ed25519 AAAA hardpass",
            Some(&user_data_path),
            None,
        )
        .await
        .expect("render");
        let value: serde_yaml::Value =
            serde_yaml::from_slice(&render.user_data["#cloud-config\n".len()..]).expect("yaml");
        let mapping = yaml_mapping(&value).expect("mapping");
        assert_eq!(
            mapping
                .get(serde_yaml::Value::from("resize_rootfs"))
                .and_then(serde_yaml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            mapping
                .get(serde_yaml::Value::from("hostname"))
                .and_then(serde_yaml::Value::as_str),
            Some("custom")
        );
    }

    #[tokio::test]
    async fn creates_seed_image_with_expected_files() {
        let dir = tempdir().expect("tempdir");
        let seed_path = dir.path().join("seed.img");
        let render = render_cloud_init("dev", "ssh-ed25519 AAAA hardpass", None, None)
            .await
            .expect("render");
        create_seed_image(&seed_path, &render)
            .await
            .expect("seed image");

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&seed_path)
            .expect("open");
        let stream = BufStream::new(file);
        let fs = FileSystem::new(stream, FsOptions::new()).expect("fs");
        let root = fs.root_dir();
        let mut meta = root.open_file("meta-data").expect("meta-data");
        let mut payload = String::new();
        meta.read_to_string(&mut payload).expect("read");
        assert!(payload.contains("local-hostname: dev"));
    }

    #[tokio::test]
    async fn creates_seed_image_for_large_combined_payloads() {
        let dir = tempdir().expect("tempdir");
        let seed_path = dir.path().join("seed.img");
        let render = CloudInitRender {
            user_data: vec![b'u'; 3 * 1024 * 1024],
            meta_data: b"instance-id: hardpass-dev\nlocal-hostname: dev\n".to_vec(),
            network_config: Some(vec![b'n'; 3 * 1024 * 1024]),
            user_data_sha256: "abc".into(),
            network_config_sha256: Some("def".into()),
        };
        create_seed_image(&seed_path, &render)
            .await
            .expect("seed image");

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&seed_path)
            .expect("open");
        let stream = BufStream::new(file);
        let fs = FileSystem::new(stream, FsOptions::new()).expect("fs");
        let root = fs.root_dir();

        let mut user_data = root.open_file("user-data").expect("user-data");
        let mut user_payload = Vec::new();
        user_data
            .read_to_end(&mut user_payload)
            .expect("read user-data");
        assert_eq!(user_payload.len(), render.user_data.len());

        let mut network_config = root.open_file("network-config").expect("network-config");
        let mut network_payload = Vec::new();
        network_config
            .read_to_end(&mut network_payload)
            .expect("read network-config");
        assert_eq!(
            network_payload.len(),
            render
                .network_config
                .as_ref()
                .expect("network config")
                .len()
        );
    }
}
