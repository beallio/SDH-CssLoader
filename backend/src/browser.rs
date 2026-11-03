use crate::Backend;
use futures_util::{SinkExt, StreamExt};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    time::{interval, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const TAB_INIT_JS: &str = "(function(){ return {\"title\": document.title, \"url\": location.href, \"readyState\": document.readyState, \"classes\": Array.from(document.documentElement.classList).concat(Array.from(document.body.classList)).concat(Array.from(document.head.classList))} })()";
const WATCHER_JS: &str = r#"
(() => {
    if (window !== window.top || window.__cssLoaderWatcherInstalled) {
        return true;
    }
    window.__cssLoaderWatcherInstalled = true;
    const expected = window.__cssLoaderExpected instanceof Set
        ? window.__cssLoaderExpected
        : (window.__cssLoaderExpected = new Set());
    let headObserver;
    let observedHead;
    let rootElement;
    let checkScheduled = false;
    let repairRequested = false;
    const check = () => {
        if (!document.head) {
            return;
        }
        const missing = !document.getElementById("test_css_loaded")
            || [...expected].some(id => !document.getElementById(id));
        if (missing) {
            if (!repairRequested) {
                repairRequested = true;
                if (typeof window.__cssLoaderWatch === "function") {
                    window.__cssLoaderWatch("missing");
                }
            }
        } else {
            repairRequested = false;
        }
    };
    const scheduleCheck = () => {
        if (checkScheduled) {
            return;
        }
        checkScheduled = true;
        queueMicrotask(() => {
            checkScheduled = false;
            check();
        });
    };
    const observeHead = () => {
        if (observedHead === document.head) {
            return;
        }
        headObserver?.disconnect();
        observedHead = document.head;
        if (observedHead) {
            headObserver = new MutationObserver(scheduleCheck);
            headObserver.observe(observedHead, {childList: true});
        }
    };
    const rootObserver = new MutationObserver(() => {
        observeHead();
        scheduleCheck();
    });
    const observeDocument = () => {
        if (rootElement !== document.documentElement) {
            rootObserver.disconnect();
            rootElement = document.documentElement;
            if (rootElement) {
                rootObserver.observe(rootElement, {childList: true});
            }
        }
        observeHead();
        scheduleCheck();
    };
    document.addEventListener("readystatechange", observeDocument);
    document.addEventListener("DOMContentLoaded", observeDocument, {once: true});
    observeDocument();
    return true;
})()
"#;
const PREPAINT_JS: &str = r#"
(() => {
    if (window !== window.top) {
        return;
    }
    const styles = $STYLES;
    const install = () => {
        const head = document.head;
        if (!head) {
            return false;
        }
        const expected = window.__cssLoaderExpected instanceof Set
            ? window.__cssLoaderExpected
            : (window.__cssLoaderExpected = new Set());
        expected.clear();
        let marker = document.getElementById("test_css_loaded");
        if (!marker) {
            marker = document.createElement("div");
            marker.id = "test_css_loaded";
            head.append(marker);
        }
        for (const {id, css} of styles) {
            expected.add(id);
            let style = document.getElementById(id);
            if (!style) {
                style = document.createElement("style");
                style.id = id;
                style.classList.add("css-loader-style");
                style.textContent = css;
            }
            head.append(style);
        }
        // Move what is in the document now: a reset or theme change during loading may have replaced the prepaint styles.
        const moveToEnd = () => {
            const head = document.head;
            if (!head) {
                return;
            }
            const current = document.getElementById("test_css_loaded");
            if (current) {
                head.append(current);
            }
            document.querySelectorAll(".css-loader-style").forEach(style => head.append(style));
        };
        if (document.readyState === "loading") {
            document.addEventListener("DOMContentLoaded", moveToEnd, {once: true});
        } else {
            moveToEnd();
        }
        return true;
    };
    if (!install()) {
        const observer = new MutationObserver(() => {
            if (install()) {
                observer.disconnect();
            }
        });
        observer.observe(document, {childList: true, subtree: true});
    }
})()
"#;
const REMOVE_ALL_JS: &str = r#"
        (function() {
            const expected = window.__cssLoaderExpected instanceof Set
                ? window.__cssLoaderExpected
                : (window.__cssLoaderExpected = new Set());
            expected.clear();
            document.querySelectorAll('.css-loader-style').forEach(x => x.remove());
        })()
        "#;
/// Budget for the commands that only gate a new attach, and for the liveness query that follows a
/// failed one. A tab that dies mid-attach used to swallow the full command timeout, and that await
/// blocks the whole event loop, which stalls CSS work for every other tab.
const ATTACH_COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
/// A browser is down for a while on every restart, and the health loop probes it every 3 s. One
/// warning once the outage is old enough is worth reading; a warning per probe is not.
const OUTAGE_ALERT_AFTER: Duration = Duration::from_secs(30);
/// The event loop can drop many CDP events in one burst while a Steam page loads. One line per
/// window, carrying the sum it covers, keeps the log readable and still shows the real volume.
const LAG_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Whether the Steam browser can be reached, and for how long it could not. The health loop keeps
/// probing every 3 s, so this is what turns a wall of DEBUG lines into one WARNING and one INFO.
#[derive(Default)]
struct Outage {
    /// When the current outage started, or `None` while the browser is reachable.
    since: Option<Instant>,
    /// Whether the outage has already been reported; cleared when the browser is back.
    warned: bool,
}

impl Outage {
    /// Records a failed probe and returns the warning once the outage is old enough. A browser that
    /// stays down never warns twice, and a probe that fails again right after a report stays quiet.
    fn failed(&mut self, now: Instant, error: &str) -> Option<String> {
        let since = *self.since.get_or_insert(now);
        let down = now.saturating_duration_since(since);
        if self.warned || down < OUTAGE_ALERT_AFTER {
            return None;
        }
        self.warned = true;
        Some(format!(
            "Steam browser unreachable for {}: {error}",
            short_duration(down)
        ))
    }

    /// Records a successful probe and returns the recovery line only when the outage was reported,
    /// so a restart in the first half minute stays quiet and does not pair with a warning.
    fn recovered(&mut self, now: Instant) -> Option<String> {
        let since = self.since.take()?;
        std::mem::replace(&mut self.warned, false).then(|| {
            format!(
                "Steam browser reachable again after {}",
                short_duration(now.saturating_duration_since(since))
            )
        })
    }
}

/// Dropped broadcast events, summed and reported at most once per window. The totals also feed the
/// connection summary, so events dropped after the last warning still show up there.
#[derive(Default)]
struct LagLog {
    /// Everything dropped since the connection started.
    total: u64,
    /// Dropped since the last warning was printed.
    pending: u64,
    last_warn: Option<Instant>,
}

impl LagLog {
    /// Records `count` dropped events and returns the warning when this lag is the first after a
    /// quiet window; earlier lags in that window are summed into it.
    fn dropped(&mut self, count: u64, now: Instant) -> Option<String> {
        self.total += count;
        self.pending += count;
        if self
            .last_warn
            .is_some_and(|last| now.saturating_duration_since(last) < LAG_WARN_INTERVAL)
        {
            return None;
        }
        self.last_warn = Some(now);
        Some(format!(
            "Event loop fell behind and dropped {} CDP events",
            std::mem::take(&mut self.pending)
        ))
    }
}

/// Durations in log lines: `42s`, `2m 5s`, `1h 7m`. Two units already say how long something ran.
fn short_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

/// A page without a title is logged as its URL, and an inline `data:` page carries its whole
/// document there: keep only enough of it to recognise the page.
fn short_title(title: &str) -> Cow<'_, str> {
    /// Characters kept from an inline URL.
    const KEEP: usize = 24;
    if !title.starts_with("data:") {
        return Cow::Borrowed(title);
    }
    // `char_indices` yields valid boundaries only, so the cut never splits a character.
    match title.char_indices().nth(KEEP) {
        Some((end, _)) => Cow::Owned(format!("{}…", &title[..end])),
        None => Cow::Borrowed(title),
    }
}

