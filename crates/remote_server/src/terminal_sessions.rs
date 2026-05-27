use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use gpui::{App, AsyncApp, Context, Entity};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self, REMOTE_SERVER_PROJECT_ID},
};
use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Instant,
};
use util::ResultExt as _;

const DEFAULT_REPLAY_BUFFER_BYTES: usize = 10 * 1024 * 1024;

pub struct TerminalSessionStore {
    session: AnyProtoClient,
    sessions: HashMap<String, Arc<RemoteTerminalSession>>,
    next_attach_id: Arc<AtomicU64>,
    replay_buffer_bytes: usize,
}

struct RemoteTerminalSession {
    id: String,
    profile_id: String,
    label: String,
    cwd: Option<String>,
    persistent: bool,
    created_at: Instant,
    last_attached_at: Mutex<Option<Instant>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output: Mutex<Vec<u8>>,
    exit: Mutex<Option<TerminalExit>>,
    attachments: Mutex<HashMap<u64, Attachment>>,
    sequence: AtomicU64,
    replay_buffer_bytes: usize,
}

#[derive(Clone, Copy)]
struct Attachment {
    attach_id: u64,
}

#[derive(Clone, Copy)]
struct TerminalExit {
    exit_code: Option<i32>,
    signal: Option<i32>,
}

impl TerminalSessionStore {
    pub fn new(session: AnyProtoClient) -> Self {
        Self {
            session,
            sessions: HashMap::default(),
            next_attach_id: Arc::new(AtomicU64::new(1)),
            replay_buffer_bytes: DEFAULT_REPLAY_BUFFER_BYTES,
        }
    }

    pub fn init(session: &AnyProtoClient, store: Entity<Self>, cx: &mut App) {
        session.add_request_handler(store.downgrade(), Self::handle_create_terminal_session);
        session.add_request_handler(store.downgrade(), Self::handle_attach_terminal_session);
        session.add_request_handler(store.downgrade(), Self::handle_terminal_input);
        session.add_request_handler(store.downgrade(), Self::handle_resize_terminal_session);
        session.add_request_handler(store.downgrade(), Self::handle_close_terminal_session);
        session.add_request_handler(store.downgrade(), Self::handle_list_terminal_sessions);
        cx.observe_release(&store, |_, _, _| {}).detach();
    }

    async fn handle_create_terminal_session(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::CreateTerminalSession>,
        mut cx: AsyncApp,
    ) -> Result<proto::CreateTerminalSessionResponse> {
        let payload = envelope.payload;
        anyhow::ensure!(
            payload.project_id == REMOTE_SERVER_PROJECT_ID,
            "invalid remote terminal project id {}",
            payload.project_id
        );
        let size = payload.initial_size.context("missing terminal size")?;
        let session = this.update(&mut cx, |this, _| this.create_session(payload, size))??;
        Ok(proto::CreateTerminalSessionResponse {
            session_id: Some(proto::TerminalSessionId {
                id: session.id.clone(),
            }),
        })
    }

    async fn handle_attach_terminal_session(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AttachTerminalSession>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let payload = envelope.payload;
        let session_id = payload
            .session_id
            .context("missing terminal session id")?
            .id;
        let size = payload.size.context("missing terminal size")?;
        let (session, client) = this.update(&mut cx, |this, _| {
            let session = this
                .sessions
                .get(&session_id)
                .cloned()
                .with_context(|| format!("terminal session not found: {session_id}"))?;
            Ok::<_, anyhow::Error>((session, this.session.clone()))
        })??;
        session.attach(payload.attach_id, size, &client)?;
        Ok(proto::Ack {})
    }

    async fn handle_terminal_input(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::TerminalInput>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let payload = envelope.payload;
        let session_id = payload
            .session_id
            .context("missing terminal session id")?
            .id;
        let session = this.update(&mut cx, |this, _| {
            this.sessions
                .get(&session_id)
                .cloned()
                .with_context(|| format!("terminal session not found: {session_id}"))
        })??;
        session.write_input(&payload.data)?;
        Ok(proto::Ack {})
    }

