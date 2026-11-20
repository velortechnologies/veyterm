mod files;
mod gateway;
mod incoming;
mod journal;
mod render;
use serde_json::Value;
use std::{io, sync::mpsc, time::Duration};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph},
};

const CYAN: Color = Color::Rgb(75, 230, 230);
const PINK: Color = Color::Rgb(240, 100, 190);
const MUTED: Color = Color::Rgb(125, 140, 160);
const BG: Color = Color::Rgb(13, 17, 26);

struct Session {
    name: String,
    key: String,
    model: String,
    thinking: String,
    reasoning: String,
    provider: String,
    context_used: Option<u64>,
    context_limit: Option<u64>,
}
struct App {
    journal: Option<journal::Journal>,
    snapshot: std::cell::RefCell<Vec<Value>>,
    had_connection: bool,
    observed_tools: std::cell::RefCell<Vec<Value>>,
    incoming: std::collections::VecDeque<incoming::File>,
    media_since: u64,
    media_refresh: Option<(std::time::Instant, String, u8)>,
    seen_files: std::collections::HashSet<String>,
    downloads: Vec<mpsc::Receiver<String>>,
    tls_insecure: bool,
    tls_pin: Option<String>,
    endpoint: String,
    sessions: Vec<Session>,
    search: String,
    selection: ListState,
    selected: Option<usize>,
    notice: String,
    connection: String,
    transport_notice: Option<String>,
    view_epoch: u64,
    request_serial: std::cell::Cell<u64>,
    history_serial: u64,
    usage: Value,
    startup_session: Option<String>,
    input: String,
    cursor: usize,
    catalog: Vec<Value>,
    completion: Option<(Vec<String>, usize, String)>,
    reasoning_pending: Option<String>,
    command_refresh: bool,
    models: Vec<Value>,
    modal_filter: String,
    modal: Option<(String, Vec<String>, ListState)>,
    chat: String,
    scroll: u16,
    page_height: u16,
    max_scroll: u16,
    follow: bool,
    working: bool,
    active_runs: std::collections::HashSet<String>,
    finished_runs: std::collections::HashSet<String>,
    live_run: Option<String>,
    live: String,
    tick: usize,
    commands: mpsc::Sender<gateway::Request>,
}
impl App {
    fn new(endpoint: String, commands: mpsc::Sender<gateway::Request>) -> Self {
        Self {
            endpoint,
            commands,
            input: String::new(),
            cursor: 0,
            catalog: Vec::new(),
            completion: None,
            reasoning_pending: None,
            command_refresh: false,
            models: Vec::new(),
            modal_filter: String::new(),
            modal: None,
            chat: String::new(),
            scroll: 0,
            page_height: 1,
            max_scroll: 0,
            follow: true,
            working: false,
            active_runs: Default::default(),
            finished_runs: Default::default(),
            live_run: None,
            live: String::new(),
            tick: 0,
            sessions: Vec::new(),
            incoming: Default::default(),
            media_since: u64::MAX,
            media_refresh: None,
            seen_files: Default::default(),
            downloads: Vec::new(),
            tls_insecure: false,
            tls_pin: None,
            connection: "DISCONNECTED".into(),
            transport_notice: None,
            view_epoch: 0,
            request_serial: std::cell::Cell::new(0),
            history_serial: 0,
            journal: None,
            snapshot: std::cell::RefCell::new(Vec::new()),
            had_connection: false,
            observed_tools: std::cell::RefCell::new(Vec::new()),
            usage: Value::Null,
            startup_session: None,
            search: String::new(),
            selection: ListState::default().with_selected(Some(0)),
            selected: None,
            notice: "Connecting to Gateway…".into(),
        }
    }
    fn observe_run(&mut self, payload: &Value, finished: bool) {
        if let Some(run_id) = payload["runId"].as_str() {
            if finished {
                self.active_runs.remove(run_id);
            } else {
                self.active_runs.insert(run_id.to_owned());
            }
            self.working = !self.active_runs.is_empty();
        } else {
            // Older Gateways/events may omit runId.
            if finished {
                self.active_runs.clear();
            }
            self.working = !finished;
        }
    }
    fn schedule_media_refresh(&mut self) {
        if let Some(i) = self.selected {
            self.media_refresh = Some((
                std::time::Instant::now() + Duration::from_secs(1),
                self.sessions[i].key.clone(),
                3,
            ));
        }
    }
    fn log(&self, kind: &str, data: Value) {
        if let Some(journal) = &self.journal {
            journal.record(kind, data);
        }
    }

    fn session_metadata(&self) -> Value {
        let session = self.selected.and_then(|i| self.sessions.get(i));
        let server = url::Url::parse(&self.endpoint)
            .ok()
            .and_then(|url| {
                Some(format!(
                    "{}:{}",
                    url.host_str()?,
                    url.port_or_known_default()?
                ))
            })
            .unwrap_or_else(|| self.endpoint.clone());
        serde_json::json!({
            "host": self.endpoint, "server": server,
            "name": session.map(|s| s.name.as_str()),
            "sessionKey": session.map(|s| s.key.as_str())
        })
    }

    fn log_session(&self) {
        self.snapshot.borrow_mut().clear();
        self.observed_tools.borrow_mut().clear();
        self.log("session", self.session_metadata());
    }

    fn message_record(&self, role: &str, text: &str) -> Value {
        journal::entry(
            if role == "user" && text.starts_with('/') {
                "command"
            } else {
                "message"
            },
            serde_json::json!({"role":role, "text":text}),
        )
    }

    fn log_message(&self, record: &Value) {
        self.snapshot.borrow_mut().push(record.clone());
        if let Some(journal) = &self.journal {
            journal.record_entry(record);
        }
    }

