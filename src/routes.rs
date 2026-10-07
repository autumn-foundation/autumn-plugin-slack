//! Axum routes for the three Slack surfaces.

use std::sync::{Arc, OnceLock};

use autumn_web::openapi::ApiDoc;
use autumn_web::reexports::axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header::CONTENT_TYPE},
    response::Response,
    routing::{MethodRouter, post},
};
use autumn_web::{AppState, Route};

use crate::engine::Engine;
use crate::metrics::Surface;

/// Route paths under the base path.
pub(crate) const EVENTS: &str = "/events";
pub(crate) const COMMANDS: &str = "/commands";
pub(crate) const INTERACTIONS: &str = "/interactions";

/// The engine, set at startup. Routes reply 503 before that.
pub(crate) type EngineCell = Arc<OnceLock<Arc<Engine>>>;

const PATHS: [(&str, &str); 3] = [
    (EVENTS, "slack_events"),
    (COMMANDS, "slack_commands"),
    (INTERACTIONS, "slack_interactions"),
];

fn handler<S>(cell: &EngineCell, path: &'static str) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    let cell = Arc::clone(cell);
    post(move |headers: HeaderMap, body: Body| {
        let cell = Arc::clone(&cell);
        async move { dispatch(&cell, path, headers, body).await }
    })
}

/// Builds a router. Paths are relative to the base path.
pub(crate) fn router<S>(cell: &EngineCell) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    PATHS.iter().fold(Router::new(), |r, (path, _)| {
        r.route(path, handler(cell, path))
    })
}

/// Builds autumn routes under `base`. autumn needs one or more `Route` to
/// boot, and lists them in `autumn routes`.
///
/// A `Route` path is `&'static str`. The plugin leaks three short strings,
/// once per app build.
pub(crate) fn autumn_routes(cell: &EngineCell, base: &str) -> Vec<Route> {
    PATHS
        .iter()
        .map(|(path, name)| {
            let full: &'static str = Box::leak(format!("{base}{path}").into_boxed_str());
            Route {
                method: Method::POST,
                path: full,
                handler: handler::<AppState>(cell, path),
                name,
                api_doc: ApiDoc {
                    method: "POST",
                    path: full,
                    operation_id: name,
                    // Not an app API. The Slack signature is the access control.
                    hidden: true,
                    public: true,
                    success_status: 200,
                    module_path: module_path!(),
                    source_file: file!(),
                    source_line: line!(),
                    ..ApiDoc::default()
                },
                api_version: None,
                sunset_opt_out: false,
                repository: None,
                idempotency: autumn_web::RouteIdempotency::default(),
                timeout: autumn_web::RouteTimeout::default(),
                seo: autumn_web::seo::SeoRouteDefaults::default(),
            }
        })
        .collect()
}

async fn dispatch(
    cell: &EngineCell,
    path: &'static str,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(engine) = cell.get() else {
        return status(StatusCode::SERVICE_UNAVAILABLE);
    };
    let surface = match path {
        EVENTS => Surface::Events,
        COMMANDS => Surface::Commands,
        _ => Surface::Interactions,
    };
    // Read at most `max_body_bytes`. Verify needs the exact bytes.
    let Ok(bytes) = to_bytes(body, engine.config.max_body_bytes).await else {
        engine.metrics.request(surface, "too_large");
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    };
    match surface {
        Surface::Events => engine.events(&headers, bytes).await,
        Surface::Commands => engine.commands(&headers, bytes).await,
        Surface::Interactions => engine.interactions(&headers, bytes).await,
    }
}

/// A status with no body.
pub(crate) fn status(code: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = code;
    r
}

/// A 200 with a JSON body.
pub(crate) fn json(value: &serde_json::Value) -> Response {
    let mut r = Response::new(Body::from(value.to_string()));
    r.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn unstarted_routes_reply_503() {
        let cell: EngineCell = Arc::new(OnceLock::new());
        let app: Router = router(&cell);
        for path in [EVENTS, COMMANDS, INTERACTIONS] {
            let req = autumn_web::reexports::http::Request::post(path)
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        }
    }
}