    async fn handle_resize_terminal_session(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ResizeTerminalSession>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let payload = envelope.payload;
        let session_id = payload
            .session_id
            .context("missing terminal session id")?
            .id;
        let size = payload.size.context("missing terminal size")?;
        let session = this.update(&mut cx, |this, _| {
            this.sessions
                .get(&session_id)
                .cloned()
                .with_context(|| format!("terminal session not found: {session_id}"))
        })??;
        session.resize(size)?;
        Ok(proto::Ack {})
    }

    async fn handle_close_terminal_session(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::CloseTerminalSession>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let payload = envelope.payload;
        let session_id = payload
            .session_id
            .context("missing terminal session id")?
            .id;
        let session = this.update(&mut cx, |this, _| this.sessions.remove(&session_id))?;
        if let Some(session) = session {
            session.close().log_err();
        }
        Ok(proto::Ack {})
    }

    async fn handle_list_terminal_sessions(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ListTerminalSessions>,
        mut cx: AsyncApp,
    ) -> Result<proto::ListTerminalSessionsResponse> {
        anyhow::ensure!(
            envelope.payload.project_id == REMOTE_SERVER_PROJECT_ID,
            "invalid remote terminal project id {}",
            envelope.payload.project_id
        );
        let sessions = this.update(&mut cx, |this, _| {
            this.sessions
                .values()
                .map(|session| session.info())
                .collect::<Vec<_>>()
        })?;
        Ok(proto::ListTerminalSessionsResponse { sessions })
    }

    fn create_session(
        &mut self,
        payload: proto::CreateTerminalSession,
        size: proto::TerminalSize,
    ) -> Result<Arc<RemoteTerminalSession>> {
        let id = uuid::Uuid::new_v4().to_string();
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(size_to_pty_size(&size))?;

        let mut command = if let Some(program) = payload.program.clone() {
            let mut command = CommandBuilder::new(program);
            command.args(payload.args.clone());
            command
        } else {
            CommandBuilder::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()))
        };

        if let Some(cwd) = payload.cwd.as_ref().filter(|cwd| !cwd.is_empty()) {
            command.cwd(PathBuf::from(shellexpand::tilde(cwd).to_string()));
        }
        for (key, value) in &payload.env {
            command.env(key, value);
        }

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let mut child = pair.slave.spawn_command(command)?;
        let killer = child.clone_killer();
        drop(pair.slave);

        let session = Arc::new(RemoteTerminalSession {
            id: id.clone(),
            profile_id: payload.profile_id,
            label: payload.label,
            cwd: payload.cwd,
            persistent: payload.persistent,
            created_at: Instant::now(),
            last_attached_at: Mutex::new(None),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            output: Mutex::new(Vec::new()),
            exit: Mutex::new(None),
            attachments: Mutex::new(HashMap::default()),
            sequence: AtomicU64::new(0),
            replay_buffer_bytes: self.replay_buffer_bytes,
        });

        let output_session = session.clone();
        let client = self.session.clone();
        thread::Builder::new()
            .name(format!("remote-terminal-output-{id}"))
            .spawn(move || {
                let mut buffer = [0; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => output_session.record_output(&client, &buffer[..count]),
                        Err(error) => {
                            log::debug!("remote terminal reader failed: {error}");
                            break;
                        }
                    }
                }
            })?;

        let exit_session = session.clone();
        let client = self.session.clone();
        thread::Builder::new()
            .name(format!("remote-terminal-wait-{id}"))
            .spawn(move || {
                let exit = child.wait().ok().map(|status| TerminalExit {
                    exit_code: Some(status.exit_code() as i32),
                    signal: None,
                });
                if let Some(exit) = exit {
                    exit_session.set_exit(&client, exit);
                }
            })?;

        self.sessions.insert(id, session.clone());
        Ok(session)
    }
}

