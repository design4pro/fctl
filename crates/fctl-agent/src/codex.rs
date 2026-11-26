//! Codex App Server's stdio RPC.

mod events;
pub mod responses;

use anyhow::{Context, Result, ensure};
pub use events::Update;
use fctl_domain::updates::Version;
use fctl_proto::runner::CodexCredential;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

const MIN_VERSION: Version = Version {
    major: 0,
    minor: 172,
    patch: 0,
};
const PROVIDER: &str = "fctl_chatgpt";
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

fn check_version(user_agent: &str) -> Result<()> {
    let version = user_agent
        .split_whitespace()
        .next()
        .and_then(|token| token.strip_prefix("fctl/"))
        .context("Codex App Server did not report a version")?;
    let version = version
        .split_once('+')
        .map_or(version, |(version, _)| version);
    let (release, prerelease) = version
        .split_once('-')
        .map_or((version, false), |(release, _)| (release, true));
    let release =
        Version::parse(release).context("Codex App Server reported an invalid version")?;
    ensure!(
        release > MIN_VERSION || (release == MIN_VERSION && !prerelease),
        "Codex App Server {MIN_VERSION} or later is required"
    );
    Ok(())
}

pub enum Output {
    Notification(Value),
    Access(responses::Signal),
    Exited,
}

type Notify = Arc<dyn Fn(Output) + Send + Sync>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

#[derive(Debug)]
pub enum RpcError {
    Closed,
    Timeout,
    InvalidReply,
    Refused { code: i64, turn_ended: bool },
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("Codex App Server closed its connection"),
            Self::Timeout => f.write_str("Codex App Server did not answer"),
            Self::InvalidReply => f.write_str("Codex App Server returned an invalid response"),
            Self::Refused { code, .. } => {
                write!(f, "Codex App Server refused the request ({code})")
            }
        }
    }
}

impl std::error::Error for RpcError {}

pub struct Launch<'a> {
    pub bin: &'a str,
    pub cwd: &'a Path,
    pub home: &'a Path,
    pub session_id: &'a str,
    pub environment: BTreeMap<String, String>,
    pub credential: &'a CodexCredential,
    pub instructions: &'a str,
}

pub struct Thread<'a> {
    pub cwd: &'a Path,
    pub system: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub resume: Option<&'a str>,
}

pub struct Codex {
    child: Child,
    writer: Option<mpsc::UnboundedSender<Value>>,
    pending: Pending,
    next_id: u64,
    pub proxy: responses::Proxy,
    _config: ConfigDir,
    history: PathBuf,
    cwd: PathBuf,
    pub thread: String,
    pub turn: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    events: events::Events,
    delivered: HashSet<String>,
}

struct ConfigDir(PathBuf);

impl ConfigDir {
    fn new(history: &Path, cwd: &Path, instructions: &str) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, symlink};

        std::fs::create_dir_all(history)?;
        let root = history.parent().context("Codex history directory")?;
        let path = root.join(format!("codex-config-{}", fctl_domain::ids::uuid_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        let config = Self(path);
        let sessions = history.join("sessions");
        std::fs::create_dir_all(&sessions)?;
        symlink(sessions, config.0.join("sessions"))?;
        std::fs::write(config.0.join("AGENTS.md"), instructions)?;
        let mut paths = BTreeSet::from([cwd.to_path_buf()]);
        if let Ok(canonical) = cwd.canonicalize() {
            paths.insert(canonical);
        }
        let mut project_config = String::new();
        for path in paths {
            let key = serde_json::to_string(&path)?;
            project_config.push_str(&format!("[projects.{key}]\ntrust_level = \"untrusted\"\n"));
        }
        std::fs::write(config.0.join("config.toml"), project_config)?;
        Ok(config)
    }
}

