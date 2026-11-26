//! The control plane owns ChatGPT registrations and renewable credentials.

use crate::config::Config;
use crate::crypto::{self, Keys};
use crate::ctx::now_ms;
use crate::db::{Query, n, on, os, q, s};
use crate::error::{
    AppError, AppResult, conflict, forbidden, internal, not_configured, not_found, unavailable,
    validation,
};
use fctl_domain::ids::{credential_id, uuid_v4};
use fctl_domain::webauthn::PublicKey;
use fctl_proto::codex::{
    CodexConnection, CodexLoginFinishRequest, CodexLoginStart, CodexLoginStartRequest,
    CodexSettings, CodexWorkspaceConnection,
};
use fctl_proto::runner::CodexCredential;
use fctl_proto::{ApiError, CredentialId, ErrorCode};
use serde::{Deserialize, Serialize};
use std::future::Future;
use url::Url;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use worker::{
    D1Database, Env, Fetch, Headers, Method, Request, RequestInit, RequestRedirect, Response,
};

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS_URL: &str = "https://auth.openai.com/.well-known/jwks.json";
const REVOKE_URL: &str = "https://auth.openai.com/api/accounts/oauth/revoke";
const RESOURCE: &str = "https://api.openai.com/v1";
const REGISTRATION_CLIENT: &str = "dynamic_agent_client";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const REQUESTED_SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const CALLBACK_PATH: &str = "/auth/callback";
const STATE_TTL_MS: i64 = 10 * 60 * 1000;
const OAUTH_TIMEOUT_MS: u32 = 10_000;
const OPERATION_TTL_MS: i64 = 60_000;
const RENEW_BEFORE_MS: i64 = 60_000;
const MAX_ACCESS_LIFETIME_SECS: i64 = 3600;
const DEV_CREDENTIAL: &str = "crd_dev_codex";
const DEV_CLIENT: &str = "fctl_dev_codex";

pub fn require_enabled(config: &Config) -> AppResult<()> {
    if config.codex_enabled {
        Ok(())
    } else {
        Err(not_configured("Codex is disabled on this instance"))
    }
}

pub fn local_dev(cfg: &Config) -> bool {
    cfg.dev
        && Url::parse(&cfg.public_url).is_ok_and(|url| match url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain(domain)) => domain == "localhost",
            None => false,
        })
}

pub async fn dev_seed(
    db: &D1Database,
    keys: &Keys,
    user_id: &str,
    now: i64,
) -> AppResult<CodexConnection> {
    let sealed = keys.seal_json(
        &credential_aad(DEV_CREDENTIAL, user_id, DEV_CLIENT),
        &Tokens {
            access_token: "fctl-dev-codex-access".into(),
            refresh_token: None,
            id_token: String::new(),
            scope: PLAN_SCOPE.into(),
            expires_at: now + MAX_ACCESS_LIFETIME_SECS * 1000,
        },
    )?;
    let changes = crate::db::batch_changes(
        db,
        vec![
            q(
                db,
                "INSERT INTO codex_credentials (id, user_id, client_id, subject, email, status, secret_enc, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'dev', 'codex@fctl.test', 'active', ?4, ?5, ?5)
                 ON CONFLICT (id) DO UPDATE SET status = 'active', secret_enc = ?4, paused = 0,
                 reset_at = NULL, updated_at = ?5
                 WHERE client_id = ?3 AND user_id = ?2 AND operation_id IS NULL",
                &[s(DEV_CREDENTIAL), s(user_id), s(DEV_CLIENT), s(&sealed), n(now)],
            )?,
            choose_first(db, user_id, DEV_CREDENTIAL)?,
        ],
    ).await?;
    if changes.first() != Some(&1) {
        return Err(connection_changed());
    }
    Ok(owned_credential(db, user_id, DEV_CREDENTIAL)
        .await?
        .connection(now))
}

#[derive(Deserialize)]
struct CredentialRow {
    id: String,
    user_id: String,
    client_id: String,
    subject: String,
    email: Option<String>,
    status: String,
    secret_enc: Option<String>,
    #[serde(deserialize_with = "crate::db::int_bool")]
    paused: bool,
    #[serde(deserialize_with = "crate::db::opt_int")]
    reset_at: Option<i64>,
    #[serde(deserialize_with = "crate::db::int")]
    generation: i64,
    operation_id: Option<String>,
}

impl CredentialRow {
    fn aad(&self) -> String {
        credential_aad(&self.id, &self.user_id, &self.client_id)
    }

    fn secret(&self, keys: &Keys) -> AppResult<Option<Tokens>> {
        self.secret_enc
            .as_deref()
            .map(|sealed| keys.open_json(&self.aad(), sealed))
            .transpose()
    }

    fn paused_at(&self, now: i64) -> bool {
        self.paused && self.reset_at.is_none_or(|until| until > now)
    }

    fn connection(&self, now: i64) -> CodexConnection {
        let paused = self.status == "active" && self.paused_at(now);
        CodexConnection {
            id: CredentialId(self.id.clone()),
            email: self.email.clone(),
            connected: matches!(self.status.as_str(), "active" | "signed_in"),
            plan_enabled: self.status == "active",
            paused,
            reset_at: self.reset_at.filter(|_| paused),
        }
    }
}

fn credential_aad(id: &str, user_id: &str, client_id: &str) -> String {
    format!("codex:credential:{id}:{user_id}:{client_id}")
}

#[derive(Deserialize)]
struct Owner {
    user_id: String,
}

pub async fn owner(db: &D1Database, id: &str) -> AppResult<Option<String>> {
    let row: Option<Owner> = q(
        db,
        "SELECT user_id FROM codex_credentials WHERE id = ?",
        &[s(id)],
    )?
    .first()
    .await?;
    Ok(row.map(|row| row.user_id))
}

