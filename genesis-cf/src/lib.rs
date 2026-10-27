//! Cloudflare Workers shell for the Safe Genesis Registry service.
//!
//! Reads (genesis lookups, per-chain status, replay checks, import
//! resolution) run in the stateless Worker; the sponsored-import queue lives
//! in one Durable Object ([`submitter::SubmitterDo`]). Every decision is
//! `genesis_rules`'.

mod config;
mod edge;
mod reads;
mod rpc;
mod signer;
mod submit;
mod submitter;
mod telegram;

pub use submitter::SubmitterDo;

use worker::{Context, Env, Method, Request, Response, Result, ScheduleContext, ScheduledEvent, event};

use edge::{Edge, apply_cors, error_response};

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    if req.method() == Method::Options {
        let mut r = Response::empty()?.with_status(204);
        apply_cors(&mut r)?;
        return Ok(r);
    }
    let mut response = match Edge::new(env) {
        Ok(edge) => match edge.route(req).await {
            Ok(r) => r,
            // A path parameter that is not an address is the caller's error;
            // anything else is ours.
            Err(e) if e.to_string().contains("not an address") => error_response(400, "not an address")?,
            Err(_) => error_response(500, "internal error")?,
        },
        Err(e) => error_response(500, &format!("service misconfigured: {e}"))?,
    };
    apply_cors(&mut response)?;
    Ok(response)
}

/// The maintenance tick: re-arms a lost submitter alarm and evaluates alerts.
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    let kicked = async { env.durable_object("SUBMITTER")?.id_from_name("v1")?.get_stub()?.fetch_with_str("https://do/maintain").await };
    if kicked.await.is_err() {
        worker::console_warn!("could not reach the submitter");
    }
}
