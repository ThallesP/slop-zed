use std::{
    any::Any,
    cell::RefCell,
    path::PathBuf,
    process::{Command, Stdio},
    rc::Rc,
    sync::{Arc, LazyLock},
};

use acp_thread::{
    AcpThread, AgentConnection, AgentModelInfo, AgentModelList, AgentModelSelector,
    AgentSessionConfigOptions, AgentSessionModes, AgentSessionTruncate, TokenUsage, UserMessageId,
};
use agent_client_protocol::schema as acp;
use agent_servers::{AgentServer, AgentServerDelegate};
use anyhow::{Context as _, Result, anyhow};
use collections::{HashMap, HashSet};
use fs::Fs;
use futures::{AsyncBufReadExt as _, AsyncReadExt as _, StreamExt as _, io::BufReader};
use gpui::{App, AppContext as _, Entity, SharedString, Task, WeakEntity};
use parking_lot::Mutex;
use paths::data_dir;
use project::{
    AgentId, Project,
    agent_server_store::{AllAgentServersSettings, CustomAgentServerSettings},
};
use remote::{Interactive, RemoteClient};
use serde::Deserialize;
use settings::{SettingsStore, update_settings_file};
use util::{
    ResultExt as _, path_list::PathList, process::Child, truncate_and_trailoff,
    truncate_lines_and_trailoff,
};
use uuid::Uuid;

pub static CODEX_AGENT_ID: LazyLock<AgentId> = LazyLock::new(|| AgentId::new("codex"));

const STANDARD_MODE_ID: &str = "standard";
const FAST_MODE_ID: &str = "fast";
const DEFAULT_CODEX_MODEL: &str = "gpt-5.5";
const MODEL_CONFIG_ID: &str = "model";
const MODE_CONFIG_ID: &str = "mode";
const REASONING_EFFORT_CONFIG_ID: &str = "reasoning_effort";
const LOW_REASONING_EFFORT: &str = "low";
const MEDIUM_REASONING_EFFORT: &str = "medium";
const HIGH_REASONING_EFFORT: &str = "high";
const MAX_CODEX_TOOL_COMMAND_CHARS: usize = 160;
const MAX_CODEX_SUCCESS_OUTPUT_CHARS: usize = 600;
const MAX_CODEX_SUCCESS_OUTPUT_LINES: usize = 8;
const MAX_CODEX_FAILED_OUTPUT_LINES: usize = 40;
const MAX_CODEX_FILE_CHANGE_ENTRIES: usize = 12;

#[derive(Clone)]
pub struct CodexAgentServer {
    fs: Arc<dyn Fs>,
}

impl CodexAgentServer {
    pub fn new(fs: Arc<dyn Fs>) -> Self {
        Self { fs }
    }
}

impl AgentServer for CodexAgentServer {
    fn agent_id(&self) -> AgentId {
        CODEX_AGENT_ID.clone()
    }

    fn logo(&self) -> ui::IconName {
        ui::IconName::AiOpenAi
    }

    fn connect(
        &self,
        _delegate: AgentServerDelegate,
        _project: Entity<Project>,
        _cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        Task::ready(Ok(Rc::new(CodexConnection::new(self.fs.clone()))))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }

    fn default_mode(&self, cx: &App) -> Option<acp::SessionModeId> {
        codex_agent_settings(cx).and_then(|settings| {
            settings
                .default_mode()
                .map(|mode_id| acp::SessionModeId::new(mode_id.to_string()))
        })
    }

    fn set_default_mode(&self, mode_id: Option<acp::SessionModeId>, fs: Arc<dyn Fs>, cx: &mut App) {
        update_settings_file(fs, cx, move |settings, _cx| {
            let agent = settings
                .agent_servers
                .get_or_insert_default()
                .entry(CODEX_AGENT_ID.0.to_string())
                .or_insert_with(default_codex_settings);

            match agent {
                settings::CustomAgentServerSettings::Custom { default_mode, .. }
                | settings::CustomAgentServerSettings::Registry { default_mode, .. } => {
                    *default_mode = mode_id.map(|mode_id| mode_id.to_string());
                }
            }
        });
    }

    fn default_model(&self, cx: &App) -> Option<acp::ModelId> {
        codex_agent_settings(cx).and_then(|settings| {
            settings
                .default_model()
                .map(|model_id| acp::ModelId::new(model_id.to_string()))
        })
    }

    fn set_default_model(&self, model_id: Option<acp::ModelId>, fs: Arc<dyn Fs>, cx: &mut App) {
        persist_codex_model(fs, model_id, cx);
    }

