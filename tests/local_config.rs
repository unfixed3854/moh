use std::{path::Path, time::Duration};

use moh::local::{MohConfig, ServerConfig};

#[test]
fn missing_config_uses_fifteen_minute_idle_timeout() {
    assert_eq!(
        ServerConfig::default().idle_timeout,
        Duration::from_secs(15 * 60)
    );
}

#[test]
fn config_parses_human_duration_and_rejects_unknown_keys() {
    let config = MohConfig::parse(
        "[server]\nidle_timeout = \"45s\"\n",
        Path::new("/tmp/config.toml"),
    )
    .unwrap();
    assert_eq!(config.server.idle_timeout, Duration::from_secs(45));

    let error = MohConfig::parse(
        "[server]\nidle_timeout = \"15m\"\nidle_timout = \"1s\"\n",
        Path::new("/tmp/config.toml"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("idle_timout"));
    assert!(error.to_string().contains("/tmp/config.toml"));
}

#[test]
fn config_rejects_zero_idle_timeout() {
    let error = MohConfig::parse(
        "[server]\nidle_timeout = \"0s\"\n",
        Path::new("/tmp/config.toml"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("idle_timeout"));
    assert!(error.to_string().contains("/tmp/config.toml"));
}

#[test]
fn config_rejects_malformed_toml_with_its_path() {
    let error = MohConfig::parse(
        "[server\nidle_timeout = \"15m\"\n",
        Path::new("/tmp/config.toml"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("/tmp/config.toml"));
}

#[test]
fn config_errors_do_not_expose_unrelated_values() {
    let error = MohConfig::parse(
        "[server]\nidle_timeout = { invalid = \"45s\", unrelated = \"sentinel-secret-value\" }\n",
        Path::new("/tmp/config.toml"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("/tmp/config.toml"));
    assert!(!error.to_string().contains("sentinel-secret-value"));
}

#[test]
fn missing_config_file_loads_defaults() {
    let temporary_directory = tempfile::tempdir().unwrap();
    let path = temporary_directory.path().join("missing-config.toml");

    let config = MohConfig::load(&path).unwrap();

    assert_eq!(config.server.idle_timeout, Duration::from_secs(15 * 60));
}