impl RemoteTerminalSession {
    fn attach(
        &self,
        attach_id: u64,
        size: proto::TerminalSize,
        client: &AnyProtoClient,
    ) -> Result<()> {
        self.resize(size)?;
        *self
            .last_attached_at
            .lock()
            .map_err(|_| anyhow!("lock poisoned"))? = Some(Instant::now());
        self.attachments
            .lock()
            .map_err(|_| anyhow!("lock poisoned"))?
            .insert(attach_id, Attachment { attach_id });

        let output = self
            .output
            .lock()
            .map_err(|_| anyhow!("lock poisoned"))?
            .clone();
        if !output.is_empty() {
            client.send(proto::TerminalOutput {
                session_id: Some(proto::TerminalSessionId {
                    id: self.id.clone(),
                }),
                attach_id,
                sequence: self.sequence.load(Ordering::SeqCst),
                data: output,
                replay: true,
            })?;
        }

        if let Some(exit) = *self.exit.lock().map_err(|_| anyhow!("lock poisoned"))? {
            client.send(proto::TerminalSessionExited {
                session_id: Some(proto::TerminalSessionId {
                    id: self.id.clone(),
                }),
                exit_code: exit.exit_code,
                signal: exit.signal,
            })?;
        }

        Ok(())
    }

    fn write_input(&self, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().map_err(|_| anyhow!("lock poisoned"))?;
        writer.write_all(data)?;
        writer.flush()?;
        Ok(())
    }

    fn resize(&self, size: proto::TerminalSize) -> Result<()> {
        self.master
            .lock()
            .map_err(|_| anyhow!("lock poisoned"))?
            .resize(size_to_pty_size(&size))?;
        Ok(())
    }

    fn close(&self) -> Result<()> {
        self.killer
            .lock()
            .map_err(|_| anyhow!("lock poisoned"))?
            .kill()?;
        Ok(())
    }

    fn record_output(&self, client: &AnyProtoClient, bytes: &[u8]) {
        if let Ok(mut output) = self.output.lock() {
            output.extend_from_slice(bytes);
            if output.len() > self.replay_buffer_bytes {
                let excess = output.len() - self.replay_buffer_bytes;
                output.drain(..excess);
            }
        }

        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let attachments = self
            .attachments
            .lock()
            .map(|attachments| attachments.values().copied().collect::<Vec<_>>())
            .unwrap_or_default();

        for attachment in attachments {
            client
                .send(proto::TerminalOutput {
                    session_id: Some(proto::TerminalSessionId {
                        id: self.id.clone(),
                    }),
                    attach_id: attachment.attach_id,
                    sequence,
                    data: bytes.to_vec(),
                    replay: false,
                })
                .log_err();
        }
    }

    fn set_exit(&self, client: &AnyProtoClient, exit: TerminalExit) {
        if let Ok(mut current_exit) = self.exit.lock() {
            *current_exit = Some(exit);
        }
        client
            .send(proto::TerminalSessionExited {
                session_id: Some(proto::TerminalSessionId {
                    id: self.id.clone(),
                }),
                exit_code: exit.exit_code,
                signal: exit.signal,
            })
            .log_err();
    }

    fn info(&self) -> proto::TerminalSessionInfo {
        let exit = self.exit.lock().ok().and_then(|exit| *exit);
        let _age = self.created_at.elapsed();
        proto::TerminalSessionInfo {
            session_id: Some(proto::TerminalSessionId {
                id: self.id.clone(),
            }),
            profile_id: self.profile_id.clone(),
            label: self.label.clone(),
            cwd: self.cwd.clone(),
            persistent: self.persistent,
            exited: exit.is_some(),
            exit_code: exit.and_then(|exit| exit.exit_code),
            signal: exit.and_then(|exit| exit.signal),
        }
    }
}

fn size_to_pty_size(size: &proto::TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows.try_into().unwrap_or(u16::MAX).max(1),
        cols: size.columns.try_into().unwrap_or(u16::MAX).max(1),
        pixel_width: size.cell_width.try_into().unwrap_or(u16::MAX),
        pixel_height: size.cell_height.try_into().unwrap_or(u16::MAX),
    }
}
