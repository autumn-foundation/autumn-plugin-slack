//! `SlackPlugin` and the started runtime.

use std::borrow::Cow;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use autumn_web::actuator::{HealthIndicator, MetricsSource};
use autumn_web::app::AppBuilder;
use autumn_web::config::AutumnConfig;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_contract::PluginContract;
use autumn_web::reexports::axum::Router;
use autumn_web::time::{ClockSource, SystemClock};
use autumn_web::webhook::{InMemoryWebhookReplayStore, WebhookReplayStore};
use autumn_web::{AppState, AutumnError, ProcessRole};
use tokio_util::task::TaskTracker;

use crate::client::SlackClient;
use crate::config::{SECTION, SlackConfig, env_var};
use crate::engine::Engine;
use crate::error::{HandlerResult, SlackError};
use crate::handlers::{Registration, Registry, Runner, SlackContext, boxed};
use crate::health::SlackHealth;
use crate::metrics::SlackMetrics;
use crate::payload::{
    BlockAction, CommandResponse, EventCallback, Message, Shortcut, SlashCommand, ViewPayload,
    ViewResponse,
};
use crate::routes::{self, COMMANDS, EVENTS, EngineCell, INTERACTIONS};
use crate::transport::{HttpTransport, ReqwestTransport};
use crate::verify::Verifier;

/// Plugin name for duplicate detection.
pub const PLUGIN_NAME: &str = "autumn-plugin-slack";
/// Default base path of the Slack routes.
pub const DEFAULT_BASE_PATH: &str = "/slack";

/// Slack plugin.
///
/// ```rust,ignore
/// autumn_web::app()
///     .plugin(
///         SlackPlugin::new()
///             .on_event("app_mention", on_mention)
///             .command("/deploy", deploy),
///     )
///     .run()
///     .await;
/// ```
pub struct SlackPlugin {
    config: Option<SlackConfig>,
    base_path: String,
    readiness: bool,
    signing_secret: Option<String>,
    previous_secrets: Vec<String>,
    bot_token: Option<String>,
    transport: Option<Arc<dyn HttpTransport>>,
    clock: Option<Arc<dyn ClockSource>>,
    replay: Option<Arc<dyn WebhookReplayStore>>,
    registrations: Vec<Registration>,
    metrics: Arc<SlackMetrics>,
}

