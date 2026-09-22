//! The review queue: one read, one decide, and the dismissed-pair ledger.
//!
//! Grant checks and the key grammar live in `services::review_queue`; this file authenticates,
//! parses the query and publishes the service's own error code.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;

use super::{authed, domain_error, Http};
use crate::domain::errors::DomainError;
use crate::services::review_queue::{self, Decision, QueueQuery, Source};

/// Publishes the service's own code when it set one, the fallback otherwise. One call site so a
/// handler cannot drift onto a hardcoded string that would pass a test asserting the wrong thing.
fn refusal(e: &DomainError, fallback: &'static str) -> Response {
    domain_error(e, e.code().unwrap_or(fallback))
}

pub fn routes() -> Router<Http> {
    Router::new()
        .route("/admin/review/queue", get(queue))
        .route("/admin/review/decide", post(decide))
        .route("/admin/review/dismissed", get(dismissed))
        .route("/admin/review/dismissed/{a}/{b}", delete(undismiss))
}

#[derive(Deserialize)]
struct QueueParams {
    source: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
    days: Option<i32>,
    min_similarity: Option<f64>,
}

/// A comma list of `conflict`, `stale`, `proposal`. Absent or empty means every source the server
/// fills, which the service reads as `None` rather than an explicit empty list.
fn parse_sources(raw: &str) -> Result<Option<Vec<Source>>, Response> {
    let words: Vec<&str> = raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if words.is_empty() {
        return Ok(None);
    }
    let mut sources = Vec::with_capacity(words.len());
    for word in words {
        let source = match word {
            "conflict" => Source::Conflict,
            "stale" => Source::Stale,
            "proposal" => Source::Proposal,
            other => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": review_queue::codes::UNKNOWN_SOURCE,
                        "detail": other,
                    })),
                )
                    .into_response())
            }
        };
        sources.push(source);
    }
    Ok(Some(sources))
}

async fn queue(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<QueueParams>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let sources = match q.source.as_deref().map(parse_sources).transpose() {
        Ok(s) => s.flatten(),
        Err(r) => return r,
    };
    let query = QueueQuery {
        sources,
        limit: q.limit,
        offset: q.offset,
        days: q.days,
        min_similarity: q.min_similarity,
    };
    match review_queue::queue(&ctx, &http.state.proposals, query).await {
        Ok(envelope) => Json(envelope).into_response(),
        Err(e) => refusal(&e, "review_queue_failed"),
    }
}

async fn decide(
    State(http): State<Http>,
    headers: HeaderMap,
    Json(body): Json<Decision>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match review_queue::decide(&ctx, &http.state.proposals, body).await {
        Ok(decided) => Json(decided).into_response(),
        Err(e) => refusal(&e, "review_decide_failed"),
    }
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

async fn dismissed(
    State(http): State<Http>,
    headers: HeaderMap,
    Query(q): Query<LimitQuery>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match review_queue::dismissed(&ctx, q.limit).await {
        Ok(pairs) => Json(serde_json::json!({ "pairs": pairs })).into_response(),
        Err(e) => refusal(&e, "review_dismissed_failed"),
    }
}

async fn undismiss(
    State(http): State<Http>,
    headers: HeaderMap,
    Path((a, b)): Path<(String, String)>,
) -> Response {
    let ctx = match authed(&http, &headers).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    match review_queue::undismiss(&ctx, &a, &b).await {
        Ok(removed) => Json(serde_json::json!({ "removed": removed })).into_response(),
        Err(e) => refusal(&e, "review_undismiss_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_list_parses_and_refuses_an_unknown_word() {
        assert_eq!(parse_sources("").unwrap(), None);
        assert_eq!(
            parse_sources("conflict, stale").unwrap(),
            Some(vec![Source::Conflict, Source::Stale])
        );
        let refused = parse_sources("conflict,nonsense").unwrap_err();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    }

    async fn error_body(r: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn a_coded_refusal_publishes_its_own_code_and_an_uncoded_one_the_fallback() {
        let coded =
            DomainError::validation("bad key").with_code(review_queue::codes::NOT_A_QUEUE_KEY);
        let body = error_body(refusal(&coded, "review_decide_failed")).await;
        assert_eq!(body["error"], review_queue::codes::NOT_A_QUEUE_KEY);

        let uncoded = DomainError::internal("boom");
        let body = error_body(refusal(&uncoded, "review_decide_failed")).await;
        assert_eq!(body["error"], "review_decide_failed");
    }
}
