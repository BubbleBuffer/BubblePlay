//! `init`: writes an example `relay.json` with fresh random secrets. It never overwrites a file.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// 32 random bytes, base64url without padding.
pub fn random_secret() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The example file's JSON text. It holds only the fields the operator must set, plus the
/// defaults written out, and no optional blocks.
pub fn example_config_text(token: &str, admin_token: &str) -> String {
    let value = serde_json::json!({
        "public_origin": "https://relay.example.com",
        "listen": "0.0.0.0:8080",
        "assets_dir": "/assets",
        "state_dir": "/data",
        "token": token,
        "admin_token": admin_token,
        "max_connections": crate::config::DEFAULT_MAX_CONNECTIONS,
        "approval_ttl_s": 90,
        "trusted_proxy_cidrs": [],
    });
    let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
    text.push('\n');
    text
}

/// Creates `path` with mode 0600. Fails with `AlreadyExists` when the file is already there.
pub fn write_example_config(path: &Path) -> io::Result<()> {
    let text = example_config_text(&random_secret()?, &random_secret()?);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;

    #[test]
    fn random_secrets_are_distinct_and_unpadded() {
        let a = random_secret().unwrap();
        let b = random_secret().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(!a.contains('='));
    }

    #[test]
    fn example_text_is_a_valid_relay_config() {
        let text = example_config_text("tok-example", "adm-example");
        let cfg = parse_config(&text, &|_: &str| None).expect("example parses");
        assert_eq!(cfg.token.expose(), "tok-example");
        assert_eq!(cfg.admin_token.expose(), "adm-example");
    }

    #[test]
    fn init_writes_a_new_file_and_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.json");
        write_example_config(&path).expect("first write succeeds");
        let first = std::fs::read_to_string(&path).unwrap();
        let err = write_example_config(&path).expect_err("second write must fail");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn init_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relay.json");
        write_example_config(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
