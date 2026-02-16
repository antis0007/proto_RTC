use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Settings {
    pub server_bind: String,
    pub database_url: String,
    pub server_public_url: Option<String>,
    pub livekit_api_key: String,
    pub livekit_api_secret: String,
    pub livekit_url: Option<String>,
    pub livekit_ttl_seconds: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_bind: "127.0.0.1:8443".into(),
            database_url: "sqlite://./data/server.db".into(),
            server_public_url: None,
            livekit_api_key: "devkey".into(),
            livekit_api_secret: "devsecret".into(),
            livekit_url: None,
            livekit_ttl_seconds: 3600,
        }
    }
}

pub fn load_settings() -> Settings {
    let mut settings = Settings::default();

    if let Ok(raw) = fs::read_to_string("server.toml") {
        if let Ok(file_cfg) = toml::from_str::<HashMap<String, String>>(&raw) {
            if let Some(v) = file_cfg.get("bind_addr") {
                settings.server_bind = v.clone();
            }
            if let Some(v) = file_cfg.get("database_url") {
                settings.database_url = v.clone();
            }
            if let Some(v) = file_cfg.get("server_public_url") {
                settings.server_public_url = Some(v.clone());
            }
        }
    }

    if let Ok(v) = std::env::var("SERVER_BIND") {
        settings.server_bind = v;
    }
    if let Ok(v) = std::env::var("APP__BIND_ADDR") {
        settings.server_bind = v;
    }

    if let Ok(v) = std::env::var("DATABASE_URL") {
        settings.database_url = v;
    }
    if let Ok(v) = std::env::var("APP__DATABASE_URL") {
        settings.database_url = v;
    }

    if let Ok(v) = std::env::var("SERVER_PUBLIC_URL") {
        settings.server_public_url = Some(v);
    }

    if let Ok(v) = std::env::var("LIVEKIT_API_KEY") {
        settings.livekit_api_key = v;
    }
    if let Ok(v) = std::env::var("APP__LIVEKIT_API_KEY") {
        settings.livekit_api_key = v;
    }

    if let Ok(v) = std::env::var("LIVEKIT_API_SECRET") {
        settings.livekit_api_secret = v;
    }
    if let Ok(v) = std::env::var("APP__LIVEKIT_API_SECRET") {
        settings.livekit_api_secret = v;
    }

    if let Ok(v) = std::env::var("LIVEKIT_URL") {
        settings.livekit_url = Some(v);
    }
    if let Ok(v) = std::env::var("APP__LIVEKIT_URL") {
        settings.livekit_url = Some(v);
    }

    if let Ok(v) = std::env::var("APP__LIVEKIT_TTL_SECONDS") {
        if let Ok(parsed) = v.parse::<i64>() {
            settings.livekit_ttl_seconds = parsed;
        }
    }

    settings
}

pub fn prepare_database_url(raw_database_url: &str) -> anyhow::Result<String> {
    let database_url = normalize_database_url(raw_database_url);
    ensure_parent_dir_exists(&database_url)?;
    Ok(database_url)
}

fn normalize_database_url(raw_database_url: &str) -> String {
    let raw_database_url = raw_database_url.trim();

    if raw_database_url.is_empty() {
        return Settings::default().database_url;
    }

    if raw_database_url.starts_with("sqlite::memory:")
        || raw_database_url.starts_with("sqlite://")
        || raw_database_url.contains("://")
    {
        return raw_database_url.to_string();
    }

    if let Some(path) = raw_database_url.strip_prefix("sqlite:") {
        let path = path.replace('\\', "/");
        return format!("sqlite://{path}");
    }

    format!("sqlite://{}", raw_database_url.replace('\\', "/"))
}

fn ensure_parent_dir_exists(database_url: &str) -> anyhow::Result<()> {
    let Some(path) = sqlite_path(database_url) else {
        return Ok(());
    };

    let Some(parent) = path.parent() else {
        return Ok(());
    };

    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create parent directory '{}' for database url '{database_url}'",
            parent.display()
        )
    })?;

    Ok(())
}

fn sqlite_path(database_url: &str) -> Option<PathBuf> {
    if database_url == "sqlite::memory:" || !database_url.starts_with("sqlite:") {
        return None;
    }

    let path = database_url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:")
        .split('?')
        .next()
        .unwrap_or_default();

    if path.is_empty() {
        return None;
    }

    Some(Path::new(path).to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::{env, time::{SystemTime, UNIX_EPOCH}};

    use super::*;

    #[test]
    fn normalizes_plain_file_path_to_sqlite_url() {
        assert_eq!(
            normalize_database_url("./data/test.db"),
            "sqlite://./data/test.db"
        );
    }

    #[test]
    fn creates_parent_dir_for_relative_sqlite_url() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();

        let temp_root = env::temp_dir().join(format!("proto_rtc_server_test_{suffix}"));
        fs::create_dir_all(&temp_root).expect("temp root");

        let original_dir = env::current_dir().expect("cwd");
        env::set_current_dir(&temp_root).expect("set cwd");

        prepare_database_url("./data/test.db").expect("prepare db url");
        assert!(temp_root.join("data").exists());

        env::set_current_dir(original_dir).expect("restore cwd");
        fs::remove_dir_all(temp_root).expect("cleanup");
    }
}