async fn owned_credential(db: &D1Database, user_id: &str, id: &str) -> AppResult<CredentialRow> {
    q(
        db,
        "SELECT * FROM codex_credentials WHERE id = ? AND user_id = ?",
        &[s(id), s(user_id)],
    )?
    .first()
    .await?
    .ok_or_else(|| not_found("ChatGPT connection"))
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

pub async fn credential_for(
    db: &D1Database,
    user_id: &str,
    workspace_id: &str,
) -> AppResult<Option<String>> {
    let row: Option<Id> = q(
        db,
        "SELECT c.id FROM users u
         LEFT JOIN workspace_members m ON m.user_id = u.id AND m.workspace_id = ?2
         JOIN codex_credentials c ON c.id = COALESCE(m.codex_credential_id, u.default_codex_credential_id)
         WHERE u.id = ?1 AND u.status = 'active' AND c.user_id = u.id AND c.status = 'active'",
        &[s(user_id), s(workspace_id)],
    )?
    .first()
    .await?;
    Ok(row.map(|row| row.id))
}

pub async fn settings(db: &D1Database, user_id: &str, now: i64) -> AppResult<CodexSettings> {
    #[derive(Deserialize)]
    struct DefaultConnection {
        default_codex_credential_id: Option<CredentialId>,
    }
    let rows: Vec<CredentialRow> = q(
        db,
        "SELECT * FROM codex_credentials WHERE user_id = ? ORDER BY created_at, id",
        &[s(user_id)],
    )?
    .all()
    .await?;
    let default: Option<DefaultConnection> = q(
        db,
        "SELECT default_codex_credential_id FROM users WHERE id = ?",
        &[s(user_id)],
    )?
    .first()
    .await?;
    let workspace_connections = q(
        db,
        "SELECT workspace_id, codex_credential_id AS connection_id FROM workspace_members
         WHERE user_id = ? AND codex_credential_id IS NOT NULL ORDER BY workspace_id",
        &[s(user_id)],
    )?
    .all::<CodexWorkspaceConnection>()
    .await?;
    Ok(CodexSettings {
        enabled: true,
        connections: rows.iter().map(|row| row.connection(now)).collect(),
        default_connection_id: default.and_then(|row| row.default_codex_credential_id),
        workspace_connections,
    })
}

pub async fn select(
    db: &D1Database,
    user_id: &str,
    workspace_id: Option<&str>,
    id: Option<&str>,
) -> AppResult<()> {
    let changed = match workspace_id {
        Some(workspace_id) => q(
            db,
            "UPDATE workspace_members SET codex_credential_id = ?2 WHERE user_id = ?1 AND workspace_id = ?3
             AND (?2 IS NULL OR EXISTS (SELECT 1 FROM codex_credentials
                 WHERE id = ?2 AND user_id = ?1 AND status = 'active'))",
            &[s(user_id), os(id), s(workspace_id)],
        )?,
        None => q(
            db,
            "UPDATE users SET default_codex_credential_id = ?2 WHERE id = ?1
             AND (?2 IS NULL OR EXISTS (SELECT 1 FROM codex_credentials
                 WHERE id = ?2 AND user_id = ?1 AND status = 'active'))",
            &[s(user_id), os(id)],
        )?,
    }
    .run()
    .await?;
    if changed == 0 {
        return Err(not_found("Available ChatGPT connection"));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct LoginAttempt {
    redirect_uri: String,
    verifier: String,
    nonce: String,
    registration: Option<Registration>,
}

#[derive(Serialize, Deserialize)]
struct Registration {
    id: String,
    client_id: String,
    generation: i64,
}

fn validate_redirect(redirect_uri: &str) -> AppResult<()> {
    let port = redirect_uri
        .strip_prefix("http://127.0.0.1:")
        .and_then(|suffix| suffix.strip_suffix(CALLBACK_PATH))
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port > 0);
    if port.is_some_and(|port| redirect_uri == format!("http://127.0.0.1:{port}{CALLBACK_PATH}")) {
        Ok(())
    } else {
        Err(validation(
            "redirect_uri",
            "Use http://127.0.0.1:<port>/auth/callback",
        ))
    }
}

async fn host_id(db: &D1Database) -> AppResult<String> {
    #[derive(Deserialize)]
    struct Host {
        host_id: String,
    }
    let candidate = format!("urn:uuid:{}", uuid_v4());
    let row: Option<Host> = q(
        db,
        "INSERT INTO codex_runtime (id, host_id) VALUES (1, ?)
         ON CONFLICT (id) DO UPDATE SET host_id = codex_runtime.host_id RETURNING host_id",
        &[s(&candidate)],
    )?
    .first()
    .await?;
    row.map(|row| row.host_id)
        .ok_or_else(|| internal("missing Codex host ID"))
}

pub async fn login_start(
    db: &D1Database,
    keys: &Keys,
    user_id: &str,
    req: CodexLoginStartRequest,
    now: i64,
) -> AppResult<CodexLoginStart> {
    validate_redirect(&req.redirect_uri)?;
    let registration = match req.connection_id {
        Some(id) => {
            let row = owned_credential(db, user_id, id.as_str()).await?;
            if row.status == "revoking" {
                return Err(conflict("ChatGPT sign-out is in progress"));
            }
            if row.client_id == DEV_CLIENT {
                return Err(validation(
                    "connection_id",
                    "This is a development fixture. Connect a new account.",
                ));
            }
            Some(Registration {
                id: row.id,
                client_id: row.client_id,
                generation: row.generation,
            })
        }
        None => None,
    };
    let host = host_id(db).await?;
    let (verifier, challenge) = crypto::pkce_pair();
    let state = crypto::b64url(&fctl_domain::random_bytes::<32>());
    let attempt = LoginAttempt {
        redirect_uri: req.redirect_uri,
        verifier,
        nonce: crypto::b64url(&fctl_domain::random_bytes::<32>()),
        registration,
    };
    let mut url = Url::parse(AUTHORIZE_URL).map_err(internal)?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("ext_agent_host_id", &host)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &attempt.redirect_uri)
            .append_pair("scope", REQUESTED_SCOPE)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &state)
            .append_pair("nonce", &attempt.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge);
        if let Some(registration) = &attempt.registration {
            pairs.append_pair("client_id", &registration.client_id);
            if req.enable_plan == Some(true) {
                pairs.append_pair("prompt", "consent");
            }
        } else {
            pairs
                .append_pair("client_id", REGISTRATION_CLIENT)
                .append_pair("agent_name_hint", "fctl");
        }
        pairs.finish();
    }

    let hash = fctl_domain::tokens::hash_secret(&state);
    let sealed = keys.seal_json(&format!("codex:login:{user_id}:{hash}"), &attempt)?;
    crate::db::batch(
        db,
        vec![
            q(
                db,
                "DELETE FROM codex_login_attempts WHERE expires_at <= ?",
                &[n(now)],
            )?,
            q(
                db,
                "INSERT INTO codex_login_attempts (state_hash, user_id, attempt_enc, expires_at)
                 VALUES (?, ?, ?, ?)",
                &[s(&hash), s(user_id), s(&sealed), n(now + STATE_TTL_MS)],
            )?,
        ],
    )
    .await?;
    Ok(CodexLoginStart {
        authorize_url: url.into(),
        state,
    })
}

async fn take_attempt(
    db: &D1Database,
    keys: &Keys,
    state: &str,
    user_id: &str,
    now: i64,
) -> AppResult<LoginAttempt> {
    #[derive(Deserialize)]
    struct Attempt {
        attempt_enc: String,
    }
    let invalid = || {
        validation(
            "state",
            "ChatGPT sign-in has expired or was already used. Start again.",
        )
    };
    if state.len() != 43 {
        return Err(invalid());
    }
    let hash = fctl_domain::tokens::hash_secret(state);
    let row: Attempt = q(
        db,
        "DELETE FROM codex_login_attempts WHERE state_hash = ? AND user_id = ? AND expires_at > ?
         RETURNING attempt_enc",
        &[s(&hash), s(user_id), n(now)],
    )?
    .first()
    .await?
    .ok_or_else(invalid)?;
    keys.open_json(&format!("codex:login:{user_id}:{hash}"), &row.attempt_enc)
}

fn issued_client<'a>(
    callback: Option<&'a str>,
    registration: Option<&'a Registration>,
) -> AppResult<&'a str> {
    match (callback, registration) {
        (Some(received), Some(registration)) if received != registration.client_id => Err(
            validation("client_id", "ChatGPT returned a different registration"),
        ),
        (_, Some(registration)) => Ok(&registration.client_id),
        (Some(received), None)
            if received.strip_prefix("oaiapp_").is_some_and(|id| {
                !id.is_empty()
                    && id.len() <= 200
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            }) =>
        {
            Ok(received)
        }
        _ => Err(validation(
            "client_id",
            "ChatGPT did not return its issued client ID",
        )),
    }
}