    fn history_records(&self, payload: &Value) {
        let mut records: Vec<_> = payload["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| matches!(m["role"].as_str(), Some("user" | "assistant")))
            .filter_map(|m| {
                let text = message_text(m);
                if text.is_empty() {
                    return None;
                }
                let mut record = self.message_record(m["role"].as_str().unwrap(), &text);
                if let Some(timestamp) = m.get("timestamp") {
                    record["timestamp"] = timestamp.clone();
                }
                Some(record)
            })
            .collect();
        // History is a display/snapshot source, never a journal event. A refresh
        // may contain the entire session, including messages predating this client.
        records.extend(self.observed_tools.borrow().iter().cloned());
        records.sort_by_key(|record| record["timestamp"].as_u64().unwrap_or_default());
        *self.snapshot.borrow_mut() = records;
    }

    fn request(&self, method: &str, params: Value) {
        let serial = self.request_serial.get() + 1;
        self.request_serial.set(serial);
        if method == "chat.send" {
            if let Some(text) = params["message"].as_str() {
                let record = self.message_record("user", text);
                self.log_message(&record);
            }
        }
        let _ = self.commands.send(gateway::Request {
            method: method.into(),
            params,
            epoch: self.view_epoch,
            serial,
        });
    }
    fn history(&self) {
        if let Some(i) = self.selected {
            self.request(
                "commands.list",
                serde_json::json!({"sessionKey":self.sessions[i].key,"includeArgs":true}),
            );
            self.request(
                "models.list",
                serde_json::json!({"sessionKey":self.sessions[i].key}),
            );
            self.request(
                "chat.history",
                serde_json::json!({"sessionKey":self.sessions[i].key,"limit":100}),
            );
        }
    }
    fn visible(&self) -> Vec<usize> {
        let query = self.search.to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                format!("{} {}", s.name, s.key)
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }
    fn move_selection(&mut self, delta: isize) {
        let count = self.visible().len() + 1; // Last item is always New session.
        let current = self.selection.selected().unwrap_or(0).min(count - 1);
        let next = current.saturating_add_signed(delta).min(count - 1);
        self.selection.select(Some(next));
    }
    fn pick(&mut self) {
        self.view_epoch += 1;
        let visible = self.visible();
        if let Some(&id) = visible.get(self.selection.selected().unwrap_or(0)) {
            self.media_since = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            self.media_refresh = None;
            self.selected = Some(id);
            self.log_session();
            self.finished_runs.clear();
            self.active_runs.clear();
            self.working = false;
            self.live.clear();
            self.live_run = None;
            self.chat.clear();
            self.history();
            self.scroll = 0;
            self.follow = true;
            self.notice.clear();
            self.request(
                "chat.history",
                serde_json::json!({"sessionKey":self.sessions[id].key,"limit":100}),
            );
        } else {
            self.request(
                "sessions.create",
                serde_json::json!({"displayName":"session"}),
            );
        }
    }
}
fn context_label(session: &Session, models: &[Value]) -> String {
    let limit = session.context_limit.or_else(|| {
        models
            .iter()
            .find(|m| {
                let id = m["id"].as_str().unwrap_or("");
                let provider = m["provider"].as_str().unwrap_or("");
                !id.is_empty()
                    && (session.model == format!("{provider}/{id}")
                        || (session.model == id
                            && (provider.is_empty() || provider == session.provider)))
            })
            .and_then(|m| {
                m["contextWindow"]
                    .as_u64()
                    .or_else(|| m["contextTokens"].as_u64())
            })
            .filter(|n| *n > 0)
    });
    fn k(n: Option<u64>) -> String {
        n.map(|n| {
            format!("{:.1}", n as f64 / 1000.0)
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
                + "k"
        })
        .unwrap_or_else(|| "?".into())
    }
    format!("{}/{}", k(session.context_used), k(limit))
}

fn field(value: &Value, names: &[&str]) -> String {
    names
        .iter()
        .find_map(|name| value[*name].as_str().filter(|s| !s.is_empty()))
        .unwrap_or("unknown")
        .to_string()
}
impl App {
    fn update(&mut self, update: gateway::Update) {
        match update {
            gateway::Update::Response(request, payload, ok) => {
                if let Some(key) = request.params["sessionKey"].as_str()
                    && !self.selected.is_some_and(|i| self.sessions[i].key == key)
                {
                    return;
                }
                if request.method == "chat.history" {
                    if request.epoch != self.view_epoch || request.serial < self.history_serial {
                        return;
                    }
                    self.history_serial = request.serial;
                }
                if request.method == "chat.send"
                    && let Some(local) = request.params["idempotencyKey"].as_str()
                {
                    if !ok {
                        self.active_runs.remove(local);
                        self.working = !self.active_runs.is_empty();
                    } else if let Some(actual) = payload["runId"].as_str()
                        && actual != local
                        && self.active_runs.remove(local)
                        && !self.finished_runs.contains(actual)
                    {
                        self.active_runs.insert(actual.into());
                    }
                    self.working = !self.active_runs.is_empty();
                }
                if !ok {
                    self.notice = format!("{}: {}", request.method, payload);
                    if request.method == "sessions.patch" {
                        self.reasoning_pending = None;
                    }
                    return;
                }
                self.update(gateway::Update::Reply(request.method, payload));
            }
            gateway::Update::RequestFailed(request) => {
                if request.method == "chat.send" {
                    if let Some(id) = request.params["idempotencyKey"].as_str() {
                        self.active_runs.remove(id);
                    }
                    self.working = !self.active_runs.is_empty();
                    if self
                        .selected
                        .is_some_and(|i| request.params["sessionKey"] == self.sessions[i].key)
                    {
                        self.notice =
                            "Message delivery unconfirmed; check history before resending".into();
                    }
                }
            }

            gateway::Update::Reply(method, payload) => {
                if method == "commands.list" {
                    self.catalog = payload["commands"].as_array().cloned().unwrap_or_default();
                }
                if method == "models.list" {
                    self.models = payload["models"].as_array().cloned().unwrap_or_default();
                    if self.modal.is_some()
                        && self
                            .modal
                            .as_ref()
                            .is_some_and(|(_, options, _)| options.len() <= 1)
                    {
                        let kind = self.modal.as_ref().unwrap().0.clone();
                        self.open_modal(&kind);
                    }
                }
                if method == "chat.send"
                    && (self.command_refresh || self.reasoning_pending.is_some())
                {
                    self.request("sessions.list", serde_json::json!({"limit":200,"includeDerivedTitles":true,"excludeSubagents":true}));
                }
                if method == "sessions.patch" {
                    if let Some(value) = self.reasoning_pending.take()
                        && let Some(i) = self.selected
                    {
                        self.sessions[i].reasoning = value;
                    }
                    self.notice = "Session settings updated".into();
                    self.request("sessions.list", serde_json::json!({"limit":200,"includeDerivedTitles":true,"excludeSubagents":true}));
                }
                if method == "error" {
                    self.reasoning_pending = None;
                    self.notice = payload.to_string();
                    self.active_runs.clear();
                    self.working = false;
                }
                if method == "sessions.create" {
                    let key = payload["key"]
                        .as_str()
                        .or(payload["session"]["key"].as_str());
                    if let Some(key) = key {
                        self.sessions.push(Session {
                            name: "session".into(),
                            key: key.into(),
                            model: "unknown".into(),
                            thinking: "unknown".into(),
                            reasoning: "unknown".into(),
                            provider: String::new(),
                            context_used: None,
                            context_limit: None,
                        });
                        self.selected = Some(self.sessions.len() - 1);
                        self.log_session();
                        self.notice.clear();
                        self.chat.clear();
                        self.input.clear();
                        self.cursor = 0;
                        self.history();
                    } else {
                        self.notice = "Created; refresh session list to select".into();
                    }
                }
                if method == "chat.history" {
                    if let Some(messages) = payload["messages"].as_array() {
                        for message in messages.iter().filter(|m| m["role"] == "assistant") {
                            let new_reply = message["timestamp"]
                                .as_u64()
                                .is_some_and(|t| t >= self.media_since);
                            let mut files = Vec::new();
                            incoming::collect(message, &mut files);
                            for file in files {
                                if new_reply && self.seen_files.insert(file.url.clone()) {
                                    self.incoming.push_back(file);
                                }
                            }
                        }
                    }

                    if let Some(i) = self.selected
                        && let Some(t) = payload["thinkingLevel"].as_str()
                    {
                        self.sessions[i].thinking = t.into();
                    }

                    // Delayed post-command/media refreshes can contain a snapshot
                    // from before the next chat.send. Keep the optimistic user turn
                    // and live tool output until the run finishes; final events
                    // schedule fresh history again. Media above is still collected.
                    if !self.working {
                        self.history_records(&payload);
                        self.chat = payload["messages"]
                            .as_array()
                            .map(|rows| {
                                rows.iter()
                                    .filter_map(|m| {
                                        let role = m["role"].as_str()?;
                                        if role != "user" && role != "assistant" {
                                            return None;
                                        }
                                        let text = message_text(m);
                                        if text.is_empty() {
                                            None
                                        } else {
                                            Some(format!("{role} › {text}"))
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n\n")
                            })
                            .unwrap_or_default();
                    }
                }
            }
            gateway::Update::Event(v) => {
                let p = &v["payload"];
                if p["runId"]
                    .as_str()
                    .is_some_and(|id| self.finished_runs.contains(id))
                {
                    return;
                }
                if self.selected.is_some_and(|i| {
                    p["sessionKey"].as_str() == Some(self.sessions[i].key.as_str())
                }) {
                    if v["event"] == "chat" || v["event"] == "agent" {
                        let mut files = Vec::new();
                        incoming::collect(p, &mut files);
                        for file in files {
                            if self.seen_files.insert(file.url.clone()) {
                                self.incoming.push_back(file);
                            }
                        }
                    }
                    if v["event"] == "chat" {
                        self.notice =
                            format!("Agent: {}", p["state"].as_str().unwrap_or("working"));
                        if p["state"] == "delta" {
                            self.live = message_text(&p["message"]);
                            self.live_run = p["runId"].as_str().map(str::to_owned);
                            self.observe_run(p, false);
                        } else if matches!(p["state"].as_str(), Some("final" | "aborted" | "error"))
                        {
                            self.view_epoch += 1;
                            if let Some(id) = p["runId"].as_str() {
                                self.finished_runs.insert(id.into());
                            }
                            self.observe_run(p, true);
                            self.command_refresh = false;
                            self.reasoning_pending = None;
                            self.request("sessions.list", serde_json::json!({"limit":200,"includeDerivedTitles":true,"excludeSubagents":true}));
                            let final_text = message_text(&p["message"]);
                            self.schedule_media_refresh();

                            let owns_live = self.live_run.as_deref() == p["runId"].as_str();
                            let text = if final_text.is_empty() && owns_live {
                                self.live_run = None;
                                std::mem::take(&mut self.live)
                            } else {
                                if owns_live {
                                    self.live.clear();
                                    self.live_run = None;
                                }
                                final_text
                            };
                            if !text.is_empty() {
                                self.log_message(&self.message_record("assistant", &text));
                                self.chat.push_str(&format!("\n\nassistant › {text}"));
                            }
                        } else if p["state"] == "status" {
                            self.observe_run(p, false);
                        }
                    }
                    if v["event"] == "agent" {
                        let data = &p["data"];
                        if p["stream"] == "lifecycle" {
                            match data["phase"].as_str() {
                                Some("start") => self.observe_run(p, false),
                                Some("end" | "error") => {
                                    // chat terminal owns run retirement. A lifecycle end
                                    // can precede its final and another run's start.
                                    self.working = !self.active_runs.is_empty();
                                    self.schedule_media_refresh();
                                }
                                _ => (),
                            }
                        }
                        if p["stream"] == "tool" {
                            self.observe_run(p, false);
                            if data["phase"] == "start" {
                                let record = journal::entry(
                                    "tool_call",
                                    serde_json::json!({
                                        "name": data["name"], "args": data["args"]
                                    }),
                                );
                                self.observed_tools.borrow_mut().push(record.clone());
                                self.snapshot.borrow_mut().push(record.clone());
                                if let Some(journal) = &self.journal {
                                    journal.record_entry(&record);
                                }
                                self.chat
                                    .push_str(&format!("\n\n⚙ {}", tool_call_label(data)));
                            }
                        }
                    }
                }
            }

            gateway::Update::Status(status) => {
                let restored =
                    status.starts_with("CONNECTED") && !self.connection.starts_with("CONNECTED");
                if status.starts_with("CONNECTED") {
                    if restored && self.had_connection {
                        self.log("connection", serde_json::json!({"status":"restored"}));
                    }
                    self.had_connection = true;
                }
                self.connection = status;
                if restored {
                    if self.transport_notice.as_deref() == Some(self.notice.as_str()) {
                        self.notice.clear();
                    }
                    self.transport_notice = None;
                    if self.selected.is_some() {
                        self.history();
                    }
                }
            }
            gateway::Update::Error(error) => {
                if self.connection.starts_with("CONNECTED") {
                    self.log("connection", serde_json::json!({"status":"disconnected"}));
                }
                self.connection = "DISCONNECTED".into();
                self.view_epoch += 1;
                self.live.clear();
                self.live_run = None;
                self.media_refresh = None;
                self.reasoning_pending = None;
                self.command_refresh = false;
                self.transport_notice = Some(error.clone());
                self.active_runs.clear();
                self.working = false;
                self.notice = error;
                self.usage = Value::Null;
            }
            gateway::Update::Usage(usage) => self.usage = usage,
            gateway::Update::Sessions(payload) => {
                let selected_key = self
                    .selected
                    .and_then(|i| self.sessions.get(i))
                    .map(|s| s.key.clone());
                if let Some(rows) = payload["sessions"].as_array() {
                    self.sessions = rows
                        .iter()
                        .filter(|r| r["key"].is_string())
                        .map(|r| {
                            let mut model = field(r, &["model", "modelOverride"]);
                            if model == "unknown" {
                                model = field(&payload["defaults"], &["model"]);
                            }
                            Session {
                                name: field(r, &["label", "displayName", "derivedTitle", "key"]),
                                key: field(r, &["key"]),
                                context_used: if r["totalTokensFresh"] == false
                                    || r["usageReport"]["state"] == "unavailable"
                                {
                                    None
                                } else {
                                    r["totalTokens"].as_u64()
                                },
                                context_limit: r["contextTokens"].as_u64().filter(|n| *n > 0),
                                model,
                                thinking: inherited(r, &payload["defaults"], "thinkingLevel"),
                                reasoning: inherited(r, &payload["defaults"], "reasoningLevel"),
                                provider: field(r, &["modelProvider", "providerOverride"]),
                            }
                        })
                        .collect();
                    self.selected = selected_key
                        .and_then(|key| self.sessions.iter().position(|s| s.key == key));
                    if let Some(target) = self.startup_session.take() {
                        match resolve_startup_session(rows, &target) {
                            Ok(key) => {
                                if let Some(index) = self.sessions.iter().position(|s| s.key == key)
                                {
                                    self.selection.select(Some(index));
                                    self.pick();
                                }
                            }
                            Err(error) => {
                                self.notice = error;
                                return;
                            }
                        }
                    }
                    let max = self.visible().len();
                    self.selection
                        .select(Some(self.selection.selected().unwrap_or(0).min(max)));
                    if self.selected.is_none() {
                        self.notice = format!("{} sessions loaded", self.sessions.len());
                    }
                } else {
                    self.notice = "Unexpected sessions.list response".into();
                }
            }
        }
    }
    fn quota(&self, provider: Option<&str>) -> String {
        let Some(providers) = self.usage["providers"].as_array() else {
            return "quota unavailable".into();
        };
        let Some(provider) = provider else {
            return "quota: select session".into();
        };
        for p in providers {
            if p["provider"].as_str() != Some(provider) {
                continue;
            }
            if let Some(windows) = p["windows"].as_array() {
                let labels = windows
                    .iter()
                    .filter_map(|w| {
                        let label = w["label"].as_str()?;
                        let used = w["usedPercent"].as_f64()?;
                        Some(format!(
                            "{} {:.0}%",
                            label,
                            (100.0 - used).clamp(0.0, 100.0)
                        ))
                    })
                    .collect::<Vec<_>>();
                if !labels.is_empty() {
                    return labels.join(" / ");
                }
            }
        }
        "quota unavailable".into()
    }
}
fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(Color::White)),
        area,
    );
    let rows = Layout::vertical([
        Constraint::Length(0),
        Constraint::Length(if app.selected.is_some() { 0 } else { 3 }),
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(if app.selected.is_none() {
            0
        } else {
            (app.input.lines().count() + usize::from(app.input.ends_with('\n'))).clamp(1, 10) as u16
                + 2
        }),
        Constraint::Length(1),
    ])
    .split(area);
    app.page_height = rows[2]
        .height
        .saturating_sub(if app.selected.is_some() { 1 } else { 2 })
        .max(1);
    let connection_title = Line::from(vec![
        Span::styled(
            format!(
                " {}{}",
                app.endpoint
                    .trim_start_matches("wss://")
                    .trim_end_matches("/"),
                app.connection
                    .strip_prefix("CONNECTED · Gateway ")
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default()
            ),
            Style::default().fg(MUTED),
        ),
        Span::styled(" [ ", Style::default().fg(Color::Reset)),
        Span::styled(
            if app.connection.starts_with("CONNECTED") {
                "LIVE"
            } else {
                "OFFLINE"
            },
            Style::default().fg(if app.connection.starts_with("CONNECTED") {
                Color::Green
            } else {
                MUTED
            }),
        ),
        Span::styled(" ]", Style::default().fg(Color::Reset)),
    ])
    .right_aligned();
    frame.render_widget(
        Paragraph::new(format!("⌕ {}▏", app.search))
            .style(Style::default().fg(CYAN))
            .block(
                Block::bordered()
                    .title(" SEARCH / type to filter ")
                    .border_style(Style::default().fg(MUTED)),
            ),
        rows[1],
    );
    if let Some(selected) = app.selected {
        let body = if app.live.is_empty() {
            app.chat.clone()
        } else {
            format!("{}\n\nassistant › {}", app.chat, app.live)
        };
        let body = render::tables(&body);
        let mut user = false;
        let mut command = false;
        let command_color = command_color();
        let mut styled: Vec<Line> = body
            .lines()
            .map(|line| {
                let text = if let Some(text) = line.strip_prefix("user › ") {
                    user = true;
                    command = false;
                    format!("> {text}")
                } else if let Some(text) = line.strip_prefix("assistant › ") {
                    user = false;
                    command = false;
                    text.to_owned()
                } else if line.starts_with("⚙ ") {
                    user = false;
                    command = true;
                    line.to_owned()
                } else {
                    line.to_owned()
                };
                render::bold(
                    text,
                    Style::default().fg(
                        if line.starts_with("📎 ") && line.ends_with(" · saved") {
                            Color::Green
                        } else if line.starts_with("📎 ") && line.ends_with(" · ignored") {
                            MUTED
                        } else if command {
                            command_color
                        } else if user {
                            MUTED
                        } else {
                            Color::White
                        },
                    ),
                )
            })
            .collect();
        styled.push(Line::raw(""));
        let paragraph = Paragraph::new(styled).wrap(ratatui::widgets::Wrap { trim: false });
        let lines = paragraph.line_count(rows[2].width);
        app.max_scroll = lines
            .saturating_sub(app.page_height as usize)
            .min(u16::MAX as usize) as u16;
        app.scroll = if app.follow {
            app.max_scroll
        } else {
            app.scroll.min(app.max_scroll)
        };
        frame.render_widget(
            paragraph.scroll((app.scroll, 0)).block(
                Block::default()
                    .borders(ratatui::widgets::Borders::BOTTOM)
                    .title_bottom(format!("{} ", app.sessions[selected].name))
                    .title_bottom(connection_title.clone())
                    .border_style(Style::default().fg(Color::Rgb(145, 117, 42))),
            ),
            rows[2],
        );
    } else {
        let mut items: Vec<ListItem> = app
            .visible()
            .iter()
            .map(|&i| {
                let s = &app.sessions[i];
                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {}  ", s.name)),
                    Span::styled(s.key.as_str(), Style::default().fg(MUTED)),
                ]))
            })
            .collect();
        items.push(ListItem::new(" ＋ New session"));
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(" SESSIONS ")
                    .title(connection_title.clone())
                    .border_style(Style::default().fg(CYAN)),
            )
            .highlight_symbol("▸ ")
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(29, 53, 65))
                    .fg(CYAN)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, rows[2], &mut app.selection);
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                if app.selected.is_some() {
                    " F2 model · F3 thinking · F4 reasoning · F8 stop · C-q quit"
                } else {
                    " ↑↓ select · PgUp/PgDn page · Home/End · Enter open · type search · C-q quit"
                },
                Style::default().fg(MUTED),
            ),
            Line::styled(
                if app.working {
                    format!(
                        " {} ━╸ Thinking / working",
                        ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][app.tick % 10]
                    )
                } else {
                    format!(" {}", app.notice)
                },
                Style::default().fg(PINK),
            ),
        ]),
        rows[3],
    );
    let before = &app.input[..app.cursor.min(app.input.len())];
    let cursor_row = before.bytes().filter(|b| *b == b'\n').count();
    let cursor_col = before.rsplit('\n').next().unwrap_or("").chars().count();
    let input_height = rows[4].height.saturating_sub(2).max(1) as usize;
    let offset = cursor_row.saturating_sub(input_height - 1);
    let horizontal = cursor_col.saturating_sub(rows[4].width.saturating_sub(4) as usize);
    frame.render_widget(
        Paragraph::new(app.input.as_str())
            .scroll((offset as u16, horizontal as u16))
            .style(Style::default().fg(Color::Reset))
            .block(
                Block::bordered()
                    .padding(ratatui::widgets::Padding::new(1, 0, 0, 0))
                    .title(" INPUT / Enter send · Shift+Enter newline "),
            ),
        rows[4],
    );
    if app.incoming.is_empty()
        && app.modal.is_none()
        && app.selected.is_some()
        && rows[4].width > 3
        && rows[4].height > 2
    {
        frame.set_cursor_position((
            rows[4].x + 2 + (cursor_col - horizontal) as u16,
            rows[4].y + 1 + (cursor_row - offset) as u16,
        ));
    }
    let selected = app.selected.and_then(|i| app.sessions.get(i));
    let details = selected.map_or("Choose a session".into(), |s| {
        format!(
            "{} │ {} │ think {} │ reas {}",
            s.model,
            context_label(s, &app.models),
            s.thinking,
            s.reasoning
        )
    });
    let quota = app.quota(selected.map(|s| s.provider.as_str()));
    frame.render_widget(
        Paragraph::new(vec![Line::raw(format!(" {} │ {}", details, quota))])
            .style(Style::default().bg(Color::Rgb(27, 38, 52)).fg(CYAN)),
        rows[5],
    );
    if let Some((kind, options, state)) = &mut app.modal {
        let width = area.width.saturating_sub(4).min(80);
        let height = area.height.saturating_sub(4).min(20);
        let popup = ratatui::layout::Rect::new(
            (area.width - width) / 2,
            (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(ratatui::widgets::Clear, popup);
        let query = app.modal_filter.to_lowercase();
        let items: Vec<ListItem> = options
            .iter()
            .filter(|v| v.to_lowercase().contains(&query))
            .map(|v| ListItem::new(v.as_str()))
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::bordered().title(format!(
                    " {kind} · filter: {} · Enter apply · Esc cancel ",
                    app.modal_filter
                )))
                .highlight_symbol("▸ ")
                .highlight_style(
                    Style::default()
                        .bg(Color::Rgb(29, 53, 65))
                        .fg(CYAN)
                        .add_modifier(Modifier::BOLD),
                )
                .style(Style::default().bg(BG)),
            popup,
            state,
        );
    }
    if let Some(file) = app.incoming.front() {
        let width = area.width.saturating_sub(4).min(90);
        let height = area.height.min(5);
        let popup = ratatui::layout::Rect::new(
            (area.width - width) / 2,
            (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(ratatui::widgets::Clear, popup);
        frame.render_widget(
            Paragraph::new(file.name.as_str())
                .wrap(ratatui::widgets::Wrap { trim: false })
                .block(
                    Block::bordered()
                        .title(" Incoming file ")
                        .title_bottom(Line::from(" Enter - save · Esc - ignore ").centered())
                        .padding(ratatui::widgets::Padding::new(1, 1, 1, 0))
                        .border_style(Style::default().fg(CYAN)),
                )
                .style(Style::default().bg(BG).fg(Color::Reset)),
            popup,
        );
    }
}
fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: mpsc::Receiver<gateway::Update>,
) -> io::Result<()> {
    let mut dirty = true;
    let mut modal_was_open = false;
    let mut escape_prefix = false;
    loop {
        if let Some(error) = app.journal.as_ref().and_then(journal::Journal::take_error) {
            app.notice = error;
            dirty = true;
        }
        app.downloads.retain(|rx| match rx.try_recv() {
            Ok(status) => {
                app.chat.push_str(&format!("\n\n{status}"));
                dirty = true;
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        if app
            .media_refresh
            .as_ref()
            .is_some_and(|(at, _, _)| std::time::Instant::now() >= *at)
        {
            let (_, key, remaining) = app.media_refresh.take().unwrap();
            if app.selected.is_some_and(|i| app.sessions[i].key == key) {
                app.request(
                    "chat.history",
                    serde_json::json!({"sessionKey":key,"limit":200}),
                );
                if remaining > 1 {
                    app.media_refresh = Some((
                        std::time::Instant::now() + Duration::from_secs(1),
                        key,
                        remaining - 1,
                    ));
                }
            }
        }
        for update in rx.try_iter() {
            app.update(update);
            dirty = true;
        }
        if app.working {
            app.tick = app.tick.wrapping_add(1);
            dirty = true;
        }
        if dirty {
            if modal_was_open != app.modal.is_some() {
                terminal.clear()?;
                modal_was_open = app.modal.is_some();
            }
            terminal.draw(|f| draw(f, app))?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        dirty = true;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return Ok(());
        }
        if !app.incoming.is_empty() {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                let file = app.incoming.pop_front().unwrap();
                if key.code == KeyCode::Esc {
                    app.chat
                        .push_str(&format!("\n\n📎 {} · ignored", file.name));
                } else {
                    app.chat.push_str(&format!("\n\n📎 {} · saving", file.name));
                    let (tx, rx) = mpsc::channel();
                    app.downloads.push(rx);
                    let endpoint = app.endpoint.clone();
                    let insecure = app.tls_insecure;
                    let pin = app.tls_pin.clone();
                    std::thread::spawn(move || {
                        let status =
                            match incoming::save(&file, &endpoint, insecure, pin.as_deref()) {
                                Ok(path) => format!("📎 {path} · saved"),
                                Err(error) => format!("📎 {} · failed: {error}", file.name),
                            };
                        let _ = tx.send(status);
                    });
                }
                terminal.clear()?;
            }
            continue;
        }
        let escaped = std::mem::take(&mut escape_prefix);
        if app.modal.is_none() && app.selected.is_some() {
            if key.code == KeyCode::Esc {
                escape_prefix = true;
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let word = (key.code == KeyCode::Backspace
                && (escaped
                    || key.modifiers.intersects(
                        KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
                    )))
                || (ctrl && key.code == KeyCode::Char('w'));
            if word || (ctrl && key.code == KeyCode::Char('u')) {
                app.completion = None;
                erase_input(&mut app.input, &mut app.cursor, word);
                continue;
            }
        }
        if app.modal.is_none()
            && app.selected.is_some()
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
        {
            app.complete(key.code == KeyCode::BackTab);
            continue;
        }
        app.completion = None;
        if let Some((kind, options, state)) = &mut app.modal {
            let query = app.modal_filter.to_lowercase();
            let visible: Vec<usize> = options
                .iter()
                .enumerate()
                .filter(|(_, v)| v.to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect();
            let count = visible.len();
            let selected = state.selected().unwrap_or(0);
            match key.code {
                KeyCode::Esc => app.modal = None,
                KeyCode::Up => state.select(Some(selected.saturating_sub(1))),
                KeyCode::Down => state.select(Some((selected + 1).min(count.saturating_sub(1)))),
                KeyCode::Home => state.select(Some(0)),
                KeyCode::End => state.select(Some(count.saturating_sub(1))),
                KeyCode::Enter if count > 0 => {
                    let kind = kind.clone();
                    let choice = options[visible[selected.min(count - 1)]].clone();
                    let command = if kind == "model" { "/model" } else { "/think" };
                    app.send_chat(format!("{command} {choice}"));
                    app.modal = None;
                }
                KeyCode::Backspace => {
                    app.modal_filter.pop();
                    state.select(Some(0));
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    app.modal_filter.push(c);
                    state.select(Some(0));
                }
                _ => (),
            }
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Up => {
                    app.follow = false;
                    app.scroll = app.scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    app.scroll = app.scroll.saturating_add(1).min(app.max_scroll);
                    app.follow = app.scroll == app.max_scroll;
                }
                KeyCode::Left => app.word_move(false),
                KeyCode::Right => app.word_move(true),
                KeyCode::Char('j') => {
                    if app.selected.is_some() {
                        app.input.insert(app.cursor, '\n');
                        app.cursor += 1;
                    }
                }
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('c') => {
                    if app.input.is_empty()
                        && let Some(i) = app.selected
                    {
                        app.request(
                            "chat.abort",
                            serde_json::json!({"sessionKey":app.sessions[i].key}),
                        );
                    }
                    app.input.clear();
                    app.cursor = 0;
                }
                _ => (),
            }
            continue;
        }
        match key.code {
            KeyCode::F(4) if app.selected.is_some() && app.reasoning_pending.is_none() => {
                let i = app.selected.unwrap();
                let next = match app.sessions[i].reasoning.as_str() {
                    "on" => "stream",
                    "stream" => "off",
                    _ => "on",
                };
                app.request(
                    "chat.send",
                    serde_json::json!({"sessionKey":app.sessions[i].key,"message":format!("/reasoning {next}"),"idempotencyKey":format!("veyterm-reasoning-{}",std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos())}),
                );
                app.reasoning_pending = Some(next.into());
            }
            KeyCode::F(n @ (2 | 3)) if app.selected.is_some() => {
                let kind = if n == 2 { "model" } else { "thinkingLevel" };
                app.modal_filter.clear();
                app.open_modal(kind);
                app.request(
                    "models.list",
                    serde_json::json!({"sessionKey":app.sessions[app.selected.unwrap()].key}),
                );
            }
            KeyCode::F(8) => {
                if let Some(i) = app.selected {
                    app.request(
                        "chat.abort",
                        serde_json::json!({"sessionKey":app.sessions[i].key}),
                    );
                }
            }
            KeyCode::Esc => {
                if app.selected.is_none() {
                    app.search.clear();
                    app.selection.select(Some(0));
                }
            }
            KeyCode::Char('b') | KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                app.word_move(false)
            }
            KeyCode::Char('f') | KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                app.word_move(true)
            }
            _ if app.selected.is_some() => match key.code {
                KeyCode::PageUp => {
                    app.follow = false;
                    app.scroll = app.scroll.saturating_sub(app.page_height);
                }
                KeyCode::PageDown => {
                    app.scroll = app
                        .scroll
                        .saturating_add(app.page_height)
                        .min(app.max_scroll);
                    app.follow = app.scroll == app.max_scroll;
                }
                KeyCode::Home => {
                    app.cursor = app.input[..app.cursor].rfind('\n').map_or(0, |i| i + 1)
                }
                KeyCode::End => {
                    app.cursor = app.input[app.cursor..]
                        .find('\n')
                        .map_or(app.input.len(), |i| app.cursor + i)
                }
                KeyCode::Left => {
                    if let Some((i, _)) = app.input[..app.cursor].char_indices().next_back() {
                        app.cursor = i;
                    }
                }
                KeyCode::Right => {
                    if let Some(c) = app.input[app.cursor..].chars().next() {
                        app.cursor += c.len_utf8();
                    }
                }
                KeyCode::Up => app.move_cursor_vertical(-1),
                KeyCode::Down => app.move_cursor_vertical(1),
                KeyCode::Backspace => {
                    if let Some((i, _)) = app.input[..app.cursor].char_indices().next_back() {
                        app.input.drain(i..app.cursor);
                        app.cursor = i;
                    }
                }
                KeyCode::Delete => {
                    if let Some(c) = app.input[app.cursor..].chars().next() {
                        app.input.drain(app.cursor..app.cursor + c.len_utf8());
                    }
                }
                KeyCode::Char(c) => {
                    app.input.insert(app.cursor, c);
                    app.cursor += c.len_utf8();
                }
                KeyCode::Enter
                    if key.modifiers.contains(KeyModifiers::SHIFT)
                        || key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    app.input.insert(app.cursor, '\n');
                    app.cursor += 1;
                }
                KeyCode::Enter if !app.input.trim().is_empty() => {
                    let message = std::mem::take(&mut app.input);
                    app.cursor = 0;
                    app.send_chat(message);
                }
                _ => (),
            },
            KeyCode::PageUp => app.move_selection(-(app.page_height as isize)),
            KeyCode::PageDown => app.move_selection(app.page_height as isize),
            KeyCode::Home => app.selection.select(Some(0)),
            KeyCode::End => app.selection.select(Some(app.visible().len())),
            KeyCode::Up => app.move_selection(-1),
            KeyCode::Down => app.move_selection(1),
            KeyCode::Enter => app.pick(),
            KeyCode::Backspace => {
                app.search.pop();
                app.selection.select(Some(0));
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                app.search.push(c);
                app.selection.select(Some(0));
            }
            _ => (),
        }
    }
}
#[derive(clap::Parser)]
#[command(version, about = "OpenClaw terminal client")]
struct Cli {
    /// Gateway host:port or wss:// URL
    endpoint: String,
    /// Open a session by exact name, session ID, or key
    #[arg(short = 's', long = "session", value_name = "NAME_OR_ID")]
    session: Option<String>,
    /// Append session activity as NDJSON (without binary attachments)
    #[arg(short = 'l', long, value_name = "FILENAME")]
    log: Option<std::path::PathBuf>,
    /// SHA-256 fingerprint of the DER leaf certificate
    #[arg(long, value_name = "HEX", conflicts_with = "tls_insecure")]
    tls_sha256: Option<String>,
    /// Disable TLS certificate verification (self-signed gateways)
    #[arg(long)]
    tls_insecure: bool,
}
fn main() -> io::Result<()> {
    use clap::Parser;
    let cli = Cli::parse();
    let address = cli.endpoint;
    let endpoint = if address.contains("://") {
        address
    } else {
        format!("wss://{address}")
    };
    let pin = cli.tls_sha256;
    let (commands, command_rx) = mpsc::channel();
    let mut app = App::new(endpoint.clone(), commands);
    app.journal = cli
        .log
        .as_ref()
        .map(|path| journal::Journal::open(path, &endpoint, cli.session.as_deref()))
        .transpose()?;
    app.startup_session = cli.session;
    app.tls_insecure = cli.tls_insecure;
    app.tls_pin = pin.clone();
    let (tx, rx) = mpsc::channel();
    gateway::start(endpoint, pin, cli.tls_insecure, tx, command_rx);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(
        io::stdout(),
        event::PushKeyboardEnhancementFlags(
            event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
        )
    );
    let result = run(&mut terminal, &mut app, rx);
    let _ = crossterm::execute!(io::stdout(), event::PopKeyboardEnhancementFlags);
    ratatui::restore();
    result
}

