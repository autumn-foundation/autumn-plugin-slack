//! Slack plugin for autumn-web.
//!
//! - **Inbound:** verified Events API, slash commands, and interactivity
//!   routes. Handlers by event type, command, `action_id`, or `callback_id`.
//! - **Outbound:** a Web API client with retry rules, and `response_url`
//!   replies.
//! - **Operations:** health indicator, Prometheus metrics, drain on shutdown.
//!
//! See the README for setup.

mod client;
pub mod config;
mod engine;
mod error;
mod handlers;
mod health;
mod metrics;
pub mod payload;
mod plugin;
pub mod policy;
mod routes;
pub mod testing;
pub mod transport;
mod verify;

pub use client::{AuthTest, MessageRef, SlackClient};
pub use config::SlackConfig;
pub use error::{HandlerError, HandlerResult, SlackError};
pub use handlers::SlackContext;
pub use health::SlackHealth;
pub use metrics::SlackMetrics;
pub use plugin::{DEFAULT_BASE_PATH, PLUGIN_NAME, SlackPlugin, SlackRuntime};
pub use verify::{SIGNATURE_HEADER, TIMESTAMP_HEADER, Verifier, VerifyError};