pub async fn login_finish(
    db: &D1Database,
    keys: &Keys,
    user_id: &str,
    req: CodexLoginFinishRequest,
    now: i64,
) -> AppResult<CodexConnection> {
    let attempt = take_attempt(db, keys, &req.state, user_id, now).await?;
    if req.error.is_some() {
        return Err(forbidden(
            "ChatGPT sign-in was declined or could not be completed",
        ));
    }
    let code = req
        .code
        .as_deref()
        .filter(|code| !code.is_empty() && code.len() <= 16_384)
        .ok_or_else(|| validation("code", "ChatGPT did not return an authorization code"))?;
    let client_id = issued_client(req.client_id.as_deref(), attempt.registration.as_ref())?;
    if let Some(registration) = &attempt.registration {
        let row = owned_credential(db, user_id, &registration.id).await?;
        if row.generation != registration.generation || row.status == "revoking" {
            return Err(connection_changed());
        }
        let lock = lock_connection(db, &row, now_ms(), false)
            .await?
            .ok_or_else(connection_changed)?;
        let result = async {
            let account = exchange_code(client_id, code, &attempt).await?;
            if account.subject != row.subject {
                return Err(forbidden(
                    "Sign in to the ChatGPT account that registered this connection",
                ));
            }
            persist_login(db, keys, user_id, client_id, account, Some(&lock), now_ms()).await
        }
        .await;
        lock.release(db).await?;
        result
    } else {
        let account = exchange_code(client_id, code, &attempt).await?;
        persist_login(db, keys, user_id, client_id, account, None, now_ms()).await
    }
}

struct VerifiedLogin {
    subject: String,
    email: Option<String>,
    tokens: Tokens,
}

async fn persist_login(
    db: &D1Database,
    keys: &Keys,
    user_id: &str,
    client_id: &str,
    account: VerifiedLogin,
    lock: Option<&ConnectionLock>,
    now: i64,
) -> AppResult<CodexConnection> {
    let id = lock.map_or_else(|| credential_id(now).0, |lock| lock.id.clone());
    let sealed = keys.seal_json(&credential_aad(&id, user_id, client_id), &account.tokens)?;
    let status = account.tokens.status();
    let save = match lock {
        Some(lock) => q(
            db,
            "UPDATE codex_credentials SET email = ?3, status = ?4, secret_enc = ?5,
             generation = generation + 1, updated_at = ?6 WHERE id = ?1 AND operation_id = ?2 AND user_id = ?7",
            &[s(&id), s(&lock.operation), os(account.email.as_deref()), s(status), s(&sealed), n(now), s(user_id)],
        )?,
        None => q(
            db,
            "INSERT INTO codex_credentials (id, user_id, client_id, subject, email, status, secret_enc, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) ON CONFLICT (client_id) DO NOTHING",
            &[s(&id), s(user_id), s(client_id), s(&account.subject), os(account.email.as_deref()), s(status), s(&sealed), n(now)],
        )?,
    };
    let mut statements = vec![save];
    if status == "active" {
        statements.push(choose_first(db, user_id, &id)?);
    }
    let changes = crate::db::batch_changes(db, statements).await?;
    if changes.first() != Some(&1) {
        return Err(connection_changed());
    }
    Ok(owned_credential(db, user_id, &id).await?.connection(now))
}

fn choose_first(db: &D1Database, user_id: &str, id: &str) -> AppResult<Query> {
    q(
        db,
        "UPDATE users SET default_codex_credential_id = ?2 WHERE id = ?1 AND default_codex_credential_id IS NULL
         AND EXISTS (SELECT 1 FROM codex_credentials WHERE id = ?2 AND user_id = ?1 AND status = 'active')
         AND NOT EXISTS (SELECT 1 FROM codex_credentials WHERE user_id = ?1 AND id != ?2 AND status = 'active')",
        &[s(user_id), s(id)],
    )
}

#[derive(Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    id_token: String,
    scope: String,
    expires_at: i64,
}

