use super::*;
use anyhow::Context;

const AUTH_EARLY_MS: i64 = 60_000;
const AUTH_WAIT_MS: i64 = 45_000;

impl Supervisor {
    pub(super) async fn start_codex(&mut self) -> Result<()> {
        let (Some(config), Some(paths)) = (self.config.clone(), self.paths.clone()) else {
            anyhow::bail!("not configured");
        };
        let spec = &config.spec;
        let history = codex::history_home(&paths.home, spec.session_id.as_str());
        let thread = self.persisted.codex_thread.clone();
        let wants_resume = spec.resume || thread.is_some();
        let previous = thread
            .as_deref()
            .filter(|id| codex::rollout(&history, id).is_some());
        let (instructions, _) = self
            .agent_instructions(&config, &paths, previous.is_some())
            .await?;
        let env = self.agent_environment(&config);
        let system = self.agent_system(&config, &paths).await;
        let credential = self
            .codex_credential
            .as_ref()
            .context("ChatGPT plan credential is missing")?;
        self.codex_generation += 1;
        let generation = self.codex_generation;
        let work = self.work_tx.clone();
        let launch = codex::Launch {
            bin: "codex",
            cwd: &paths.workdir,
            home: &paths.home,
            session_id: spec.session_id.as_str(),
            environment: env,
            credential,
            instructions: instructions.as_deref().unwrap_or_default(),
        };
        let mut codex = Codex::spawn(
            launch,
            std::sync::Arc::new(move |output| {
                let _ = work.send(Work::Codex(generation, output));
            }),
        )
        .await?;
        self.persisted.claude = codex.pid().and_then(orphan::Process::of);
        self.persist();
        let thread = codex::Thread {
            cwd: &paths.workdir,
            system: &system,
            model: spec
                .model
                .as_deref()
                .or(spec.project.settings.codex_model.as_deref()),
            effort: spec
                .effort
                .as_deref()
                .or(spec.project.settings.codex_effort.as_deref()),
            resume: previous,
        };
        if let Err(e) = codex.open(thread).await {
            codex.stop().await;
            return Err(e);
        }
        self.log(LogSource::Agent, LogLevel::Info, "Started Codex App Server");
        self.persisted.codex_thread = Some(codex.thread.clone());
        self.persist();
        self.state.model = Some(codex.model.clone());
        self.state.effort.clone_from(&codex.effort);
        self.emit(EventBody::Codex(json!({
            "type": "system", "subtype": "init", "model": codex.model, "cwd": paths.workdir
        })));
        self.oom_kills = oom::Count::start(Instant::now(), oom::kills());
        self.kernel_log = self.watches_memory().then(oom::KernelLog::open).flatten();
        self.memory = oom::Watch::default();
        if let Some(limited) = &self.persisted.codex_limit {
            codex.proxy.set_limited(true);
            self.resume_at = limited.reset_at;
        }
        self.codex = Some(codex);
        if self.persisted.codex_limit.is_some() {
            if previous.is_none() {
                self.codex_deferred_start = Some((false, wants_resume));
            }
            self.set_status(
                SessionStatus::Paused,
                Some("Waiting for the ChatGPT plan usage limit to reset".into()),
            );
            return Ok(());
        }
        if self.codex_auth_paused
            || self
                .codex_credential
                .as_ref()
                .is_some_and(|c| c.expires_at <= now_ms())
        {
            self.codex_deferred_start =
                Some((previous.is_some(), wants_resume && previous.is_none()));
            self.pause_for_codex_credentials();
            self.check_codex_credentials(now_ms());
            return Ok(());
        }
        self.initial_turns(spec, previous.is_some(), wants_resume && previous.is_none())
            .await;
        Ok(())
    }