fn message_text(m: &Value) -> String {
    if let Some(s) = m["content"].as_str() {
        return s.into();
    }
    m["content"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|v| v["type"] == "text")
                .filter_map(|v| v["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn inherited(row: &Value, defaults: &Value, name: &str) -> String {
    row[name]
        .as_str()
        .or(defaults[name].as_str())
        .unwrap_or("default")
        .into()
}

impl App {
    fn move_cursor_vertical(&mut self, direction: isize) {
        let start = self.input[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let column = self.input[start..self.cursor].chars().count();
        let target = if direction < 0 {
            if start == 0 {
                return;
            }
            let end = start - 1;
            (self.input[..end].rfind('\n').map_or(0, |i| i + 1), end)
        } else {
            let Some(end) = self.input[self.cursor..]
                .find('\n')
                .map(|i| self.cursor + i)
            else {
                return;
            };
            let start = end + 1;
            (
                start,
                self.input[start..]
                    .find('\n')
                    .map_or(self.input.len(), |i| start + i),
            )
        };
        self.cursor = self.input[target.0..target.1]
            .char_indices()
            .nth(column)
            .map_or(target.1, |(i, _)| target.0 + i);
    }
}

impl App {
    fn word_move(&mut self, right: bool) {
        if self.selected.is_none() {
            return;
        }
        if right {
            while let Some(c) = self.input[self.cursor..].chars().next() {
                if c.is_whitespace() {
                    break;
                }
                self.cursor += c.len_utf8();
            }
            while let Some(c) = self.input[self.cursor..].chars().next() {
                if !c.is_whitespace() {
                    break;
                }
                self.cursor += c.len_utf8();
            }
        } else {
            while let Some(c) = self.input[..self.cursor].chars().next_back() {
                if !c.is_whitespace() {
                    break;
                }
                self.cursor -= c.len_utf8();
            }
            while let Some(c) = self.input[..self.cursor].chars().next_back() {
                if c.is_whitespace() {
                    break;
                }
                self.cursor -= c.len_utf8();
            }
        }
    }
}

fn command_color() -> Color {
    let colorterm = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(colorterm.as_str(), "truecolor" | "24bit") || term.ends_with("-direct") {
        Color::Rgb(120, 148, 176)
    } else {
        Color::Blue
    }
}

impl App {
    fn open_modal(&mut self, kind: &str) {
        let Some(i) = self.selected else {
            return;
        };
        let current = if kind == "model" {
            &self.sessions[i].model
        } else {
            &self.sessions[i].thinking
        };
        let mut options = vec!["default".to_owned()];
        if kind == "model" {
            options.extend(self.models.iter().filter_map(|m| {
                let id = m["id"].as_str()?;
                let provider = m["provider"].as_str().unwrap_or("");
                Some(
                    if provider.is_empty() || id.starts_with(&format!("{provider}/")) {
                        id.into()
                    } else {
                        format!("{provider}/{id}")
                    },
                )
            }));
        } else if let Some(m) = self.models.iter().find(|m| {
            m["id"].as_str().is_some_and(|id| {
                id == self.sessions[i].model || self.sessions[i].model.ends_with(&format!("/{id}"))
            })
        }) && let Some(levels) = m["thinkingLevels"].as_array()
        {
            options.extend(
                levels
                    .iter()
                    .filter_map(|v| v.as_str().or_else(|| v["id"].as_str()).map(str::to_owned)),
            );
        }
        options.sort_by_cached_key(|value| value.to_lowercase());
        options.dedup();
        let selected = options
            .iter()
            .position(|v| v == current || v.ends_with(&format!("/{current}")))
            .unwrap_or(0);
        let selected = if self.modal_filter.is_empty() {
            selected
        } else {
            0
        };
        self.modal = Some((
            kind.into(),
            options,
            ListState::default().with_selected(Some(selected)),
        ));
    }
}

impl App {
    fn complete(&mut self, reverse: bool) {
        if !self.input.starts_with('/') {
            return;
        }
        // A completed directory starts a new completion scope on the next Tab.
        if self.input.starts_with("/attach ") && self.input[..self.cursor].ends_with('/') {
            self.completion = None;
        }
        if let Some((choices, index, suffix)) = &mut self.completion {
            *index = if reverse {
                (*index + choices.len() - 1) % choices.len()
            } else {
                (*index + 1) % choices.len()
            };
            self.input = format!("{}{}", choices[*index], suffix);
            self.cursor = choices[*index].len();
            return;
        }
        let prefix = &self.input[..self.cursor];
        let mut choices = Vec::new();
        if let Some(path) = prefix.strip_prefix("/attach ") {
            choices = files::complete(path);
        } else if !prefix.contains(char::is_whitespace) {
            choices.push("/attach".into());
            choices.push("/save".into());
            for c in &self.catalog {
                if let Some(name) = c["name"].as_str() {
                    choices.push(format!("/{}", name.trim_start_matches('/')));
                }
                if let Some(aliases) = c["textAliases"].as_array() {
                    choices.extend(aliases.iter().filter_map(|a| a.as_str().map(str::to_owned)));
                }
            }
        } else {
            let start = prefix.rfind(char::is_whitespace).unwrap() + 1;
            let head = &prefix[..start];
            let words: Vec<_> = prefix.split_whitespace().collect();
            let command = words.first().copied().unwrap_or("");
            let arg_index = head.split_whitespace().count().saturating_sub(1);
            if command == "/model" && arg_index == 0 {
                for m in &self.models {
                    if let (Some(provider), Some(id)) = (m["provider"].as_str(), m["id"].as_str()) {
                        choices.push(format!("{head}{provider}/{id}"));
                    }
                }
            }
            for c in &self.catalog {
                let matches = c["name"]
                    .as_str()
                    .is_some_and(|n| n.trim_start_matches('/') == command.trim_start_matches('/'))
                    || c["textAliases"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|v| v == command));
                if matches && let Some(values) = c["args"][arg_index]["choices"].as_array() {
                    for v in values {
                        if let Some(value) = v.as_str().or(v["value"].as_str()) {
                            choices.push(format!("{head}{value}"));
                        }
                    }
                }
            }
        }
        choices.retain(|v| v.starts_with(prefix));
        choices.sort();
        choices.dedup();
        if choices.is_empty() {
            return;
        }
        let suffix = self.input[self.cursor..].to_owned();
        if !suffix.starts_with(char::is_whitespace) {
            for choice in &mut choices {
                if !choice.ends_with('/') {
                    choice.push(' ');
                }
            }
        }
        let index = if reverse { choices.len() - 1 } else { 0 };
        self.input = format!("{}{}", choices[index], suffix);
        self.cursor = choices[index].len();
        self.completion = Some((choices, index, suffix));
    }
}

impl App {
    fn send_chat(&mut self, message: String) {
        if message.trim() == "/save" || message.starts_with("/save ") {
            let filename = message.strip_prefix("/save").unwrap().trim();
            if filename.is_empty() {
                self.notice = "Usage: /save FILENAME".into();
                return;
            }
            self.log(
                "command",
                serde_json::json!({"role":"user", "text":message}),
            );
            let mut records = vec![journal::entry("session", self.session_metadata())];
            records.extend(self.snapshot.borrow().iter().cloned());
            if !self.live.is_empty() {
                records.push(self.message_record("assistant", &self.live));
            }
            let path = files::expand(filename);
            match journal::save(&path, &records) {
                Ok(()) => {
                    self.notice = format!("History snapshot saved to {filename}");
                }
                Err(error) => self.notice = format!("History snapshot save failed: {error}"),
            }
            return;
        }
        if !self.connection.starts_with("CONNECTED") {
            self.notice = "Offline; message kept in input".into();
            self.input = message;
            self.cursor = self.input.len();
            return;
        }

        let attachment_label = message.strip_prefix("/attach ").map(|raw| {
            let (path, _) = files::split_attachment(raw);
            format!("📎 {path}")
        });
        let (message, attachments) = if let Some(path) = message.strip_prefix("/attach ") {
            let (path, caption) = files::split_attachment(path);
            match files::load(&path) {
                Ok(file) => (caption.to_owned(), vec![file]),
                Err(e) => {
                    self.notice = format!("Attachment: {e}");
                    self.input = message;
                    self.cursor = self.input.len();
                    return;
                }
            }
        } else {
            (message, Vec::new())
        };

        self.view_epoch += 1;
        self.command_refresh |= message.starts_with('/');
        let Some(i) = self.selected else {
            return;
        };
        let nonce = format!(
            "veyterm-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        self.active_runs.insert(nonce.clone());
        // Terminal followups should steer the running turn, not inherit a
        // channel's interrupt policy and cancel context preparation repeatedly.
        self.request("chat.send", serde_json::json!({"sessionKey":self.sessions[i].key,"message":message,"idempotencyKey":nonce,"attachments":attachments,"queueMode":"steer"}));
        let display = match attachment_label {
            Some(label) if message.is_empty() => label,
            Some(label) => format!("{label}\n{message}"),
            None => message.clone(),
        };
        self.chat.push_str(&format!("\n\nuser › {display}"));
        self.notice = "Sending…".into();
        self.follow = true;
        if !self.working {
            self.live.clear();
            self.live_run = None;
        }
        self.working = true;
    }
}

fn erase_input(input: &mut String, cursor: &mut usize, word: bool) {
    let end = *cursor;
    let mut start = end;
    if word {
        while start > 0 {
            let c = input[..start].chars().next_back().unwrap();
            if !c.is_whitespace() {
                break;
            }
            start -= c.len_utf8();
        }
        while start > 0 {
            let c = input[..start].chars().next_back().unwrap();
            if c.is_whitespace() {
                break;
            }
            start -= c.len_utf8();
        }
    } else {
        start = input[..end].rfind('\n').map_or(0, |i| i + 1);
    }
    input.drain(start..end);
    *cursor = start;
}
#[cfg(test)]
mod editing_tests {
    use super::*;
    #[test]
    fn terminal_deletion() {
        let mut s = "одне два  suffix".to_owned();
        let mut c = "одне два  ".len();
        erase_input(&mut s, &mut c, true);
        assert_eq!(s, "одне suffix");
        assert_eq!(c, "одне ".len());
        s = "first\nдругий рядок suffix".into();
        c = "first\nдругий рядок".len();
        erase_input(&mut s, &mut c, false);
        assert_eq!(s, "first\n suffix");
        assert_eq!(c, 6);
        c = 0;
        erase_input(&mut s, &mut c, true);
        assert_eq!(c, 0);
    }
}

fn resolve_startup_session(rows: &[Value], target: &str) -> Result<String, String> {
    let ids: Vec<_> = rows
        .iter()
        .filter(|r| r["key"].as_str() == Some(target) || r["sessionId"].as_str() == Some(target))
        .collect();
    let matches = if ids.is_empty() {
        rows.iter()
            .filter(|r| field(r, &["label", "displayName", "derivedTitle", "key"]) == target)
            .collect::<Vec<_>>()
    } else {
        ids
    };
    match matches.as_slice() {
        [row] => row["key"]
            .as_str()
            .map(str::to_owned)
            .ok_or("Session has no key".into()),
        [] => Err(format!("Session not found: {target}")),
        _ => Err(format!(
            "Ambiguous session name or ID: {target}; use a unique session ID"
        )),
    }
}
#[cfg(test)]
mod session_arg_tests {
    use super::*;
    #[test]
    fn exact_resolution_and_duplicates() {
        let rows = vec![
            serde_json::json!({"key":"k1","sessionId":"id1","label":"x2"}),
            serde_json::json!({"key":"k2","sessionId":"id2","label":"x2"}),
        ];
        assert!(resolve_startup_session(&rows, "x2").is_err());
        assert_eq!(resolve_startup_session(&rows, "id2").unwrap(), "k2");
        assert_eq!(resolve_startup_session(&rows[..1], "x2").unwrap(), "k1");
        assert!(resolve_startup_session(&rows, "x").is_err());
    }
    #[test]
    fn session_alias() {
        use clap::Parser;
        assert_eq!(
            Cli::try_parse_from(["veyterm", "host:123", "-s", "x2"])
                .unwrap()
                .session
                .as_deref(),
            Some("x2")
        );
        assert!(Cli::try_parse_from(["veyterm", "--demo"]).is_err());
    }
}

fn tool_call_label(data: &Value) -> String {
    let name = data["name"].as_str().unwrap_or("tool");
    let args = &data["args"];
    if args.is_null() {
        return name.to_owned();
    }
    let formatted = serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string());
    format!("{name}\n{formatted}")
}

#[cfg(test)]
mod tool_call_tests {
    use super::*;

    #[test]
    fn displays_nested_mcp_arguments_and_shell_options() {
        let data = serde_json::json!({"name":"tb_primary.read", "args":{"path":"/notes", "options":{"limit":3}}});
        let label = tool_call_label(&data);
        assert!(label.starts_with("tb_primary.read\n"));
        let parsed: Value = serde_json::from_str(label.split_once('\n').unwrap().1).unwrap();
        assert_eq!(parsed, data["args"]);
        let shell = serde_json::json!({"name":"exec", "args":{"command":"pwd", "workdir":"/tmp"}});
        assert!(tool_call_label(&shell).contains("workdir"));
        assert_eq!(tool_call_label(&serde_json::json!({"name":"read"})), "read");
    }
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_updates_with_session_model_and_preserves_unknown() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new("test".into(), tx);
        app.update(gateway::Update::Sessions(json!({"sessions":[{
            "key":"test", "model":"a", "totalTokens":42300,
            "totalTokensFresh":true, "contextTokens":200000
        }]})));
        assert_eq!(context_label(&app.sessions[0], &[]), "42.3k/200k");
        app.update(gateway::Update::Sessions(json!({"sessions":[{
            "key":"test", "model":"b", "totalTokens":42300,
            "totalTokensFresh":false, "contextTokens":1000000
        }]})));
        assert_eq!(context_label(&app.sessions[0], &[]), "?/1000k");
        app.update(gateway::Update::Sessions(
            json!({"sessions":[{"key":"test"}]}),
        ));
        assert_eq!(context_label(&app.sessions[0], &[]), "?/?");
    }

    #[test]
    fn catalog_fallback_matches_provider_and_prefers_session_limit() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new("test".into(), tx);
        app.update(gateway::Update::Sessions(json!({"sessions":[{
            "key":"test", "model":"p/m", "totalTokens":0
        }]})));
        let models = vec![
            json!({"provider":"other", "id":"m", "contextWindow":1000}),
            json!({"provider":"p", "id":"m", "contextWindow":128000}),
        ];
        assert_eq!(context_label(&app.sessions[0], &models), "0k/128k");
        app.sessions[0].context_limit = Some(64000);
        assert_eq!(context_label(&app.sessions[0], &models), "0k/64k");
    }
}

