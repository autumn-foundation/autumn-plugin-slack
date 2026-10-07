//! `[slack]` config from files and env vars.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail loudly.

use autumn_plugin_slack::config::SlackConfig;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("slack-load-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn loads_base_profile_file_and_env_in_order() {
    let dir = temp_dir("order");
    std::fs::write(
        dir.join("autumn.toml"),
        r#"
[slack]
bot_token_env = "BASE_TOKEN"
ack_timeout_ms = 1000

[slack.api]
max_attempts = 4

# Ignored: autumn strict config rejects this section.
[profile.prod.slack]
ack_timeout_ms = 1500
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("autumn-prod.toml"),
        "[slack]\nsigning_secret_env = \"PROD_SECRET\"\nack_timeout_ms = 1200\n",
    )
    .unwrap();
    let env = vec![
        ("AUTUMN_SLACK__API__MAX_ATTEMPTS".to_owned(), "5".to_owned()),
        (
            "AUTUMN_SLACK__RESPONSE_URL_HOSTS".to_owned(),
            "hooks.slack.com, hooks.example".to_owned(),
        ),
        ("OTHER".to_owned(), "x".to_owned()),
    ];
    let c = SlackConfig::load_from_dir(&dir, &["prod".to_owned()], env).unwrap();
    assert_eq!(c.bot_token_env, "BASE_TOKEN");
    assert_eq!(c.ack_timeout_ms, 1200);
    assert_eq!(c.signing_secret_env, "PROD_SECRET");
    assert_eq!(c.api.max_attempts, 5);
    assert_eq!(c.response_url_hosts, ["hooks.slack.com", "hooks.example"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn no_files_gives_defaults() {
    let dir = temp_dir("empty");
    let c = SlackConfig::load_from_dir(&dir, &[], Vec::new()).unwrap();
    assert_eq!(c, SlackConfig::default());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bad_env_value_names_the_variable() {
    let dir = temp_dir("badenv");
    let env = vec![("AUTUMN_SLACK__ACK_TIMEOUT_MS".to_owned(), "soon".to_owned())];
    let e = SlackConfig::load_from_dir(&dir, &[], env).unwrap_err();
    assert!(
        e.to_string().contains("AUTUMN_SLACK__ACK_TIMEOUT_MS"),
        "{e}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
