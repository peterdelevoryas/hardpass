use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

const MANAGED_BEGIN: &str = "# >>> hardpass managed ssh include v1 >>>";
const MANAGED_END: &str = "# <<< hardpass managed ssh include v1 <<<";
const MANAGED_BLOCK: &str = "# >>> hardpass managed ssh include v1 >>>\nInclude ~/.ssh/config.d/hardpass.conf\n# <<< hardpass managed ssh include v1 <<<\n";
const MANAGED_HEADER: &str = "# managed by hardpass; edits will be overwritten\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshAliasEntry {
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub identity_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SshConfigManager {
    home: PathBuf,
}

impl SshConfigManager {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn from_home_dir() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("unable to determine home directory"))?;
        Ok(Self::new(home))
    }

    pub fn main_config_path(&self) -> PathBuf {
        self.ssh_dir().join("config")
    }

    pub fn managed_include_path(&self) -> PathBuf {
        self.ssh_config_dir().join("hardpass.conf")
    }

    pub async fn install(&self) -> Result<()> {
        ensure_dir_mode(&self.ssh_dir(), 0o700).await?;
        ensure_dir_mode(&self.ssh_config_dir(), 0o700).await?;
        let existing = match tokio::fs::read_to_string(self.main_config_path()).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };
        let updated = install_managed_block(&existing)?;
        write_file_atomic(&self.main_config_path(), updated.as_bytes(), 0o600).await
    }

    pub async fn is_installed(&self) -> Result<bool> {
        let existing = match tokio::fs::read_to_string(self.main_config_path()).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        managed_block_state(&existing).map(ManagedBlockState::is_installed)
    }

    pub async fn sync(&self, entries: &[SshAliasEntry]) -> Result<()> {
        ensure_dir_mode(&self.ssh_dir(), 0o700).await?;
        ensure_dir_mode(&self.ssh_config_dir(), 0o700).await?;
        let rendered = render_managed_include(entries);
        write_file_atomic(&self.managed_include_path(), rendered.as_bytes(), 0o600).await
    }

    pub async fn sync_if_installed(&self, entries: &[SshAliasEntry]) -> Result<bool> {
        if self.is_installed().await? {
            self.sync(entries).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn ssh_dir(&self) -> PathBuf {
        self.home.join(".ssh")
    }

    fn ssh_config_dir(&self) -> PathBuf {
        self.ssh_dir().join("config.d")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedBlockState {
    Missing,
    Installed,
}

impl ManagedBlockState {
    fn is_installed(self) -> bool {
        matches!(self, Self::Installed)
    }
}

fn managed_block_state(content: &str) -> Result<ManagedBlockState> {
    let begin_count = content.match_indices(MANAGED_BEGIN).count();
    let end_count = content.match_indices(MANAGED_END).count();
    if begin_count == 0 && end_count == 0 {
        return Ok(ManagedBlockState::Missing);
    }
    if begin_count != 1 || end_count != 1 {
        bail!("invalid ~/.ssh/config: duplicate hardpass managed block markers");
    }
    let begin = content
        .find(MANAGED_BEGIN)
        .ok_or_else(|| anyhow!("missing begin marker"))?;
    let end = content
        .find(MANAGED_END)
        .ok_or_else(|| anyhow!("missing end marker"))?;
    if begin > end {
        bail!("invalid ~/.ssh/config: hardpass managed block markers are out of order");
    }
    Ok(ManagedBlockState::Installed)
}

fn install_managed_block(content: &str) -> Result<String> {
    match managed_block_state(content)? {
        ManagedBlockState::Missing => {
            if content.is_empty() {
                Ok(MANAGED_BLOCK.to_string())
            } else {
                let trimmed = content.trim_end_matches('\n');
                Ok(format!("{trimmed}\n\n{MANAGED_BLOCK}"))
            }
        }
        ManagedBlockState::Installed => {
            let begin = content
                .find(MANAGED_BEGIN)
                .ok_or_else(|| anyhow!("missing begin marker"))?;
            let end = content
                .find(MANAGED_END)
                .ok_or_else(|| anyhow!("missing end marker"))?;
            let end_index = end + MANAGED_END.len();
            let mut updated = String::new();
            updated.push_str(&content[..begin]);
            updated.push_str(MANAGED_BLOCK);
            let suffix = content[end_index..]
                .strip_prefix('\n')
                .unwrap_or(&content[end_index..]);
            updated.push_str(suffix);
            Ok(updated)
        }
    }
}

fn render_managed_include(entries: &[SshAliasEntry]) -> String {
    let mut entries = entries.to_vec();
    entries.sort_by(|left, right| left.alias.cmp(&right.alias));

    let mut output = String::from(MANAGED_HEADER);
    if entries.is_empty() {
        return output;
    }

    output.push('\n');
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push_str(&format!("Host {}\n", entry.alias));
        output.push_str(&format!("    HostName {}\n", entry.host));
        output.push_str(&format!("    Port {}\n", entry.port));
        output.push_str(&format!("    User {}\n", entry.user));
        output.push_str(&format!(
            "    IdentityFile {}\n",
            ssh_config_quote_path(&entry.identity_file)
        ));
        output.push_str("    IdentitiesOnly yes\n");
        output.push_str("    StrictHostKeyChecking no\n");
        output.push_str("    UserKnownHostsFile /dev/null\n");
        output.push_str("    LogLevel ERROR\n");
    }
    output
}

fn ssh_config_quote_path(path: &Path) -> String {
    let text = path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{text}\"")
}

async fn ensure_dir_mode(path: &Path, mode: u32) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(&path).with_context(|| format!("create dir {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {:o} {}", mode, path.display()))
    })
    .await?
}