impl std::fmt::Debug for SlackPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackPlugin")
            .field("base_path", &self.base_path)
            .field(
                "handlers",
                &self
                    .registrations
                    .iter()
                    .map(Registration::describe)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl Default for SlackPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl SlackPlugin {
    /// Makes the plugin. It reads `[slack]` at startup.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: None,
            base_path: DEFAULT_BASE_PATH.to_owned(),
            readiness: false,
            signing_secret: None,
            previous_secrets: Vec::new(),
            bot_token: None,
            transport: None,
            clock: None,
            replay: None,
            registrations: Vec::new(),
            metrics: Arc::new(SlackMetrics::new()),
        }
    }

    /// Makes the plugin with this config. It reads no files.
    #[must_use]
    pub fn with_config(config: SlackConfig) -> Self {
        Self {
            config: Some(config),
            ..Self::new()
        }
    }

    /// Sets the base path of the routes. Default: `/slack`.
    ///
    /// autumn mounts routes before it loads config. So this is a builder
    /// setting, not a config key.
    #[must_use]
    pub fn base_path(mut self, path: &str) -> Self {
        self.base_path = normalize_base(path);
        self
    }

    /// Puts the health indicator in `/ready` too. Default: only in
    /// `/actuator/health`.
    #[must_use]
    pub const fn readiness(mut self, on: bool) -> Self {
        self.readiness = on;
        self
    }

    /// Uses this signing secret, not the env var.
    #[must_use]
    pub fn with_signing_secret(mut self, secret: impl Into<String>) -> Self {
        self.signing_secret = Some(secret.into());
        self
    }

    /// Also accepts this old signing secret. Use during rotation.
    #[must_use]
    pub fn with_previous_signing_secret(mut self, secret: impl Into<String>) -> Self {
        self.previous_secrets.push(secret.into());
        self
    }

    /// Uses this bot token, not the env var.
    #[must_use]
    pub fn with_bot_token(mut self, token: impl Into<String>) -> Self {
        self.bot_token = Some(token.into());
        self
    }

    /// Uses this transport. Default: [`crate::transport::ReqwestTransport`].
    #[must_use]
    pub fn with_transport(self, transport: impl HttpTransport) -> Self {
        self.with_transport_arc(Arc::new(transport))
    }

    /// Uses this shared transport.
    #[must_use]
    pub fn with_transport_arc(mut self, transport: Arc<dyn HttpTransport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Uses this clock for the timestamp check. Default: the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Uses this store to drop event retries. Default: in memory, per
    /// process. Use a Redis store for many replicas.
    #[must_use]
    pub fn with_replay_store(mut self, store: Arc<dyn WebhookReplayStore>) -> Self {
        self.replay = Some(store);
        self
    }

    /// Runs `handler` for Events API events of `event_type`, after the ack.
    #[must_use]
    pub fn on_event<F, Fut>(mut self, event_type: &str, handler: F) -> Self
    where
        F: Fn(SlackContext, EventCallback) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<()>> + Send + 'static,
    {
        self.registrations
            .push(Registration::Event(event_type.to_owned(), boxed(handler)));
        self
    }

    /// Runs `handler` for the slash command `name`, for example `/deploy`.
    #[must_use]
    pub fn command<F, Fut>(mut self, name: &str, handler: F) -> Self
    where
        F: Fn(SlackContext, SlashCommand) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<CommandResponse>> + Send + 'static,
    {
        self.registrations
            .push(Registration::Command(name.to_owned(), boxed(handler)));
        self
    }

    /// Runs `handler` for a block action with this `action_id`, after the
    /// ack. A returned message goes to the `response_url`.
    #[must_use]
    pub fn action<F, Fut>(mut self, action_id: &str, handler: F) -> Self
    where
        F: Fn(SlackContext, BlockAction) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<Option<Message>>> + Send + 'static,
    {
        self.registrations
            .push(Registration::Action(action_id.to_owned(), boxed(handler)));
        self
    }

    /// Runs `handler` for a modal submit with this `callback_id`.
    #[must_use]
    pub fn view_submission<F, Fut>(mut self, callback_id: &str, handler: F) -> Self
    where
        F: Fn(SlackContext, ViewPayload) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<ViewResponse>> + Send + 'static,
    {
        self.registrations.push(Registration::ViewSubmission(
            callback_id.to_owned(),
            boxed(handler),
        ));
        self
    }

    /// Runs `handler` when a user closes a modal with this `callback_id`.
    /// The modal must set `notify_on_close`.
    #[must_use]
    pub fn view_closed<F, Fut>(mut self, callback_id: &str, handler: F) -> Self
    where
        F: Fn(SlackContext, ViewPayload) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<()>> + Send + 'static,
    {
        self.registrations.push(Registration::ViewClosed(
            callback_id.to_owned(),
            boxed(handler),
        ));
        self
    }

    /// Runs `handler` for a global or message shortcut with this
    /// `callback_id`, after the ack.
    #[must_use]
    pub fn shortcut<F, Fut>(mut self, callback_id: &str, handler: F) -> Self
    where
        F: Fn(SlackContext, Shortcut) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult<()>> + Send + 'static,
    {
        self.registrations.push(Registration::Shortcut(
            callback_id.to_owned(),
            boxed(handler),
        ));
        self
    }

    /// The metrics. They are shared with the started runtime.
    #[must_use]
    pub fn metrics(&self) -> Arc<SlackMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Starts the plugin outside an app build. Use it in tests, with
    /// [`SlackRuntime::router`]. [`Plugin::build`] does the same at startup.
    ///
    /// # Errors
    /// Returns a config error.
    pub fn start(self, state: &AppState) -> Result<SlackRuntime, SlackError> {
        let role = state.role();
        self.start_with_role(state, role)
    }

    /// Starts the plugin as if the process has `role`. A role that serves no
    /// HTTP (`worker`) needs no signing secret and no CSRF exemption.
    ///
    /// # Errors
    /// Same as [`Self::start`].
    pub fn start_with_role(
        self,
        state: &AppState,
        role: ProcessRole,
    ) -> Result<SlackRuntime, SlackError> {
        let health = Arc::new(SlackHealth::new(self.readiness));
        self.start_inner(state, role, health)
    }

    #[allow(clippy::too_many_lines)] // One linear setup sequence.
    fn start_inner(
        self,
        state: &AppState,
        role: ProcessRole,
        health: Arc<SlackHealth>,
    ) -> Result<SlackRuntime, SlackError> {
        let config = match self.config {
            Some(c) => c,
            // autumn sets the state profile from the config; "default" means none.
            None => SlackConfig::load(Some(state.profile()).filter(|p| *p != "default"))?,
        };
        config.validate()?;
        // A worker serves no Slack route, so it needs no inbound checks.
        let inbound = role.serves_http();
        if inbound {
            check_security(&state.config(), &self.base_path)?;
        }
        let registry = Registry::build(self.registrations)?;
        let secret = self
            .signing_secret
            .or_else(|| env_var(&config.signing_secret_env))
            .filter(|s| !s.trim().is_empty());
        let secret = match secret {
            Some(s) => s,
            None if !inbound => String::new(),
            None => {
                return Err(SlackError::Config(format!(
                    "no signing secret; set the env var {}",
                    config.signing_secret_env
                )));
            }
        };
        // An empty secret would let anyone sign. `Verifier` drops it too.
        let mut previous: Vec<String> = self
            .previous_secrets
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        for name in &config.previous_signing_secret_envs {
            if let Some(v) = env_var(name).filter(|s| !s.trim().is_empty()) {
                previous.push(v);
            } else {
                tracing::warn!(env = %name, "slack: previous signing secret env var is not set");
            }
        }
        let token = self
            .bot_token
            .or_else(|| env_var(&config.bot_token_env))
            .filter(|t| !t.trim().is_empty());
        if token.is_none() {
            tracing::warn!(env = %config.bot_token_env, "slack: no bot token; Web API calls fail");
        }
        let transport: Arc<dyn HttpTransport> = match self.transport {
            Some(t) => t,
            None => Arc::new(
                ReqwestTransport::new(Duration::from_millis(config.api.timeout_ms))
                    .map_err(|e| SlackError::Config(e.0))?,
            ),
        };
        let client = SlackClient::new(transport, config.clone(), token, Arc::clone(&self.metrics));
        health.set(
            client.clone(),
            Duration::from_secs(config.health.cache_secs),
        );
        state.insert_extension(client.clone());
        tracing::info!(
            base_path = %self.base_path,
            handlers = registry.len(),
            "slack started"
        );
        let engine = Engine {
            verifier: Verifier::new(&secret, &previous, config.timestamp_tolerance_secs),
            registry,
            client,
            metrics: Arc::clone(&self.metrics),
            clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            replay: self
                .replay
                .unwrap_or_else(|| Arc::new(InMemoryWebhookReplayStore::default())),
            runner: Runner {
                tracker: TaskTracker::new(),
                metrics: Arc::clone(&self.metrics),
            },
            state: state.clone(),
            config,
        };
        Ok(SlackRuntime {
            engine: Arc::new(engine),
            health,
        })
    }
}