    fn favorite_model_ids(&self, cx: &mut App) -> HashSet<acp::ModelId> {
        codex_agent_settings(cx)
            .map(|settings| {
                settings
                    .favorite_models()
                    .iter()
                    .map(|model_id| acp::ModelId::new(model_id.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn toggle_favorite_model(
        &self,
        model_id: acp::ModelId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        update_settings_file(fs, cx, move |settings, _cx| {
            let agent = settings
                .agent_servers
                .get_or_insert_default()
                .entry(CODEX_AGENT_ID.0.to_string())
                .or_insert_with(default_codex_settings);
            let model_id = model_id.to_string();
            match agent {
                settings::CustomAgentServerSettings::Custom {
                    favorite_models, ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    favorite_models, ..
                } => {
                    if should_be_favorite {
                        if !favorite_models.iter().any(|favorite| favorite == &model_id) {
                            favorite_models.push(model_id);
                        }
                    } else {
                        favorite_models.retain(|favorite| favorite != &model_id);
                    }
                }
            }
        });
    }

    fn default_config_option(&self, config_id: &str, cx: &App) -> Option<String> {
        codex_agent_settings(cx).and_then(|settings| match config_id {
            MODEL_CONFIG_ID => settings.default_model().map(|value| value.to_string()),
            MODE_CONFIG_ID => settings.default_mode().map(|value| value.to_string()),
            REASONING_EFFORT_CONFIG_ID => settings
                .default_config_option(config_id)
                .filter(|value| is_valid_reasoning_effort(value))
                .map(|value| value.to_string()),
            _ => None,
        })
    }

    fn set_default_config_option(
        &self,
        config_id: &str,
        value_id: Option<&str>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        match config_id {
            MODEL_CONFIG_ID => {
                persist_codex_model(fs, value_id.map(acp::ModelId::new), cx);
            }
            MODE_CONFIG_ID => {
                persist_codex_mode(fs, value_id.map(acp::SessionModeId::new), cx);
            }
            REASONING_EFFORT_CONFIG_ID => {
                persist_codex_config_option(
                    fs,
                    config_id.to_string(),
                    value_id.map(str::to_string),
                    cx,
                );
            }
            _ => {}
        }
    }

    fn favorite_config_option_value_ids(
        &self,
        config_id: &acp::SessionConfigId,
        cx: &mut App,
    ) -> HashSet<acp::SessionConfigValueId> {
        codex_agent_settings(cx).map_or_else(HashSet::default, |settings| {
            if config_id.0.as_ref() == MODEL_CONFIG_ID {
                return settings
                    .favorite_models()
                    .iter()
                    .map(|value| acp::SessionConfigValueId::new(value.clone()))
                    .collect();
            }

            settings
                .favorite_config_option_values(config_id.0.as_ref())
                .map(|values| {
                    values
                        .iter()
                        .map(|value| acp::SessionConfigValueId::new(value.clone()))
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    fn toggle_favorite_config_option_value(
        &self,
        config_id: acp::SessionConfigId,
        value_id: acp::SessionConfigValueId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        if config_id.0.as_ref() == MODEL_CONFIG_ID {
            update_settings_file(fs, cx, move |settings, _cx| {
                let agent = settings
                    .agent_servers
                    .get_or_insert_default()
                    .entry(CODEX_AGENT_ID.0.to_string())
                    .or_insert_with(default_codex_settings);
                let value_id = value_id.to_string();
                match agent {
                    settings::CustomAgentServerSettings::Custom {
                        favorite_models, ..
                    }
                    | settings::CustomAgentServerSettings::Registry {
                        favorite_models, ..
                    } => {
                        if should_be_favorite {
                            if !favorite_models.iter().any(|value| value == &value_id) {
                                favorite_models.push(value_id);
                            }
                        } else {
                            favorite_models.retain(|value| value != &value_id);
                        }
                    }
                }
            });
            return;
        }

        update_settings_file(fs, cx, move |settings, _cx| {
            let agent = settings
                .agent_servers
                .get_or_insert_default()
                .entry(CODEX_AGENT_ID.0.to_string())
                .or_insert_with(default_codex_settings);
            let config_id = config_id.to_string();
            let value_id = value_id.to_string();
            match agent {
                settings::CustomAgentServerSettings::Custom {
                    favorite_config_option_values,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    favorite_config_option_values,
                    ..
                } => {
                    let entry = favorite_config_option_values
                        .entry(config_id.clone())
                        .or_insert_with(Vec::new);

                    if should_be_favorite {
                        if !entry.iter().any(|value| value == &value_id) {
                            entry.push(value_id);
                        }
                    } else {
                        entry.retain(|value| value != &value_id);
                        if entry.is_empty() {
                            favorite_config_option_values.remove(&config_id);
                        }
                    }
                }
            }
        });
    }
}

#[derive(Clone)]
struct CodexConnection {
    fs: Arc<dyn Fs>,
    sessions: Rc<RefCell<HashMap<acp::SessionId, CodexSession>>>,
}

struct CodexSession {
    thread: WeakEntity<AcpThread>,
    codex_thread_id: Option<String>,
    history: Vec<CodexHistoryMessage>,
    work_dirs: PathList,
    remote_client: Option<Entity<RemoteClient>>,
    selected_model: acp::ModelId,
    mode_id: acp::SessionModeId,
    reasoning_effort: acp::SessionConfigValueId,
    running_child: Rc<RefCell<Option<Child>>>,
}

struct CodexHistoryMessage {
    user_message_id: Option<UserMessageId>,
    role: CodexHistoryRole,
    text: String,
}

enum CodexHistoryRole {
    User,
    Assistant,
}

impl CodexConnection {
    fn new(fs: Arc<dyn Fs>) -> Self {
        Self {
            fs,
            sessions: Rc::default(),
        }
    }

    fn create_thread(
        self: &Rc<Self>,
        session_id: acp::SessionId,
        title: Option<SharedString>,
        project: Entity<Project>,
        work_dirs: PathList,
        codex_thread_id: Option<String>,
        cx: &mut App,
    ) -> Entity<AcpThread> {
        let remote_client = project.read(cx).remote_client();
        let action_log = cx.new(|_| action_log::ActionLog::new(project.clone()));
        let thread = cx.new(|cx| {
            AcpThread::new(
                None,
                title,
                Some(work_dirs.clone()),
                self.clone(),
                project,
                action_log,
                session_id.clone(),
                watch::Receiver::constant(acp::PromptCapabilities::new().image(true)),
                cx,
            )
        });

        let selected_model = codex_default_model(cx);
        let mode_id = codex_default_mode(cx);
        let reasoning_effort = codex_default_reasoning_effort(&mode_id, cx);
        self.sessions.borrow_mut().insert(
            session_id,
            CodexSession {
                thread: thread.downgrade(),
                codex_thread_id,
                history: Vec::new(),
                work_dirs,
                remote_client,
                selected_model,
                mode_id,
                reasoning_effort,
                running_child: Rc::default(),
            },
        );

        thread
    }
}

impl AgentConnection for CodexConnection {
    fn agent_id(&self) -> AgentId {
        CODEX_AGENT_ID.clone()
    }

    fn telemetry_id(&self) -> SharedString {
        "codex".into()
    }

    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let session_id = acp::SessionId::new(Uuid::new_v4().to_string());
        let thread = self.create_thread(session_id, None, project, work_dirs, None, cx);

        Task::ready(Ok(thread))
    }

    fn supports_load_session(&self) -> bool {
        true
    }

    fn load_session(
        self: Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let codex_session_id = resolve_codex_session_id(&session_id);
        let remote_client = project.read(cx).remote_client();
        let thread = self.create_thread(
            session_id.clone(),
            title,
            project,
            work_dirs,
            Some(codex_session_id.clone()),
            cx,
        );
        let sessions = self.sessions.clone();

        cx.spawn(async move |cx| {
            let rollout = load_codex_rollout(&codex_session_id, remote_client, cx).await?;
            let history = parse_codex_rollout_history(&rollout)?;
            seed_thread_from_history(&thread, &history, cx)?;

            if let Some(session) = sessions.borrow_mut().get_mut(&session_id) {
                session.history = history;
                session.codex_thread_id = Some(codex_session_id);
            }

            Ok(thread)
        })
    }

    fn supports_resume_session(&self) -> bool {
        true
    }

    fn resume_session(
        self: Rc<Self>,
        session_id: acp::SessionId,
        project: Entity<Project>,
        work_dirs: PathList,
        title: Option<SharedString>,
        cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        let codex_session_id = resolve_codex_session_id(&session_id);
        let thread = self.create_thread(
            session_id.clone(),
            title,
            project,
            work_dirs,
            Some(codex_session_id),
            cx,
        );

        Task::ready(Ok(thread))
    }

    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &[]
    }

    fn authenticate(&self, _method: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt(
        &self,
        user_message_id: UserMessageId,
        params: acp::PromptRequest,
        cx: &mut App,
    ) -> Task<Result<acp::PromptResponse>> {
        let session_id = params.session_id.clone();
        let user_text = prompt_text(params.prompt);
        let Some(run) = self
            .sessions
            .borrow_mut()
            .get_mut(&session_id)
            .map(|session| {
                let input = codex_input(session, &user_text);
                let run = CodexRun {
                    session_id: session_id.clone(),
                    thread: session.thread.clone(),
                    codex_thread_id: session.codex_thread_id.clone(),
                    input,
                    work_dirs: session.work_dirs.clone(),
                    remote_client: session.remote_client.clone(),
                    selected_model: session.selected_model.clone(),
                    reasoning_effort: session.reasoning_effort.clone(),
                    running_child: session.running_child.clone(),
                };
                session.history.push(CodexHistoryMessage {
                    user_message_id: Some(user_message_id.clone()),
                    role: CodexHistoryRole::User,
                    text: user_text,
                });
                run
            })
        else {
            return Task::ready(Err(anyhow!("Codex session not found")));
        };

        let sessions = self.sessions.clone();
        cx.spawn(async move |cx| {
            let output = run_codex(run, cx).await?;

            if let Some(session) = sessions.borrow_mut().get_mut(&session_id) {
                if let Some(codex_thread_id) = output.codex_thread_id {
                    if codex_thread_id != session_id.0.as_ref() {
                        persist_codex_session_alias(&session_id, &codex_thread_id).log_err();
                    }
                    session.codex_thread_id = Some(codex_thread_id);
                }
                if !output.final_response.is_empty() {
                    session.history.push(CodexHistoryMessage {
                        user_message_id: None,
                        role: CodexHistoryRole::Assistant,
                        text: output.final_response,
                    });
                }
            }

            let mut response = acp::PromptResponse::new(acp::StopReason::EndTurn);
            if let Some(usage) = output.usage {
                response = response.usage(usage);
            }
            Ok(response)
        })
    }

    fn cancel(&self, session_id: &acp::SessionId, _cx: &mut App) {
        if let Some(session) = self.sessions.borrow_mut().get_mut(session_id)
            && let Some(child) = session.running_child.borrow_mut().as_mut()
        {
            child.kill().log_err();
        }
    }

    fn truncate(
        &self,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentSessionTruncate>> {
        self.sessions.borrow().contains_key(session_id).then(|| {
            Rc::new(CodexSessionTruncate {
                session_id: session_id.clone(),
                sessions: self.sessions.clone(),
            }) as Rc<dyn AgentSessionTruncate>
        })
    }

    fn model_selector(&self, session_id: &acp::SessionId) -> Option<Rc<dyn AgentModelSelector>> {
        self.sessions.borrow().contains_key(session_id).then(|| {
            Rc::new(CodexModelSelector {
                fs: self.fs.clone(),
                session_id: session_id.clone(),
                sessions: self.sessions.clone(),
            }) as Rc<dyn AgentModelSelector>
        })
    }

    fn session_modes(
        &self,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentSessionModes>> {
        self.sessions.borrow().contains_key(session_id).then(|| {
            Rc::new(CodexSessionModes {
                fs: self.fs.clone(),
                session_id: session_id.clone(),
                sessions: self.sessions.clone(),
            }) as Rc<dyn AgentSessionModes>
        })
    }

    fn session_config_options(
        &self,
        session_id: &acp::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn AgentSessionConfigOptions>> {
        self.sessions.borrow().contains_key(session_id).then(|| {
            Rc::new(CodexSessionConfigOptions {
                fs: self.fs.clone(),
                session_id: session_id.clone(),
                sessions: self.sessions.clone(),
            }) as Rc<dyn AgentSessionConfigOptions>
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

struct CodexSessionTruncate {
    session_id: acp::SessionId,
    sessions: Rc<RefCell<HashMap<acp::SessionId, CodexSession>>>,
}

impl AgentSessionTruncate for CodexSessionTruncate {
    fn run(&self, message_id: UserMessageId, _cx: &mut App) -> Task<Result<()>> {
        if let Some(session) = self.sessions.borrow_mut().get_mut(&self.session_id) {
            session.codex_thread_id = None;
            if let Some(index) = session
                .history
                .iter()
                .position(|message| message.user_message_id.as_ref() == Some(&message_id))
            {
                session.history.truncate(index);
            }
        }
        Task::ready(Ok(()))
    }
}

struct CodexModelSelector {
    fs: Arc<dyn Fs>,
    session_id: acp::SessionId,
    sessions: Rc<RefCell<HashMap<acp::SessionId, CodexSession>>>,
}

impl AgentModelSelector for CodexModelSelector {
    fn list_models(&self, _cx: &mut App) -> Task<Result<AgentModelList>> {
        Task::ready(Ok(AgentModelList::Flat(codex_models())))
    }

    fn select_model(&self, model_id: acp::ModelId, cx: &mut App) -> Task<Result<()>> {
        if let Some(session) = self.sessions.borrow_mut().get_mut(&self.session_id) {
            session.selected_model = model_id.clone();
        }
        persist_codex_model(self.fs.clone(), Some(model_id), cx);
        Task::ready(Ok(()))
    }

    fn selected_model(&self, _cx: &mut App) -> Task<Result<AgentModelInfo>> {
        let selected_model = self
            .sessions
            .borrow()
            .get(&self.session_id)
            .map(|session| session.selected_model.clone())
            .unwrap_or_else(|| acp::ModelId::new(DEFAULT_CODEX_MODEL));

        Task::ready(
            codex_models()
                .into_iter()
                .find(|model| model.id == selected_model)
                .ok_or_else(|| anyhow!("Codex model not found")),
        )
    }
}

struct CodexSessionModes {
    fs: Arc<dyn Fs>,
    session_id: acp::SessionId,
    sessions: Rc<RefCell<HashMap<acp::SessionId, CodexSession>>>,
}

impl AgentSessionModes for CodexSessionModes {
    fn current_mode(&self) -> acp::SessionModeId {
        self.sessions
            .borrow()
            .get(&self.session_id)
            .map(|session| session.mode_id.clone())
            .unwrap_or_else(|| acp::SessionModeId::new(STANDARD_MODE_ID))
    }

    fn all_modes(&self) -> Vec<acp::SessionMode> {
        vec![
            acp::SessionMode::new(
                acp::SessionModeId::new(STANDARD_MODE_ID),
                "Standard".to_string(),
            )
            .description("Use Codex with its default reasoning effort.".to_string()),
            acp::SessionMode::new(acp::SessionModeId::new(FAST_MODE_ID), "Fast".to_string())
                .description("Use lower reasoning effort for quicker Codex turns.".to_string()),
        ]
    }

    fn set_mode(&self, mode_id: acp::SessionModeId, cx: &mut App) -> Task<Result<()>> {
        if mode_id.0.as_ref() != STANDARD_MODE_ID && mode_id.0.as_ref() != FAST_MODE_ID {
            return Task::ready(Err(anyhow!("Invalid Codex mode: {mode_id}")));
        }

        let reasoning_effort = codex_default_reasoning_effort(&mode_id, cx);
        if let Some(session) = self.sessions.borrow_mut().get_mut(&self.session_id) {
            session.mode_id = mode_id.clone();
            session.reasoning_effort = reasoning_effort;
        }

        persist_codex_mode(self.fs.clone(), Some(mode_id), cx);
        Task::ready(Ok(()))
    }
}

struct CodexSessionConfigOptions {
    fs: Arc<dyn Fs>,
    session_id: acp::SessionId,
    sessions: Rc<RefCell<HashMap<acp::SessionId, CodexSession>>>,
}

impl AgentSessionConfigOptions for CodexSessionConfigOptions {
    fn config_options(&self) -> Vec<acp::SessionConfigOption> {
        let (selected_model, mode_id, reasoning_effort) = self
            .sessions
            .borrow()
            .get(&self.session_id)
            .map(|session| {
                (
                    session.selected_model.clone(),
                    session.mode_id.clone(),
                    session.reasoning_effort.clone(),
                )
            })
            .unwrap_or_else(|| {
                (
                    acp::ModelId::new(DEFAULT_CODEX_MODEL),
                    acp::SessionModeId::new(STANDARD_MODE_ID),
                    acp::SessionConfigValueId::new(MEDIUM_REASONING_EFFORT),
                )
            });

        vec![
            codex_mode_config_option(mode_id),
            codex_model_config_option(selected_model),
            codex_reasoning_config_option(reasoning_effort),
        ]
    }

    fn set_config_option(
        &self,
        config_id: acp::SessionConfigId,
        value: acp::SessionConfigValueId,
        cx: &mut App,
    ) -> Task<Result<Vec<acp::SessionConfigOption>>> {
        match config_id.0.as_ref() {
            MODEL_CONFIG_ID => {
                let model_id = acp::ModelId::new(value.to_string());
                if !codex_models().into_iter().any(|model| model.id == model_id) {
                    return Task::ready(Err(anyhow!("Invalid Codex model: {value}")));
                }

                if let Some(session) = self.sessions.borrow_mut().get_mut(&self.session_id) {
                    session.selected_model = model_id.clone();
                }
                persist_codex_model(self.fs.clone(), Some(model_id), cx);
            }
            MODE_CONFIG_ID => {
                let mode_id = acp::SessionModeId::new(value.to_string());
                if mode_id.0.as_ref() != STANDARD_MODE_ID && mode_id.0.as_ref() != FAST_MODE_ID {
                    return Task::ready(Err(anyhow!("Invalid Codex mode: {value}")));
                }

                let reasoning_effort = codex_default_reasoning_effort(&mode_id, cx);
                if let Some(session) = self.sessions.borrow_mut().get_mut(&self.session_id) {
                    session.mode_id = mode_id.clone();
                    session.reasoning_effort = reasoning_effort;
                }
                persist_codex_mode(self.fs.clone(), Some(mode_id), cx);
            }
            REASONING_EFFORT_CONFIG_ID => {
                if !is_valid_reasoning_effort(value.0.as_ref()) {
                    return Task::ready(Err(anyhow!("Invalid Codex reasoning effort: {value}")));
                }

                if let Some(session) = self.sessions.borrow_mut().get_mut(&self.session_id) {
                    session.reasoning_effort = value.clone();
                }
                persist_codex_config_option(
                    self.fs.clone(),
                    REASONING_EFFORT_CONFIG_ID.to_string(),
                    Some(value.to_string()),
                    cx,
                );
            }
            _ => return Task::ready(Err(anyhow!("Invalid Codex config option: {config_id}"))),
        }

        Task::ready(Ok(self.config_options()))
    }
}

struct CodexRun {
    session_id: acp::SessionId,
    thread: WeakEntity<AcpThread>,
    codex_thread_id: Option<String>,
    input: String,
    work_dirs: PathList,
    remote_client: Option<Entity<RemoteClient>>,
    selected_model: acp::ModelId,
    reasoning_effort: acp::SessionConfigValueId,
    running_child: Rc<RefCell<Option<Child>>>,
}

struct CodexRunOutput {
    codex_thread_id: Option<String>,
    final_response: String,
    usage: Option<acp::Usage>,
}

async fn run_codex(run: CodexRun, cx: &mut gpui::AsyncApp) -> Result<CodexRunOutput> {
    let mut codex_args = vec![
        "exec".to_string(),
        "--experimental-json".to_string(),
        "--sandbox".to_string(),
        "danger-full-access".to_string(),
        "--skip-git-repo-check".to_string(),
    ];

    if run.selected_model.0.as_ref() != "default" {
        codex_args.push("--model".to_string());
        codex_args.push(run.selected_model.0.to_string());
    }

    codex_args.push("--config".to_string());
    codex_args.push(format!(
        "model_reasoning_effort=\"{}\"",
        run.reasoning_effort.0.as_ref()
    ));

    let mut ordered_paths = run.work_dirs.ordered_paths();
    let cwd = ordered_paths
        .next()
        .filter(|path| !path.as_os_str().is_empty())
        .cloned();
    let additional_dirs = ordered_paths
        .filter(|path| !path.as_os_str().is_empty())
        .cloned()
        .collect::<Vec<_>>();

    if let Some(cwd) = &cwd {
        codex_args.push("--cd".to_string());
        codex_args.push(cwd.display().to_string());
    }
    for path in &additional_dirs {
        codex_args.push("--add-dir".to_string());
        codex_args.push(path.display().to_string());
    }

    if let Some(codex_thread_id) = &run.codex_thread_id {
        codex_args.push("resume".to_string());
        codex_args.push(codex_thread_id.clone());
    }
    codex_args.push(run.input.clone());

    let mut command = if let Some(remote_client) = run.remote_client.clone() {
        cx.update(|cx| {
            let command = remote_client.read(cx).build_command_with_options(
                Some("codex".to_string()),
                &codex_args,
                &HashMap::default(),
                None,
                None,
                Interactive::No,
            )?;
            let mut process = Command::new(command.program);
            process.args(command.args);
            process.envs(command.env);
            anyhow::Ok(process)
        })?
    } else {
        let mut command = Command::new("codex");
        command.args(&codex_args);
        command
    };

    let mut child = Child::spawn(command, Stdio::null(), Stdio::piped(), Stdio::piped())?;
    let stdout = child.stdout.take().context("failed to take Codex stdout")?;
    let stderr = child.stderr.take().context("failed to take Codex stderr")?;

    *run.running_child.borrow_mut() = Some(child);

    let stderr_output = Arc::new(Mutex::new(String::new()));
    let stderr_output_for_task = stderr_output.clone();
    let stderr_task = cx.background_spawn(async move {
        let mut stderr = BufReader::new(stderr);
        let mut line = String::new();
        while stderr.read_line(&mut line).await.unwrap_or(0) > 0 {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            log::warn!("codex stderr: {trimmed}");
            let mut stderr_output = stderr_output_for_task.lock();
            stderr_output.push_str(trimmed);
            stderr_output.push('\n');
            line.clear();
        }
    });

    let mut codex_thread_id = run.codex_thread_id.clone();
    let mut final_response = String::new();
    let mut usage = None;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next().await {
        let line = line?;
        let event = serde_json::from_str::<CodexEvent>(&line)
            .with_context(|| format!("failed to parse Codex event: {line}"))?;

        match event {
            CodexEvent::ThreadStarted { thread_id } => {
                codex_thread_id = Some(thread_id);
            }
            CodexEvent::TurnCompleted { usage: event_usage } => {
                let token_usage =
                    event_usage.to_token_usage(codex_model_context_window(&run.selected_model));
                usage = Some(event_usage.into_acp_usage());
                run.thread.update(cx, |thread, cx| {
                    thread.update_token_usage(Some(token_usage), cx);
                })?;
            }
            CodexEvent::TurnFailed { error } => {
                return Err(anyhow!(error.message));
            }
            CodexEvent::ItemCompleted { item } => {
                if let Some(update) = item_to_session_update(&item) {
                    run.thread
                        .update(cx, |thread, cx| thread.handle_session_update(update, cx))??;
                }
                if let CodexItem::AgentMessage { text, .. } = item {
                    final_response = text;
                }
            }
            CodexEvent::Error { message } => return Err(anyhow!(message)),
            CodexEvent::TurnStarted
            | CodexEvent::ItemStarted { .. }
            | CodexEvent::ItemUpdated { .. } => {}
        }
    }

    let Some(mut child) = run.running_child.borrow_mut().take() else {
        return Ok(CodexRunOutput {
            codex_thread_id,
            final_response,
            usage,
        });
    };

    stderr_task.await;
    let status = child.status().await?;
    if !status.success() {
        let stderr_output = stderr_output.lock().trim().to_string();
        if stderr_output.is_empty() {
            return Err(anyhow!("Codex exited with {status}"));
        }
        return Err(anyhow!("Codex exited with {status}: {stderr_output}"));
    }

    if codex_thread_id.is_none() {
        log::warn!(
            "Codex did not emit thread.started for Zed session {}",
            run.session_id
        );
    }

    Ok(CodexRunOutput {
        codex_thread_id,
        final_response,
        usage,
    })
}

fn item_to_session_update(item: &CodexItem) -> Option<acp::SessionUpdate> {
    match item {
        CodexItem::AgentMessage { text, .. } if !text.is_empty() => Some(
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.clone()),
            ))),
        ),
        CodexItem::Reasoning { text, .. } if !text.is_empty() => Some(
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text.clone()),
            ))),
        ),
        CodexItem::CommandExecution {
            command,
            aggregated_output,
            exit_code,
            status,
            ..
        } => {
            let mut text = format!(
                "Command `{}` {status}",
                truncate_and_trailoff(command, MAX_CODEX_TOOL_COMMAND_CHARS)
            );
            if let Some(exit_code) = exit_code {
                text.push_str(&format!(" with exit code {exit_code}"));
            }

            if let Some(output) = summarize_codex_command_output(aggregated_output, *exit_code) {
                text.push_str("\n\n```text\n");
                text.push_str(&output);
                text.push_str("\n```");
            }
            Some(acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text))),
            ))
        }
        CodexItem::FileChange {
            changes, status, ..
        } => {
            let changes = summarize_codex_file_changes(changes);
            Some(acp::SessionUpdate::AgentThoughtChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(format!(
                    "File changes {status}:\n{changes}"
                )))),
            ))
        }
        CodexItem::TodoList { items, .. } => {
            let entries = items
                .iter()
                .map(|item| {
                    acp::PlanEntry::new(
                        item.text.clone(),
                        acp::PlanEntryPriority::Medium,
                        if item.completed {
                            acp::PlanEntryStatus::Completed
                        } else {
                            acp::PlanEntryStatus::Pending
                        },
                    )
                })
                .collect();
            Some(acp::SessionUpdate::Plan(acp::Plan::new(entries)))
        }
        CodexItem::Error { message, .. } if !message.is_empty() => Some(
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(message.clone()),
            ))),
        ),
        _ => None,
    }
}

