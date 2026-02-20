use anyhow::Context;
use clap::{Arg, Command as ClapCommand};
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
    idle_timeout: Duration,
    switch_grace: Duration,
    min_session_secs: i64,
    category: String,
    app: String,
    entity_type: String,
    source: String,
    sync_interval: Duration,
}

impl CliConfig {
    /// Defaults from env
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
        let sync_secs = std::env::var("SKOPIO_ZED_SYNC_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(180);

        Self {
            skopio_cli,
            idle_timeout: Duration::from_secs(idle_secs),
            switch_grace: Duration::from_secs(grace_secs),
            min_session_secs,
            category: "Coding".into(),
            app: "Zed".into(),
            entity_type: "File".into(),
            source: "skopio-zed".into(),
            sync_interval: Duration::from_secs(sync_secs),
        }
    }

    fn from_args() -> anyhow::Result<Self> {
        let mut cfg = Self::from_env();

        let matches = ClapCommand::new("skopio-ls")
            .version(env!("CARGO_PKG_VERSION"))
            .about("Skopio language server for Zed")
            .arg(
                Arg::new("skopio-cli")
                    .long("skopio-cli")
                    .value_name("PATH")
                    .help("Path to skopio-cli binary")
                    .required(true),
            )
            .arg(
                Arg::new("idle-secs")
                    .long("idle-secs")
                    .value_name("SECS")
                    .help("Flush current active session after this many seconds of no activity")
                    .required(false),
            )
            .arg(
                Arg::new("switch-grace-secs")
                    .long("switch-grace-secs")
                    .value_name("SECS")
                    .help("Flush non-current sessions after this many seconds since last activity")
                    .required(false),
            )
            .arg(
                Arg::new("min-session-secs")
                    .long("min-session-secs")
                    .value_name("SECS")
                    .help("Do not emit sessions shorter than this duration")
                    .required(false),
            )
            .arg(
                Arg::new("category")
                    .long("category")
                    .value_name("NAME")
                    .help("Category to send to skopio-cli")
                    .required(false),
            )
            .arg(
                Arg::new("app")
                    .long("app")
                    .value_name("NAME")
                    .help("App name to send to skopio-cli")
                    .required(false),
            )
            .arg(
                Arg::new("entity-type")
                    .long("entity-type")
                    .value_name("NAME")
                    .help("Entity type to send to skopio-cli")
                    .required(false),
            )
            .arg(
                Arg::new("source")
                    .long("source")
                    .value_name("NAME")
                    .help("Source identifier to send to skopio-cli")
                    .required(false),
            )
            .get_matches();

        cfg.skopio_cli = matches
            .get_one::<String>("skopio-cli")
            .expect("required")
            .to_string();

        if let Some(v) = matches.get_one::<String>("idle-secs") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.idle_timeout = Duration::from_secs(secs);
            }
        }

        if let Some(v) = matches.get_one::<String>("switch-grace-secs") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.switch_grace = Duration::from_secs(secs);
            }
        }

        if let Some(v) = matches.get_one::<String>("min-session-secs") {
            if let Ok(secs) = v.parse::<i64>() {
                cfg.min_session_secs = secs;
            }
        }

        if let Some(v) = matches.get_one::<String>("category") {
            cfg.category = v.to_string();
        }
        if let Some(v) = matches.get_one::<String>("app") {
            cfg.app = v.to_string();
        }
        if let Some(v) = matches.get_one::<String>("entity-type") {
            cfg.entity_type = v.to_string();
        }
        if let Some(v) = matches.get_one::<String>("source") {
            cfg.source = v.to_string();
        }

        Ok(cfg)
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
    #[allow(dead_code)]
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
    tick_count: u64,
    last_sync: Instant,
    sync_running: bool,
}

impl State {
    fn project_string(&self) -> String {
        self.workspace_root
            .clone()
            .unwrap_or_else(|| "unknown".into())
    }
}

async fn run_cli_sync(cfg: &CliConfig) -> anyhow::Result<(String, String)> {
    let mut cwd = Command::new(&cfg.skopio_cli);
    cwd.arg("sync");

    let output = cwd
        .output()
        .await
        .with_context(|| format!("Failed to run `{}` sync", cfg.skopio_cli))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        anyhow::bail!(
            "Skopio CLI exited non-zero: status={:?}, stderr={}",
            output.status.code(),
            stderr.trim()
        );
    }

    Ok((stdout, stderr))
}

