use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Context;
use codex_protocol::protocol::Event;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;

use crate::app_server::upstream_protocol::ClientInfo;
use crate::app_server::upstream_protocol::InitializeParams;
use crate::app_server::upstream_protocol::JSONRPCMessage;
use crate::app_server::upstream_protocol::RequestId;
use crate::app_server::upstream_protocol::Result as JsonRpcResult;

use super::protocol::POTTER_EVENT_NOTIFICATION_METHOD;
use super::protocol::PotterAppServerClientNotification;
use super::protocol::PotterAppServerClientRequest;
use super::protocol::ProjectInterruptParams;
use super::protocol::ProjectListParams;
use super::protocol::ProjectListResponse;
use super::protocol::ProjectResumeParams;
use super::protocol::ProjectResumeResponse;
use super::protocol::ProjectStartParams;
use super::protocol::ProjectStartResponse;
use super::protocol::ProjectStartRoundsParams;
use super::protocol::ProjectStartRoundsResponse;

pub struct PotterAppServerClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout_lines: tokio::io::Lines<BufReader<ChildStdout>>,
    next_id: i64,
}

impl PotterAppServerClient {
    pub async fn spawn(
        workdir: PathBuf,
        codex_bin: String,
        rounds: NonZeroUsize,
        launch: crate::app_server::AppServerLaunchConfig,
    ) -> anyhow::Result<Self> {
        let exe = std::env::current_exe().context("resolve codex-potter executable path")?;

        let mut cmd = Command::new(exe);
        cmd.kill_on_drop(true);
        cmd.current_dir(&workdir);

        cmd.arg("--codex-bin");
        cmd.arg(&codex_bin);

        cmd.arg("--rounds");
        cmd.arg(rounds.get().to_string());

        if launch.bypass_approvals_and_sandbox {
            cmd.arg("--dangerously-bypass-approvals-and-sandbox");
        }

        if let Some(mode) = launch.spawn_sandbox {
            cmd.arg("--sandbox");
            cmd.arg(crate::app_server::sandbox_mode_cli_arg(mode));
        }

        let mut child = cmd
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn codex-potter app-server")?;

        let stdin = child
            .stdin
            .take()
            .context("potter app-server stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("potter app-server stdout unavailable")?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout_lines: BufReader::new(stdout).lines(),
            next_id: 1,
        })
    }

    pub async fn initialize(&mut self) -> anyhow::Result<()> {
        let request_id = self.next_request_id();
        let request = PotterAppServerClientRequest::Initialize {
            request_id: request_id.clone(),
            params: InitializeParams {
                client_info: ClientInfo {
                    name: "codex-potter".to_string(),
                    title: Some("codex-potter".to_string()),
                    version: codex_tui::CODEX_POTTER_VERSION.to_string(),
                },
            },
        };

        let mut buffered_events = Vec::new();
        let _: serde_json::Value = self
            .send_request(request_id, request, &mut buffered_events)
            .await?;
        anyhow::ensure!(
            buffered_events.is_empty(),
            "internal error: unexpected events during potter app-server initialize"
        );

        self.send_notification(PotterAppServerClientNotification::Initialized)
            .await?;
        Ok(())
    }

    pub async fn project_list(
        &mut self,
        params: ProjectListParams,
        buffered_events: &mut Vec<Event>,
    ) -> anyhow::Result<ProjectListResponse> {
        let request_id = self.next_request_id();
        self.send_request(
            request_id.clone(),
            PotterAppServerClientRequest::ProjectList { request_id, params },
            buffered_events,
        )
        .await
    }

    pub async fn project_start(
        &mut self,
        params: ProjectStartParams,
        buffered_events: &mut Vec<Event>,
    ) -> anyhow::Result<ProjectStartResponse> {
        let request_id = self.next_request_id();
        self.send_request(
            request_id.clone(),
            PotterAppServerClientRequest::ProjectStart { request_id, params },
            buffered_events,
        )
        .await
    }

    pub async fn project_resume(
        &mut self,
        params: ProjectResumeParams,
        buffered_events: &mut Vec<Event>,
    ) -> anyhow::Result<ProjectResumeResponse> {
        let request_id = self.next_request_id();
        self.send_request(
            request_id.clone(),
            PotterAppServerClientRequest::ProjectResume { request_id, params },
            buffered_events,
        )
        .await
    }

    pub async fn project_start_rounds(
        &mut self,
        params: ProjectStartRoundsParams,
        buffered_events: &mut Vec<Event>,
    ) -> anyhow::Result<ProjectStartRoundsResponse> {
        let request_id = self.next_request_id();
        self.send_request(
            request_id.clone(),
            PotterAppServerClientRequest::ProjectStartRounds { request_id, params },
            buffered_events,
        )
        .await
    }

    pub async fn project_interrupt(
        &mut self,
        params: ProjectInterruptParams,
        buffered_events: &mut Vec<Event>,
    ) -> anyhow::Result<()> {
        let request_id = self.next_request_id();
        let _: serde_json::Value = self
            .send_request(
                request_id.clone(),
                PotterAppServerClientRequest::ProjectInterrupt { request_id, params },
                buffered_events,
            )
            .await?;
        Ok(())
    }

    pub async fn read_next_event(&mut self) -> anyhow::Result<Option<Event>> {
        loop {
            let Some(line) = self
                .stdout_lines
                .next_line()
                .await
                .context("read potter app-server stdout line")?
            else {
                return Ok(None);
            };

            if line.trim().is_empty() {
                continue;
            }

            let msg: JSONRPCMessage = serde_json::from_str(&line)
                .with_context(|| format!("decode potter app-server JSON-RPC: {line:?}"))?;

            match msg {
                JSONRPCMessage::Notification(notification) => {
                    if notification.method == POTTER_EVENT_NOTIFICATION_METHOD {
                        let params = notification
                            .params
                            .context("potter app-server event notification missing params")?;
                        let event: Event = serde_json::from_value(params)
                            .context("deserialize potter app-server event payload")?;
                        return Ok(Some(event));
                    }
                }
                JSONRPCMessage::Request(_)
                | JSONRPCMessage::Response(_)
                | JSONRPCMessage::Error(_) => {}
            }
        }
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        drop(self.stdin.take());
        let wait = self.child.wait();
        match tokio::time::timeout(std::time::Duration::from_secs(2), wait).await {
            Ok(status) => {
                status.context("wait for potter app-server process")?;
            }
            Err(_) => {
                self.child
                    .kill()
                    .await
                    .context("kill potter app-server process")?;
                self.child
                    .wait()
                    .await
                    .context("wait for killed potter app-server process")?;
            }
        }
        Ok(())
    }

    fn next_request_id(&mut self) -> RequestId {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        RequestId::Integer(id)
    }

    async fn send_request<T>(
        &mut self,
        request_id: RequestId,
        request: PotterAppServerClientRequest,
        buffered_events: &mut Vec<Event>,
    ) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let stdin = self
            .stdin
            .as_mut()
            .context("potter app-server stdin unavailable")?;
        send_message(stdin, &request)
            .await
            .context("send potter app-server request")?;

        let result = self
            .read_until_response(request_id, buffered_events)
            .await
            .context("await potter app-server response")?;

        serde_json::from_value(result).context("deserialize potter app-server response payload")
    }

    async fn send_notification(
        &mut self,
        notification: PotterAppServerClientNotification,
    ) -> anyhow::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .context("potter app-server stdin unavailable")?;
        send_message(stdin, &notification)
            .await
            .context("send potter app-server notification")?;
        Ok(())
    }

    async fn read_until_response(
        &mut self,
        request_id: RequestId,
        buffered_events: &mut Vec<Event>,
    ) -> anyhow::Result<JsonRpcResult> {
        loop {
            let Some(line) = self
                .stdout_lines
                .next_line()
                .await
                .context("read potter app-server stdout line")?
            else {
                anyhow::bail!("potter app-server closed stdout unexpectedly");
            };
            if line.trim().is_empty() {
                continue;
            }

            let msg: JSONRPCMessage = serde_json::from_str(&line)
                .with_context(|| format!("decode potter app-server JSON-RPC: {line:?}"))?;

            match msg {
                JSONRPCMessage::Notification(notification) => {
                    if notification.method != POTTER_EVENT_NOTIFICATION_METHOD {
                        continue;
                    }
                    let params = notification
                        .params
                        .context("potter app-server event notification missing params")?;
                    let event: Event = serde_json::from_value(params)
                        .context("deserialize potter event payload")?;
                    buffered_events.push(event);
                }
                JSONRPCMessage::Response(response) => {
                    if response.id == request_id {
                        return Ok(response.result);
                    }
                }
                JSONRPCMessage::Error(error) => {
                    if error.id == request_id {
                        anyhow::bail!(
                            "potter app-server JSON-RPC error: code={} message={}",
                            error.error.code,
                            error.error.message
                        );
                    }
                }
                JSONRPCMessage::Request(_) => {}
            }
        }
    }
}

impl crate::workflow::project_render_loop::PotterEventSource for PotterAppServerClient {
    fn read_next_event<'a>(
        &'a mut self,
    ) -> crate::workflow::round_runner::UiFuture<'a, Option<Event>> {
        Box::pin(PotterAppServerClient::read_next_event(self))
    }
}

async fn send_message<T: serde::Serialize>(stdin: &mut ChildStdin, msg: &T) -> anyhow::Result<()> {
    let json = serde_json::to_vec(&msg).context("serialize potter app-server JSON-RPC message")?;
    stdin
        .write_all(&json)
        .await
        .context("write potter app-server stdin")?;
    stdin
        .write_all(b"\n")
        .await
        .context("write potter app-server stdin newline")?;
    stdin
        .flush()
        .await
        .context("flush potter app-server stdin")?;
    Ok(())
}