fn summarize_codex_command_output(
    aggregated_output: &str,
    exit_code: Option<i32>,
) -> Option<String> {
    let aggregated_output = aggregated_output.trim();
    if aggregated_output.is_empty() {
        return None;
    }

    let output_line_count = aggregated_output.lines().count();
    let is_failed_command = exit_code.is_some_and(|exit_code| exit_code != 0);
    let should_include_success_output = aggregated_output.len() <= MAX_CODEX_SUCCESS_OUTPUT_CHARS
        && output_line_count <= MAX_CODEX_SUCCESS_OUTPUT_LINES;

    if !is_failed_command && !should_include_success_output {
        return None;
    }

    Some(truncate_lines_and_trailoff(
        aggregated_output,
        if is_failed_command {
            MAX_CODEX_FAILED_OUTPUT_LINES
        } else {
            MAX_CODEX_SUCCESS_OUTPUT_LINES
        },
    ))
}

fn summarize_codex_file_changes(changes: &[CodexFileChange]) -> String {
    let mut summarized_changes = changes
        .iter()
        .take(MAX_CODEX_FILE_CHANGE_ENTRIES)
        .map(|change| {
            format!(
                "- {} {}",
                change.kind,
                truncate_and_trailoff(&change.path, MAX_CODEX_TOOL_COMMAND_CHARS)
            )
        })
        .collect::<Vec<_>>();

    if changes.len() > MAX_CODEX_FILE_CHANGE_ENTRIES {
        summarized_changes.push(format!(
            "- ... and {} more",
            changes.len() - MAX_CODEX_FILE_CHANGE_ENTRIES
        ));
    }

    summarized_changes.join("\n")
}