#[cfg(test)]
mod history_refresh_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn late_command_history_does_not_hide_next_user_message() {
        let (tx, _rx) = mpsc::channel();
        let mut app = App::new(String::new(), tx);
        app.connection = "CONNECTED".into();
        app.update(gateway::Update::Sessions(
            json!({"sessions":[{"key":"test"}]}),
        ));
        app.selected = Some(0);
        app.send_chat("/model sol".into());
        app.update(gateway::Update::Event(json!({
            "event":"chat", "payload":{"sessionKey":"test", "state":"final",
            "message":{"content":"Model switched"}}
        })));
        // A command schedules delayed history refreshes. The next user turn
        // can be sent before one of those older snapshots arrives.
        app.send_chat("Наступне повідомлення".into());
        let visible = app.chat.clone();
        let history = json!({"messages":[
            {"role":"user","content":"/model sol"},
            {"role":"assistant","content":"Model switched"}
        ]});
        app.update(gateway::Update::Reply("chat.history".into(), history));
        assert_eq!(app.chat, visible);
        app.update(gateway::Update::Event(json!({
            "event":"chat", "payload":{"sessionKey":"test", "state":"delta",
            "message":{"content":"Відповідаю"}}
        })));
        assert!(app.chat.contains("user › Наступне повідомлення"));
        app.update(gateway::Update::Event(json!({
            "event":"chat", "payload":{"sessionKey":"test", "state":"final",
            "message":{"content":"Готово"}}
        })));
        let history = json!({"messages":[
            {"role":"user","content":"/model sol"},
            {"role":"assistant","content":"Model switched"},
            {"role":"user","content":"Наступне повідомлення"},
            {"role":"assistant","content":"Готово"}
        ]});
        app.update(gateway::Update::Reply("chat.history".into(), history));
        assert_eq!(app.chat.matches("user › Наступне повідомлення").count(), 1);
        assert!(app.chat.ends_with("assistant › Готово"));
    }
}

