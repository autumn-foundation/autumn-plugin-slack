//! Boots a real app (not `TestApp`) with only the Slack plugin.
//!
//! Regression: autumn refuses to start with no `Route`. The plugin must give
//! real routes, not only a nested router.
//!
//! The test runs this test binary again as a child process, with the env
//! vars set. So it changes no env in this process.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test helpers fail loudly.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use autumn_plugin_slack::{SlackPlugin, testing};

const SECRET: &str = "boot-test-secret";
const CHILD_MARK: &str = "SLACK_BOOT_TEST_CHILD";

/// The server. It runs only in the child process.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "runs only as the child of slack_only_app_boots_and_serves_signed_requests"]
async fn boot_child_server() {
    if std::env::var(CHILD_MARK).is_err() {
        return;
    }
    autumn_web::app().plugin(SlackPlugin::new()).run().await;
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn slack_only_app_boots_and_serves_signed_requests() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut child = KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "boot_child_server",
                "--include-ignored",
                "--nocapture",
            ])
            .env(CHILD_MARK, "1")
            .env("AUTUMN_SERVER__PORT", port.to_string())
            .env("AUTUMN_SERVER__HOST", "127.0.0.1")
            .env("SLACK_SIGNING_SECRET", SECRET)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let body = r#"{"type":"url_verification","challenge":"boot"}"#;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://127.0.0.1:{port}/slack/events");
    let start = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let mut err = String::new();
            if let Some(mut e) = child.0.stderr.take() {
                std::io::Read::read_to_string(&mut e, &mut err).unwrap();
            }
            panic!("the app stopped at boot ({status}):\n{err}");
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the app did not answer"
        );
        let ts = u64::try_from(autumn_web::reexports::chrono::Utc::now().timestamp()).unwrap();
        let [(h1, v1), (h2, v2)] = testing::signed_headers(SECRET, ts, body.as_bytes());
        if let Ok(res) = client
            .post(&url)
            .header(h1, v1)
            .header(h2, v2)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
        {
            assert_eq!(res.status().as_u16(), 200);
            assert!(res.text().await.unwrap().contains("boot"));
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