    pub(super) fn codex_settings(
        &mut self,
        delivery: &DeliveryId,
        model: Option<&String>,
        effort: Option<&String>,
    ) {
        if effort.is_some_and(|e| {
            !matches!(
                e.as_str(),
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        }) {
            self.refuse_delivery(delivery, "Codex does not support that effort level".into());
            return;
        }
        if let Some(config) = &mut self.config {
            if let Some(model) = model {
                config.spec.model = Some(model.clone());
                self.state.model = Some(model.clone());
            }
            if let Some(effort) = effort {
                config.spec.effort = Some(effort.clone());
                self.state.effort = Some(effort.clone());
            }
        }
        self.ack_delivery(delivery);
        self.send(&AgentToHost::State {
            state: self.state.clone(),
        });
    }

    pub(super) async fn on_codex(&mut self, output: codex::Output) {
        if self.codex.is_none() || self.exit_after.is_some() {
            return;
        }
        match output {
            codex::Output::Notification(value) => {
                let Some(update) = self.codex.as_mut().and_then(|c| c.notification(&value)) else {
                    return;
                };
                match update {
                    codex::Update::Messages { journal, stream } => {
                        for event in stream {
                            self.send(&AgentToHost::Stream { event });
                        }
                        for event in journal {
                            self.refresh.observe(&event, Instant::now());
                            self.emit(EventBody::Codex(event));
                        }
                    }
                    codex::Update::Finished { current, result } => {
                        self.emit(EventBody::Codex(result));
                        if current {
                            self.turn_finished(false).await;
                            if self.stopping.is_some() {
                                self.finish().await;
                            } else {
                                self.drain_deliveries().await;
                            }
                        }
                    }
                    codex::Update::Usage { window, call } => {
                        if let Some(event) = call {
                            self.emit(EventBody::Codex(event));
                        }
                        let window =
                            window
                                .filter(|tokens| *tokens > 0)
                                .map(|tokens| ContextWindow {
                                    tokens,
                                    compact_at: None,
                                });
                        if window.is_some() && self.state.context != window {
                            self.state.context = window;
                            self.send(&AgentToHost::State {
                                state: self.state.clone(),
                            });
                        }
                    }
                }
            }
            codex::Output::Access(codex::responses::Signal::Limit { reset_at }) => {
                self.pause_codex_limit(reset_at, true);
            }
            codex::Output::Access(codex::responses::Signal::CredentialRejected) => {
                self.codex_credential_rejected();
            }
            codex::Output::Exited => self.on_codex_exit().await,
        }
    }

    pub(super) fn pause_codex_limit(&mut self, reset_at: Option<i64>, report: bool) {
        if self.persisted.codex_limit.is_some() || self.state.status.is_terminal() {
            return;
        }
        let reset_at = reset_at.filter(|at| *at > now_ms());
        let run_id = self.config.as_ref().and_then(|c| c.spec.run_id.clone());
        self.persisted.codex_limit = Some(CodexLimit {
            run_id,
            reset_at,
            resume_work: self.turn_active,
        });
        self.persist();
        self.resume_at = reset_at;
        if let Some(c) = &self.codex {
            c.proxy.set_limited(true);
        }
        if report {
            let credential_id = self.codex_credential.as_ref().map(|c| c.id.clone());
            self.emit(EventBody::CodexLimit {
                credential_id,
                reset_at,
            });
        }
        if self.stopping.is_none() {
            self.set_status(
                SessionStatus::Paused,
                Some("Waiting for the ChatGPT plan usage limit to reset".into()),
            );
            self.interrupt_turn();
        }
    }

    pub(super) async fn reset_codex_limit(&mut self) {
        let resume_work = self
            .persisted
            .codex_limit
            .take()
            .is_some_and(|limit| limit.resume_work);
        self.persist();
        if let Some(c) = &self.codex {
            c.proxy.set_limited(false);
        }
        if self.codex_auth_paused {
            self.set_status(
                SessionStatus::Paused,
                Some("Waiting for a fresh ChatGPT credential".into()),
            );
            return;
        }
        self.resume_codex_work(resume_work).await;
    }

    async fn resume_codex_work(&mut self, resume_turn: bool) {
        if let Some((resume, fresh_resume)) = self.codex_deferred_start.take()
            && let Some(spec) = self.config.as_ref().map(|c| c.spec.clone())
        {
            self.set_status(SessionStatus::Preparing, None);
            self.initial_turns(&spec, resume, fresh_resume).await;
            return;
        }
        if resume_turn {
            self.set_status(
                SessionStatus::Running,
                Some("Resuming after the ChatGPT plan usage limit reset".into()),
            );
            self.send_turn(
                "The ChatGPT plan usage limit has reset. Check what was done before continuing.",
                None,
                None,
                &[],
            )
            .await;
        }
        if self.turn_active {
            self.set_status(SessionStatus::Running, None);
        } else {
            self.wait().await;
            self.drain_deliveries().await;
        }
    }

    fn codex_credential_rejected(&mut self) {
        self.codex_credential_rejected = true;
        self.pause_for_codex_credentials();
        self.check_codex_credentials(now_ms());
    }

    fn pause_for_codex_credentials(&mut self) {
        if self.codex_auth_paused || self.stopping.is_some() {
            return;
        }
        self.codex_auth_paused = true;
        if self.persisted.codex_limit.is_none() {
            self.set_status(
                SessionStatus::Paused,
                Some("Waiting for a fresh ChatGPT credential".into()),
            );
        }
        self.interrupt_turn();
    }

    pub(super) fn check_codex_credentials(&mut self, now: i64) {
        if !self.is_codex() || self.stopping.is_some() || self.state.status.is_terminal() {
            return;
        }
        let Some(expires) = self.codex_credential.as_ref().map(|c| c.expires_at) else {
            return;
        };
        if expires <= now && self.codex.is_some() {
            self.pause_for_codex_credentials();
        }
        if self
            .codex_refresh
            .as_ref()
            .is_some_and(|(_, until)| now >= *until)
        {
            self.codex_refresh = None;
        }
        if self.host.is_some()
            && self.codex_refresh.is_none()
            && now >= self.codex_refresh_after
            && (self.codex_auth_paused || expires.saturating_sub(now) <= AUTH_EARLY_MS)
        {
            let request_id = self.request_id();
            self.codex_refresh = Some((request_id.clone(), now + AUTH_WAIT_MS));
            self.codex_refresh_after = now + AUTH_EARLY_MS;
            let kind = if self.codex_credential_rejected {
                CredentialKind::CodexRejected
            } else {
                CredentialKind::Codex
            };
            self.send(&AgentToHost::CredentialRequest { request_id, kind });
        }
    }

    pub(super) async fn on_codex_credentials(
        &mut self,
        request_id: Option<&str>,
        credential: Option<CodexCredential>,
    ) {
        if !self.is_codex() {
            return;
        }
        if let Some(id) = request_id {
            if !self
                .codex_refresh
                .as_ref()
                .is_some_and(|(pending, _)| pending == id)
            {
                return;
            }
            self.codex_refresh = None;
        }
        let Some(credential) = credential.filter(|c| c.expires_at > now_ms()) else {
            return;
        };
        if self.codex_credential.as_ref().is_some_and(|old| {
            old.id != credential.id
                || (self.codex_credential_rejected && old.access_token == credential.access_token)
        }) {
            return;
        }
        if self
            .codex
            .as_ref()
            .is_some_and(|c| !matches!(c.proxy.renew(&credential), Ok(true)))
        {
            return;
        }
        self.codex_credential = Some(credential);
        self.codex_credential_rejected = false;
        if !std::mem::take(&mut self.codex_auth_paused)
            || self.stopping.is_some()
            || self.persisted.codex_limit.is_some()
        {
            return;
        }
        self.resume_codex_work(false).await;
    }

    async fn on_codex_exit(&mut self) {
        let Some(mut codex) = self.codex.take() else {
            return;
        };
        codex.stop().await;
        if let Some(group) = self.persisted.claude.take() {
            group.stop(Duration::from_secs(5)).await;
        }
        let active = std::mem::take(&mut self.turn_active);
        if active && self.state.status == SessionStatus::Running {
            self.persisted.cut_off = Some(CutOff::Stopped);
        }
        self.persist();
        if self.stopping.is_some() {
            self.finish().await;
            return;
        }
        if self.state.status.is_terminal() {
            return;
        }
        let now = Instant::now();
        self.restarts
            .retain(|t| now.duration_since(*t) < Duration::from_secs(600));
        if self.restarts.len() >= 3 {
            self.set_status(
                SessionStatus::Failed,
                Some("Codex App Server exited repeatedly".into()),
            );
            self.finish().await;
            return;
        }
        self.restarts.push_back(now);
        self.notice(
            NoticeLevel::Warn,
            "codex_restarted",
            "Codex App Server exited; resuming its thread",
            None,
        );
        if let Err(e) = self.start_codex().await {
            self.set_status(
                SessionStatus::Failed,
                Some(format!("Codex could not restart: {e:#}")),
            );
            self.finish().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_limit_without_a_reset_pauses_and_survives_a_restart() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().to_path_buf();
        let (mut supervisor, _, _) = Supervisor::new(path.clone(), None)?;
        supervisor.persisted.agent_kind = Some(AgentKind::Codex);
        supervisor.codex_credential = Some(CodexCredential {
            id: fctl_proto::CredentialId("crd_fixture".into()),
            access_token: "fixture_oauth".into(),
            expires_at: now_ms() + 600_000,
        });
        supervisor.pause_codex_limit(None, true);
        supervisor.pause_codex_limit(Some(now_ms() + 60_000), true);
        assert_eq!(supervisor.state.status, SessionStatus::Paused);
        assert_eq!(supervisor.state.resume_at, None);
        let events = supervisor.journal.read_after(0)?;
        let limits: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.body {
                EventBody::CodexLimit {
                    reset_at,
                    credential_id,
                } => Some((*reset_at, credential_id.as_ref().map(|id| id.as_str()))),
                _ => None,
            })
            .collect();
        assert_eq!(limits, [(None, Some("crd_fixture"))]);
        drop(supervisor);
        let (restarted, _, _) = Supervisor::new(path, None)?;
        assert!(restarted.is_codex());
        assert!(restarted.persisted.codex_limit.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn a_pause_from_the_host_does_not_report_the_limit_again() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (mut supervisor, _, _) = Supervisor::new(dir.path().to_path_buf(), None)?;
        let reset_at = Some(now_ms() + 60_000);
        supervisor
            .on_frame(HostToAgent::PauseCodex { reset_at })
            .await;
        assert_eq!(supervisor.state.status, SessionStatus::Provisioning);
        supervisor.persisted.agent_kind = Some(AgentKind::Codex);
        supervisor.set_status(SessionStatus::Idle, None);
        supervisor
            .on_frame(HostToAgent::PauseCodex { reset_at })
            .await;
        assert_eq!(supervisor.state.status, SessionStatus::Paused);
        assert_eq!(supervisor.state.resume_at, reset_at);
        let events = supervisor.journal.read_after(0)?;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.body, EventBody::CodexLimit { .. }))
        );
        supervisor.on_frame(HostToAgent::ResumeCodex).await;
        assert!(supervisor.resume_at.is_some_and(|at| at <= now_ms()));
        supervisor.reset_codex_limit().await;
        assert_eq!(supervisor.state.status, SessionStatus::Idle);
        assert!(!supervisor.turn_active);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_credentials_request_one_renewal_and_do_not_resend_a_turn() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (mut supervisor, _, _) = Supervisor::new(dir.path().to_path_buf(), None)?;
        supervisor.persisted.agent_kind = Some(AgentKind::Codex);
        let credential = |id: &str, token: &str| CodexCredential {
            id: fctl_proto::CredentialId(id.to_owned()),
            access_token: token.to_owned(),
            expires_at: now_ms() + 600_000,
        };
        supervisor.codex_credential = Some(credential("crd_fixture", "fixture_oauth"));
        let (host, mut out) = mpsc::unbounded_channel();
        supervisor.host = Some((1, host));
        supervisor.codex_credential_rejected();
        supervisor.check_codex_credentials(now_ms());
        supervisor.check_codex_credentials(now_ms());
        assert_eq!(supervisor.state.status, SessionStatus::Paused);
        let requests: Vec<_> = std::iter::from_fn(|| out.try_recv().ok())
            .map(|text| serde_json::from_str::<AgentToHost>(&text))
            .collect::<std::result::Result<_, _>>()?;
        let [
            AgentToHost::CredentialRequest {
                request_id,
                kind: CredentialKind::CodexRejected,
            },
        ] = requests
            .iter()
            .filter(|m| matches!(m, AgentToHost::CredentialRequest { .. }))
            .collect::<Vec<_>>()
            .as_slice()
        else {
            anyhow::bail!("Expected one Codex credential request");
        };
        let id = request_id.clone();
        supervisor
            .on_codex_credentials(Some(&id), Some(credential("crd_fixture", "fixture_oauth")))
            .await;
        assert_eq!(supervisor.state.status, SessionStatus::Paused);
        supervisor
            .on_codex_credentials(None, Some(credential("crd_fixture", "fixture_new")))
            .await;
        assert_eq!(supervisor.state.status, SessionStatus::Idle);
        assert!(!supervisor.turn_active);
        assert!(supervisor.codex_refresh.is_none());
        assert!(
            !std::fs::read_to_string(dir.path().join("state.json"))
                .unwrap_or_default()
                .contains("fixture_new")
        );
        Ok(())
    }
}
