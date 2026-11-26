//! `fctl codex …`: manage the ChatGPT connections Codex runs on.

use crate::{config, style};
use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use fctl_client::Api;
use fctl_proto::CredentialId;
use fctl_proto::codex::{
    CodexConnection, CodexLoginFinishRequest, CodexLoginStartRequest, CodexSelection,
};
use fctl_proto::session::AgentKind;
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

const CALLBACK: &str = "/auth/callback";
pub(crate) const USAGE_URL: &str = "https://chatgpt.com/settings/usage";

#[derive(Clone, Copy, ValueEnum)]
pub enum AgentArg {
    Claude,
    Codex,
}

impl From<AgentArg> for AgentKind {
    fn from(value: AgentArg) -> Self {
        match value {
            AgentArg::Claude => Self::Claude,
            AgentArg::Codex => Self::Codex,
        }
    }
}

pub fn subscription(agent: Option<AgentKind>) -> &'static str {
    match agent.unwrap_or_default() {
        AgentKind::Claude => "Claude subscription",
        AgentKind::Codex => "ChatGPT plan",
    }
}

/// Older control planes ignore an explicit agent choice. Check support
/// before sending a request that could otherwise start Claude.
pub async fn require_supported(api: &Api, agent: Option<AgentKind>) -> Result<()> {
    if agent == Some(AgentKind::Codex) && api.instance().await?.codex_enabled != Some(true) {
        bail!("Codex is unavailable on this instance.");
    }
    Ok(())
}

#[derive(Subcommand)]
pub enum CodexCmd {
    /// Continue with ChatGPT in the browser on this machine.
    Connect {
        /// Sign in again on a connection from `fctl codex list`.
        #[arg(long, value_parser = connection_id)]
        connection: Option<CredentialId>,
        /// Ask ChatGPT to grant plan usage for this connection again.
        #[arg(long, requires = "connection")]
        enable_plan: bool,
    },
    /// List your ChatGPT connections and their status.
    List,
    /// Show your ChatGPT connections and which workspaces use them.
    Status,
    /// Choose the connection Codex sessions run on.
    Use {
        /// A connection from `fctl codex list`.
        #[arg(required_unless_present = "clear", value_parser = connection_id)]
        connection: Option<CredentialId>,
        /// Set the connection for this workspace (slug or ID).
        #[arg(short, long)]
        workspace: Option<String>,
        /// Clear this selection.
        #[arg(long, conflicts_with = "connection")]
        clear: bool,
    },
    /// Retry using a connection after checking your ChatGPT plan limits.
    Retry {
        #[arg(value_parser = connection_id)]
        connection: CredentialId,
    },
    /// Disconnect a ChatGPT account.
    Disconnect {
        #[arg(value_parser = connection_id)]
        connection: CredentialId,
    },
}

fn connection_id(s: &str) -> Result<CredentialId, String> {
    CredentialId::parse(s).ok_or_else(|| "use a connection ID from `fctl codex list`".into())
}

pub async fn run(server: Option<&str>, command: CodexCmd, json: bool) -> Result<()> {
    let api = config::api(server)?;
    let list = matches!(command, CodexCmd::List);
    match command {
        CodexCmd::Connect {
            connection,
            enable_plan,
        } => {
            let connected = connect(&api, connection, enable_plan, json).await?;
            if json {
                return style::print_json(&connected);
            }
            print_connection(&connected, false);
            return Ok(());
        }
        CodexCmd::Use {
            connection,
            workspace,
            ..
        } => {
            let workspace = match workspace {
                Some(w) => Some(api.workspace(&w).await?.id),
                None => None,
            };
            api.select_codex_connection(
                workspace.as_ref().map(|w| w.as_str()),
                &CodexSelection {
                    connection_id: connection,
                },
            )
            .await?;
        }
        CodexCmd::Disconnect { connection } => api.disconnect_codex(connection.as_str()).await?,
        CodexCmd::Retry { connection } => api.retry_codex_connection(connection.as_str()).await?,
        CodexCmd::List | CodexCmd::Status => {}
    }

    let settings = api.codex_settings().await?;
    if json {
        return style::print_json(&settings);
    }
    if !settings.enabled {
        println!("Codex is disabled on this instance.");
    } else if settings.connections.is_empty() {
        println!("Connect your ChatGPT plan with `fctl codex connect`.");
    } else {
        for c in &settings.connections {
            print_connection(c, settings.default_connection_id.as_ref() == Some(&c.id));
        }
        if !list {
            for choice in &settings.workspace_connections {
                println!(
                    "{} uses {}",
                    style::plain(choice.workspace_id.as_str()),
                    style::plain(choice.connection_id.as_str())
                );
            }
            println!("Manage usage: {USAGE_URL}");
        }
    }
    Ok(())
}

fn print_connection(connection: &CodexConnection, default: bool) {
    let status = match (
        connection.connected,
        connection.plan_enabled,
        connection.paused,
    ) {
        (false, _, _) => "Disconnected".to_owned(),
        (_, false, _) => "ChatGPT plan permission needed".to_owned(),
        (_, _, true) => match connection.reset_at {
            Some(reset) if reset > style::now_ms() => {
                format!("Usage limit reached · resets {}", style::until(reset))
            }
            _ => "Usage limit reached".into(),
        },
        _ => "Using ChatGPT plan".into(),
    };
    println!(
        "{}  {}  {}{}",
        style::plain(connection.id.as_str()),
        style::plain(connection.email.as_deref().unwrap_or("ChatGPT")),
        status,
        if default { " · default" } else { "" }
    );
}