impl Drop for ConfigDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Codex {
    pub async fn spawn(launch: Launch<'_>, notify: Notify) -> Result<Self> {
        let history = history_home(launch.home, launch.session_id);
        let config = ConfigDir::new(&history, launch.cwd, launch.instructions)?;
        let proxy = responses::Proxy::serve(launch.credential, {
            let notify = notify.clone();
            Arc::new(move |signal| notify(Output::Access(signal)))
        })
        .await?;
        let mut command = crate::user::command_in_own_group(launch.bin);
        command
            .current_dir(&config.0)
            .env_clear()
            .env("HOME", launch.home)
            .env("CODEX_HOME", &config.0)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for key in [
            "PATH", "LANG", "LC_ALL", "TZ", "TMPDIR", "USER", "LOGNAME", "SHELL", "TERM",
        ] {
            if let Some(value) = crate::user::var(key) {
                command.env(key, value);
            }
        }
        command.envs(
            launch
                .environment
                .into_iter()
                .filter(|(key, _)| project_env(key)),
        );
        command.args(["app-server", "--listen", "stdio://", "--strict-config"]);
        command.args(["-c", &format!("model_provider=\"{PROVIDER}\"")]);
        command.args([
            "-c",
            &format!(
                "model_providers.{PROVIDER}={{name=\"ChatGPT plan\",base_url=\"{}\",wire_api=\"responses\",requires_openai_auth=false,supports_websockets=false,request_max_retries=0,stream_max_retries=0}}",
                proxy.base_url()
            ),
        ]);
        for setting in [
            "approval_policy=\"never\"",
            "sandbox_mode=\"danger-full-access\"",
            "web_search=\"disabled\"",
            "mcp_servers={}",
            "analytics.enabled=false",
            "history.persistence=\"none\"",
            "features.enable_request_compression=false",
            "features.api_key_model_discovery=false",
            "features.apps=false",
            "features.hooks=false",
            "features.computer_use=false",
            "features.browser_use=false",
            "features.image_generation=false",
        ] {
            command.args(["-c", setting]);
        }
        let mut child = command
            .spawn()
            .context("Starting Codex App Server (is Codex installed?)")?;
        let mut stdin = child.stdin.take().context("Codex App Server stdin")?;
        let stdout = child.stdout.take().context("Codex App Server stdout")?;
        let (writer, mut writes) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(value) = writes.recv().await {
                let Ok(mut bytes) = serde_json::to_vec(&value) else {
                    continue;
                };
                bytes.push(b'\n');
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Arc::default();
        let incoming = pending.clone();
        let responses = writer.downgrade();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message.get("method").is_none() {
                    deliver_reply(&incoming, message);
                } else if let Some(id) = message.get("id") {
                    if let Some(writer) = responses.upgrade() {
                        let _ = writer.send(json!({
                            "id": id,
                            "error": {"code": -32601, "message": "This client does not support that request"}
                        }));
                    }
                } else {
                    notify(Output::Notification(message));
                }
            }
            incoming.lock().unwrap_or_else(|e| e.into_inner()).clear();
            notify(Output::Exited);
        });

        Ok(Self {
            child,
            writer: Some(writer),
            pending,
            next_id: 0,
            proxy,
            _config: config,
            history,
            cwd: launch.cwd.to_path_buf(),
            thread: String::new(),
            turn: None,
            model: String::new(),
            effort: None,
            events: events::Events::default(),
            delivered: HashSet::new(),
        })
    }

    pub async fn open(&mut self, thread: Thread<'_>) -> Result<()> {
        let init = self.request("initialize", json!({
            "clientInfo": {"name": "fctl", "title": "fctl", "version": crate::supervisor::VERSION}
        })).await.context("Initializing Codex App Server")?;
        let version = init
            .get("userAgent")
            .and_then(Value::as_str)
            .unwrap_or_default();
        check_version(version)?;
        self.write(json!({"method": "initialized"}))?;
        let mut params = json!({
            "cwd": thread.cwd,
            "modelProvider": PROVIDER,
            "developerInstructions": thread.system,
            "approvalPolicy": "never",
            "sandbox": "danger-full-access"
        });
        if let Some(model) = thread.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = thread.effort {
            params["config"] = json!({"model_reasoning_effort": effort});
        }
        let method = match thread.resume {
            Some(id) => {
                params["threadId"] = json!(id);
                params["excludeTurns"] = json!(true);
                "thread/resume"
            }
            None => "thread/start",
        };
        let reply = self
            .request(method, params)
            .await
            .with_context(|| format!("Codex {method}"))?;
        let id = reply
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .context("Codex did not return a thread ID")?;
        ensure!(
            thread.resume.is_none_or(|expected| expected == id),
            "Codex returned a different thread"
        );
        ensure!(
            reply.get("modelProvider").and_then(Value::as_str) == Some(PROVIDER),
            "Codex selected a different Responses provider"
        );
        self.thread = id.to_owned();
        self.model = reply
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.effort = reply
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if thread.resume.is_some()
            && let Some(path) = rollout(&self.history, id)
        {
            self.delivered = delivered_in(&path);
        }
        Ok(())
    }

    pub fn was_delivered(&self, id: &str) -> bool {
        self.delivered.contains(id)
    }

    pub async fn send(
        &mut self,
        text: &str,
        images: Vec<Value>,
        delivery: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<(), RpcError> {
        let mut input = vec![json!({"type": "text", "text": text, "text_elements": []})];
        input.extend(images.into_iter().filter_map(|v| {
            let mime = v.pointer("/source/media_type")?.as_str()?;
            let data = v.pointer("/source/data")?.as_str()?;
            Some(json!({"type":"image", "url": format!("data:{mime};base64,{data}")}))
        }));
        let mut params = json!({"threadId": self.thread, "input": input});
        if let Some(id) = delivery {
            params["clientUserMessageId"] = json!(id);
        }
        if let Some(turn) = &self.turn {
            params["expectedTurnId"] = json!(turn);
            match self.request("turn/steer", params.clone()).await {
                Ok(_) => return Ok(()),
                Err(RpcError::Refused {
                    turn_ended: true, ..
                }) => {}
                Err(e) => return Err(e),
            }
            if let Some(params) = params.as_object_mut() {
                params.remove("expectedTurnId");
            }
        }
        params["cwd"] = json!(self.cwd);
        if let Some(model) = model {
            params["model"] = json!(model);
        }
        if let Some(effort) = effort {
            params["effort"] = json!(effort);
        }
        let reply = self.request("turn/start", params).await?;
        self.turn = Some(
            reply
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .ok_or(RpcError::InvalidReply)?
                .to_owned(),
        );
        Ok(())
    }

    pub fn interrupt(&mut self) {
        if let Some(turn) = &self.turn {
            let params = json!({"threadId": self.thread, "turnId": turn});
            self.next_id += 1;
            let _ = self
                .write(json!({"id": self.next_id, "method": "turn/interrupt", "params": params}));
        }
    }

    pub fn notification(&mut self, value: &Value) -> Option<Update> {
        if value.pointer("/params/threadId").and_then(Value::as_str) != Some(&self.thread) {
            return None;
        }
        let update = self.events.project(value, self.turn.as_deref())?;
        if matches!(&update, Update::Finished { current: true, .. }) {
            self.turn = None;
        }
        Some(update)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    pub async fn stop(&mut self) {
        self.writer.take();
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let id = self.next_id;
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if let Err(error) = self.write(json!({"id": id, "method": method, "params": params})) {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(RpcError::Closed),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(RpcError::Timeout)
            }
        }
    }

    fn write(&self, value: Value) -> Result<(), RpcError> {
        self.writer
            .as_ref()
            .ok_or(RpcError::Closed)?
            .send(value)
            .map_err(|_| RpcError::Closed)
    }
}