impl Tokens {
    fn status(&self) -> &'static str {
        if self
            .scope
            .split_ascii_whitespace()
            .any(|scope| scope == PLAN_SCOPE)
        {
            "active"
        } else {
            "signed_in"
        }
    }

    fn for_runner(self, id: &str) -> CodexCredential {
        CodexCredential {
            id: CredentialId(id.to_owned()),
            access_token: self.access_token,
            expires_at: self.expires_at,
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

impl TokenResponse {
    fn into_tokens(self, id_token: String, now: i64) -> AppResult<Tokens> {
        let token = Tokens {
            access_token: self.access_token.unwrap_or_default(),
            refresh_token: self.refresh_token.filter(|token| !token.is_empty()),
            id_token,
            scope: self.scope.unwrap_or_default(),
            expires_at: now
                + self
                    .expires_in
                    .unwrap_or(0)
                    .clamp(0, MAX_ACCESS_LIFETIME_SECS)
                    * 1000,
        };
        if token.status() == "active"
            && (!self
                .token_type
                .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
                || token.access_token.is_empty()
                || token
                    .access_token
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace())
                || token.expires_at <= now)
        {
            return Err(unavailable("OpenAI returned unusable ChatGPT credentials"));
        }
        Ok(token)
    }
}

async fn exchange_code(
    client_id: &str,
    code: &str,
    attempt: &LoginAttempt,
) -> AppResult<VerifiedLogin> {
    let tokens = token_request(&[
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", code),
        ("code_verifier", &attempt.verifier),
        ("redirect_uri", &attempt.redirect_uri),
        ("resource", RESOURCE),
    ])
    .await
    .map_err(|problem| match problem {
        OAuthFailure::InvalidGrant => validation(
            "code",
            "ChatGPT rejected the authorization code. Start sign-in again.",
        ),
        _ => problem.into_error(),
    })?;
    let received_at = now_ms();
    let id_token = tokens.id_token.clone().ok_or_else(invalid_id_token)?;
    let claims = verify_id_token(&id_token, client_id, &attempt.nonce).await?;
    if let Some(expected) = claims.at_hash.as_deref() {
        let access_token = tokens
            .access_token
            .as_deref()
            .ok_or_else(invalid_id_token)?;
        let digest = crypto::sha256(access_token.as_bytes());
        if crypto::b64url(digest.split_at(16).0) != expected {
            return Err(invalid_id_token());
        }
    }
    Ok(VerifiedLogin {
        subject: claims.sub,
        email: claims.email,
        tokens: tokens.into_tokens(id_token, received_at)?,
    })
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: Audience,
    azp: Option<String>,
    sub: String,
    exp: i64,
    iat: i64,
    nbf: Option<i64>,
    nonce: String,
    at_hash: Option<String>,
    email: Option<String>,
}

impl Claims {
    fn valid(&self, client_id: &str, nonce: &str, now: i64) -> bool {
        let now = now.div_euclid(1000);
        let audience_matches = match &self.aud {
            Audience::One(audience) => audience == client_id,
            Audience::Many(audiences) => audiences.as_slice() == [client_id],
        };
        self.iss == ISSUER
            && audience_matches
            && self.azp.as_deref().is_none_or(|party| party == client_id)
            && !self.sub.is_empty()
            && self.exp > now
            && self.iat <= now + 5
            && self.nbf.is_none_or(|not_before| not_before <= now + 5)
            && self.nonce == nonce
    }
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    kid: String,
    crit: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    #[serde(rename = "use")]
    purpose: Option<String>,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

fn invalid_id_token() -> AppError {
    forbidden("OpenAI did not return a valid ChatGPT identity")
}

fn jwt_json<T: serde::de::DeserializeOwned>(part: &str) -> Option<T> {
    serde_json::from_slice(&crypto::b64url_decode(part)?).ok()
}

struct SignedIdentity<'a> {
    signing_input: &'a str,
    header: JwtHeader,
    claims: Claims,
    signature: Vec<u8>,
}

impl<'a> SignedIdentity<'a> {
    fn parse(jwt: &'a str) -> AppResult<Self> {
        let (signing_input, signature) = jwt.rsplit_once('.').ok_or_else(invalid_id_token)?;
        let (header, claims) = signing_input.split_once('.').ok_or_else(invalid_id_token)?;
        let header: JwtHeader = jwt_json(header).ok_or_else(invalid_id_token)?;
        if header.alg != "RS256" || header.kid.is_empty() || header.crit.is_some() {
            return Err(invalid_id_token());
        }
        Ok(Self {
            signing_input,
            header,
            claims: jwt_json(claims).ok_or_else(invalid_id_token)?,
            signature: crypto::b64url_decode(signature).ok_or_else(invalid_id_token)?,
        })
    }

    async fn verify(self, keys: Jwks, client_id: &str, nonce: &str, now: i64) -> AppResult<Claims> {
        let key = keys
            .keys
            .iter()
            .find(|key| {
                key.kid == self.header.kid
                    && key.kty == "RSA"
                    && key
                        .purpose
                        .as_deref()
                        .is_none_or(|purpose| purpose == "sig")
                    && key
                        .alg
                        .as_deref()
                        .is_none_or(|algorithm| algorithm == "RS256")
            })
            .ok_or_else(invalid_id_token)?;
        let n = key
            .n
            .as_deref()
            .and_then(crypto::b64url_decode)
            .ok_or_else(invalid_id_token)?;
        let e = key
            .e
            .as_deref()
            .and_then(crypto::b64url_decode)
            .ok_or_else(invalid_id_token)?;
        if !crypto::verify_passkey(
            &PublicKey::Rsa { n, e },
            &self.signature,
            self.signing_input.as_bytes(),
        )
        .await?
            || !self.claims.valid(client_id, nonce, now)
        {
            return Err(invalid_id_token());
        }
        Ok(self.claims)
    }
}

async fn verify_id_token(jwt: &str, client_id: &str, nonce: &str) -> AppResult<Claims> {
    let identity = SignedIdentity::parse(jwt)?;
    let mut response = oauth_request(JWKS_URL, None)
        .await
        .map_err(|problem| problem.into_error())?;
    if response.status_code() != 200 {
        return Err(unavailable(
            "OpenAI signing keys are temporarily unavailable",
        ));
    }
    let keys: Jwks = response
        .json()
        .await
        .map_err(|_| unavailable("OpenAI signing keys are temporarily unavailable"))?;
    identity.verify(keys, client_id, nonce, now_ms()).await
}

#[derive(Clone, Copy)]
enum OAuthFailure {
    InvalidGrant,
    InvalidClient,
    Temporary,
    Rejected,
}

impl OAuthFailure {
    fn parse(code: &str) -> Self {
        match code {
            "invalid_grant"
            | "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused" => Self::InvalidGrant,
            "invalid_client" => Self::InvalidClient,
            _ => Self::Rejected,
        }
    }

    fn into_error(self) -> AppError {
        match self {
            Self::InvalidGrant => forbidden("This ChatGPT connection expired. Sign in again."),
            Self::InvalidClient => {
                forbidden("OpenAI no longer recognizes this ChatGPT registration")
            }
            Self::Temporary => {
                unavailable("ChatGPT authorization is temporarily unavailable. Try again.")
            }
            Self::Rejected => forbidden("OpenAI rejected the ChatGPT authorization"),
        }
    }
}

async fn oauth_request(url: &str, form: Option<&[(&str, &str)]>) -> Result<Response, OAuthFailure> {
    let mut init = RequestInit::new();
    init.with_redirect(RequestRedirect::Manual);
    if let Some(fields) = form {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish();
        let headers = Headers::new();
        headers
            .set("Content-Type", "application/x-www-form-urlencoded")
            .map_err(|_| OAuthFailure::Temporary)?;
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(body.into()));
    }
    let signal =
        worker::AbortSignal::from(web_sys::AbortSignal::timeout_with_u32(OAUTH_TIMEOUT_MS));
    let request = Request::new_with_init(url, &init).map_err(|_| OAuthFailure::Temporary)?;
    Fetch::Request(request)
        .send_with_signal(&signal)
        .await
        .map_err(|_| OAuthFailure::Temporary)
}

async fn token_request(fields: &[(&str, &str)]) -> Result<TokenResponse, OAuthFailure> {
    let mut response = oauth_request(TOKEN_URL, Some(fields)).await?;
    let status = response.status_code();
    match status {
        200 => response.json().await.map_err(|_| OAuthFailure::Temporary),
        429 | 500..=599 => Err(OAuthFailure::Temporary),
        _ => {
            #[derive(Deserialize)]
            struct OAuthError {
                error: String,
            }
            Err(response
                .json::<OAuthError>()
                .await
                .map_or(OAuthFailure::Rejected, |body| {
                    OAuthFailure::parse(&body.error)
                }))
        }
    }
}

struct ConnectionLock {
    id: String,
    operation: String,
}

impl ConnectionLock {
    async fn release(&self, db: &D1Database) -> AppResult<()> {
        q(
            db,
            "UPDATE codex_credentials SET operation_id = NULL, operation_until = NULL
             WHERE id = ? AND operation_id = ?",
            &[s(&self.id), s(&self.operation)],
        )?
        .run()
        .await?;
        Ok(())
    }
}

fn connection_changed() -> AppError {
    conflict("The ChatGPT connection changed or is being updated. Try again.")
}

/// The CAS binds the lock to the credential as read, including its rotating
/// token. An expired owner can neither overwrite nor unlock a successor.
async fn lock_connection(
    db: &D1Database,
    row: &CredentialRow,
    now: i64,
    revoke: bool,
) -> AppResult<Option<ConnectionLock>> {
    let lock = ConnectionLock {
        id: row.id.clone(),
        operation: uuid_v4(),
    };
    let changed = q(
        db,
        "UPDATE codex_credentials SET operation_id = ?2, operation_until = ?3,
         status = CASE WHEN ?7 THEN 'revoking' ELSE status END,
         generation = generation + CASE WHEN ?7 THEN 1 ELSE 0 END
         WHERE id = ?1 AND (operation_until IS NULL OR operation_until <= ?4)
         AND generation = ?5 AND secret_enc IS ?6 AND status = ?8",
        &[
            s(&row.id),
            s(&lock.operation),
            n(now + OPERATION_TTL_MS),
            n(now),
            n(row.generation),
            os(row.secret_enc.as_deref()),
            crate::db::b(revoke),
            s(&row.status),
        ],
    )?
    .run()
    .await?;
    Ok((changed == 1).then_some(lock))
}

async fn active_credential(db: &D1Database, id: &str) -> AppResult<CredentialRow> {
    q(
        db,
        "SELECT c.* FROM codex_credentials c JOIN users u ON u.id = c.user_id
         WHERE c.id = ? AND c.status = 'active' AND u.status = 'active'",
        &[s(id)],
    )?
    .first()
    .await?
    .ok_or_else(|| {
        forbidden(
            "This ChatGPT connection is unavailable. Sign in again or select another connection.",
        )
    })
}

