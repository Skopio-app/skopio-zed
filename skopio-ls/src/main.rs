use anyhow::Context;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    process::Command,
    sync::Mutex,
    time::{Instant, interval},
};
use tower_lsp::{
    Client, LanguageServer, LspService, Server, jsonrpc::Result as LspResult, lsp_types::*,
};
use url::Url;

#[derive(Debug, Clone)]
struct CliConfig {
    skopio_cli: String,
    // Flush current active session after no activity for this duration
    idle_timeout: Duration,
    // Keep sessions alive for this long after switching away;
    // if no activity, flush them.
    switch_grace: Duration,
    // Don't emit events shorter than this
    min_session_secs: i64,
    category: String,
    app: String,
    entity_type: String,
    source: String,
}

impl CliConfig {
    fn from_env() -> Self {
        let skopio_cli = std::env::var("SKOPIO_CLI_PATH").unwrap_or_else(|_| "skopio-cli".into());
        let idle_secs = std::env::var("SKOPIO_ZED_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        let grace_secs = std::env::var("SKOPIO_ZED_SWITCH_GRACE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        let min_session_secs = std::env::var("SKOPIO_ZED_MIN_SESSION_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(2);

        Self {
            skopio_cli,
            idle_timeout: Duration::from_secs(idle_secs),
            switch_grace: Duration::from_secs(grace_secs),
            min_session_secs,
            category: "Coding".into(),
            app: "Zed".into(),
            entity_type: "File".into(),
            source: "skopio-zed".into(),
        }
    }
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn uri_to_path_string(uri: &Url) -> Option<String> {
    if uri.scheme() != "file" {
        return None;
    }
    uri.to_file_path()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

#[derive(Debug, Clone)]
struct Session {
    uri: Url,
    entity: String,
    project: String,
    start_ts: i64,
    last_ts: i64,
    last_seen: Instant,
}

#[derive(Debug)]
struct State {
    workspace_root: Option<String>,
    sessions: HashMap<String, Session>,
    current_key: Option<String>,
}

impl State {
    fn project_string(&self) -> String {
        self.workspace_root
            .clone()
            .unwrap_or_else(|| "unknown".into())
    }
}

async fn emit_cli_event(cfg: &CliConfig, sess: &Session) -> anyhow::Result<()> {
    let end_ts = sess.last_ts;
    let duration = end_ts - sess.start_ts;

    if duration < cfg.min_session_secs {
        return Ok(());
    }

    let status = Command::new(&cfg.skopio_cli)
        .arg("event")
        .arg("--timestamp")
        .arg(sess.start_ts.to_string())
        .arg("--category")
        .arg(&cfg.category)
        .arg("--app")
        .arg(&cfg.app)
        .arg("--entity")
        .arg(&sess.entity)
        .arg("--entity-type")
        .arg(&cfg.entity_type)
        .arg("--duration")
        .arg(duration.to_string())
        .arg("--project")
        .arg(&sess.project)
        .arg("--source")
        .arg(&cfg.source)
        .arg("--end-timestamp")
        .arg(end_ts.to_string())
        .status()
        .await
        .with_context(|| format!("Failed to run `{}`", cfg.skopio_cli))?;

    if !status.success() {
        anyhow::bail!("Skopio CLI exited with status {status}");
    }

    Ok(())
}

struct Backend {
    client: Client,
    cfg: CliConfig,
    state: Arc<Mutex<State>>,
}

impl Backend {
    async fn note_activity(&self, uri: Url) {
        let now_ts = now_unix_secs();
        let now_instant = Instant::now();

        let key = uri.to_string();

        let (entity, project) = {
            let st = self.state.lock().await;
            (
                uri_to_path_string(&uri).unwrap_or_else(|| key.clone()),
                st.project_string(),
            )
        };

        let mut st = self.state.lock().await;

        // Update or insert session
        match st.sessions.get_mut(&key) {
            Some(s) => {
                s.last_ts = now_ts;
                s.last_seen = now_instant;
            }
            None => {
                st.sessions.insert(
                    key.clone(),
                    Session {
                        uri,
                        entity,
                        project,
                        start_ts: now_ts,
                        last_ts: now_ts,
                        last_seen: now_instant,
                    },
                );
            }
        }

        // Mark current file as active
        st.current_key = Some(key)
    }

    async fn flush_closed(&self, uri: &Url) {
        let key = uri.to_string();

        let maybe = {
            let mut st = self.state.lock().await;
            let removed = st.sessions.remove(&key);
            if st.current_key.as_deref() == Some(&key) {
                st.current_key = None;
            }
            removed
        };

        if let Some(sess) = maybe {
            if let Err(err) = emit_cli_event(&self.cfg, &sess).await {
                let _ = self
                    .client
                    .log_message(
                        MessageType::ERROR,
                        format!("Skopio CLI event failed: {err:#}"),
                    )
                    .await;
            }
        }
    }

    async fn periodic_flush_tick(cfg: CliConfig, state: Arc<Mutex<State>>) {
        let mut tick = interval(Duration::from_secs(5));
        loop {
            tick.tick().await;

            let now = Instant::now();
            let mut to_flush: Vec<Session> = Vec::new();

            {
                let mut st = state.lock().await;
                let current_key = st.current_key.clone();

                // Idle flush current session
                if let Some(cur_key) = &current_key {
                    if let Some(cur_sess) = st.sessions.get(cur_key) {
                        if now.duration_since(cur_sess.last_seen) >= cfg.idle_timeout {
                            if let Some(s) = st.sessions.remove(cur_key) {
                                to_flush.push(s);
                            }
                            st.current_key = None;
                        }
                    } else {
                        st.current_key = None;
                    }
                }
                let current_key = st.current_key.clone();

                // Grace flush all non-current sessions
                let grace = cfg.switch_grace;
                let keys_to_remove: Vec<String> = st
                    .sessions
                    .iter()
                    .filter_map(|(k, s)| {
                        let is_current = current_key.as_deref() == Some(k.as_str());
                        if is_current {
                            return None;
                        }
                        if now.duration_since(s.last_seen) >= grace {
                            Some(k.clone())
                        } else {
                            None
                        }
                    })
                    .collect();

                for k in keys_to_remove {
                    if let Some(s) = st.sessions.remove(&k) {
                        to_flush.push(s);
                    }
                }
            }

            for sess in to_flush {
                let _ = emit_cli_event(&cfg, &sess).await;
            }
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        let root = params
            .root_uri
            .map(|u| uri_to_path_string(&u).unwrap_or_else(|| u.to_string()))
            .or_else(|| {
                params.workspace_folders.as_ref().and_then(|wf| {
                    wf.first()
                        .map(|f| uri_to_path_string(&f.uri).unwrap_or_else(|| f.uri.to_string()))
                })
            });
        {
            let mut st = self.state.lock().await;
            st.workspace_root = root;
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "skopio-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let _ = self
            .client
            .log_message(MessageType::INFO, "Skopio LSP initialized")
            .await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        let mut sessions: Vec<Session> = Vec::new();
        {
            let mut st = self.state.lock().await;
            sessions.extend(st.sessions.drain().map(|(_, v)| v));
            st.current_key = None;
        }
        for sess in sessions {
            let _ = emit_cli_event(&self.cfg, &sess).await;
        }
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.note_activity(params.text_document.uri).await
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.note_activity(params.text_document.uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.note_activity(params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.flush_closed(&params.text_document.uri).await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = CliConfig::from_env();

    let state = Arc::new(Mutex::new(State {
        workspace_root: None,
        sessions: HashMap::new(),
        current_key: None,
    }));

    tokio::spawn(Backend::periodic_flush_tick(cfg.clone(), state.clone()));

    let (service, socket) = LspService::new(|client| Backend {
        client,
        cfg: cfg.clone(),
        state: state.clone(),
    });

    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;

    Ok(())
}