/// Runs skopio-cli and returns stdout/stderr for debugging.
async fn emit_cli_event(cfg: &CliConfig, sess: &Session) -> anyhow::Result<(i64, String, String)> {
    let end_ts = sess.last_ts;
    let duration = end_ts - sess.start_ts;

    if duration < cfg.min_session_secs {
        return Ok((duration, String::new(), String::new()));
    }

    let mut cmd = Command::new(&cfg.skopio_cli);
    cmd.arg("event")
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
        .arg(end_ts.to_string());

    let output = cmd
        .output()
        .await
        .with_context(|| format!("Failed to run `{}`", cfg.skopio_cli))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        anyhow::bail!(
            "Skopio CLI exited non-zero: status={:?}, stderr={}",
            output.status.code(),
            stderr.trim()
        );
    }

    Ok((duration, stdout, stderr))
}

struct Backend {
    client: Client,
    cfg: CliConfig,
    state: Arc<Mutex<State>>,
}

impl Backend {
    async fn log(&self, ty: MessageType, msg: impl Into<String>) {
        let _ = self.client.log_message(ty, msg.into()).await;
    }

    async fn sync(&self, reason: &'static str) {
        let should_start = {
            let mut st = self.state.lock().await;

            if st.sync_running {
                return;
            }

            let since = st.last_sync.elapsed();
            if since < self.cfg.sync_interval {
                return;
            }

            st.sync_running = true;
            st.last_sync = Instant::now();
            true
        };

        if !should_start {
            return;
        }

        self.log(
            MessageType::LOG,
            format!("[sync] starting (reason={reason})"),
        )
        .await;

        match run_cli_sync(&self.cfg).await {
            Ok((out, err)) => {
                if !out.trim().is_empty() {
                    self.log(MessageType::LOG, format!("[sync] stdout: {}", out.trim()))
                        .await;
                }
                if !err.trim().is_empty() {
                    self.log(MessageType::LOG, format!("[sync] stderr: {}", err.trim()))
                        .await;
                }
                self.log(MessageType::INFO, "[sync] ok").await;
            }
            Err(e) => {
                self.log(MessageType::ERROR, format!("[sync] FAILED: {e:#}"))
                    .await;
            }
        }

        let mut st = self.state.lock().await;
        st.sync_running = false;
    }