fn prompt_text(prompt: Vec<acp::ContentBlock>) -> String {
    prompt
        .into_iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text),
            acp::ContentBlock::ResourceLink(resource) => Some(resource.uri.to_string()),
            acp::ContentBlock::Resource(resource) => Some(format!("{resource:?}")),
            acp::ContentBlock::Image(_) | acp::ContentBlock::Audio(_) => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn codex_input(session: &CodexSession, user_text: &str) -> String {
    if session.codex_thread_id.is_some() || session.history.is_empty() {
        return user_text.to_string();
    }

    let mut input =
        String::from("Continue this conversation. Prior messages are included for context.\n\n");
    for message in &session.history {
        match message.role {
            CodexHistoryRole::User => input.push_str("User:\n"),
            CodexHistoryRole::Assistant => input.push_str("Assistant:\n"),
        }
        input.push_str(&message.text);
        input.push_str("\n\n");
    }
    input.push_str("User:\n");
    input.push_str(user_text);
    input
}

fn codex_session_alias_path() -> PathBuf {
    data_dir().join("codex_session_aliases.json")
}

fn resolve_codex_session_id(session_id: &acp::SessionId) -> String {
    load_codex_session_aliases()
        .get(session_id.0.as_ref())
        .cloned()
        .unwrap_or_else(|| session_id.to_string())
}

