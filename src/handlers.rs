//! Handler types, the registry, and the task runner.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt as _;

use autumn_web::AppState;
use futures::future::BoxFuture;
use tokio_util::task::TaskTracker;

use crate::client::SlackClient;
use crate::error::{HandlerResult, SlackError};
use crate::metrics::{SlackMetrics, Surface};
use crate::payload::{
    BlockAction, CommandResponse, EventCallback, Message, Shortcut, SlashCommand, ViewPayload,
    ViewResponse,
};

/// What a handler gets besides the payload.
#[derive(Clone, Debug)]
pub struct SlackContext {
    client: SlackClient,
    state: AppState,
}

impl SlackContext {
    pub(crate) const fn new(client: SlackClient, state: AppState) -> Self {
        Self { client, state }
    }

    /// The Web API client.
    #[must_use]
    pub const fn client(&self) -> &SlackClient {
        &self.client
    }

    /// The app state.
    #[must_use]
    pub const fn state(&self) -> &AppState {
        &self.state
    }
}

/// A boxed handler.
pub(crate) type Handler<T, R> =
    Arc<dyn Fn(SlackContext, T) -> BoxFuture<'static, HandlerResult<R>> + Send + Sync>;

/// Boxes a handler function.
pub(crate) fn boxed<T, R, F, Fut>(f: F) -> Handler<T, R>
where
    F: Fn(SlackContext, T) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HandlerResult<R>> + Send + 'static,
{
    Arc::new(move |ctx, payload| Box::pin(f(ctx, payload)))
}

/// One registration from the builder.
pub(crate) enum Registration {
    Event(String, Handler<EventCallback, ()>),
    Command(String, Handler<SlashCommand, CommandResponse>),
    Action(String, Handler<BlockAction, Option<Message>>),
    ViewSubmission(String, Handler<ViewPayload, ViewResponse>),
    ViewClosed(String, Handler<ViewPayload, ()>),
    Shortcut(String, Handler<Shortcut, ()>),
}

impl Registration {
    /// Kind and key, for logs and errors.
    pub(crate) fn describe(&self) -> (&'static str, &str) {
        match self {
            Self::Event(k, _) => ("event", k),
            Self::Command(k, _) => ("command", k),
            Self::Action(k, _) => ("action", k),
            Self::ViewSubmission(k, _) => ("view_submission", k),
            Self::ViewClosed(k, _) => ("view_closed", k),
            Self::Shortcut(k, _) => ("shortcut", k),
        }
    }
}

/// Handlers by key.
#[derive(Default)]
pub(crate) struct Registry {
    pub events: HashMap<String, Handler<EventCallback, ()>>,
    pub commands: HashMap<String, Handler<SlashCommand, CommandResponse>>,
    pub actions: HashMap<String, Handler<BlockAction, Option<Message>>>,
    pub view_submissions: HashMap<String, Handler<ViewPayload, ViewResponse>>,
    pub view_closed: HashMap<String, Handler<ViewPayload, ()>>,
    pub shortcuts: HashMap<String, Handler<Shortcut, ()>>,
}

impl Registry {
    /// Builds the registry.
    ///
    /// # Errors
    /// Returns [`SlackError::Config`] for an empty key, a command with no
    /// leading `/`, or a key that is registered twice.
    pub(crate) fn build(regs: Vec<Registration>) -> Result<Self, SlackError> {
        let mut r = Self::default();
        for reg in regs {
            let (kind, key) = reg.describe();
            if key.trim().is_empty() || key.chars().any(char::is_whitespace) {
                return Err(SlackError::Config(format!(
                    "{kind} key {key:?} is empty or has a space"
                )));
            }
            if kind == "command" && !key.starts_with('/') {
                return Err(SlackError::Config(format!(
                    "command {key:?} must start with /"
                )));
            }
            let dup = |kind: &str, key: &str| {
                SlackError::Config(format!("{kind} {key} is registered twice"))
            };
            match reg {
                Registration::Event(k, h) => insert(&mut r.events, k, h, |k| dup("event", k))?,
                Registration::Command(k, h) => {
                    insert(&mut r.commands, k, h, |k| dup("command", k))?;
                }
                Registration::Action(k, h) => insert(&mut r.actions, k, h, |k| dup("action", k))?,
                Registration::ViewSubmission(k, h) => {
                    insert(&mut r.view_submissions, k, h, |k| dup("view_submission", k))?;
                }
                Registration::ViewClosed(k, h) => {
                    insert(&mut r.view_closed, k, h, |k| dup("view_closed", k))?;
                }
                Registration::Shortcut(k, h) => {
                    insert(&mut r.shortcuts, k, h, |k| dup("shortcut", k))?;
                }
            }
        }
        Ok(r)
    }