/// A commit with nothing to change still runs (a reset needs its remove-all script), but it says
/// nothing a reader of the log wants, and theme toggles fire it often.
fn transaction_level(adds: usize, removes: usize) -> log::Level {
    if adds == 0 && removes == 0 {
        log::Level::Debug
    } else {
        log::Level::Info
    }
}

fn preload_script(styles: &IndexMap<String, String>) -> String {
    let styles = styles
        .iter()
        .map(|(id, css)| json!({"id": id, "css": css}))
        .collect::<Vec<_>>();
    format!(
        "{};\n{}",
        PREPAINT_JS.replace("$STYLES", &json!(styles).to_string()),
        WATCHER_JS
    )
}

#[derive(Clone, Debug)]
struct Tab {
    id: String,
    session: String,
    title: String,
    url: String,
    classes: Vec<String>,
    initialized: bool,
    applied_styles: IndexMap<String, String>,
    preload_script_id: Option<String>,
    pending_add: IndexMap<String, String>,
    pending_remove: Vec<String>,
}

impl Tab {
    fn compare(&self, selector: &str) -> bool {
        if selector.starts_with('~') && selector.ends_with('~') && selector.len() > 2 {
            self.url.contains(&selector[1..selector.len() - 1])
        } else if let Some(class) = selector.strip_prefix('!') {
            self.classes.iter().any(|x| x == class)
        } else {
            // Python's re caches compiled patterns; compile each selector once, not per comparison.
            static PATTERNS: LazyLock<
                std::sync::Mutex<HashMap<String, Option<fancy_regex::Regex>>>,
            > = LazyLock::new(Default::default);
            let mut patterns = PATTERNS.lock().unwrap();
            if !patterns.contains_key(selector) {
                let pattern = fancy_regex::Regex::new(&format!("^({selector})"))
                    .inspect_err(|error| log::warn!("Invalid title selector '{selector}': {error}"))
                    .ok();
                patterns.insert(selector.to_owned(), pattern);
            }
            patterns[selector]
                .as_ref()
                .is_some_and(|pattern| pattern.is_match(&self.title).unwrap_or(false))
        }
    }
}

/// What one connection did, counted as it happens so its end can say it in one line. These are
/// plain atomics: a counter must never cost a lock on the event loop.
#[derive(Default)]
struct Stats {
    /// Tabs that closed before a command reached them; routine for toasts, overlays and popups.
    closed_tabs: AtomicU64,
    /// Commands a live tab never answered, attach commands included.
    no_answer: AtomicU64,
    /// Transaction passes that had to be retried.
    retries: AtomicU64,
    /// Repairs that actually ran, not the health ticks that asked for one.
    repairs: AtomicU64,
}

struct Cdp {
    tx: mpsc::UnboundedSender<Message>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    events: broadcast::Sender<Value>,
    closed: watch::Receiver<bool>,
    stats: Stats,
}

impl Cdp {
    async fn command(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        wait_for_response: bool,
    ) -> Option<Value> {
        self.command_within(
            method,
            params,
            session,
            wait_for_response,
            Duration::from_secs(5),
        )
        .await
    }

    /// Like `command`, but gives up after `limit`. Used where an unresponsive browser must not hold
    /// the event loop for the full command timeout.
    async fn command_within(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        wait_for_response: bool,
        limit: Duration,
    ) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        if wait_for_response {
            self.pending.lock().await.insert(id, response_tx);
        }
        let mut command = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            command["sessionId"] = Value::String(session.to_owned());
        }
        if self
            .tx
            .send(Message::Text(command.to_string().into()))
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return None;
        }
        if wait_for_response {
            match timeout(limit, response_rx).await {
                Ok(Ok(value)) => Some(value),
                _ => {
                    self.pending.lock().await.remove(&id);
                    None
                }
            }
        } else {
            None
        }
    }

    /// True when a per-tab command failed only because that tab is gone: the connection dropped, or
    /// CDP rejected the command for a session or target that no longer exists. Every other error
    /// answer is a real failure on a live tab and keeps its warning.
    fn tab_closed(&self, response: Option<&Value>) -> bool {
        if *self.closed.borrow() {
            return true;
        }
        let Some(error) = response.and_then(|response| response.get("error")) else {
            return false;
        };
        // -32001 is the session-gone error; CEF's wording varies, so match the known texts too.
        if error.get("code").and_then(Value::as_i64) == Some(-32001) {
            return true;
        }
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        [
            "Session with given id not found",
            "No target with given id",
            "Target closed",
            "detached",
        ]
        .iter()
        .any(|known| contains_ignore_ascii_case(message, known))
    }
}

/// True when a failed command only means the tab is gone. `listed` says whether CDP still lists the
/// target (`None` when the list could not be read), and matters only when the command got no answer
/// at all: CEF sends detach events, but the event loop is busy waiting on this very command, so the
/// target list is the only timely witness. Only a valid list without the target proves it is gone.
fn tab_gone(cdp: &Cdp, response: Option<&Value>, listed: Option<bool>) -> bool {
    cdp.tab_closed(response) || (response.is_none() && listed == Some(false))
}

/// Whether a `Target.getTargets` answer still lists this page; `None` for an error or malformed answer.
fn lists_target(response: &Value, id: &str) -> Option<bool> {
    response
        .pointer("/result/targetInfos")
        .and_then(Value::as_array)
        .map(|targets| {
            targets
                .iter()
                .any(|target| target.get("targetId").and_then(Value::as_str) == Some(id))
        })
}

/// Case-insensitive ASCII substring test, so matching CDP error text allocates nothing.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    needle.is_empty()
        || (haystack.len() >= needle.len()
            && haystack
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle)))
}