pub async fn access_token(
    env: &Env,
    db: &D1Database,
    id: &str,
    now: i64,
) -> AppResult<CodexCredential> {
    read_access_token(db, &access_keys(env, id)?, id, now, false, token_request).await
}

pub async fn refresh_access_token(
    env: &Env,
    db: &D1Database,
    id: &str,
    now: i64,
) -> AppResult<CodexCredential> {
    read_access_token(db, &access_keys(env, id)?, id, now, true, token_request).await
}

fn access_keys(env: &Env, id: &str) -> AppResult<Keys> {
    let cfg = Config::from_env(env);
    require_enabled(&cfg)?;
    if id == DEV_CREDENTIAL && !local_dev(&cfg) {
        return Err(not_found("ChatGPT connection"));
    }
    crypto::keys(&cfg)
}

async fn read_access_token(
    db: &D1Database,
    keys: &Keys,
    id: &str,
    now: i64,
    force_refresh: bool,
    exchange: impl AsyncFnOnce(&[(&str, &str)]) -> Result<TokenResponse, OAuthFailure>,
) -> AppResult<CodexCredential> {
    let deadline = now + i64::from(OAUTH_TIMEOUT_MS);
    let mut rejected_ciphertext = None;
    loop {
        let now = now_ms().max(now);
        let row = active_credential(db, id).await?;
        if row.paused_at(now) {
            return Err(plan_paused());
        }
        let sealed = row.secret_enc.as_ref().ok_or_else(connection_changed)?;
        let tokens: Tokens = keys.open_json(&row.aad(), sealed)?;
        if tokens.status() != "active" {
            return Err(forbidden("Enable ChatGPT plan usage for this connection"));
        }
        // A refresh or reauthorization seals a new bundle. If either won the
        // race, return its replacement instead of refreshing it a second time.
        let rejected = force_refresh
            && rejected_ciphertext
                .get_or_insert_with(|| sealed.clone())
                .as_str()
                == sealed;
        let unexpired = tokens.expires_at > now;
        if !rejected
            && unexpired
            && (force_refresh
                || tokens.expires_at > now + RENEW_BEFORE_MS
                || tokens.refresh_token.is_none())
        {
            return Ok(tokens.for_runner(id));
        }
        if let Some(lock) = lock_connection(db, &row, now, false).await? {
            let result = refresh_locked(db, keys, &row, &lock, tokens, exchange).await;
            lock.release(db).await?;
            return result;
        }
        if !force_refresh && unexpired && row.operation_id.is_some() {
            return Ok(tokens.for_runner(id));
        }
        if now >= deadline {
            return Err(unavailable(
                "This ChatGPT connection is being updated. Try again.",
            ));
        }
        delay_ms(250).await?;
    }
}

async fn refresh_locked(
    db: &D1Database,
    keys: &Keys,
    row: &CredentialRow,
    lock: &ConnectionLock,
    current: Tokens,
    exchange: impl AsyncFnOnce(&[(&str, &str)]) -> Result<TokenResponse, OAuthFailure>,
) -> AppResult<CodexCredential> {
    let renewed = match current.refresh_token.as_deref() {
        Some(refresh_token) => {
            exchange(&[
                ("grant_type", "refresh_token"),
                ("client_id", &row.client_id),
                ("refresh_token", refresh_token),
                ("resource", RESOURCE),
            ])
            .await
        }
        None => Err(OAuthFailure::InvalidGrant),
    };
    persist_refresh(db, keys, row, lock, current.id_token, renewed, now_ms()).await
}

async fn persist_refresh(
    db: &D1Database,
    keys: &Keys,
    row: &CredentialRow,
    lock: &ConnectionLock,
    id_token: String,
    renewed: Result<TokenResponse, OAuthFailure>,
    now: i64,
) -> AppResult<CodexCredential> {
    let tokens = match renewed {
        Ok(tokens) => tokens.into_tokens(id_token, now)?,
        Err(problem @ OAuthFailure::InvalidGrant) => {
            clear_tokens(db, lock, now).await?;
            return Err(problem.into_error());
        }
        Err(problem) => return Err(problem.into_error()),
    };
    let status = tokens.status();
    let sealed = keys.seal_json(&row.aad(), &tokens)?;
    let changed = q(
        db,
        "UPDATE codex_credentials SET secret_enc = ?3, status = ?4, updated_at = ?5
         WHERE id = ?1 AND operation_id = ?2",
        &[
            s(&row.id),
            s(&lock.operation),
            s(&sealed),
            s(status),
            n(now),
        ],
    )?
    .run()
    .await?;
    if changed != 1 {
        return Err(connection_changed());
    }
    if status != "active" {
        return Err(forbidden("Enable ChatGPT plan usage for this connection"));
    }
    if is_paused(db, &row.id, now).await? {
        return Err(plan_paused());
    }
    Ok(tokens.for_runner(&row.id))
}

async fn clear_tokens(db: &D1Database, lock: &ConnectionLock, now: i64) -> AppResult<()> {
    let changed = q(
        db,
        "UPDATE codex_credentials SET status = 'disconnected', secret_enc = NULL, paused = 0,
         reset_at = NULL, updated_at = ?3 WHERE id = ?1 AND operation_id = ?2",
        &[s(&lock.id), s(&lock.operation), n(now)],
    )?
    .run()
    .await?;
    if changed != 1 {
        return Err(connection_changed());
    }
    Ok(())
}

fn plan_paused() -> AppError {
    AppError(ApiError::new(
        ErrorCode::RateLimited,
        "ChatGPT plan usage is paused. Check https://chatgpt.com/settings/usage.",
    ))
}

pub async fn notify_usage_changed(
    env: &Env,
    db: &D1Database,
    id: &str,
    except: Option<&str>,
) -> AppResult<()> {
    let sessions: Vec<Id> = q(
        db,
        "SELECT id FROM sessions WHERE agent_kind = 'codex' AND credential_id = ?1
         AND (?2 IS NULL OR id != ?2)
         AND status IN ('provisioning', 'preparing', 'running', 'idle', 'paused')",
        &[s(id), os(except)],
    )?
    .all()
    .await?;
    for session in sessions {
        let result = async {
            let stub = crate::internal::session_stub(env, &session.id)?;
            crate::internal::post::<_, serde_json::Value>(
                &stub,
                "/control",
                &crate::internal::ControlState {
                    codex_usage_changed: Some(id.to_owned()),
                    ..Default::default()
                },
            )
            .await
        }
        .await;
        if let Err(error) = result {
            worker::console_error!("synchronizing a Codex usage pause: {}", error.0.message);
        }
    }
    Ok(())
}

pub async fn pause(db: &D1Database, id: &str, reset_at: Option<i64>) -> AppResult<()> {
    q(
        db,
        "UPDATE codex_credentials SET paused = 1, reset_at = ? WHERE id = ? AND status = 'active'",
        &[on(reset_at), s(id)],
    )?
    .run()
    .await?;
    Ok(())
}

pub async fn is_paused(db: &D1Database, id: &str, now: i64) -> AppResult<bool> {
    let paused: Option<Id> = q(
        db,
        "SELECT id FROM codex_credentials WHERE id = ? AND status = 'active' AND paused = 1 AND (reset_at IS NULL OR reset_at > ?)",
        &[s(id), n(now)],
    )?
    .first()
    .await?;
    Ok(paused.is_some())
}