    /// Number of handlers.
    pub(crate) fn len(&self) -> usize {
        self.events.len()
            + self.commands.len()
            + self.actions.len()
            + self.view_submissions.len()
            + self.view_closed.len()
            + self.shortcuts.len()
    }
}

fn insert<V>(
    map: &mut HashMap<String, V>,
    key: String,
    value: V,
    dup: impl Fn(&str) -> SlackError,
) -> Result<(), SlackError> {
    if map.contains_key(&key) {
        return Err(dup(&key));
    }
    map.insert(key, value);
    Ok(())
}

/// How a handler run ended.
pub(crate) enum RunOutcome<R> {
    Ok(R),
    Failed,
    Panic,
}

impl<R> RunOutcome<R> {
    /// The value of a run that ended well.
    pub(crate) fn into_ok(self) -> Option<R> {
        match self {
            Self::Ok(r) => Some(r),
            Self::Failed | Self::Panic => None,
        }
    }
}

/// Spawns handler runs. Counts them. Catches panics.
#[derive(Clone)]
pub(crate) struct Runner {
    pub tracker: TaskTracker,
    pub metrics: Arc<SlackMetrics>,
}

impl Runner {
    /// Spawns `handler(ctx, payload)` as a tracked task.
    pub(crate) fn spawn<T, R>(
        &self,
        surface: Surface,
        handler: Handler<T, R>,
        ctx: SlackContext,
        payload: T,
    ) -> tokio::task::JoinHandle<RunOutcome<R>>
    where
        T: Send + 'static,
        R: Send + 'static,
    {
        let guard = InFlight::new(Arc::clone(&self.metrics));
        self.tracker.spawn(async move {
            // The call is in the block, so a panic before the first poll is caught too.
            let out = AssertUnwindSafe(async move { handler(ctx, payload).await })
                .catch_unwind()
                .await;
            let metrics = Arc::clone(&guard.metrics);
            drop(guard);
            match out {
                Ok(Ok(r)) => {
                    metrics.handler(surface, "ok");
                    RunOutcome::Ok(r)
                }
                Ok(Err(e)) => {
                    metrics.handler(surface, "error");
                    tracing::warn!(surface = surface.as_str(), error = %e, "slack handler failed");
                    RunOutcome::Failed
                }
                Err(_) => {
                    metrics.handler(surface, "panic");
                    tracing::error!(surface = surface.as_str(), "slack handler panicked");
                    RunOutcome::Panic
                }
            }
        })
    }

    /// Spawns a tracked task with no handler, for a late reply.
    pub(crate) fn spawn_task(&self, fut: impl Future<Output = ()> + Send + 'static) {
        drop(self.tracker.spawn(fut));
    }
}

/// Counts one running handler. The drop also runs when the task is aborted.
struct InFlight {
    metrics: Arc<SlackMetrics>,
}

impl InFlight {
    fn new(metrics: Arc<SlackMetrics>) -> Self {
        metrics.in_flight_add(1);
        Self { metrics }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.metrics.in_flight_add(-1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_cmd() -> Handler<SlashCommand, CommandResponse> {
        boxed(|_c, _p| async { Ok(CommandResponse::Ack) })
    }

    #[test]
    fn registry_rules() {
        let r = Registry::build(vec![
            Registration::Command("/a".into(), ok_cmd()),
            Registration::Event("message".into(), boxed(|_c, _p| async { Ok(()) })),
        ])
        .unwrap();
        assert_eq!(r.len(), 2);
        for bad in ["", "/a b", "a"] {
            let e = Registry::build(vec![Registration::Command(bad.into(), ok_cmd())])
                .err()
                .unwrap();
            assert!(matches!(e, SlackError::Config(_)), "{bad}");
        }
        let e = Registry::build(vec![
            Registration::Shortcut("s".into(), boxed(|_c, _p| async { Ok(()) })),
            Registration::Shortcut("s".into(), boxed(|_c, _p| async { Ok(()) })),
        ])
        .err()
        .unwrap();
        assert!(e.to_string().contains("shortcut s"), "{e}");
    }
}