/// One attempt to reach CEF and open the debugger socket. Each failure keeps the text the health
/// loop already logs at DEBUG, because the outage warning repeats the last one verbatim.
async fn probe_browser(
    client: &reqwest::Client,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    let response = client
        .get("http://127.0.0.1:8080/json/version")
        .send()
        .await
        .map_err(|error| format!("CEF version endpoint unavailable: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("/json/version returned {}", response.status()));
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| format!("Invalid CEF version response: {error}"))?;
    let url = value
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or("CEF version response has no webSocketDebuggerUrl")?;
    connect_async(url)
        .await
        .map(|(stream, _)| stream)
        .map_err(|error| format!("CEF websocket connection failed: {error}"))
}

#[derive(Default)]
pub struct Browser {
    tabs: Mutex<HashMap<String, Tab>>,
    connection: Mutex<Option<Arc<Cdp>>>,
    repairs: Mutex<HashSet<String>>,
    pending_target_info: Mutex<HashMap<String, (String, String)>>,
    // Overlapping replace calls leaked a stale prepaint script that resurrected removed styles on reload.
    preload_updates: Mutex<()>,
}

impl Browser {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Reports a failed per-tab command with the reason CDP gave. Tabs close mid-attach all the time
    /// (toasts, overlays, screensaver windows), so that is routine; a live tab that did not answer,
    /// or answered with anything else, is not.
    async fn log_tab_failure(&self, cdp: &Cdp, id: &str, action: &str, response: Option<&Value>) {
        let message = response
            .and_then(|response| response.pointer("/error/message"))
            .and_then(Value::as_str)
            .unwrap_or("no answer");
        // Only a command with no answer at all needs the target list; an error answer is judged from
        // its code and text alone, so a real failure on a live tab never pays for this query.
        let listed = if response.is_some() {
            None
        } else {
            self.target_listed(cdp, id).await
        };
        if tab_gone(cdp, response, listed) {
            cdp.stats.closed_tabs.fetch_add(1, Ordering::Relaxed);
            log::debug!("Tab {id} closed before it could {action}: {message}");
        } else {
            if response.is_none() {
                cdp.stats.no_answer.fetch_add(1, Ordering::Relaxed);
            }
            log::warn!("Failed to {action} on tab {id}: {message}");
        }
    }

    /// Whether CDP still lists this page, or `None` when the browser did not give a usable list. Timed
    /// like the attach commands it follows: this runs only on a failed attach, and a browser that
    /// ignores the command must not extend the stall.
    async fn target_listed(&self, cdp: &Cdp, id: &str) -> Option<bool> {
        cdp.command_within(
            "Target.getTargets",
            json!({}),
            None,
            true,
            ATTACH_COMMAND_TIMEOUT,
        )
        .await
        .and_then(|response| lists_target(&response, id))
    }

    pub fn start(self: &Arc<Self>, backend: Weak<Backend>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.health_loop(backend).await;
        });
    }

    async fn health_loop(self: Arc<Self>, backend: Weak<Backend>) {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                log::error!("CEF HTTP client failed: {error}");
                return;
            }
        };
        let mut outage = Outage::default();
        loop {
            if self.connection.lock().await.is_none() {
                match probe_browser(&client).await {
                    Ok(stream) => {
                        if let Some(line) = outage.recovered(Instant::now()) {
                            log::info!("{line}");
                        }
                        log::info!("Connected to Steam Browser");
                        self.connected(stream, backend.clone()).await;
                    }
                    Err(error) => {
                        log::debug!("{error}");
                        if let Some(line) = outage.failed(Instant::now(), &error) {
                            log::warn!("{line}");
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    async fn connected(
        self: &Arc<Self>,
        stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
        backend: Weak<Backend>,
    ) {
        let started = Instant::now();
        let (mut sink, mut source) = stream.split();
        let (tx, mut outgoing) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(500);
        let (closed_tx, closed) = watch::channel(false);
        let cdp = Arc::new(Cdp {
            tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events,
            closed,
            stats: Stats::default(),
        });
        *self.connection.lock().await = Some(Arc::clone(&cdp));

        let writer_closed = closed_tx.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
            let _ = writer_closed.send(true);
        });
        let reader_cdp = Arc::clone(&cdp);
        let reader =
            tokio::spawn(async move {
                while let Some(Ok(message)) = source.next().await {
                    if let Message::Text(text) = message {
                        let Ok(value) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        if let Some(id) = value.get("id").and_then(Value::as_u64) {
                            if let Some(sender) = reader_cdp.pending.lock().await.remove(&id) {
                                let _ = sender.send(value);
                            }
                        } else if value.get("method").and_then(Value::as_str).is_some_and(
                            |method| {
                                matches!(
                                    method,
                                    "Runtime.bindingCalled"
                                        | "Runtime.executionContextCreated"
                                        | "Page.domContentEventFired"
                                        | "Target.targetCreated"
                                        | "Target.attachedToTarget"
                                        | "Target.detachedFromTarget"
                                        | "Target.targetInfoChanged"
                                )
                            },
                        ) {
                            let _ = reader_cdp.events.send(value);
                        }
                    } else if matches!(message, Message::Close(_)) {
                        break;
                    }
                }
                let _ = closed_tx.send(true);
            });

        let mut event_rx = cdp.events.subscribe();
        let _ = cdp
            .command(
                "Target.setDiscoverTargets",
                json!({"discover": true}),
                None,
                false,
            )
            .await;
        self.attach_existing(&cdp).await;
        let mut closed_rx = cdp.closed.clone();
        let mut sanity = interval(Duration::from_secs(5));
        let mut health = interval(Duration::from_secs(30));
        let mut lag = LagLog::default();
        sanity.tick().await;
        health.tick().await;
        let reason = loop {
            tokio::select! {
                event = event_rx.recv() => match event {
                    Ok(value) => self.handle_event(value, &cdp, backend.clone()).await,
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        if let Some(line) = lag.dropped(count, Instant::now()) {
                            log::warn!("{line}");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break "the CDP event channel closed",
                },
                _ = sanity.tick() => self.sanity_check(&cdp, backend.clone()).await,
                _ = health.tick() => self.health_tabs(&cdp, backend.clone()).await,
                changed = closed_rx.changed() => {
                    if changed.is_err() {
                        break "the CDP connection dropped";
                    }
                    if *closed_rx.borrow() {
                        break "the CDP socket closed";
                    }
                },
            }
        };
        log::info!(
            "Steam Browser connection ended after {}: {reason}",
            short_duration(started.elapsed())
        );
        let stats = &cdp.stats;
        log::info!(
            "Connection summary: {} tabs closed, {} commands without an answer, {} transaction retries, {} repairs, {} dropped events",
            stats.closed_tabs.load(Ordering::Relaxed),
            stats.no_answer.load(Ordering::Relaxed),
            stats.retries.load(Ordering::Relaxed),
            stats.repairs.load(Ordering::Relaxed),
            lag.total
        );
        self.tabs.lock().await.clear();
        self.pending_target_info.lock().await.clear();
        *self.connection.lock().await = None;
        writer.abort();
        reader.abort();
    }

    async fn attach_existing(&self, cdp: &Arc<Cdp>) {
        if let Some(response) = cdp
            .command("Target.getTargets", json!({}), None, true)
            .await
            && let Some(targets) = response
                .pointer("/result/targetInfos")
                .and_then(Value::as_array)
        {
            for target in targets {
                if target.get("type").and_then(Value::as_str) == Some("page") {
                    let id = target
                        .get("targetId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let _ = cdp
                        .command(
                            "Target.attachToTarget",
                            json!({"targetId": id, "flatten": true}),
                            None,
                            false,
                        )
                        .await;
                }
            }
        }
    }

    async fn handle_event(&self, event: Value, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        match event
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "Runtime.bindingCalled" => {
                if event.pointer("/params/name").and_then(Value::as_str) == Some("__cssLoaderWatch")
                    && let Some(session) = event.get("sessionId").and_then(Value::as_str)
                {
                    let id = self
                        .tabs
                        .lock()
                        .await
                        .iter()
                        .find_map(|(id, tab)| (tab.session == session).then(|| id.clone()));
                    if let Some(id) = id {
                        self.repair_tab(&id, cdp, backend, "the repair binding asked for it")
                            .await;
                    }
                }
            }
            "Target.targetCreated" => {
                if event
                    .pointer("/params/targetInfo/type")
                    .and_then(Value::as_str)
                    == Some("page")
                    && let Some(info) = event.pointer("/params/targetInfo")
                    && let Some(id) = info.get("targetId").and_then(Value::as_str)
                {
                    let text = |key| {
                        info.get(key)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    };
                    // Remember the target so a following targetInfoChanged does not attach a second time.
                    self.pending_target_info
                        .lock()
                        .await
                        .insert(id.to_owned(), (text("title"), text("url")));
                    let _ = cdp
                        .command(
                            "Target.attachToTarget",
                            json!({"targetId": id, "flatten": true}),
                            None,
                            false,
                        )
                        .await;
                }
            }
            "Target.attachedToTarget" => {
                self.attach_tab(
                    event.pointer("/params").unwrap_or(&Value::Null).clone(),
                    cdp,
                    backend,
                )
                .await
            }
            "Target.detachedFromTarget" => {
                if let Some(id) = event.pointer("/params/targetId").and_then(Value::as_str) {
                    self.pending_target_info.lock().await.remove(id);
                    if let Some(tab) = self.tabs.lock().await.remove(id) {
                        log::info!("Disconnected from tab: {}", short_title(&tab.title));
                    }
                }
            }
            "Target.targetInfoChanged" => {
                if let Some(info) = event.pointer("/params/targetInfo")
                    && info.get("type").and_then(Value::as_str) == Some("page")
                    && let Some(id) = info.get("targetId").and_then(Value::as_str)
                {
                    let title = info
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let url = info
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let state = {
                        let mut tabs = self.tabs.lock().await;
                        tabs.get_mut(id).map(|tab| {
                            let url_changed = tab.url != url;
                            let changed = tab.title != title || url_changed;
                            tab.title.clone_from(&title);
                            tab.url.clone_from(&url);
                            if url_changed {
                                tab.initialized = false;
                            }
                            (changed, url_changed, tab.initialized)
                        })
                    };
                    if let Some((changed, url_changed, initialized)) = state {
                        if url_changed || !initialized {
                            self.initialize_tab(id, cdp, backend.clone()).await;
                        } else if changed && let Some(backend) = backend.upgrade() {
                            backend.force_reinject(id).await;
                        }
                    } else {
                        // Steam's CEF announces new windows as type "other"; they become pages only here.
                        let first_seen = self
                            .pending_target_info
                            .lock()
                            .await
                            .insert(id.to_owned(), (title, url))
                            .is_none();
                        if first_seen {
                            let _ = cdp
                                .command(
                                    "Target.attachToTarget",
                                    json!({"targetId": id, "flatten": true}),
                                    None,
                                    false,
                                )
                                .await;
                        }
                    }
                }
            }
            "Runtime.executionContextCreated" | "Page.domContentEventFired" => {
                if event.get("method").and_then(Value::as_str)
                    == Some("Runtime.executionContextCreated")
                    && event
                        .pointer("/params/context/auxData/isDefault")
                        .and_then(Value::as_bool)
                        == Some(false)
                {
                    return;
                }
                if let Some(session) = event.get("sessionId").and_then(Value::as_str) {
                    let id = self.tabs.lock().await.iter().find_map(|(id, tab)| {
                        (tab.session == session && !tab.initialized).then(|| id.clone())
                    });
                    if let Some(id) = id {
                        self.initialize_tab(&id, cdp, backend).await;
                    }
                }
            }
            _ => {}
        }
    }

    async fn attach_tab(&self, params: Value, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let Some(info) = params.get("targetInfo") else {
            return;
        };
        let (Some(id), Some(session)) = (
            info.get("targetId").and_then(Value::as_str),
            params.get("sessionId").and_then(Value::as_str),
        ) else {
            return;
        };
        if self.tabs.lock().await.contains_key(id) {
            return;
        }
        let latest = self.pending_target_info.lock().await.remove(id);
        let tab = Tab {
            id: id.to_owned(),
            session: session.to_owned(),
            title: latest
                .as_ref()
                .map(|info| info.0.as_str())
                .or_else(|| info.get("title").and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned(),
            url: latest
                .as_ref()
                .map(|info| info.1.as_str())
                .or_else(|| info.get("url").and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned(),
            classes: Vec::new(),
            initialized: false,
            applied_styles: IndexMap::new(),
            preload_script_id: None,
            pending_add: IndexMap::new(),
            pending_remove: Vec::new(),
        };
        self.tabs.lock().await.insert(id.to_owned(), tab);

        let runtime = cdp
            .command_within(
                "Runtime.enable",
                json!({}),
                Some(session),
                true,
                ATTACH_COMMAND_TIMEOUT,
            )
            .await;
        if !runtime
            .as_ref()
            .is_some_and(|response| response.get("error").is_none())
        {
            self.log_tab_failure(cdp, id, "enable the Runtime domain", runtime.as_ref())
                .await;
            self.tabs.lock().await.remove(id);
            return;
        }
        let page = cdp
            .command_within(
                "Page.enable",
                json!({}),
                Some(session),
                true,
                ATTACH_COMMAND_TIMEOUT,
            )
            .await;
        if !page
            .as_ref()
            .is_some_and(|response| response.get("error").is_none())
        {
            self.log_tab_failure(cdp, id, "enable the Page domain", page.as_ref())
                .await;
        }
        let binding = cdp
            .command_within(
                "Runtime.addBinding",
                json!({"name": "__cssLoaderWatch"}),
                Some(session),
                true,
                ATTACH_COMMAND_TIMEOUT,
            )
            .await;
        if !binding
            .as_ref()
            .is_some_and(|response| response.get("error").is_none())
        {
            self.log_tab_failure(cdp, id, "install CSS repair binding", binding.as_ref())
                .await;
        }
        self.update_preload_script(id, cdp, session).await;
        self.initialize_tab(id, cdp, backend).await;
    }

    async fn initialize_tab(&self, id: &str, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let Some((session, initialized, target_url)) = self
            .tabs
            .lock()
            .await
            .get(id)
            .map(|tab| (tab.session.clone(), tab.initialized, tab.url.clone()))
        else {
            return;
        };
        if initialized {
            return;
        }
        let Some(value) = evaluate(cdp, &session, TAB_INIT_JS, true)
            .await
            .and_then(|value| value.as_object().cloned())
        else {
            return;
        };
        // Steam popups keep an about:blank target URL while their document reports the opener's URL.
        if !target_url.is_empty()
            && !target_url.starts_with("about:blank")
            && value.get("url").and_then(Value::as_str) != Some(target_url.as_str())
        {
            return;
        }
        if value.get("readyState").and_then(Value::as_str) == Some("loading") {
            return;
        }
        let (title, source) = {
            let mut tabs = self.tabs.lock().await;
            let Some(tab) = tabs.get_mut(id) else {
                return;
            };
            if let Some(title) = value
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.is_empty())
            {
                tab.title = title.to_owned();
            }
            tab.classes = value
                .get("classes")
                .and_then(Value::as_array)
                .map(|classes| {
                    classes
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            tab.initialized = true;
            (tab.title.clone(), preload_script(&tab.applied_styles))
        };
        if evaluate(cdp, &session, &source, true).await != Some(Value::Bool(true)) {
            log::warn!("Failed to install CSS repair watcher in current document on tab {id}");
        }
        log::info!("Connected to tab: {}", short_title(&title));
        if let Some(backend) = backend.upgrade() {
            backend.force_reinject(id).await;
        }
    }

    /// Registers the tab's current applied styles for future documents, replacing the previous script.
    async fn update_preload_script(&self, id: &str, cdp: &Arc<Cdp>, session: &str) {
        let _serial = self.preload_updates.lock().await;
        let Some((old_id, source)) = self.tabs.lock().await.get(id).map(|tab| {
            (
                tab.preload_script_id.clone(),
                preload_script(&tab.applied_styles),
            )
        }) else {
            return;
        };
        if let Some(old_id) = old_id {
            let removed = cdp
                .command(
                    "Page.removeScriptToEvaluateOnNewDocument",
                    json!({"identifier": old_id}),
                    Some(session),
                    true,
                )
                .await;
            if !removed
                .as_ref()
                .is_some_and(|response| response.get("error").is_none())
            {
                self.log_tab_failure(cdp, id, "replace CSS prepaint script", removed.as_ref())
                    .await;
            }
        }
        let added = cdp
            .command(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": source}),
                Some(session),
                true,
            )
            .await;
        let script_id = added
            .as_ref()
            .filter(|response| response.get("error").is_none())
            .and_then(|response| {
                response
                    .pointer("/result/identifier")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        if script_id.is_none() {
            self.log_tab_failure(
                cdp,
                id,
                "register CSS prepaint script for future documents",
                added.as_ref(),
            )
            .await;
        }
        if let Some(tab) = self.tabs.lock().await.get_mut(id) {
            tab.preload_script_id = script_id;
        }
    }

    async fn sanity_check(&self, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let Some(response) = cdp
            .command("Target.getTargets", json!({}), None, true)
            .await
        else {
            return;
        };
        let Some(targets) = response
            .pointer("/result/targetInfos")
            .and_then(Value::as_array)
        else {
            return;
        };
        let page_ids: Vec<_> = targets
            .iter()
            .filter(|x| x.get("type").and_then(Value::as_str) == Some("page"))
            .filter_map(|x| x.get("targetId").and_then(Value::as_str).map(str::to_owned))
            .collect();
        self.tabs.lock().await.retain(|id, tab| {
            let keep = page_ids.contains(id);
            if !keep {
                log::info!("Disconnected from tab: {}", short_title(&tab.title));
            }
            keep
        });
        for target in targets
            .iter()
            .filter(|x| x.get("type").and_then(Value::as_str) == Some("page"))
        {
            let Some(id) = target.get("targetId").and_then(Value::as_str) else {
                continue;
            };
            let state = {
                let mut tabs = self.tabs.lock().await;
                tabs.get_mut(id).map(|tab| {
                    let title = target
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let url = target
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let changed = tab.title != title || tab.url != url;
                    tab.title = title.to_owned();
                    tab.url = url.to_owned();
                    (tab.initialized, changed)
                })
            };
            match state {
                Some((false, _)) => self.initialize_tab(id, cdp, backend.clone()).await,
                Some((true, true)) => {
                    if let Some(backend) = backend.upgrade() {
                        backend.force_reinject(id).await;
                    }
                }
                Some((true, false)) => {}
                None => {
                    let _ = cdp
                        .command(
                            "Target.attachToTarget",
                            json!({"targetId": id, "flatten": true}),
                            None,
                            false,
                        )
                        .await;
                }
            }
        }
    }

    async fn health_tabs(&self, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let ids: Vec<String> = self.tabs.lock().await.keys().cloned().collect();
        for id in ids {
            self.health_tab(&id, cdp, backend.clone()).await;
        }
    }

    async fn health_tab(&self, id: &str, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let Some((session, initialized)) = self
            .tabs
            .lock()
            .await
            .get(id)
            .map(|x| (x.session.clone(), x.initialized))
        else {
            return;
        };
        if !initialized
            || dom_contains_selector(cdp, &session, "#test_css_loaded").await != Some(false)
        {
            return;
        }
        self.repair_tab(
            id,
            cdp,
            backend,
            "the health check found #test_css_loaded missing",
        )
        .await;
    }

    /// Restores a tab's styles. The caller knows why the repair is needed; the title says which
    /// page, because a Steam window's target id means nothing in a log.
    async fn repair_tab(&self, id: &str, cdp: &Arc<Cdp>, backend: Weak<Backend>, reason: &str) {
        if !self.repairs.lock().await.insert(id.to_owned()) {
            return;
        }
        let tab = self
            .tabs
            .lock()
            .await
            .get(id)
            .map(|tab| (tab.initialized, tab.title.clone()));
        if let Some((true, title)) = tab
            && let Some(backend) = backend.upgrade()
        {
            log::info!("Repairing tab {}: {reason}", short_title(&title));
            cdp.stats.repairs.fetch_add(1, Ordering::Relaxed);
            backend.force_reinject(id).await;
        }
        self.repairs.lock().await.remove(id);
    }

    pub async fn inject_css(&self, selector: &str, css: &str, id: &str) {
        let mut tabs = self.tabs.lock().await;
        for tab in tabs.values_mut().filter(|x| x.compare(selector)) {
            tab.pending_add.insert(id.to_owned(), css.to_owned());
        }
    }

    pub async fn remove_css(&self, selector: &str, id: &str) {
        let mut tabs = self.tabs.lock().await;
        for tab in tabs.values_mut().filter(|x| x.compare(selector)) {
            if tab.pending_add.shift_remove(id).is_none() {
                tab.pending_remove.push(id.to_owned());
            }
        }
    }

    pub async fn inject_on_tab(&self, target_id: &str, selector: &str, css: &str, id: &str) {
        let mut tabs = self.tabs.lock().await;
        if let Some(tab) = tabs.get_mut(target_id)
            && tab.compare(selector)
        {
            tab.pending_add.insert(id.to_owned(), css.to_owned());
        }
    }

    pub async fn tab_matches(&self, target_id: &str, selector: &str) -> bool {
        self.tabs
            .lock()
            .await
            .get(target_id)
            .is_some_and(|x| x.compare(selector))
    }

    pub async fn commit_target(&self, id: &str, remove_all_first: bool) {
        let _ = self.commit_tab(id, 3, remove_all_first).await;
    }

    pub async fn commit_all(&self, remove_all_first: bool) {
        let ids: Vec<String> = self.tabs.lock().await.keys().cloned().collect();
        for id in ids {
            let _ = self.commit_tab(&id, 3, remove_all_first).await;
        }
    }

    async fn commit_tab(&self, id: &str, mut retry: usize, remove_all_first: bool) -> Value {
        let (session, adds, removes, title) = {
            let mut tabs = self.tabs.lock().await;
            let Some(tab) = tabs.get_mut(id) else {
                return json!({"success": true, "message": "Success"});
            };
            if tab.pending_add.is_empty() && tab.pending_remove.is_empty() && !remove_all_first {
                return json!({"success": true, "message": "Success"});
            }
            let adds = std::mem::take(&mut tab.pending_add);
            let removes = std::mem::take(&mut tab.pending_remove);
            (tab.session.clone(), adds, removes, tab.title.clone())
        };
        log::log!(
            transaction_level(adds.len(), removes.len()),
            "Committing css transaction on {} +{} -{}",
            short_title(&title),
            adds.len(),
            removes.len()
        );
        let add = adds
            .iter()
            .map(|(id, css)| json!({"id": id, "css": css}))
            .collect::<Vec<_>>();
        let js_data = json!({"add": add, "remove": &removes}).to_string();
        let extra = if remove_all_first {
            "expected.clear(); document.querySelectorAll('.css-loader-style').forEach(x => x.remove());"
        } else {
            ""
        };
        let mut js = String::from("\n        (function() {\n            let css_data = ");
        js.push_str(&js_data);
        js.push_str(";\n\n            const expected = window.__cssLoaderExpected instanceof Set ? window.__cssLoaderExpected : (window.__cssLoaderExpected = new Set());\n\n            ");
        js.push_str(extra);
        js.push_str("\n\n            if (document.head && !document.getElementById('test_css_loaded')) {\n                const marker = document.createElement('div');\n                marker.id = 'test_css_loaded';\n                document.head.append(marker);\n            }\n\n            css_data.add.forEach(x => {\n                expected.add(x.id);\n                if (document.getElementById(x.id) !== null){\n                    return;\n                }\n\n                let style = document.createElement('style');\n\t            style.id = x.id;\n                style.classList.add('css-loader-style');\n\t            document.head.append(style);\n\t            style.textContent = x.css;\n            });\n            \n            css_data.remove.forEach(x => {\n                expected.delete(x);\n                let style = document.getElementById(x);\n                style?.parentNode.removeChild(style);\n            });\n        })()\n        ");
        while retry > 0 {
            retry -= 1;
            let cdp = self.connection.lock().await.clone();
            if let Some(cdp) = &cdp
                && evaluate(cdp, &session, &js, true).await.is_some()
            {
                let styles_changed = {
                    let mut tabs = self.tabs.lock().await;
                    let Some(tab) = tabs.get_mut(id) else {
                        return Value::String("Undefined".into());
                    };
                    let mut changed = remove_all_first && !tab.applied_styles.is_empty();
                    if remove_all_first {
                        tab.applied_styles.clear();
                    }
                    for (style_id, css) in &adds {
                        if !tab.applied_styles.contains_key(style_id) {
                            tab.applied_styles.insert(style_id.clone(), css.clone());
                            changed = true;
                        }
                    }
                    for style_id in &removes {
                        changed |= tab.applied_styles.shift_remove(style_id).is_some();
                    }
                    changed
                };
                if styles_changed {
                    self.update_preload_script(id, cdp, &session).await;
                }
                return Value::String("Undefined".into());
            }
            // A pass that ends here is a retry, whether the command failed or there was no
            // connection left to send it on.
            if let Some(cdp) = &cdp {
                cdp.stats.retries.fetch_add(1, Ordering::Relaxed);
            }
            log::warn!("Transaction failed! retrying in 0.2 seconds");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        json!({"success": false, "message": "Css Commit Retry Count Exceeded"})
    }

    pub async fn remove_all(&self) {
        let ids: Vec<String> = {
            let mut tabs = self.tabs.lock().await;
            tabs.values_mut()
                .map(|tab| {
                    tab.pending_add.clear();
                    tab.pending_remove.clear();
                    tab.id.clone()
                })
                .collect()
        };
        for id in ids {
            let Some(session) = self.tabs.lock().await.get(&id).map(|x| x.session.clone()) else {
                continue;
            };
            let mut retry = 3;
            while retry > 0 {
                retry -= 1;
                let cdp = self.connection.lock().await.clone();
                if let Some(cdp) = &cdp
                    && evaluate(cdp, &session, REMOVE_ALL_JS, true).await.is_some()
                {
                    let styles_changed = {
                        let mut tabs = self.tabs.lock().await;
                        let Some(tab) = tabs.get_mut(&id) else {
                            break;
                        };
                        let changed = !tab.applied_styles.is_empty();
                        tab.applied_styles.clear();
                        changed
                    };
                    if styles_changed {
                        self.update_preload_script(&id, cdp, &session).await;
                    }
                    break;
                }
                if let Some(cdp) = &cdp {
                    cdp.stats.retries.fetch_add(1, Ordering::Relaxed);
                }
                log::warn!("Transaction failed! retrying in 0.2 seconds");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn dom_contains_selector(cdp: &Arc<Cdp>, session: &str, selector: &str) -> Option<bool> {
    let document = cdp
        .command("DOM.getDocument", json!({"depth": 0}), Some(session), true)
        .await?;
    let node_id = document.pointer("/result/root/nodeId")?.as_u64()?;
    let response = cdp
        .command(
            "DOM.querySelector",
            json!({"nodeId": node_id, "selector": selector}),
            Some(session),
            true,
        )
        .await?;
    Some(response.pointer("/result/nodeId")?.as_u64()? != 0)
}

async fn evaluate(
    cdp: &Arc<Cdp>,
    session: &str,
    expression: &str,
    get_result: bool,
) -> Option<Value> {
    let response = cdp.command("Runtime.evaluate", json!({
        "expression": expression, "userGesture": true, "awaitPromise": false, "returnByValue": true
    }), Some(session), get_result).await?;
    if let Some(result) = response.pointer("/result/result") {
        if let Some(value) = result.get("value") {
            Some(value.clone())
        } else if result.get("type").and_then(Value::as_str) == Some("undefined") {
            Some(Value::String("Undefined".into()))
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    /// Scripted CDP endpoint with one Steam popup, shaped like a live Big Picture window:
    /// the target URL is about:blank while the document reports the opener's URL.
    async fn fake_steam_popup_cdp(listener: TcpListener) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let info = json!({
            "targetId": "popup",
            "type": "page",
            "title": "Steam Big Picture Mode",
            "url": "about:blank?createflags=6292738&minwidth=853&minheight=534"
        });
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let command: Value = serde_json::from_str(&text).unwrap();
            let result = match command["method"].as_str().unwrap_or_default() {
                "Target.getTargets" => json!({"targetInfos": [info]}),
                "Target.attachToTarget" => {
                    let event = json!({
                        "method": "Target.attachedToTarget",
                        "params": {"sessionId": "session", "targetInfo": info}
                    });
                    ws.send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                    json!({"sessionId": "session"})
                }
                "Page.addScriptToEvaluateOnNewDocument" => json!({"identifier": "1"}),
                "Runtime.evaluate" if command["params"]["expression"] == TAB_INIT_JS => json!({
                    "result": {"type": "object", "value": {
                        "title": "Steam Big Picture Mode",
                        "url": "https://steamloopback.host/index.html?IN_GAMESCOPE=true",
                        "readyState": "complete",
                        "classes": []
                    }}
                }),
                "Runtime.evaluate" => json!({"result": {"type": "boolean", "value": true}}),
                _ => json!({}),
            };
            let response = json!({"id": command["id"], "result": result});
            ws.send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
        }
    }

    /// Scripted CDP endpoint for a tab that dies between the attach event and `Runtime.enable`, like
    /// a toast that closes while the plugin is still attaching: the command is never answered, and
    /// the target has left the target list by the time the plugin asks.
    async fn fake_dying_tab_cdp(listener: TcpListener) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let info = json!({"targetId": "dying", "type": "page", "title": "", "url": "about:blank"});
        let mut attached = false;
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let command: Value = serde_json::from_str(&text).unwrap();
            let method = command["method"].as_str().unwrap_or_default();
            if method == "Runtime.enable" {
                continue;
            }
            let result = match method {
                "Target.getTargets" => {
                    if attached {
                        json!({"targetInfos": []})
                    } else {
                        json!({"targetInfos": [info]})
                    }
                }
                "Target.attachToTarget" => {
                    attached = true;
                    let event = json!({
                        "method": "Target.attachedToTarget",
                        "params": {"sessionId": "session", "targetInfo": info}
                    });
                    ws.send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                    json!({"sessionId": "session"})
                }
                _ => json!({}),
            };
            let response = json!({"id": command["id"], "result": result});
            ws.send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
        }
    }

    fn cdp_for_tests(closed: bool) -> Cdp {
        let (tx, _) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(1);
        let (_closed_tx, closed) = watch::channel(closed);
        Cdp {
            tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events,
            closed,
            stats: Stats::default(),
        }
    }

    /// Toast, overlay and screensaver tabs close between the attach event and the first command, so
    /// the answer such a command gets must stay out of the log's warnings. Anything else must not.
    #[test]
    fn command_failures_of_a_closed_tab_are_not_warnings() {
        let live = cdp_for_tests(false);
        let error = |code: i64, message: &str| json!({"id": 9, "error": {"code": code, "message": message}});
        assert!(!live.tab_closed(Some(&json!({"id": 9, "result": {}}))));
        // Session-gone, by code and by CEF's wording, which is matched without case.
        assert!(live.tab_closed(Some(&error(-32001, "Session with given id not found."))));
        assert!(live.tab_closed(Some(&error(-32000, "SESSION WITH GIVEN ID NOT FOUND"))));
        assert!(live.tab_closed(Some(&error(-32602, "No target with given id found"))));
        assert!(live.tab_closed(Some(&error(-32000, "Target closed."))));
        assert!(live.tab_closed(Some(&error(
            -32000,
            "Not attached to an active page, session detached"
        ))));
        // A live tab that answered with any other error is a real failure: keep the warning.
        assert!(!live.tab_closed(Some(&error(-32601, "'Runtime.enable' wasn't found"))));
        assert!(!live.tab_closed(Some(&error(-32602, "Invalid parameters"))));
        // A live tab that never answers is a real failure too.
        assert!(!live.tab_closed(None));
        // A dropped connection takes every tab with it.
        assert!(cdp_for_tests(true).tab_closed(None));
        assert!(cdp_for_tests(true).tab_closed(Some(&json!({"id": 9, "result": {}}))));
    }

    /// A tab closed through CEF's HTTP API does not answer at all: the command times out while the
    /// detach event waits behind it in the event loop, so only the target list shows it is gone.
    #[test]
    fn a_silent_command_is_a_closed_tab_only_when_the_target_left_the_list() {
        let live = cdp_for_tests(false);
        assert!(tab_gone(&live, None, Some(false)));
        // A target that is still listed and never answered is a live tab that failed for real.
        assert!(!tab_gone(&live, None, Some(true)));
        // An unreadable target list (timeout, error, malformed) proves nothing: keep the warning.
        assert!(!tab_gone(&live, None, None));
        // The connection flag wins over the target list, which may be stale or unreadable.
        assert!(tab_gone(&cdp_for_tests(true), None, Some(true)));
        assert!(tab_gone(&cdp_for_tests(true), None, None));
        // An answered command is judged by its answer, never by the target list.
        assert!(!tab_gone(
            &live,
            Some(&json!({"id": 9, "result": {}})),
            None
        ));
        assert!(tab_gone(
            &live,
            Some(
                &json!({"id": 9, "error": {"code": -32001, "message": "Session with given id not found."}})
            ),
            None
        ));
        assert!(!tab_gone(
            &live,
            Some(
                &json!({"id": 9, "error": {"code": -32601, "message": "'Runtime.enable' wasn't found"}})
            ),
            None
        ));
    }

    #[test]
    fn target_list_answer_says_whether_the_page_is_still_listed() {
        let listed = json!({"id": 9, "result": {"targetInfos": [
            {"targetId": "other", "type": "page"},
            {"targetId": "tab", "type": "page"}
        ]}});
        assert_eq!(lists_target(&listed, "tab"), Some(true));
        assert_eq!(lists_target(&listed, "gone"), Some(false));
        assert_eq!(
            lists_target(&json!({"id": 9, "result": {"targetInfos": []}}), "tab"),
            Some(false)
        );
        // Error and malformed answers are unknown, not proof of absence.
        assert_eq!(
            lists_target(
                &json!({"id": 9, "error": {"code": -32601, "message": "not found"}}),
                "tab"
            ),
            None
        );
        assert_eq!(lists_target(&json!({"id": 9, "result": {}}), "tab"), None);
    }

    /// A browser that does not answer `Target.getTargets` gives no proof the tab is gone, so the
    /// silent command it follows must still warn.
    #[tokio::test]
    async fn an_unanswered_target_list_keeps_the_warning() {
        let live = cdp_for_tests(false);
        let listed = Browser::new().target_listed(&live, "tab").await;
        assert_eq!(listed, None);
        assert!(!tab_gone(&live, None, listed));
    }

    #[tokio::test]
    async fn initializes_popup_whose_document_url_differs_from_target_url() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(fake_steam_popup_cdp(listener));
        let (stream, _) = connect_async(url).await.unwrap();
        let browser = Browser::new();
        let connection = Arc::clone(&browser);
        tokio::spawn(async move { connection.connected(stream, Weak::new()).await });

        let initialized = timeout(Duration::from_secs(3), async {
            while !browser
                .tabs
                .lock()
                .await
                .get("popup")
                .is_some_and(|tab| tab.initialized)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(initialized.is_ok(), "the Steam popup was never initialized");
    }

    /// An unanswered attach command used to block the event loop for the full command timeout. It
    /// must give up inside the attach budget, and the tab must be dropped once the target is gone.
    #[tokio::test]
    async fn unanswered_attach_command_gives_up_within_the_attach_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(fake_dying_tab_cdp(listener));
        let (stream, _) = connect_async(url).await.unwrap();
        let browser = Browser::new();
        let connection = Arc::clone(&browser);
        tokio::spawn(async move { connection.connected(stream, Weak::new()).await });

        let appeared = timeout(Duration::from_secs(2), async {
            while !browser.tabs.lock().await.contains_key("dying") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(appeared.is_ok(), "the dying tab was never attached");

        let started = std::time::Instant::now();
        let dropped = timeout(Duration::from_secs(4), async {
            while browser.tabs.lock().await.contains_key("dying") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(dropped.is_ok(), "the dead tab was never dropped");
        // It really did wait for the unanswered command, but inside the budget, not the 5 s default.
        assert!(started.elapsed() >= Duration::from_millis(500));
    }

    /// The browser is down for a while on every Steam restart while the loop probes it every 3 s.
    /// One warning once the outage matters, one line when it ends, and nothing in between.
    #[test]
    fn a_long_outage_warns_once_and_reports_how_long_it_lasted() {
        let mut outage = Outage::default();
        let start = Instant::now();
        assert_eq!(outage.failed(start, "connect refused"), None);
        assert_eq!(
            outage.failed(start + Duration::from_secs(27), "connect refused"),
            None
        );
        let warning = outage
            .failed(start + Duration::from_secs(30), "connect refused")
            .expect("the outage should be reported after 30 s");
        assert!(warning.contains("30s"), "{warning}");
        assert!(warning.contains("connect refused"), "{warning}");
        // Still down: no warning repeats, however long it takes or whatever the last error was.
        for seconds in [60, 300, 3600] {
            assert_eq!(
                outage.failed(start + Duration::from_secs(seconds), "timed out"),
                None
            );
        }
        let recovery = outage
            .recovered(start + Duration::from_secs(3660))
            .expect("a reported outage should report its end");
        assert!(recovery.contains("1h 1m"), "{recovery}");
        // The next outage starts from zero, so a long one warns again.
        assert_eq!(
            outage.failed(start + Duration::from_secs(4000), "eof"),
            None
        );
        assert!(
            outage
                .failed(start + Duration::from_secs(4030), "eof")
                .is_some()
        );
    }

    #[test]
    fn an_outage_shorter_than_the_threshold_stays_quiet() {
        let mut outage = Outage::default();
        let start = Instant::now();
        assert_eq!(outage.failed(start, "connect refused"), None);
        assert_eq!(
            outage.recovered(start + Duration::from_secs(6)),
            None,
            "a quiet failure must not pair with a recovery line"
        );
        // No probe ever failed: nothing to report either.
        assert_eq!(outage.recovered(start + Duration::from_secs(9)), None);
    }

    /// A busy page can drop many events at once; the log gets one line per minute that carries the
    /// sum of everything dropped so far in that window.
    #[test]
    fn lag_warnings_are_rate_limited_and_carry_the_sum() {
        let mut lag = LagLog::default();
        let start = Instant::now();
        let first = lag
            .dropped(5, start)
            .expect("the first lag is worth a line");
        assert!(first.contains("dropped 5 "), "{first}");
        // Lags inside the window are summed, not logged.
        assert_eq!(lag.dropped(7, start + Duration::from_secs(10)), None);
        assert_eq!(lag.dropped(1, start + Duration::from_secs(59)), None);
        // The first lag after the window reports what the quiet lags added up to.
        let next = lag
            .dropped(2, start + Duration::from_secs(60))
            .expect("the window is over");
        assert!(next.contains("10"), "{next}");
        // The total keeps growing even while no line is due: the summary needs it.
        assert_eq!(lag.total, 15);
        assert_eq!(lag.dropped(3, start + Duration::from_secs(61)), None);
        assert_eq!(lag.total, 18);
        assert_eq!(lag.pending, 3);
    }

    #[test]
    fn data_url_titles_are_shortened_on_a_character_boundary() {
        // Ordinary titles are passed through untouched.
        assert_eq!(
            short_title("Steam Big Picture Mode").as_ref(),
            "Steam Big Picture Mode"
        );
        // A data URL that already fits stays as it is.
        let short = "data:text/html;base64,AA";
        assert_eq!(short.len(), 24);
        assert_eq!(short_title(short).as_ref(), short);
        // Longer ones keep a recognisable prefix and say they were cut.
        let long = "data:text/html,<html><body>hello</body></html>";
        assert_eq!(short_title(long).as_ref(), "data:text/html,<html><bo…");
        // Multi-byte text is cut between characters, never inside one.
        let text = "data:text/plain,日本語のテキストです";
        let shortened = short_title(text);
        assert!(text.starts_with(shortened.trim_end_matches('…')));
        assert_eq!(shortened.chars().count(), 25);
        assert!(shortened.ends_with('…'));
    }

    #[test]
    fn only_transactions_with_changes_log_at_info() {
        assert_eq!(transaction_level(0, 0), log::Level::Debug);
        assert_eq!(transaction_level(1, 0), log::Level::Info);
        assert_eq!(transaction_level(0, 2), log::Level::Info);
    }
}