#[cfg(test)]
mod overlapping_run_tests {
    use super::*;
    use serde_json::json;

    fn connected() -> (App, mpsc::Receiver<gateway::Request>) {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(String::new(), tx);
        app.connection = "CONNECTED".into();
        app.update(gateway::Update::Sessions(
            json!({"sessions":[{"key":"test"}]}),
        ));
        app.selected = Some(0);
        (app, rx)
    }
    fn send(app: &mut App, rx: &mpsc::Receiver<gateway::Request>, message: &str) -> String {
        app.send_chat(message.into());
        let request = rx.try_recv().unwrap();
        let (method, params) = (request.method, request.params);
        assert_eq!(method, "chat.send");
        assert_eq!(params["queueMode"], "steer");
        params["idempotencyKey"].as_str().unwrap().to_owned()
    }
    fn event(app: &mut App, run: &str, state: &str, text: &str) {
        app.update(gateway::Update::Event(json!({"event":"chat","payload":{
            "sessionKey":"test", "runId":run,"state":state,"message":{"content":text}
        }})));
    }
    #[test]
    fn old_terminal_cannot_hide_pending_followup_or_replace_its_live_text() {
        let (mut app, rx) = connected();
        let first = send(&mut app, &rx, "first");
        let second = send(&mut app, &rx, "second");
        assert_ne!(first, second);
        event(&mut app, &second, "delta", "new live response");
        event(&mut app, &first, "aborted", "");
        assert!(app.working);
        assert_eq!(app.live, "new live response");
        let chat = app.chat.clone();
        app.update(gateway::Update::Reply(
            "chat.history".into(),
            json!({"messages":[]}),
        ));
        assert_eq!(app.chat, chat);
        event(&mut app, &second, "final", "new response");
        assert!(!app.working);
        assert!(app.live.is_empty());
        assert!(app.chat.ends_with("assistant › new response"));
    }
    #[test]
    fn steering_receipt_does_not_finish_original_run() {
        let (mut app, rx) = connected();
        let original = send(&mut app, &rx, "first");
        event(&mut app, &original, "delta", "still working");
        let steered = send(&mut app, &rx, "addition");
        assert_eq!(app.live, "still working");
        event(&mut app, &steered, "final", "");
        assert!(app.working);
        assert_eq!(app.live, "still working");
        app.update(gateway::Update::Event(json!({"event":"agent","payload":{
            "sessionKey":"test","runId":steered,"stream":"lifecycle","data":{"phase":"end"}
        }})));
        assert!(app.working);
        event(&mut app, &original, "final", "all done");
        assert!(!app.working);
    }
}