pub async fn retry(db: &D1Database, id: &str) -> AppResult<()> {
    let changed = q(
        db,
        "UPDATE codex_credentials SET paused = 0, reset_at = NULL WHERE id = ? AND status = 'active'",
        &[s(id)],
    )?
    .run()
    .await?;
    if changed == 0 {
        return Err(forbidden("Enable ChatGPT plan usage for this connection"));
    }
    Ok(())
}

pub async fn retry_owned(db: &D1Database, user_id: &str, id: &str) -> AppResult<()> {
    if owner(db, id).await?.as_deref() != Some(user_id) {
        return Err(not_found("ChatGPT connection"));
    }
    retry(db, id).await
}

pub async fn revoke(
    db: &D1Database,
    keys: &Keys,
    user_id: &str,
    id: &str,
    stop_sessions: impl Future<Output = AppResult<()>>,
) -> AppResult<bool> {
    revoke_with(db, keys, user_id, id, stop_sessions, revoke_refresh).await
}

async fn revoke_with(
    db: &D1Database,
    keys: &Keys,
    user_id: &str,
    id: &str,
    stop_sessions: impl Future<Output = AppResult<()>>,
    revoke_remote: impl AsyncFnOnce(&str, &str) -> bool,
) -> AppResult<bool> {
    let deadline = now_ms() + i64::from(OAUTH_TIMEOUT_MS);
    let (row, lock) = loop {
        let now = now_ms();
        let row = owned_credential(db, user_id, id).await?;
        if let Some(lock) = lock_connection(db, &row, now, true).await? {
            break (row, lock);
        }
        if now >= deadline {
            return Err(connection_changed());
        }
        delay_ms(250).await?;
    };
    let revoke_tokens = async {
        let revoked = match row.secret(keys) {
            Ok(Some(tokens)) => match tokens.refresh_token {
                Some(token) => revoke_remote(&row.client_id, &token).await,
                None => true,
            },
            Ok(None) => true,
            Err(_) => false,
        };
        clear_tokens(db, &lock, now_ms()).await?;
        lock.release(db).await?;
        Ok(revoked)
    };
    let (revoked, stopped) = futures::join!(revoke_tokens, stop_sessions);
    if let Err(e) = stopped {
        worker::console_error!("stopping Codex sessions after sign-out: {}", e.0.message);
    }
    revoked
}

/// Workerd and Node return different types of timer handles.
async fn delay_ms(ms: u32) -> AppResult<()> {
    let global = js_sys::global();
    let timeout: js_sys::Function =
        js_sys::Reflect::get(&global, &"setTimeout".into())?.dyn_into()?;
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        if let Err(error) = timeout.call2(&global, &resolve, &ms.into()) {
            let _ = reject.call1(&JsValue::UNDEFINED, &error);
        }
    });
    JsFuture::from(promise).await?;
    Ok(())
}