fn load_codex_session_aliases() -> HashMap<String, String> {
    std::fs::read_to_string(codex_session_alias_path())
        .ok()
        .and_then(|content| serde_json::from_str::<HashMap<String, String>>(&content).ok())
        .unwrap_or_default()
}

fn persist_codex_session_alias(session_id: &acp::SessionId, codex_thread_id: &str) -> Result<()> {
    let path = codex_session_alias_path();
    let mut aliases = load_codex_session_aliases();
    if aliases.get(session_id.0.as_ref()) == Some(&codex_thread_id.to_string()) {
        return Ok(());
    }

    aliases.insert(session_id.to_string(), codex_thread_id.to_string());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec(&aliases)?)?;
    Ok(())
}

async fn load_codex_rollout(
    codex_session_id: &str,
    remote_client: Option<Entity<RemoteClient>>,
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    if let Some(remote_client) = remote_client {
        let script = format!(
            "path=$(find \"$HOME/.codex/sessions\" -type f -name 'rollout-*-{codex_session_id}.jsonl' | head -n 1)\nif [ -z \"$path\" ]; then exit 2; fi\ncat \"$path\""
        );
        return run_remote_capture(remote_client, "sh", &["-lc", &script], cx).await;
    }

    let Some(path) = find_local_codex_rollout_path(codex_session_id)? else {
        return Err(anyhow!(
            "Codex transcript not found for session {codex_session_id}"
        ));
    };
    Ok(std::fs::read_to_string(path)?)
}