#[cfg(test)]
mod reconnect_tests {
    use super::*;
    use serde_json::json;
    fn app() -> (App, mpsc::Receiver<gateway::Request>) {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(String::new(), tx);
        app.connection = "CONNECTED".into();
        app.update(gateway::Update::Sessions(
            json!({"sessions":[{"key":"one"},{"key":"two"}]}),
        ));
        app.selected = Some(0);
        (app, rx)
    }
    fn history(app: &App, serial: u64) -> gateway::Request {
        gateway::Request {
            method: "chat.history".into(),
            params: json!({"sessionKey":"one"}),
            epoch: app.view_epoch,
            serial,
        }
    }
    fn snapshot(text: &str) -> Value {
        json!({"messages":[{"role":"user","content":text}]})
    }
    #[test]
    fn heartbeat_notice_and_stale_live_clear_on_recovery() {
        let (mut a, rx) = app();
        a.live = "old partial answer".into();
        a.live_run = Some("old".into());
        a.update(gateway::Update::Error("Gateway heartbeat timed out".into()));
        a.update(gateway::Update::Status("CONNECTING".into()));
        a.update(gateway::Update::Status("CONNECTED · Gateway test".into()));
        assert!(a.notice.is_empty());
        assert!(a.live.is_empty());
        assert!(a.live_run.is_none());
        assert!(
            rx.try_iter()
                .any(|r| r.method == "chat.history" && r.epoch == a.view_epoch)
        );
    }
    #[test]
    fn history_cannot_cross_disconnect_send_or_session_boundaries() {
        let (mut a, rx) = app();
        let old = history(&a, 1);
        a.update(gateway::Update::Error("timeout".into()));
        a.update(gateway::Update::Status("CONNECTED".into()));
        a.chat = "current".into();
        a.update(gateway::Update::Response(
            old,
            snapshot("old connection"),
            true,
        ));
        assert_eq!(a.chat, "current");
        let before_send = history(&a, 2);
        a.send_chat("newest".into());
        let sent = rx.try_iter().find(|r| r.method == "chat.send").unwrap();
        let id = sent.params["idempotencyKey"].as_str().unwrap();
        a.update(gateway::Update::Event(json!({"event":"chat","payload":{"sessionKey":"one","runId":id,"state":"final","message":{"content":"latest reply"}}})));
        let current = a.chat.clone();
        a.update(gateway::Update::Response(
            before_send,
            snapshot("before newest message"),
            true,
        ));
        assert_eq!(a.chat, current);
        let wrong_session = history(&a, 3);
        a.selected = Some(1);
        a.update(gateway::Update::Response(
            wrong_session,
            snapshot("wrong session"),
            true,
        ));
        assert_eq!(a.chat, current);
    }
    #[test]
    fn older_history_reply_cannot_replace_newer_snapshot() {
        let (mut a, _) = app();
        let older = history(&a, 1);
        let newer = history(&a, 2);
        a.update(gateway::Update::Response(newer, snapshot("new"), true));
        a.update(gateway::Update::Response(older, snapshot("old"), true));
        assert_eq!(a.chat, "user › new");
    }
    #[test]
    fn unrelated_rpc_error_does_not_retire_running_chat() {
        let (mut a, rx) = app();
        a.send_chat("latest".into());
        let send = rx.try_recv().unwrap();
        let mut query = history(&a, 2);
        query.method = "models.list".into();
        a.update(gateway::Update::Response(
            query,
            json!({"message":"unavailable"}),
            false,
        ));
        assert!(a.working);
        assert!(
            a.active_runs
                .contains(send.params["idempotencyKey"].as_str().unwrap())
        );
    }
    #[test]
    fn failed_send_remains_visible_after_connection_recovers() {
        let (mut a, rx) = app();
        a.send_chat("uncertain message".into());
        let request = rx.try_recv().unwrap();
        a.update(gateway::Update::Error("heartbeat timeout".into()));
        a.update(gateway::Update::RequestFailed(request));
        a.update(gateway::Update::Status("CONNECTED".into()));
        assert!(a.notice.contains("unconfirmed"));
        assert!(!a.working);
        assert!(a.chat.contains("uncertain message"));
        assert!(!rx.try_iter().any(|r| r.method == "chat.send"));
    }
    #[test]
    fn late_event_or_ack_cannot_resurrect_finished_run() {
        let (mut a, rx) = app();
        a.send_chat("one".into());
        let request = rx.try_recv().unwrap();
        let id = request.params["idempotencyKey"]
            .as_str()
            .unwrap()
            .to_owned();
        let final_event = json!({"event":"chat","payload":{"sessionKey":"one","runId":id,"state":"final","message":{"content":"done"}}});
        a.update(gateway::Update::Event(final_event.clone()));
        let chat = a.chat.clone();
        a.update(gateway::Update::Response(
            request,
            json!({"runId":id,"status":"started"}),
            true,
        ));
        a.update(gateway::Update::Event(json!({"event":"chat","payload":{"sessionKey":"one","runId":id,"state":"delta","message":{"content":"old delta"}}})));
        a.update(gateway::Update::Event(final_event));
        assert!(!a.working);
        assert!(a.live.is_empty());
        assert_eq!(a.chat, chat);
    }
}