async fn revoke_refresh(client_id: &str, token: &str) -> bool {
    let fields = [
        ("token", token),
        ("token_type_hint", "refresh_token"),
        ("client_id", client_id),
    ];
    for gap in [0, 200, 600] {
        if gap > 0 && delay_ms(gap).await.is_err() {
            return false;
        }
        match oauth_request(REVOKE_URL, Some(&fields)).await {
            Ok(response) => match response.status_code() {
                200 => return true,
                429 | 500..=599 => {}
                _ => return false,
            },
            Err(OAuthFailure::Temporary) => {}
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_canonical_ipv4_loopback_callback_is_accepted() {
        assert!(validate_redirect("http://127.0.0.1:1455/auth/callback").is_ok());
        for uri in [
            "http://localhost:1455/auth/callback",
            "http://127.0.0.2:1455/auth/callback",
            "http://127.0.0.1.evil.example:1455/auth/callback",
            "http://127.0.0.1:1455@evil.example/auth/callback",
            "https://127.0.0.1:1455/auth/callback",
            "http://127.0.0.1/auth/callback",
            "http://127.0.0.1:0/auth/callback",
            "http://127.0.0.1:100000/auth/callback",
            "http://127.0.0.1:1455/callback",
            "http://127.0.0.1:1455/auth/callback?anything=1",
        ] {
            assert!(validate_redirect(uri).is_err(), "{uri}");
        }
    }

    #[test]
    fn the_returned_client_must_match_the_pending_registration() {
        assert!(issued_client(None, None).is_err());
        assert!(issued_client(Some(REGISTRATION_CLIENT), None).is_err());
        assert!(issued_client(Some("oaiapp_new"), None).is_ok());
        let saved = Registration {
            id: "cred_1".into(),
            client_id: "oaiapp_saved".into(),
            generation: 0,
        };
        assert!(issued_client(None, Some(&saved)).is_ok_and(|id| id == saved.client_id));
        assert!(issued_client(Some("oaiapp_other"), Some(&saved)).is_err());
    }

    #[cfg(target_arch = "wasm32")]
    mod wasm {
        use super::*;
        use crate::testing;
        use js_sys::{Array, Object, Reflect, Uint8Array};
        use serde_json::json;
        use wasm_bindgen::JsCast;
        use wasm_bindgen_futures::JsFuture;
        use wasm_bindgen_test::wasm_bindgen_test;

        const NOW: i64 = 1_800_000_000_000;
        const CALLBACK: &str = "http://127.0.0.1:1455/auth/callback";

        async fn database() -> AppResult<D1Database> {
            let db = testing::database();
            q(
                &db,
                "INSERT INTO users (id, email, name, created_at, updated_at)
                 VALUES ('ada','ada@example.test','Ada',0,0), ('bob','bob@example.test','Bob',0,0)",
                &[],
            )?
            .run()
            .await?;
            q(
                &db,
                "INSERT INTO workspaces (id, slug, name, created_at, updated_at) VALUES ('w','work','Work',0,0)",
                &[],
            )?
            .run()
            .await?;
            q(
                &db,
                "INSERT INTO workspace_members (workspace_id,user_id,email,role,created_at)
                 VALUES ('w','ada','ada@example.test','owner',0)",
                &[],
            )?
            .run()
            .await?;
            Ok(db)
        }

        fn keys() -> Keys {
            Keys::derive(&[45; 32])
        }

        fn token_response(scope: &str, access: &str, refresh: &str) -> TokenResponse {
            TokenResponse {
                access_token: Some(access.into()),
                refresh_token: Some(refresh.into()),
                id_token: None,
                token_type: Some("Bearer".into()),
                expires_in: Some(3600),
                scope: Some(scope.into()),
            }
        }

        async fn login(
            db: &D1Database,
            user_id: &str,
            client_id: &str,
            scope: &str,
        ) -> AppResult<CodexConnection> {
            let tokens = token_response(scope, "fake-access", "fake-refresh")
                .into_tokens("fake-identity".into(), NOW)?;
            persist_login(
                db,
                &keys(),
                user_id,
                client_id,
                VerifiedLogin {
                    subject: "example-chatgpt-user".into(),
                    email: Some("account@example.test".into()),
                    tokens,
                },
                None,
                NOW,
            )
            .await
        }

        #[wasm_bindgen_test]
        async fn login_state_is_private_bound_to_a_user_expiring_and_consumed_once() -> AppResult<()>
        {
            let db = database().await?;
            let keys = keys();
            let request = || CodexLoginStartRequest {
                redirect_uri: CALLBACK.into(),
                connection_id: None,
                enable_plan: None,
            };
            let first = login_start(&db, &keys, "ada", request(), NOW).await?;
            let second = login_start(&db, &keys, "ada", request(), NOW).await?;
            assert_ne!(first.state, second.state);
            let values = |url: &str| {
                Ok::<_, AppError>(
                    Url::parse(url)
                        .map_err(internal)?
                        .query_pairs()
                        .into_owned()
                        .collect::<std::collections::BTreeMap<_, _>>(),
                )
            };
            let params = values(&first.authorize_url)?;
            let same_installation = values(&second.authorize_url)?;
            assert_eq!(
                params.get("client_id").map(String::as_str),
                Some(REGISTRATION_CLIENT)
            );
            assert_eq!(
                params.get("code_challenge_method").map(String::as_str),
                Some("S256")
            );
            assert_eq!(
                params.get("redirect_uri").map(String::as_str),
                Some(CALLBACK)
            );
            assert_eq!(params.get("state"), Some(&first.state));
            assert_eq!(
                params.get("scope").map(String::as_str),
                Some(REQUESTED_SCOPE)
            );
            assert_eq!(
                params.get("ext_agent_host_id"),
                same_installation.get("ext_agent_host_id")
            );
            assert!(!params.contains_key("code_verifier"));
            assert!(
                take_attempt(&db, &keys, &first.state, "bob", NOW)
                    .await
                    .is_err()
            );
            let (a, b) = futures::join!(
                take_attempt(&db, &keys, &first.state, "ada", NOW),
                take_attempt(&db, &keys, &first.state, "ada", NOW)
            );
            let saved = a.or(b)?;
            let challenge = crypto::b64url(&crypto::sha256(saved.verifier.as_bytes()));
            assert_eq!(params.get("code_challenge"), Some(&challenge));
            assert_eq!(params.get("nonce"), Some(&saved.nonce));
            assert!(
                take_attempt(&db, &keys, &first.state, "ada", NOW)
                    .await
                    .is_err()
            );
            assert!(
                take_attempt(&db, &keys, &second.state, "ada", NOW + STATE_TTL_MS)
                    .await
                    .is_err()
            );
            Ok(())
        }

        #[wasm_bindgen_test]
        async fn validates_signature_issuer_audience_expiration_and_nonce() -> AppResult<()> {
            let algorithm = Object::new();
            Reflect::set(&algorithm, &"name".into(), &"RSASSA-PKCS1-v1_5".into())?;
            Reflect::set(&algorithm, &"hash".into(), &"SHA-256".into())?;
            Reflect::set(&algorithm, &"modulusLength".into(), &2048.into())?;
            let exponent = Uint8Array::from([1_u8, 0, 1].as_slice());
            Reflect::set(&algorithm, &"publicExponent".into(), &exponent)?;
            let subtle = crypto::subtle()?;
            let pair: web_sys::CryptoKeyPair = JsFuture::from(subtle.generate_key_with_object(
                &algorithm,
                true,
                &Array::of2(&"sign".into(), &"verify".into()),
            )?)
            .await?
            .unchecked_into();
            let public = JsFuture::from(subtle.export_key("jwk", &pair.get_public_key())?).await?;
            let n = Reflect::get(&public, &"n".into())?.as_string();
            let e = Reflect::get(&public, &"e".into())?.as_string();
            let jwks = || Jwks {
                keys: vec![Jwk {
                    kid: "test-key".into(),
                    kty: "RSA".into(),
                    purpose: Some("sig".into()),
                    alg: Some("RS256".into()),
                    n: n.clone(),
                    e: e.clone(),
                }],
            };
            let header = crypto::b64url(br#"{"alg":"RS256","kid":"test-key"}"#);
            let client = "oaiapp_account";
            let claims = |issuer: &str| {
                crypto::b64url(
                    &json!({
                        "iss": issuer, "aud": [client], "sub": "customer",
                        "nonce": "test-nonce", "iat": NOW / 1000, "exp": NOW / 1000 + 60,
                    })
                    .to_string()
                    .into_bytes(),
                )
            };
            let input = format!("{header}.{}", claims(ISSUER));
            let signature = JsFuture::from(subtle.sign_with_str_and_u8_array(
                "RSASSA-PKCS1-v1_5",
                &pair.get_private_key(),
                input.as_bytes(),
            )?)
            .await?;
            let sig = crypto::b64url(&Uint8Array::new(&signature).to_vec());
            let token = format!("{input}.{sig}");
            let identity = SignedIdentity::parse(&token)?
                .verify(jwks(), client, "test-nonce", NOW)
                .await?;
            assert_eq!(identity.sub, "customer");
            assert!(
                SignedIdentity::parse(&token)?
                    .verify(jwks(), "other-client", "test-nonce", NOW)
                    .await
                    .is_err()
            );
            assert!(
                SignedIdentity::parse(&token)?
                    .verify(jwks(), client, "other-nonce", NOW)
                    .await
                    .is_err()
            );
            assert!(
                SignedIdentity::parse(&token)?
                    .verify(jwks(), client, "test-nonce", NOW + 60_000)
                    .await
                    .is_err()
            );
            assert!(!identity.valid(client, "test-nonce", NOW + 60_000));
            let fake_issuer = format!("{header}.{}", claims("https://other.example"));
            let tampered = format!("{fake_issuer}.{sig}");
            let tampered = SignedIdentity::parse(&tampered)?;
            assert!(!tampered.claims.valid(client, "test-nonce", NOW));
            let forged_signature = format!("{input}.{}", crypto::b64url(&[0; 256]));
            assert!(
                SignedIdentity::parse(&forged_signature)?
                    .verify(jwks(), client, "test-nonce", NOW)
                    .await
                    .is_err()
            );
            Ok(())
        }

        #[wasm_bindgen_test]
        async fn selection_requires_an_owned_grant_and_never_switches_an_override() -> AppResult<()>
        {
            let db = database().await?;
            let no_plan = login(&db, "ada", "oaiapp_no_plan", "openid").await?;
            assert!(no_plan.connected && !no_plan.plan_enabled);
            assert_eq!(credential_for(&db, "ada", "w").await?, None);
            assert!(
                select(&db, "ada", None, Some(no_plan.id.as_str()))
                    .await
                    .is_err()
            );
            let default = login(&db, "ada", "oaiapp_default", REQUESTED_SCOPE).await?;
            let override_ = login(&db, "ada", "oaiapp_workspace", REQUESTED_SCOPE).await?;
            let foreign = login(&db, "bob", "oaiapp_foreign", REQUESTED_SCOPE).await?;
            assert!(
                select(&db, "ada", Some("w"), Some(foreign.id.as_str()))
                    .await
                    .is_err()
            );
            assert_eq!(
                credential_for(&db, "ada", "w").await?.as_deref(),
                Some(default.id.as_str())
            );
            select(&db, "ada", Some("w"), Some(override_.id.as_str())).await?;
            assert_eq!(
                credential_for(&db, "ada", "w").await?.as_deref(),
                Some(override_.id.as_str())
            );
            pause(&db, override_.id.as_str(), Some(NOW + 1_000)).await?;
            assert!(is_paused(&db, override_.id.as_str(), NOW).await?);
            assert!(!is_paused(&db, override_.id.as_str(), NOW + 1_000).await?);
            pause(&db, override_.id.as_str(), None).await?;
            assert!(is_paused(&db, override_.id.as_str(), NOW + 2_000).await?);
            retry(&db, override_.id.as_str()).await?;
            assert!(!is_paused(&db, override_.id.as_str(), NOW + 2_000).await?);
            q(
                &db,
                "UPDATE codex_credentials SET status = 'disconnected' WHERE id = ?",
                &[s(override_.id.as_str())],
            )?
            .run()
            .await?;
            assert_eq!(credential_for(&db, "ada", "w").await?, None);
            select(&db, "ada", Some("w"), None).await?;
            assert_eq!(
                credential_for(&db, "ada", "w").await?.as_deref(),
                Some(default.id.as_str())
            );
            let settings = serde_json::to_string(&settings(&db, "ada", NOW).await?)?;
            for credential in [
                "fake-access",
                "fake-refresh",
                "fake-identity",
                "oaiapp_default",
            ] {
                assert!(!settings.contains(credential));
            }
            Ok(())
        }

        #[wasm_bindgen_test]
        async fn rotation_serializes_refresh_and_discards_only_unusable_tokens() -> AppResult<()> {
            let db = database().await?;
            let keys = keys();
            let saved = login(&db, "ada", "oaiapp_rotate", REQUESTED_SCOPE).await?;
            let id = saved.id.as_str();
            let row = owned_credential(&db, "ada", id).await?;
            let (first, second) = futures::try_join!(
                lock_connection(&db, &row, NOW, false),
                lock_connection(&db, &row, NOW, false),
            )?;
            assert_ne!(first.is_some(), second.is_some());
            let lock = first.or(second).ok_or_else(connection_changed)?;
            let refreshed = persist_refresh(
                &db,
                &keys,
                &row,
                &lock,
                "fake-identity".into(),
                Ok(token_response(
                    REQUESTED_SCOPE,
                    "rotated-access",
                    "rotated-refresh",
                )),
                NOW,
            )
            .await?;
            assert_eq!(refreshed.access_token, "rotated-access");
            let changed = owned_credential(&db, "ada", id).await?;
            assert_ne!(changed.secret_enc, row.secret_enc);
            let tokens = changed.secret(&keys)?.ok_or_else(connection_changed)?;
            assert_eq!(tokens.refresh_token.as_deref(), Some("rotated-refresh"));
            assert_eq!(tokens.id_token, "fake-identity");
            lock.release(&db).await?;
            assert!(lock_connection(&db, &row, NOW, false).await?.is_none());
            let next = lock_connection(&db, &changed, NOW, false)
                .await?
                .ok_or_else(connection_changed)?;
            assert!(
                persist_refresh(
                    &db,
                    &keys,
                    &changed,
                    &next,
                    tokens.id_token.clone(),
                    Err(OAuthFailure::Temporary),
                    NOW,
                )
                .await
                .is_err()
            );
            assert_eq!(
                owned_credential(&db, "ada", id).await?.secret_enc,
                changed.secret_enc
            );
            assert!(
                persist_refresh(
                    &db,
                    &keys,
                    &changed,
                    &next,
                    tokens.id_token,
                    Err(OAuthFailure::parse("refresh_token_reused")),
                    NOW,
                )
                .await
                .is_err()
            );
            let disconnected = owned_credential(&db, "ada", id).await?;
            assert_eq!(disconnected.status, "disconnected");
            assert!(disconnected.secret_enc.is_none());
            assert_eq!(disconnected.client_id, "oaiapp_rotate");
            Ok(())
        }

        #[wasm_bindgen_test]
        async fn concurrent_rejections_share_one_refresh_of_an_unexpired_token() -> AppResult<()> {
            let db = database().await?;
            let keys = keys();
            let now = now_ms();
            let saved = login(&db, "ada", "oaiapp_rejected", REQUESTED_SCOPE).await?;
            let id = saved.id.as_str();
            let calls = std::cell::Cell::new(0);
            let refresh = async |fields: &[(&str, &str)]| {
                calls.set(calls.get() + 1);
                assert_eq!(
                    fields,
                    [
                        ("grant_type", "refresh_token"),
                        ("client_id", "oaiapp_rejected"),
                        ("refresh_token", "fake-refresh"),
                        ("resource", RESOURCE),
                    ]
                );
                Ok(token_response(
                    REQUESTED_SCOPE,
                    "replacement-access",
                    "replacement-refresh",
                ))
            };
            let cached = read_access_token(&db, &keys, id, now, false, &refresh).await?;
            assert_eq!(cached.access_token, "fake-access");
            assert_eq!(calls.get(), 0);
            let (a, b) = futures::try_join!(
                read_access_token(&db, &keys, id, now, true, &refresh),
                read_access_token(&db, &keys, id, now, true, &refresh),
            )?;
            assert_eq!(a.access_token, "replacement-access");
            assert_eq!(b.access_token, "replacement-access");
            assert_eq!(calls.get(), 1);
            Ok(())
        }

        #[wasm_bindgen_test]
        async fn the_fixture_does_not_replace_a_chatgpt_connection() -> AppResult<()> {
            let db = database().await?;
            let keys = keys();
            let original = login(&db, "ada", "oaiapp_real_registration", REQUESTED_SCOPE).await?;
            let first = dev_seed(&db, &keys, "ada", NOW).await?;
            let second = dev_seed(&db, &keys, "ada", NOW + 1).await?;
            assert_eq!(first.id, second.id);
            assert_ne!(first.id, original.id);
            assert!(dev_seed(&db, &keys, "bob", NOW).await.is_err());
            assert_eq!(
                credential_for(&db, "ada", "w").await?.as_deref(),
                Some(original.id.as_str())
            );
            let seed = owned_credential(&db, "ada", first.id.as_str()).await?;
            assert!(
                seed.secret(&keys)?
                    .is_some_and(|tokens| tokens.refresh_token.is_none())
            );
            assert_eq!(plan_paused().0.code, ErrorCode::RateLimited);
            Ok(())
        }

        #[wasm_bindgen_test]
        async fn revoke_stops_new_issuance_and_clears_secrets_even_when_remote_fails()
        -> AppResult<()> {
            let db = database().await?;
            let keys = keys();
            let saved = login(&db, "ada", "oaiapp_revoke", REQUESTED_SCOPE).await?;
            let id = saved.id.as_str();
            let remote_called = std::cell::Cell::new(false);
            let pending = login_start(
                &db,
                &keys,
                "ada",
                CodexLoginStartRequest {
                    redirect_uri: CALLBACK.into(),
                    connection_id: Some(saved.id.clone()),
                    enable_plan: None,
                },
                NOW,
            )
            .await?;
            let revoked = revoke_with(
                &db,
                &keys,
                "ada",
                id,
                async {
                    assert!(active_credential(&db, id).await.is_err());
                    Ok(())
                },
                async |client, refresh| {
                    remote_called.set(true);
                    assert_eq!(client, "oaiapp_revoke");
                    assert_eq!(refresh, "fake-refresh");
                    false
                },
            )
            .await?;
            assert!(remote_called.get());
            assert!(!revoked);
            let row = owned_credential(&db, "ada", id).await?;
            assert!(row.secret_enc.is_none());
            assert!(row.operation_id.is_none());
            assert_eq!(row.client_id, "oaiapp_revoke");
            let login = login_finish(
                &db,
                &keys,
                "ada",
                CodexLoginFinishRequest {
                    state: pending.state,
                    code: Some("unused-code".into()),
                    client_id: None,
                    error: None,
                },
                NOW,
            )
            .await;
            assert!(login.is_err());
            Ok(())
        }
    }
}