/// `"/slack/"` and `"slack"` become `"/slack"`. `"/"` becomes `""` (the root).
fn normalize_base(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

/// The full paths of the three routes.
fn route_paths(base: &str) -> [String; 3] {
    [EVENTS, COMMANDS, INTERACTIONS].map(|p| format!("{base}{p}"))
}

/// Same rule as autumn: an exact match, or a `/`-delimited subtree.
fn is_exempt(path: &str, exempt: &[String]) -> bool {
    exempt.iter().any(|p| {
        let p = p.as_str();
        path == p
            || (path.starts_with(p)
                && (p.ends_with('/') || path.as_bytes().get(p.len()) == Some(&b'/')))
    })
}

/// Fails when CSRF or CAPTCHA would block the Slack routes. A plugin cannot
/// add an exemption, so the error gives the config fix.
fn check_security(cfg: &AutumnConfig, base: &str) -> Result<(), SlackError> {
    // At the root, name the exact paths: "/" would exempt the whole app.
    let fix = if base.is_empty() {
        format!("{:?}", route_paths(base))
    } else {
        format!("[\"{base}\"]")
    };
    let mut csrf_exempt = cfg.security.csrf.exempt_paths.clone();
    csrf_exempt.extend(
        cfg.security
            .webhooks
            .endpoints
            .iter()
            .map(|e| e.path.clone()),
    );
    let [events, commands, interactions] = route_paths(base);
    for path in [&events, &commands, &interactions] {
        if cfg.security.csrf.enabled && !is_exempt(path, &csrf_exempt) {
            return Err(SlackError::Config(format!(
                "CSRF protection blocks {path}. Slack sends no CSRF token. \
                 Add {fix} to [security.csrf] exempt_paths. \
                 The Slack signature check protects these routes."
            )));
        }
    }
    // autumn checks CAPTCHA only on form bodies, and not with `dev_bypass`.
    let bot = &cfg.bot_protection;
    let mut captcha_exempt = cfg.security.captcha_exempt_paths.clone();
    captcha_exempt.extend(
        cfg.security
            .webhooks
            .endpoints
            .iter()
            .map(|e| e.path.clone()),
    );
    for path in [&commands, &interactions] {
        if bot.enabled && !bot.dev_bypass && !is_exempt(path, &captcha_exempt) {
            return Err(SlackError::Config(format!(
                "Bot protection blocks {path}. Slack sends no CAPTCHA token. \
                 Add {fix} to [security] captcha_exempt_paths."
            )));
        }
    }
    Ok(())
}

/// A started plugin.
pub struct SlackRuntime {
    engine: Arc<Engine>,
    health: Arc<SlackHealth>,
}

impl std::fmt::Debug for SlackRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackRuntime")
            .field("in_flight", &self.in_flight())
            .finish_non_exhaustive()
    }
}

