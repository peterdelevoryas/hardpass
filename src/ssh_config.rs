use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

const MANAGED_INCLUDE_LINE: &str = "Include ~/.hardpass/ssh/config";
const LEGACY_INCLUDE_LINE: &str = "Include ~/.ssh/config.d/hardpass.conf";
const LEGACY_MANAGED_BEGIN: &str = "# >>> hardpass managed ssh include v1 >>>";
const LEGACY_MANAGED_END: &str = "# <<< hardpass managed ssh include v1 <<<";
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
        self.managed_include_dir().join("config")
    }

    pub async fn install(&self) -> Result<()> {
        ensure_dir_mode(&self.ssh_dir(), 0o700).await?;
        ensure_dir_mode(&self.hardpass_dir(), 0o700).await?;
        ensure_dir_mode(&self.managed_include_dir(), 0o700).await?;
        let existing = match tokio::fs::read_to_string(self.main_config_path()).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };
        let updated = install_managed_include(&existing);
        write_file_atomic(&self.main_config_path(), updated.as_bytes(), 0o600).await
    }

    pub async fn sync(&self, entries: &[SshAliasEntry]) -> Result<()> {
        ensure_dir_mode(&self.ssh_dir(), 0o700).await?;
        ensure_dir_mode(&self.hardpass_dir(), 0o700).await?;
        ensure_dir_mode(&self.managed_include_dir(), 0o700).await?;
        let rendered = render_managed_include(entries);
        write_file_atomic(&self.managed_include_path(), rendered.as_bytes(), 0o600).await
    }

    fn ssh_dir(&self) -> PathBuf {
        self.home.join(".ssh")
    }

    fn hardpass_dir(&self) -> PathBuf {
        self.home.join(".hardpass")
    }

    fn managed_include_dir(&self) -> PathBuf {
        self.hardpass_dir().join("ssh")
    }
}

fn install_managed_include(content: &str) -> String {
    let (without_integration, _) = remove_hardpass_integration(content);
    let insertion =
        first_host_or_match_offset(&without_integration).unwrap_or(without_integration.len());
    let before = without_integration[..insertion].trim_end_matches('\n');
    let after = without_integration[insertion..].trim_start_matches('\n');

    let mut updated = String::new();
    if before.is_empty() {
        updated.push_str(MANAGED_INCLUDE_LINE);
        updated.push('\n');
    } else {
        updated.push_str(before);
        updated.push_str("\n\n");
        updated.push_str(MANAGED_INCLUDE_LINE);
        updated.push('\n');
    }
    if !after.is_empty() {
        updated.push('\n');
        updated.push_str(after);
    }
    updated
}

fn remove_hardpass_integration(content: &str) -> (String, bool) {
    let mut updated = String::new();
    let mut found = false;
    let mut skipping_legacy_block = false;

    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if skipping_legacy_block {
            found = true;
            if trimmed == LEGACY_MANAGED_END {
                skipping_legacy_block = false;
            }
            continue;
        }
        if trimmed == LEGACY_MANAGED_BEGIN {
            found = true;
            skipping_legacy_block = true;
            continue;
        }
        if trimmed == MANAGED_INCLUDE_LINE || trimmed == LEGACY_INCLUDE_LINE {
            found = true;
            continue;
        }
        updated.push_str(line);
    }

    (updated, found)
}

fn first_host_or_match_offset(content: &str) -> Option<usize> {
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        if is_scoped_ssh_directive(line) {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn is_scoped_ssh_directive(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return false;
    }
    let Some(keyword) = trimmed.split_whitespace().next() else {
        return false;
    };
    keyword.eq_ignore_ascii_case("host") || keyword.eq_ignore_ascii_case("match")
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

    use super::{
        MANAGED_INCLUDE_LINE, SshAliasEntry, SshConfigManager, install_managed_include,
        render_managed_include,
    };

    #[test]
    fn install_managed_include_inserts_into_empty_config() {
        let content = install_managed_include("");
        assert_eq!(content, format!("{MANAGED_INCLUDE_LINE}\n"));
    }

    #[test]
    fn install_managed_include_replaces_existing_block_idempotently() {
        let initial = "# comment\n\n# >>> hardpass managed ssh include v1 >>>\nInclude ~/.ssh/old\n# <<< hardpass managed ssh include v1 <<<\n";
        let content = install_managed_include(initial);
        assert_eq!(content.matches(MANAGED_INCLUDE_LINE).count(), 1);
        assert_eq!(install_managed_include(&content), content);
    }

    #[test]
    fn install_managed_include_places_include_before_first_host_block() {
        let initial = "# Added by OrbStack\nInclude ~/.orbstack/ssh/config\nInclude ~/.ssh/feathervm/config\n\nHost *.example.com\n  User ubuntu\n";
        let content = install_managed_include(initial);
        let include = content
            .find(MANAGED_INCLUDE_LINE)
            .expect("hardpass include");
        let host = content.find("Host *.example.com").expect("host block");
        assert!(
            include < host,
            "hardpass include must be before first Host block"
        );
        assert!(content.contains("Include ~/.orbstack/ssh/config"));
        assert!(content.contains("Include ~/.ssh/feathervm/config"));
    }

    #[test]
    fn install_managed_include_relocates_existing_block_out_of_host_scope() {
        let initial = "Host *.example.com\n  User ubuntu\n\n# >>> hardpass managed ssh include v1 >>>\nInclude ~/.ssh/old\n# <<< hardpass managed ssh include v1 <<<\n";
        let content = install_managed_include(initial);
        let include = content
            .find(MANAGED_INCLUDE_LINE)
            .expect("hardpass include");
        let host = content.find("Host *.example.com").expect("host block");
        assert!(
            include < host,
            "hardpass include must be before first Host block"
        );
        assert_eq!(content.matches(MANAGED_INCLUDE_LINE).count(), 1);
    }

    #[test]
    fn install_managed_include_relocates_existing_plain_include() {
        let initial = "Host *.example.com\n  User ubuntu\n\nInclude ~/.hardpass/ssh/config\n";
        let content = install_managed_include(initial);
        let include = content
            .find(MANAGED_INCLUDE_LINE)
            .expect("hardpass include");
        let host = content.find("Host *.example.com").expect("host block");
        assert!(include < host);
        assert_eq!(content.matches(MANAGED_INCLUDE_LINE).count(), 1);
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
        assert!(config.contains(MANAGED_INCLUDE_LINE));
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