async fn write_file_atomic(path: &Path, payload: &[u8], mode: u32) -> Result<()> {
    let path = path.to_path_buf();
    let payload = payload.to_vec();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("missing parent for {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(&payload)
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {:o} {}", mode, tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::{SshAliasEntry, SshConfigManager, install_managed_block, render_managed_include};

    #[test]
    fn install_managed_block_inserts_into_empty_config() {
        let content = install_managed_block("").expect("install");
        assert!(content.contains("Include ~/.ssh/config.d/hardpass.conf"));
    }

    #[test]
    fn install_managed_block_replaces_existing_block_idempotently() {
        let initial = "# comment\n\n# >>> hardpass managed ssh include v1 >>>\nInclude ~/.ssh/old\n# <<< hardpass managed ssh include v1 <<<\n";
        let content = install_managed_block(initial).expect("install");
        assert_eq!(
            content
                .matches("Include ~/.ssh/config.d/hardpass.conf")
                .count(),
            1
        );
        assert_eq!(install_managed_block(&content).expect("reinstall"), content);
    }

    #[test]
    fn render_managed_include_sorts_aliases_and_quotes_identity() {
        let rendered = render_managed_include(&[
            SshAliasEntry {
                alias: "beta".into(),
                host: "127.0.0.1".into(),
                port: 40222,
                user: "ubuntu".into(),
                identity_file: PathBuf::from("/tmp/path with spaces/id_ed25519"),
            },
            SshAliasEntry {
                alias: "alpha".into(),
                host: "127.0.0.1".into(),
                port: 40221,
                user: "ubuntu".into(),
                identity_file: PathBuf::from("/tmp/id_ed25519"),
            },
        ]);
        let alpha = rendered.find("Host alpha").expect("alpha host");
        let beta = rendered.find("Host beta").expect("beta host");
        assert!(alpha < beta);
        assert!(rendered.contains("IdentityFile \"/tmp/path with spaces/id_ed25519\""));
    }

    #[tokio::test]
    async fn install_and_sync_write_expected_files_with_permissions() {
        let dir = tempdir().expect("tempdir");
        let manager = SshConfigManager::new(dir.path().to_path_buf());
        manager.install().await.expect("install");
        manager
            .sync(&[SshAliasEntry {
                alias: "neuromancer".into(),
                host: "127.0.0.1".into(),
                port: 40222,
                user: "ubuntu".into(),
                identity_file: PathBuf::from("/tmp/id_ed25519"),
            }])
            .await
            .expect("sync");

        let config = tokio::fs::read_to_string(manager.main_config_path())
            .await
            .expect("read config");
        let include = tokio::fs::read_to_string(manager.managed_include_path())
            .await
            .expect("read include");
        assert!(config.contains("Include ~/.ssh/config.d/hardpass.conf"));
        assert!(include.contains("Host neuromancer"));

        let config_mode = std::fs::metadata(manager.main_config_path())
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o777;
        let include_mode = std::fs::metadata(manager.managed_include_path())
            .expect("include metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(config_mode, 0o600);
        assert_eq!(include_mode, 0o600);
    }
}