fn find_local_codex_rollout_path(codex_session_id: &str) -> Result<Option<PathBuf>> {
    let root = util::paths::home_dir().join(".codex").join("sessions");
    if !root.exists() {
        return Ok(None);
    }

    let expected_suffix = format!("-{codex_session_id}.jsonl");
    let mut stack = vec![root];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(&expected_suffix) {
                return Ok(Some(entry.path()));
            }
        }
    }

    Ok(None)
}

async fn run_remote_capture(
    remote_client: Entity<RemoteClient>,
    program: &str,
    args: &[&str],
    cx: &mut gpui::AsyncApp,
) -> Result<String> {
    let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let mut command = cx.update(|cx| {
        let command = remote_client.read(cx).build_command_with_options(
            Some(program.to_string()),
            &args,
            &HashMap::default(),
            None,
            None,
            Interactive::No,
        )?;
        let mut process = Command::new(command.program);
        process.args(command.args);
        process.envs(command.env);
        anyhow::Ok(process)
    })?;

    let mut child = Child::spawn(command, Stdio::null(), Stdio::piped(), Stdio::piped())?;
    let mut stdout = child
        .stdout
        .take()
        .context("failed to take remote command stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("failed to take remote command stderr")?;

    let mut stdout_output = String::new();
    let mut stderr_output = String::new();
    stdout.read_to_string(&mut stdout_output).await?;
    stderr.read_to_string(&mut stderr_output).await?;

    let status = child.status().await?;
    if !status.success() {
        let stderr_output = stderr_output.trim();
        if stderr_output.is_empty() {
            return Err(anyhow!("remote command exited with {status}"));
        }
        return Err(anyhow!(
            "remote command exited with {status}: {stderr_output}"
        ));
    }

    Ok(stdout_output)
}

fn parse_codex_rollout_history(rollout: &str) -> Result<Vec<CodexHistoryMessage>> {
    let event_history = parse_codex_rollout_event_history(rollout)?;
    if !event_history.is_empty() {
        return Ok(event_history);
    }

    parse_codex_rollout_response_item_history(rollout)
}

fn parse_codex_rollout_event_history(rollout: &str) -> Result<Vec<CodexHistoryMessage>> {
    let mut history = Vec::new();

    for line in rollout.lines() {
        let value = serde_json::from_str::<serde_json::Value>(line)?;
        if value.get("type").and_then(|value| value.as_str()) != Some("event_msg") {
            continue;
        }

        let Some(payload) = value.get("payload") else {
            continue;
        };
        let Some(message_type) = payload.get("type").and_then(|value| value.as_str()) else {
            continue;
        };

        let (role, text) = match message_type {
            "user_message" => (
                CodexHistoryRole::User,
                payload
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default(),
            ),
            "agent_message"
                if payload.get("phase").and_then(|value| value.as_str())
                    == Some("final_answer") =>
            {
                (
                    CodexHistoryRole::Assistant,
                    payload
                        .get("message")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default(),
                )
            }
            _ => continue,
        };

        if text.is_empty() {
            continue;
        }

        history.push(CodexHistoryMessage {
            user_message_id: matches!(role, CodexHistoryRole::User).then(UserMessageId::new),
            role,
            text: text.to_string(),
        });
    }

    Ok(history)
}

fn parse_codex_rollout_response_item_history(rollout: &str) -> Result<Vec<CodexHistoryMessage>> {
    let mut history = Vec::new();

    for line in rollout.lines() {
        let value = serde_json::from_str::<serde_json::Value>(line)?;
        if value.get("type").and_then(|value| value.as_str()) != Some("response_item") {
            continue;
        }

        let Some(payload) = value.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|value| value.as_str()) != Some("message") {
            continue;
        }

        let Some(role) = payload.get("role").and_then(|value| value.as_str()) else {
            continue;
        };
        if role != "user" && role != "assistant" {
            continue;
        }
        if role == "assistant"
            && payload.get("phase").and_then(|value| value.as_str()) == Some("commentary")
        {
            continue;
        }

        let Some(content) = payload.get("content").and_then(|value| value.as_array()) else {
            continue;
        };
        let text = content
            .iter()
            .filter_map(|item| {
                let item_type = item.get("type").and_then(|value| value.as_str())?;
                let expected_type = if role == "user" {
                    "input_text"
                } else {
                    "output_text"
                };
                if item_type != expected_type {
                    return None;
                }
                item.get("text").and_then(|value| value.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        if text.is_empty() {
            continue;
        }

        history.push(CodexHistoryMessage {
            user_message_id: (role == "user").then(UserMessageId::new),
            role: if role == "user" {
                CodexHistoryRole::User
            } else {
                CodexHistoryRole::Assistant
            },
            text,
        });
    }

    Ok(history)
}

fn seed_thread_from_history(
    thread: &Entity<AcpThread>,
    history: &[CodexHistoryMessage],
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    thread.update(cx, |thread, cx| {
        for message in history {
            let content = acp::ContentBlock::Text(acp::TextContent::new(message.text.clone()));
            match message.role {
                CodexHistoryRole::User => {
                    thread.push_user_content_block(message.user_message_id.clone(), content, cx);
                }
                CodexHistoryRole::Assistant => {
                    thread.push_assistant_content_block(content, false, cx);
                }
            }
        }
    });
    Ok(())
}

fn codex_models() -> Vec<AgentModelInfo> {
    [
        (
            DEFAULT_CODEX_MODEL,
            "GPT-5.5",
            "Default Codex model for coding and agentic work.",
            true,
        ),
        (
            "gpt-5.3-codex",
            "GPT-5.3 Codex",
            "Codex-optimized model for agentic coding tasks.",
            false,
        ),
        (
            "gpt-5.1-codex",
            "GPT-5.1 Codex",
            "Earlier Codex-optimized model.",
            false,
        ),
        (
            "gpt-5-codex",
            "GPT-5 Codex",
            "Previous Codex-optimized model.",
            false,
        ),
    ]
    .into_iter()
    .map(|(id, name, description, is_latest)| AgentModelInfo {
        id: acp::ModelId::new(id),
        name: name.into(),
        description: Some(description.into()),
        icon: Some(acp_thread::AgentModelIcon::Named(ui::IconName::AiOpenAi)),
        is_latest,
        cost: None,
    })
    .collect()
}

fn codex_model_context_window(_model_id: &acp::ModelId) -> u64 {
    258_400
}

fn codex_default_model(cx: &App) -> acp::ModelId {
    codex_agent_settings(cx)
        .and_then(|settings| settings.default_model().map(acp::ModelId::new))
        .unwrap_or_else(|| acp::ModelId::new(DEFAULT_CODEX_MODEL))
}

fn codex_default_mode(cx: &App) -> acp::SessionModeId {
    codex_agent_settings(cx)
        .and_then(|settings| settings.default_mode().map(acp::SessionModeId::new))
        .filter(|mode_id| {
            mode_id.0.as_ref() == STANDARD_MODE_ID || mode_id.0.as_ref() == FAST_MODE_ID
        })
        .unwrap_or_else(|| acp::SessionModeId::new(STANDARD_MODE_ID))
}

fn codex_default_reasoning_effort(
    mode_id: &acp::SessionModeId,
    cx: &App,
) -> acp::SessionConfigValueId {
    if mode_id.0.as_ref() == FAST_MODE_ID {
        return acp::SessionConfigValueId::new(LOW_REASONING_EFFORT);
    }

    codex_agent_settings(cx)
        .and_then(|settings| {
            settings
                .default_config_option(REASONING_EFFORT_CONFIG_ID)
                .filter(|value| is_valid_reasoning_effort(value))
                .map(acp::SessionConfigValueId::new)
        })
        .unwrap_or_else(|| acp::SessionConfigValueId::new(MEDIUM_REASONING_EFFORT))
}

fn is_valid_reasoning_effort(value: &str) -> bool {
    matches!(
        value,
        LOW_REASONING_EFFORT | MEDIUM_REASONING_EFFORT | HIGH_REASONING_EFFORT
    )
}

fn codex_mode_config_option(current_value: acp::SessionModeId) -> acp::SessionConfigOption {
    acp::SessionConfigOption::select(
        acp::SessionConfigId::new(MODE_CONFIG_ID),
        "Mode",
        acp::SessionConfigValueId::new(current_value.to_string()),
        vec![
            acp::SessionConfigSelectOption::new(STANDARD_MODE_ID, "Standard")
                .description("Use Codex with the selected reasoning effort."),
            acp::SessionConfigSelectOption::new(FAST_MODE_ID, "Fast")
                .description("Use low reasoning effort for quicker Codex turns."),
        ],
    )
    .description("Controls the Codex session mode.")
    .category(acp::SessionConfigOptionCategory::Mode)
}

fn codex_model_config_option(current_value: acp::ModelId) -> acp::SessionConfigOption {
    let options = codex_models()
        .into_iter()
        .map(|model| {
            let mut option =
                acp::SessionConfigSelectOption::new(model.id.to_string(), model.name.to_string());
            if let Some(description) = model.description {
                option = option.description(description.to_string());
            }
            option
        })
        .collect::<Vec<_>>();

    acp::SessionConfigOption::select(
        acp::SessionConfigId::new(MODEL_CONFIG_ID),
        "Model",
        acp::SessionConfigValueId::new(current_value.to_string()),
        options,
    )
    .description("Controls the Codex model.")
    .category(acp::SessionConfigOptionCategory::Model)
}

fn codex_reasoning_config_option(
    current_value: acp::SessionConfigValueId,
) -> acp::SessionConfigOption {
    acp::SessionConfigOption::select(
        acp::SessionConfigId::new(REASONING_EFFORT_CONFIG_ID),
        "Reasoning",
        current_value,
        vec![
            acp::SessionConfigSelectOption::new(LOW_REASONING_EFFORT, "Low")
                .description("Use less reasoning for quicker responses."),
            acp::SessionConfigSelectOption::new(MEDIUM_REASONING_EFFORT, "Medium")
                .description("Use the default reasoning level."),
            acp::SessionConfigSelectOption::new(HIGH_REASONING_EFFORT, "High")
                .description("Use more reasoning for harder tasks."),
        ],
    )
    .description("Controls Codex reasoning effort.")
    .category(acp::SessionConfigOptionCategory::ThoughtLevel)
}

fn codex_agent_settings(cx: &App) -> Option<CustomAgentServerSettings> {
    cx.read_global(|settings: &SettingsStore, _| {
        settings
            .get::<AllAgentServersSettings>(None)
            .get(CODEX_AGENT_ID.as_ref())
            .cloned()
    })
}

fn persist_codex_model(fs: Arc<dyn Fs>, model_id: Option<acp::ModelId>, cx: &App) {
    update_settings_file(fs, cx, move |settings, _cx| {
        let agent = settings
            .agent_servers
            .get_or_insert_default()
            .entry(CODEX_AGENT_ID.0.to_string())
            .or_insert_with(default_codex_settings);

        match agent {
            settings::CustomAgentServerSettings::Custom { default_model, .. }
            | settings::CustomAgentServerSettings::Registry { default_model, .. } => {
                *default_model = model_id.map(|model_id| model_id.to_string());
            }
        }
    });
}

fn persist_codex_mode(fs: Arc<dyn Fs>, mode_id: Option<acp::SessionModeId>, cx: &App) {
    update_settings_file(fs, cx, move |settings, _cx| {
        let agent = settings
            .agent_servers
            .get_or_insert_default()
            .entry(CODEX_AGENT_ID.0.to_string())
            .or_insert_with(default_codex_settings);

        match agent {
            settings::CustomAgentServerSettings::Custom { default_mode, .. }
            | settings::CustomAgentServerSettings::Registry { default_mode, .. } => {
                *default_mode = mode_id.map(|mode_id| mode_id.to_string());
            }
        }
    });
}

fn persist_codex_config_option(
    fs: Arc<dyn Fs>,
    config_id: String,
    value_id: Option<String>,
    cx: &App,
) {
    update_settings_file(fs, cx, move |settings, _cx| {
        let agent = settings
            .agent_servers
            .get_or_insert_default()
            .entry(CODEX_AGENT_ID.0.to_string())
            .or_insert_with(default_codex_settings);

        match agent {
            settings::CustomAgentServerSettings::Custom {
                default_config_options,
                ..
            }
            | settings::CustomAgentServerSettings::Registry {
                default_config_options,
                ..
            } => {
                if let Some(value_id) = &value_id {
                    default_config_options.insert(config_id.clone(), value_id.clone());
                } else {
                    default_config_options.remove(&config_id);
                }
            }
        }
    });
}

fn default_codex_settings() -> settings::CustomAgentServerSettings {
    settings::CustomAgentServerSettings::Registry {
        env: Default::default(),
        default_mode: None,
        default_model: None,
        favorite_models: Vec::new(),
        default_config_options: Default::default(),
        favorite_config_option_values: Default::default(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: CodexUsage },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: CodexError },
    #[serde(rename = "item.started")]
    ItemStarted {
        #[serde(rename = "item")]
        _item: CodexItem,
    },
    #[serde(rename = "item.updated")]
    ItemUpdated {
        #[serde(rename = "item")]
        _item: CodexItem,
    },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexItem },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Deserialize)]
struct CodexError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
}