fn deliver_reply(incoming: &Pending, mut message: Value) {
    let Some(id) = message.get("id").and_then(Value::as_u64) else {
        return;
    };
    let Some(listener) = incoming
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
    else {
        return;
    };
    let answer = if let Some(error) = message.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let turn_ended =
            error.get("message").and_then(Value::as_str) == Some("no active turn to steer");
        Err(RpcError::Refused { code, turn_ended })
    } else if let Some(result) = message.get_mut("result") {
        Ok(result.take())
    } else {
        Err(RpcError::InvalidReply)
    };
    let _ = listener.send(answer);
}

fn project_env(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    !matches!(
        key.as_str(),
        "ACCESS_TOKEN"
            | "HOME"
            | "PATH"
            | "SHELL"
            | "BASH_ENV"
            | "ENV"
            | "RUST_LOG"
            | "RUST_BACKTRACE"
            | "NODE_OPTIONS"
            | "ANTHROPIC_API_KEY"
    ) && ![
        "CODEX_",
        "OPENAI_",
        "CHATGPT_",
        "ANTHROPIC_",
        "CLAUDE_CODE_",
        "XDG_",
        "LD_",
        "DYLD_",
        "OTEL_",
        "SSL_CERT_",
        "CURL_CA_",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
        && !key.ends_with("_PROXY")
}

pub fn history_home(home: &Path, session_id: &str) -> PathBuf {
    // The fctl session ID names a sandbox's persistent history, independent
    // of the thread UUID the App Server assigns.
    home.join(".fctl/codex-history").join(session_id)
}

pub fn rollout(history: &Path, thread: &str) -> Option<PathBuf> {
    if !valid_id(thread) {
        return None;
    }
    let base = history.join("sessions");
    let mut dirs = vec![(base, 0)];
    let suffix = format!("-{thread}.jsonl");
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && depth < 3 {
                dirs.push((entry.path(), depth + 1));
            } else if kind.is_file()
                && depth == 3
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
            {
                return Some(entry.path());
            }
        }
    }
    None
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn delivered_in(rollout: &Path) -> HashSet<String> {
    let Ok(file) = std::fs::File::open(rollout) else {
        return HashSet::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|line| line.contains("\"item_completed\""))
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter(|line| {
            line.pointer("/payload/item/type").and_then(Value::as_str) == Some("UserMessage")
        })
        .filter_map(|line| {
            line.pointer("/payload/item/client_id")?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fctl_proto::CredentialId;
    use std::io::Write;

    const THREAD_ID: &str = "0193fe6b-0000-7000-8000-000000000001";
    const FAKE: &str = r#"#!/bin/sh
set -eu
test -z "${OPENAI_API_KEY:-}${HTTPS_PROXY:-}${ACCESS_TOKEN:-}${CODEX_API_KEY:-}"
test "$(cat "$CODEX_HOME/AGENTS.md")" = 'Shared fctl instructions'
grep -Fx "[projects.\"$HOME\"]" "$CODEX_HOME/config.toml" >/dev/null
case "$*" in
    *requires_openai_auth=false*supports_websockets=false*) ;;
    *) exit 42 ;;
esac
state=initializing
while IFS= read -r msg; do
    printf '%s\n' "$msg" >> "$HOME/rpc.jsonl"
    id=$(printf '%s' "$msg" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
    case "$msg" in
        *'"method":"initialize"'*)
            printf '{"id":%s,"result":{"userAgent":"fctl/0.173.0-alpha.5 (fixture)"}}\n' "$id"
            ;;
        *'"method":"initialized"'*) state=ready ;;
        *'"method":"thread/start"'*|*'"method":"thread/resume"'*)
            test "$state" = ready
            printf '{"id":%s,"result":{"thread":{"id":"0193fe6b-0000-7000-8000-000000000001"},"modelProvider":"fctl_chatgpt","model":"gpt-5.5"}}\n' "$id"
            ;;
        *'"method":"turn/start"'*)
            printf '{"id":%s,"result":{"turn":{"id":"turn-%s"}}}\n' "$id" "$id"
            if [ "$id" = 3 ]; then
                mkdir -p "$CODEX_HOME/sessions/2026/11/26"
                printf '%s\n' '{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","client_id":"dlv_first"}}}' > "$CODEX_HOME/sessions/2026/11/26/rollout-fixture-0193fe6b-0000-7000-8000-000000000001.jsonl"
            fi
            ;;
        *'"method":"turn/steer"'*'"text":"raced"'*)
            printf '{"id":%s,"error":{"code":-32600,"message":"no active turn to steer"}}\n' "$id"
            ;;
        *'"method":"turn/steer"'*)
            printf '{"id":%s,"result":{"turnId":"turn-3"}}\n' "$id"
            ;;
        *'"method":"turn/interrupt"'*)
            printf '{"id":%s,"result":{}}\n' "$id"
            printf '%s\n' '{"method":"turn/completed","params":{"threadId":"another-thread","turn":{"id":"turn-6","status":"completed"}}}'
            printf '%s\n' '{"method":"turn/completed","params":{"threadId":"0193fe6b-0000-7000-8000-000000000001","turn":{"id":"turn-6","status":"interrupted"}}}'
            ;;
    esac