impl SlackRuntime {
    /// The Web API client.
    #[must_use]
    pub fn client(&self) -> SlackClient {
        self.engine.client.clone()
    }

    /// The metrics.
    #[must_use]
    pub fn metrics(&self) -> Arc<SlackMetrics> {
        Arc::clone(&self.engine.metrics)
    }

    /// The health indicator.
    #[must_use]
    pub fn health(&self) -> Arc<SlackHealth> {
        Arc::clone(&self.health)
    }

    /// Handlers that run now. Reply tasks do not count.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.engine.metrics.in_flight()
    }

    /// The three routes, relative to the base path. Use it in tests, or to
    /// mount the routes yourself.
    pub fn router(&self) -> Router {
        let cell: EngineCell = Arc::new(OnceLock::new());
        let _ = cell.set(Arc::clone(&self.engine));
        routes::router(&cell)
    }

    /// Stops new handler runs and waits for running handlers, up to
    /// `drain_timeout_secs`. After this, the routes reply 503.
    ///
    /// In an app, autumn calls this at shutdown. autumn gives all shutdown
    /// hooks `[server] shutdown_timeout_secs` in total, so the wait can be
    /// shorter than `drain_timeout_secs`.
    pub async fn shutdown(&self) {
        drain(&self.engine).await;
    }
}

/// Closes the tracker and waits for its tasks, up to the drain timeout.
async fn drain(engine: &Engine) {
    let tracker = &engine.runner.tracker;
    tracker.close();
    let limit = Duration::from_secs(engine.config.drain_timeout_secs);
    if tokio::time::timeout(limit, tracker.wait()).await.is_err() {
        tracing::warn!(
            left = tracker.len(),
            "slack: handlers still run after the drain timeout"
        );
    }
}

