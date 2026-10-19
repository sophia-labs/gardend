//! Who a request acts for, as gardend authenticated it, carried to the records
//! a write leaves behind (the audit log's `actor`).
//!
//! The MCP handler sets it for the duration of one call (`loopback_mcp_routes`),
//! on a gardend that is not a hosted cell: the caller's own principal (`owner`,
//! `token:<id>`, an Access email, `service:<id>`), or, for a delegation token,
//! the person the service acts for, with the service as `via`. A hosted cell
//! sets none, so its records stay as they were (`local-runtime`).
//!
//! It is a task-local: code the call awaits sees it; work it spawns does not.

use std::future::Future;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestActor {
    /// The principal as `X-User-ID` names it (`LoopbackPrincipal::user_id`).
    pub(crate) user_id: String,
    /// The supervised service acting for it (`service:<id>`), if any.
    pub(crate) via: Option<String>,
}

tokio::task_local! {
    static REQUEST_ACTOR: RequestActor;
}

/// The actor of the request this task serves, if one was set.
pub(crate) fn current_request_actor() -> Option<RequestActor> {
    REQUEST_ACTOR.try_with(Clone::clone).ok()
}

/// Run `future` acting for `actor` (as is, when there is none).
pub(crate) async fn acting_for<F: Future>(actor: Option<RequestActor>, future: F) -> F::Output {
    match actor {
        Some(actor) => REQUEST_ACTOR.scope(actor, future).await,
        None => future.await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_actor_is_seen_inside_the_call_only() {
        assert_eq!(current_request_actor(), None);
        let actor = RequestActor {
            user_id: "owner".into(),
            via: Some("service:vehicle".into()),
        };
        let seen = acting_for(Some(actor.clone()), async { current_request_actor() }).await;
        assert_eq!(seen, Some(actor));
        assert_eq!(current_request_actor(), None);
        assert_eq!(
            acting_for(None, async { current_request_actor() }).await,
            None
        );
    }
}
