//! Personal ChatGPT connections for the CLI and web app.

use crate::codex;
use crate::ctx::Ctx;
use crate::db::{q, s};
use crate::error::{AppResult, forbidden, not_found, unavailable};
use crate::http;
use fctl_domain::verify::Sensitive;
use fctl_proto::codex::{
    CodexLoginFinishRequest, CodexLoginStartRequest, CodexSelection, CodexSettings,
};
use serde::Deserialize;
use worker::{Request, Response};

pub async fn dev_seed(ctx: &Ctx) -> AppResult<Response> {
    if !ctx.cfg.codex_enabled || !codex::local_dev(&ctx.cfg) {
        return Err(not_found("Endpoint"));
    }
    let user = ctx.user()?;
    if !user.is_owner() {
        return Err(forbidden(
            "Only the instance owner can create the Codex fixture",
        ));
    }
    http::ok(&codex::dev_seed(&ctx.db, &ctx.keys()?, &user.id, ctx.now).await?)
}

pub async fn settings(ctx: &Ctx) -> AppResult<Response> {
    let user = ctx.user()?;
    if !ctx.cfg.codex_enabled {
        return http::ok(&CodexSettings::default());
    }
    http::ok(&codex::settings(&ctx.db, &user.id, ctx.now).await?)
}

pub async fn login_start(ctx: &mut Ctx, req: &mut Request) -> AppResult<Response> {
    codex::require_enabled(&ctx.cfg)?;
    let user = ctx.user()?.id.clone();
    let body: CodexLoginStartRequest = http::body(req).await?;
    ctx.verified(Sensitive::ConnectCodex)?;
    let keys = ctx.keys()?;
    ctx.rate_limit(&format!("codex-login:{user}")).await?;
    http::ok(&codex::login_start(&ctx.db, &keys, &user, body, ctx.now).await?)
}

pub async fn login_finish(ctx: &mut Ctx, req: &mut Request) -> AppResult<Response> {
    codex::require_enabled(&ctx.cfg)?;
    let user = ctx.user()?.id.clone();
    let body: CodexLoginFinishRequest = http::body(req).await?;
    let connection = codex::login_finish(&ctx.db, &ctx.keys()?, &user, body, ctx.now).await?;
    ctx.audit(
        "codex.connected",
        Some(connection.id.as_str()),
        Some(serde_json::json!({ "plan_enabled": connection.plan_enabled })),
    );
    http::ok(&connection)
}

pub async fn select(
    ctx: &mut Ctx,
    req: &mut Request,
    workspace: Option<&str>,
) -> AppResult<Response> {
    codex::require_enabled(&ctx.cfg)?;
    let user = ctx.user()?.id.clone();
    let body: CodexSelection = http::body(req).await?;
    let workspace = match workspace {
        Some(id) => {
            let access = crate::workspaces::load(ctx, id).await?;
            access.require_open()?;
            Some(access.row.id)
        }
        None => None,
    };
    let connection = body.connection_id.as_ref().map(|id| id.as_str());
    ctx.verified(Sensitive::ConnectCodex)?;
    codex::select(&ctx.db, &user, workspace.as_deref(), connection).await?;
    ctx.audit_in(
        workspace.as_deref(),
        "codex.selection_changed",
        connection,
        None,
    );
    no_content()
}

pub async fn retry(ctx: &mut Ctx, id: &str) -> AppResult<Response> {
    codex::require_enabled(&ctx.cfg)?;
    let user = ctx.user()?.id.clone();
    codex::retry_owned(&ctx.db, &user, id).await?;
    codex::notify_usage_changed(&ctx.env, &ctx.db, id, None).await?;
    crate::schedules::wake(&ctx.env).await;
    ctx.audit("codex.retried", Some(id), None);
    no_content()
}

pub async fn revoke(ctx: &mut Ctx, id: &str) -> AppResult<Response> {
    let user = ctx.user()?.id.clone();
    let revoked = codex::revoke(&ctx.db, &ctx.keys()?, &user, id, stop_sessions(ctx, id)).await?;
    ctx.audit(
        "codex.disconnected",
        Some(id),
        Some(serde_json::json!({ "revoked": revoked })),
    );
    if !revoked {
        return Err(unavailable(
            "This connection was cleared in fctl. Remote revocation could not be confirmed. Disconnect fctl in ChatGPT settings.",
        ));
    }
    no_content()
}

async fn stop_sessions(ctx: &Ctx, connection_id: &str) -> AppResult<()> {
    #[derive(Deserialize)]
    struct Active {
        id: String,
        runner_id: Option<String>,
    }
    let sessions: Vec<Active> = q(
        &ctx.db,
        "SELECT id, runner_id FROM sessions WHERE credential_id = ?
         AND status IN ('queued','provisioning','preparing','running','idle','paused')",
        &[s(connection_id)],
    )?
    .all()
    .await?;
    super::claude::stop_running(
        ctx,
        &sessions
            .iter()
            .map(|row| (row.id.as_str(), row.runner_id.as_deref()))
            .collect::<Vec<_>>(),
    )
    .await;
    Ok(())
}

fn no_content() -> AppResult<Response> {
    Ok(Response::empty()?
        .with_status(204)
        .with_headers(http::api_headers()))
}