impl Plugin for SlackPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(PLUGIN_NAME)
    }

    fn contract(&self) -> Option<PluginContract> {
        Some(
            PluginContract::new(env!("CARGO_PKG_NAME"))
                .plugin_version(env!("CARGO_PKG_VERSION"))
                .autumn_web("0.8"),
        )
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let base = self.base_path.clone();
        let cell: EngineCell = Arc::new(OnceLock::new());
        let health = Arc::new(SlackHealth::new(self.readiness));
        let metrics = Arc::clone(&self.metrics);
        let failure: Arc<Mutex<Option<SlackError>>> = Arc::default();
        let routes = routes::autumn_routes(&cell, &base);
        let (init_cell, init_health, init_failure) =
            (Arc::clone(&cell), Arc::clone(&health), Arc::clone(&failure));
        let (check_cell, stop_cell) = (Arc::clone(&cell), Arc::clone(&cell));
        app.config_section(SECTION)
            .metrics_source("slack", metrics as Arc<dyn MetricsSource>)
            .health_indicator("slack", health as Arc<dyn HealthIndicator>)
            .routes(routes)
            // Start before job workers, so a `#[job]` can use `SlackClient::from_state`.
            .state_initializer(move |state| {
                match self.start_inner(state, state.role(), init_health) {
                    Ok(rt) => {
                        let _ = init_cell.set(rt.engine);
                    }
                    Err(e) => {
                        // Also log: some autumn modes run no startup hook.
                        tracing::error!(error = %e, "slack plugin did not start");
                        *init_failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e);
                    }
                }
            })
            // An initializer cannot fail the boot. This hook reports its error.
            .on_startup(move |_state| {
                let failed = failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                let started = check_cell.get().is_some();
                async move {
                    match failed {
                        Some(e) => Err(AutumnError::internal_server_error(e)),
                        None if !started => Err(AutumnError::internal_server_error(
                            SlackError::Config("the slack plugin did not start".to_owned()),
                        )),
                        None => Ok(()),
                    }
                }
            })
            .on_shutdown(move || shutdown_hook(&stop_cell))
    }
}

/// The autumn shutdown hook: drain the started engine, if any.
fn shutdown_hook(cell: &EngineCell) -> impl Future<Output = ()> + Send + use<> {
    let engine = cell.get().cloned();
    async move {
        if let Some(engine) = engine {
            drain(&engine).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_hook_drains_the_started_engine() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // No engine: the hook ends at once.
        shutdown_hook(&Arc::new(OnceLock::new())).await;
        let rt = SlackPlugin::with_config(SlackConfig::default())
            .with_signing_secret("s")
            .with_transport(crate::transport::MemoryTransport::new())
            .start(&AppState::for_test())
            .unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        let done = Arc::new(AtomicBool::new(false));
        let (g, d) = (Arc::clone(&gate), Arc::clone(&done));
        rt.engine.runner.spawn_task(async move {
            g.notified().await;
            d.store(true, Ordering::SeqCst);
        });
        let cell: EngineCell = Arc::new(OnceLock::new());
        let _ = cell.set(Arc::clone(&rt.engine));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            gate.notify_one();
        });
        shutdown_hook(&cell).await;
        assert!(
            done.load(Ordering::SeqCst),
            "the hook did not wait for the task"
        );
        assert!(rt.engine.runner.tracker.is_closed());
    }

    #[test]
    fn base_path_normalizes() {
        assert_eq!(normalize_base("/slack/"), "/slack");
        assert_eq!(normalize_base("hooks/slack"), "/hooks/slack");
        assert_eq!(normalize_base("/"), "");
        assert_eq!(route_paths("")[0], "/events");
    }

    #[test]
    fn exempt_rule_matches_autumn() {
        let ex = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(is_exempt("/slack/events", &ex(&["/slack"])));
        assert!(is_exempt("/slack/events", &ex(&["/slack/"])));
        assert!(is_exempt("/slack/events", &ex(&["/slack/events"])));
        assert!(!is_exempt("/slack/events", &ex(&["/sla"])));
        assert!(!is_exempt("/slack/events", &ex(&["/slack/e"])));
        // Like autumn: an empty entry exempts all paths.
        assert!(is_exempt("/slack/events", &ex(&[""])));
    }

    #[test]
    fn webhook_endpoint_paths_count_as_csrf_exempt() {
        let mut cfg = AutumnConfig::default();
        cfg.security.csrf.enabled = true;
        assert!(check_security(&cfg, "/slack").is_err());
        for p in route_paths("/slack") {
            cfg.security.webhooks.endpoints.push(
                autumn_web::webhook::WebhookEndpointConfig::slack("s", p, "S"),
            );
        }
        check_security(&cfg, "/slack").unwrap();
    }
}