    async fn note_activity(&self, uri: Url, source: &'static str) {
        let now_ts = now_unix_secs();
        let now_instant = Instant::now();
        let key = uri.to_string();

        let (entity, project, root, before_sessions, prev_current) = {
            let st = self.state.lock().await;
            (
                uri_to_path_string(&uri).unwrap_or_else(|| key.clone()),
                st.project_string(),
                st.workspace_root
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                st.sessions.len(),
                st.current_key.clone(),
            )
        };

        let mut st = self.state.lock().await;

        let existed = st.sessions.contains_key(&key);
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

        st.current_key = Some(key.clone());
        let after_sessions = st.sessions.len();

        drop(st);

        self.log(
            MessageType::LOG,
            format!(
                "[{source}] activity: key={key} existed={existed} root={root} sessions {before_sessions}->{after_sessions} prev_current={:?}",
                prev_current.as_deref()
            ),
        )
        .await;
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
            self.log(
                MessageType::INFO,
                format!(
                    "[did_close] flushing session: entity={} start={} last={} (key={})",
                    sess.entity, sess.start_ts, sess.last_ts, key
                ),
            )
            .await;

            match emit_cli_event(&self.cfg, &sess).await {
                Ok((duration, stdout, stderr)) => {
                    if duration < self.cfg.min_session_secs {
                        self.log(
                            MessageType::LOG,
                            format!(
                                "[did_close] skipped (too short): duration={}s < min_session_secs={}",
                                duration, self.cfg.min_session_secs
                            ),
                        )
                        .await;
                    } else {
                        if !stdout.trim().is_empty() {
                            self.log(
                                MessageType::LOG,
                                format!("[did_close] cli stdout: {}", stdout.trim()),
                            )
                            .await;
                        }
                        if !stderr.trim().is_empty() {
                            self.log(
                                MessageType::LOG,
                                format!("[did_close] cli stderr: {}", stderr.trim()),
                            )
                            .await;
                        }

                        self.log(
                            MessageType::INFO,
                            format!(
                                "[did_close] CLI event OK: duration={}s entity={}",
                                duration, sess.entity
                            ),
                        )
                        .await;

                        self.sync("did_close").await;
                    }
                }
                Err(err) => {
                    self.log(
                        MessageType::ERROR,
                        format!(
                            "[did_close] CLI event FAILED: {err:#} (entity={})",
                            sess.entity
                        ),
                    )
                    .await;
                }
            }
        } else {
            self.log(
                MessageType::LOG,
                format!("[did_close] no session found for key={key}"),
            )
            .await;
        }
    }

    async fn periodic_flush_tick(self: Arc<Self>) {
        let mut tick = interval(Duration::from_secs(5));
        loop {
            tick.tick().await;

            let now = Instant::now();
            let (tick_no, cur_key, total_sessions) = {
                let mut st = self.state.lock().await;
                st.tick_count += 1;
                (st.tick_count, st.current_key.clone(), st.sessions.len())
            };

            if tick_no % 6 == 0 {
                let _ = self.client
                    .log_message(
                        MessageType::LOG,
                        format!(
                            "[tick] running (every 5s). tick={} sessions={} current={:?} idle={}s grace={}s sync_every={}s",
                            tick_no,
                            total_sessions,
                            cur_key.as_deref(),
                            self.cfg.idle_timeout.as_secs(),
                            self.cfg.switch_grace.as_secs(),
                            self.cfg.sync_interval.as_secs(),
                        ),
                    )
                    .await;
            }

            let mut to_flush: Vec<(Session, &'static str)> = Vec::new();

            {
                let mut st = self.state.lock().await;
                let current_key = st.current_key.clone();

                if let Some(cur_key) = &current_key {
                    if let Some(cur_sess) = st.sessions.get(cur_key) {
                        if now.duration_since(cur_sess.last_seen) >= self.cfg.idle_timeout {
                            if let Some(s) = st.sessions.remove(cur_key) {
                                to_flush.push((s, "idle_timeout"));
                            }
                            st.current_key = None;
                        }
                    } else {
                        st.current_key = None;
                    }
                }

                let current_key = st.current_key.clone();
                let grace = self.cfg.switch_grace;

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
                        to_flush.push((s, "switch_grace"));
                    }
                }
            }

            for (sess, reason) in to_flush {
                let _ = self
                    .client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "[tick] flushing ({reason}): entity={} start={} last={}",
                            sess.entity, sess.start_ts, sess.last_ts
                        ),
                    )
                    .await;

                match emit_cli_event(&self.cfg, &sess).await {
                    Ok((duration, stdout, stderr)) => {
                        if duration < self.cfg.min_session_secs {
                            let _ = self.client
                                .log_message(
                                    MessageType::LOG,
                                    format!(
                                        "[tick] skipped (too short): duration={}s < min_session_secs={}",
                                        duration, self.cfg.min_session_secs
                                    ),
                                )
                                .await;
                        } else {
                            if !stdout.trim().is_empty() {
                                let _ = self
                                    .client
                                    .log_message(
                                        MessageType::LOG,
                                        format!("[tick] CLI stdout: {}", stdout.trim()),
                                    )
                                    .await;
                            }
                            if !stderr.trim().is_empty() {
                                let _ = self
                                    .client
                                    .log_message(
                                        MessageType::LOG,
                                        format!("[tick] CLI stderr: {}", stderr.trim()),
                                    )
                                    .await;
                            }
                            let _ = self
                                .client
                                .log_message(
                                    MessageType::INFO,
                                    format!(
                                        "[tick] CLI event OK: duration={}s entity={}",
                                        duration, sess.entity
                                    ),
                                )
                                .await;

                            self.sync("post-flush").await;
                        }
                    }
                    Err(err) => {
                        let _ = self
                            .client
                            .log_message(
                                MessageType::ERROR,
                                format!(
                                    "[tick] CLI event FAILED: {err:#} (entity={})",
                                    sess.entity
                                ),
                            )
                            .await;
                    }
                }
            }

            self.sync("periodic").await;
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
            st.workspace_root = root.clone();
        }

        self.log(
            MessageType::INFO,
            format!(
                "[initialize] root={:?} cfg={{cli: {}, idle:{}s, grace:{}s, min_session:{}s, sync_interval:{}s, category:{}, app:{}, entity_type:{}, source:{}}}",
                root.as_deref(),
                self.cfg.skopio_cli,
                self.cfg.idle_timeout.as_secs(),
                self.cfg.switch_grace.as_secs(),
                self.cfg.min_session_secs,
                self.cfg.sync_interval.as_secs(),
                self.cfg.category,
                self.cfg.app,
                self.cfg.entity_type,
                self.cfg.source
            ),
        )
        .await;

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
        self.log(MessageType::INFO, "Skopio LSP initialized").await;
        self.sync("initialized").await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        self.log(
            MessageType::INFO,
            "[shutdown] flushing all remaining sessions...",
        )
        .await;

        let mut sessions: Vec<Session> = Vec::new();
        {
            let mut st = self.state.lock().await;
            sessions.extend(st.sessions.drain().map(|(_, v)| v));
            st.current_key = None;
        }

        for sess in sessions {
            self.log(
                MessageType::INFO,
                format!(
                    "[shutdown] flushing: entity={} start={} last={}",
                    sess.entity, sess.start_ts, sess.last_ts
                ),
            )
            .await;

            match emit_cli_event(&self.cfg, &sess).await {
                Ok((duration, stdout, stderr)) => {
                    if duration < self.cfg.min_session_secs {
                        self.log(
                            MessageType::LOG,
                            format!(
                                "[shutdown] skipped (too short): duration={}s < min_session_secs={}",
                                duration, self.cfg.min_session_secs
                            ),
                        )
                        .await;
                    } else {
                        if !stdout.trim().is_empty() {
                            self.log(
                                MessageType::LOG,
                                format!("[shutdown] CLI stdout: {}", stdout.trim()),
                            )
                            .await;
                        }
                        if !stderr.trim().is_empty() {
                            self.log(
                                MessageType::LOG,
                                format!("[shutdown] CLI stderr: {}", stderr.trim()),
                            )
                            .await;
                        }
                        self.log(
                            MessageType::INFO,
                            format!(
                                "[shutdown] CLI event OK: duration={}s entity={}",
                                duration, sess.entity
                            ),
                        )
                        .await;
                    }
                }
                Err(err) => {
                    self.log(
                        MessageType::ERROR,
                        format!(
                            "[shutdown] CLI event FAILED: {err:#} (entity={})",
                            sess.entity
                        ),
                    )
                    .await;
                }
            }
        }

        self.sync("shutdown").await;

        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.log(
            MessageType::LOG,
            format!("[did_open] uri={}", params.text_document.uri),
        )
        .await;
        self.note_activity(params.text_document.uri, "did_open")
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.log(
            MessageType::LOG,
            format!(
                "[did_change] uri={} changes={}",
                params.text_document.uri,
                params.content_changes.len()
            ),
        )
        .await;
        self.note_activity(params.text_document.uri, "did_change")
            .await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.log(
            MessageType::LOG,
            format!("[did_save] uri={}", params.text_document.uri),
        )
        .await;
        self.note_activity(params.text_document.uri, "did_save")
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.log(
            MessageType::LOG,
            format!("[did_close] uri={}", params.text_document.uri),
        )
        .await;
        self.flush_closed(&params.text_document.uri).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = CliConfig::from_args()?;

    let state = Arc::new(Mutex::new(State {
        workspace_root: None,
        sessions: HashMap::new(),
        current_key: None,
        tick_count: 0,
        last_sync: Instant::now() - cfg.sync_interval,
        sync_running: false,
    }));

    let (service, socket) = LspService::new(|client| {
        let backend = Arc::new(Backend {
            client,
            cfg: cfg.clone(),
            state: state.clone(),
        });

        let bg = backend.clone();
        tokio::spawn(async move {
            bg.periodic_flush_tick().await;
        });

        backend
    });

    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(service)
        .await;

    Ok(())
}