done
"#;

    async fn fake(dir: &Path) -> Result<PathBuf> {
        let bin = dir.join("codex");
        let mut child = std::process::Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "sh"])
            .arg(&bin)
            .stdin(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("writing test app server")?
            .write_all(FAKE.as_bytes())?;
        ensure!(child.wait()?.success(), "writing test app server");
        Ok(bin)
    }

    #[test]
    fn checks_the_minimum_app_server_version() -> Result<()> {
        for agent in [
            "fctl/0.172.0 (fixture)",
            "fctl/0.172.0+build.1 (fixture)",
            "fctl/0.172.1 (fixture)",
            "fctl/0.173.0-alpha.5 (fixture)",
        ] {
            check_version(agent)?;
        }
        for agent in ["fctl/0.171.99 (fixture)", "fctl/0.172.0-alpha.1 (fixture)"] {
            let Err(error) = check_version(agent) else {
                anyhow::bail!("Accepted unsupported version {agent}");
            };
            assert_eq!(
                error.to_string(),
                "Codex App Server 0.172.0 or later is required"
            );
        }
        assert!(check_version("").is_err());
        assert!(check_version("fctl/invalid (fixture)").is_err());
        Ok(())
    }

    #[tokio::test]
    async fn starts_steers_interrupts_and_resumes_one_thread() -> Result<()> {
        let dir = tempfile::Builder::new().prefix("fctl.codex.").tempdir()?;
        let bin = fake(dir.path()).await?;
        let cred = CodexCredential {
            id: CredentialId("crd_fixture".into()),
            access_token: "fixture-oauth".into(),
            expires_at: i64::MAX,
        };
        let (out, mut outputs) = mpsc::unbounded_channel();
        let notify: Notify = Arc::new(move |event| {
            let _ = out.send(event);
        });
        let launch = || Launch {
            bin: bin.to_str().unwrap_or_default(),
            cwd: dir.path(),
            home: dir.path(),
            session_id: "ses_fixture",
            environment: [
                "OPENAI_API_KEY",
                "HTTPS_PROXY",
                "ACCESS_TOKEN",
                "CODEX_API_KEY",
            ]
            .into_iter()
            .map(|key| (key.to_owned(), "should not reach Codex".to_owned()))
            .collect(),
            credential: &cred,
            instructions: "Shared fctl instructions",
        };
        let thread = |resume| Thread {
            cwd: dir.path(),
            system: "Developer instructions",
            model: Some("gpt-5.5"),
            effort: Some("high"),
            resume,
        };
        let mut codex = Codex::spawn(launch(), notify.clone()).await?;
        codex.open(thread(None)).await?;
        assert_eq!(codex.thread, THREAD_ID);
        codex
            .send(
                "task",
                vec![json!({"source": {"media_type": "image/png", "data": "fixture-image"}})],
                Some("dlv_first"),
                None,
                None,
            )
            .await?;
        codex
            .send("steer", Vec::new(), Some("dlv_second"), None, None)
            .await?;
        codex
            .send("raced", Vec::new(), Some("dlv_raced"), None, None)
            .await?;
        assert_eq!(codex.turn.as_deref(), Some("turn-6"));
        codex.interrupt();

        for belongs in [false, true] {
            let out = tokio::time::timeout(Duration::from_secs(3), outputs.recv()).await?;
            let Some(Output::Notification(event)) = out else {
                anyhow::bail!("expected turn completion");
            };
            let projected = codex.notification(&event);
            assert_eq!(projected.is_some(), belongs);
            if let Some(Update::Finished { current, result }) = projected {
                assert!(current);
                assert_eq!(result["subtype"], "interrupted");
            }
        }
        codex.stop().await;
        assert!(codex.turn.is_none());

        let mut resumed = Codex::spawn(launch(), notify).await?;
        resumed.open(thread(Some(THREAD_ID))).await?;
        assert!(resumed.was_delivered("dlv_first"));
        assert!(!resumed.was_delivered("dlv_unseen"));
        resumed.stop().await;

        let trace = std::fs::read_to_string(dir.path().join("rpc.jsonl"))?;
        assert!(trace.contains("\"expectedTurnId\":\"turn-3\""));
        assert!(trace.contains("\"clientUserMessageId\":\"dlv_first\""));
        assert!(trace.contains("data:image/png;base64,fixture-image"));
        assert!(trace.contains("\"method\":\"thread/resume\""));
        assert!(trace.contains("Developer instructions"));
        assert!(!trace.contains(&cred.access_token));
        Ok(())
    }
}
