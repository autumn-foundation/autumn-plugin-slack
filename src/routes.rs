//! Axum routes for the three Slack surfaces.

use std::sync::{Arc, OnceLock};

use autumn_web::reexports::axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE},
    response::Response,
    routing::post,
};

use crate::engine::Engine;
use crate::metrics::Surface;

/// Route paths under the base path.
pub(crate) const EVENTS: &str = "/events";
pub(crate) const COMMANDS: &str = "/commands";
pub(crate) const INTERACTIONS: &str = "/interactions";

/// The engine, set at startup. Routes reply 503 before that.
pub(crate) type EngineCell = Arc<OnceLock<Arc<Engine>>>;

/// Builds the router. Paths are relative to the base path.
pub(crate) fn router<S>(cell: &EngineCell) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let mut r = Router::new();
    for path in [EVENTS, COMMANDS, INTERACTIONS] {
        let cell = Arc::clone(cell);
        r = r.route(
            path,
            post(move |headers: HeaderMap, body: Body| {
                let cell = Arc::clone(&cell);
                async move { dispatch(&cell, path, headers, body).await }
            }),
        );
    }
    r
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