#[cfg(test)]
mod local_log_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn save_is_local_offline_and_includes_live_text() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new("wss://host".into(), tx);
        let path =
            std::env::temp_dir().join(format!("veyterm-snapshot-{}.txt", std::process::id()));
        app.chat = "user › hello".into();
        app.snapshot
            .borrow_mut()
            .push(app.message_record("user", "hello"));
        app.live = "reply in progress".into();
        app.send_chat(format!("/save {}", path.display()));
        let records: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records[0]["type"], "session");
        assert_eq!(records[0]["data"]["server"], "host:443");
        assert_eq!(records[1]["data"]["text"], "hello");
        assert_eq!(records[2]["data"]["text"], "reply in progress");
        assert_eq!(
            app.notice,
            format!("History snapshot saved to {}", path.display())
        );
        assert!(rx.try_recv().is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn session_header_is_below_chat_and_first_row_is_available() {
        let (tx, _) = mpsc::channel();
        let mut app = App::new("wss://host".into(), tx);
        app.update(gateway::Update::Sessions(
            json!({"sessions":[{"key":"test","displayName":"test session"}]}),
        ));
        app.selected = Some(0);
        app.chat = "user › First message".into();
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y| {
            (0..100)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(row(0).contains("First message"), "{}", row(0));
        assert!(row(17).contains("host"), "{}", row(17));
    }
}

#[cfg(test)]
mod conversation_log_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn logs_conversation_once_without_api_or_stream_noise() {
        let (tx, _) = mpsc::channel();
        let mut app = App::new("wss://host:18789".into(), tx);
        app.update(gateway::Update::Sessions(
            json!({"sessions":[{"key":"test"}]}),
        ));
        app.selected = Some(0);
        let path = std::env::temp_dir().join(format!(
            "veyterm-conversation-{}.ndjson",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        app.journal = Some(journal::Journal::open(&path, &app.endpoint, Some("test")).unwrap());
        app.update(gateway::Update::Status("CONNECTED".into()));
        app.request("models.list", json!({}));
        app.update(gateway::Update::Usage(json!({"providers":[]})));
        app.update(gateway::Update::Reply(
            "models.list".into(),
            json!({"models":[]}),
        ));
        let history = json!({"messages":[
            {"role":"user","content":"hello","timestamp":1},
            {"role":"assistant","content":"hi","timestamp":2}
        ]});
        app.update(gateway::Update::Reply(
            "chat.history".into(),
            history.clone(),
        ));
        app.update(gateway::Update::Reply(
            "chat.history".into(),
            history.clone(),
        ));
        app.request("chat.send", json!({"message":"/model sol"}));
        app.update(gateway::Update::Event(json!({"event":"agent","payload":{
            "sessionKey":"test","stream":"tool","data":{"phase":"start","name":"exec","args":{"command":"pwd"}}
        }})));
        app.update(gateway::Update::Event(json!({"event":"chat","payload":{
            "sessionKey":"test","runId":"new-run","state":"final",
            "message":{"content":"new reply"}
        }})));
        app.update(gateway::Update::Reply(
            "chat.history".into(),
            history.clone(),
        ));
        app.update(gateway::Update::Reply(
            "chat.history".into(),
            history.clone(),
        ));
        assert_eq!(app.snapshot.borrow().len(), 3);
        app.update(gateway::Update::Error("connection lost".into()));
        app.update(gateway::Update::Error("retry failed".into()));
        app.update(gateway::Update::Status("CONNECTED".into()));
        let records: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let kinds: Vec<_> = records
            .iter()
            .map(|r| r["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "session",
                "command",
                "tool_call",
                "message",
                "connection",
                "connection"
            ]
        );
        assert_eq!(records[1]["data"]["text"], "/model sol");
        assert_eq!(records[2]["data"]["name"], "exec");
        assert_eq!(records[2]["data"]["args"]["command"], "pwd");
        assert_eq!(records[3]["data"]["text"], "new reply");
        assert_eq!(records[4]["data"]["status"], "disconnected");
        assert_eq!(records[5]["data"]["status"], "restored");
        assert!(
            !records
                .iter()
                .any(|r| matches!(r["data"]["text"].as_str(), Some("hello" | "hi")))
        );
        std::fs::remove_file(path).unwrap();
    }
}