impl CodexUsage {
    fn to_token_usage(&self, max_tokens: u64) -> TokenUsage {
        TokenUsage {
            max_tokens,
            used_tokens: self.input_tokens + self.output_tokens,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            max_output_tokens: None,
        }
    }

    fn into_acp_usage(self) -> acp::Usage {
        acp::Usage::new(
            self.input_tokens + self.output_tokens,
            self.input_tokens,
            self.output_tokens,
        )
        .cached_read_tokens(self.cached_input_tokens)
        .thought_tokens(self.reasoning_output_tokens)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexItem {
    #[serde(rename = "agent_message")]
    AgentMessage { text: String },
    #[serde(rename = "reasoning")]
    Reasoning { text: String },
    #[serde(rename = "command_execution")]
    CommandExecution {
        command: String,
        aggregated_output: String,
        exit_code: Option<i32>,
        status: String,
    },
    #[serde(rename = "file_change")]
    FileChange {
        changes: Vec<CodexFileChange>,
        status: String,
    },
    #[serde(rename = "mcp_tool_call")]
    McpToolCall {},
    #[serde(rename = "web_search")]
    WebSearch {},
    #[serde(rename = "todo_list")]
    TodoList { items: Vec<CodexTodoItem> },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Deserialize)]
struct CodexFileChange {
    path: String,
    kind: String,
}

#[derive(Debug, Deserialize)]
struct CodexTodoItem {
    text: String,
    completed: bool,
}