async fn connect(
    api: &Api,
    connection_id: Option<CredentialId>,
    enable_plan: bool,
    json: bool,
) -> Result<CodexConnection> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("Opening a local callback for ChatGPT sign-in")?;
    let redirect_uri = format!("http://{}{CALLBACK}", listener.local_addr()?);
    let start = api.start_codex_login(&CodexLoginStartRequest {
        redirect_uri: redirect_uri.clone(),
        connection_id,
        enable_plan: enable_plan.then_some(true),
    }).await.map_err(|_| anyhow::anyhow!("Could not start ChatGPT sign-in. Check that Codex is enabled on this instance and the connection still exists."))?;
    let authorize = authorize_url(&start.authorize_url, &start.state, &redirect_uri)
        .context("This instance sent an invalid ChatGPT sign-in link")?;
    if !super::auth::open_browser(authorize.as_str()) {
        bail!("Could not open your browser. Run `fctl codex connect` on a machine with a browser.");
    }
    if !json {
        eprintln!("Continue with ChatGPT in your browser.");
    }

    let (mut stream, request) = tokio::time::timeout(
        Duration::from_secs(300),
        wait_for_callback(listener, &start.state),
    )
    .await
    .context("ChatGPT sign-in timed out. Run `fctl codex connect` again.")??;
    let denied = request.error.as_deref() == Some("access_denied");
    let result = api.finish_codex_login(&request).await;
    let message = if result.is_ok() {
        "ChatGPT is connected. You can return to fctl."
    } else {
        "ChatGPT sign-in could not be completed. Return to fctl to try again."
    };
    let _ = respond(&mut stream, "200 OK", message).await;
    result.map_err(|_| {
        anyhow::anyhow!(if denied {
            "You declined ChatGPT sign-in. Run `fctl codex connect` to try again."
        } else {
            "Could not complete ChatGPT sign-in. Run `fctl codex connect` to try again."
        })
    })
}

fn authorize_url(link: &str, state: &str, redirect: &str) -> Option<Url> {
    let url = Url::parse(link).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("auth.openai.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/api/accounts/authorize"
        || url.fragment().is_some()
        || state.is_empty()
    {
        return None;
    }
    for (param, value) in [("state", state), ("redirect_uri", redirect)] {
        let mut found = url.query_pairs().filter(|(name, _)| name == param);
        if !found.next().is_some_and(|(_, v)| v == value) || found.next().is_some() {
            return None;
        }
    }
    Some(url)
}

async fn wait_for_callback(
    listener: TcpListener,
    state: &str,
) -> Result<(TcpStream, CodexLoginFinishRequest)> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        if let Ok(Some(callback)) =
            tokio::time::timeout(Duration::from_secs(2), read_callback(&mut stream, state)).await
        {
            return Ok((stream, callback));
        }
        let _ = respond(
            &mut stream,
            "400 Bad Request",
            "This request does not match a pending ChatGPT sign-in.",
        )
        .await;
    }
}

async fn read_callback(stream: &mut TcpStream, state: &str) -> Option<CodexLoginFinishRequest> {
    let mut header = [0; 8192];
    let mut len = 0;
    while len < header.len() {
        let read = stream.read(&mut header[len..]).await.ok()?;
        if read == 0 {
            break;
        }
        len += read;
        if header[..len].windows(4).any(|w| w == b"\r\n\r\n") {
            return parse_callback(
                std::str::from_utf8(&header[..len]).ok()?.lines().next()?,
                state,
            );
        }
    }
    None
}

fn parse_callback(line: &str, expected: &str) -> Option<CodexLoginFinishRequest> {
    let mut line = line.split_ascii_whitespace();
    let (Some("GET"), Some(path), Some("HTTP/1.0" | "HTTP/1.1"), None) =
        (line.next(), line.next(), line.next(), line.next())
    else {
        return None;
    };
    let (CALLBACK, query) = path.split_once('?')? else {
        return None;
    };
    let mut state = None;
    let mut code = None;
    let mut client_id = None;
    let mut error = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let slot = match name.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "client_id" => &mut client_id,
            "error" => &mut error,
            _ => continue,
        };
        if value.is_empty() || slot.replace(value.into_owned()).is_some() {
            return None;
        }
    }
    let state = state.filter(|s| s == expected)?;
    if error.is_some() {
        code = None;
        client_id = None;
    } else if code.is_none() {
        return None;
    }
    Some(CodexLoginFinishRequest {
        state,
        code,
        client_id,
        error,
    })
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}",
        message.len()
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        stream.write_all(response.as_bytes()),
    )
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_binds_the_state_and_decodes_dynamic_registration() -> Result<()> {
        let result = parse_callback(
            "GET /auth/callback?state=bound&code=a%2Bb&client_id=oaiapp_example HTTP/1.1",
            "bound",
        )
        .context("expected a callback for the current attempt")?;
        assert_eq!(result.code.as_deref(), Some("a+b"));
        assert_eq!(result.client_id.as_deref(), Some("oaiapp_example"));
        for path in [
            "/auth/callback?state=other&code=x",
            "/auth/callback?state=bound&state=other&code=x",
            "/favicon.ico?state=bound&code=x",
            "/auth/callback?state=bound",
        ] {
            assert!(parse_callback(&format!("GET {path} HTTP/1.1"), "bound").is_none());
        }
        Ok(())
    }

    #[test]
    fn respects_denial_and_accepts_a_returning_registration_without_client_id() -> Result<()> {
        let result = parse_callback(
            "GET /auth/callback?state=bound&error=access_denied&code=unused HTTP/1.1",
            "bound",
        )
        .context("expected the authorization denial")?;
        assert_eq!(result.error.as_deref(), Some("access_denied"));
        assert!(result.code.is_none());
        let result = parse_callback(
            "GET /auth/callback?state=bound&code=returning HTTP/1.1",
            "bound",
        )
        .context("expected a callback for the existing registration")?;
        assert!(result.client_id.is_none());
        Ok(())
    }
}
