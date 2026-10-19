use futures_util::{SinkExt, StreamExt};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::{collections::{HashMap, HashSet}, sync::{Arc, LazyLock, Weak, atomic::{AtomicU64, Ordering}}, time::Duration};
use tokio::{net::TcpStream, sync::{Mutex, broadcast, mpsc, oneshot, watch}, time::{interval, timeout}};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use crate::{Backend, model::python_json_dump};

const TAB_INIT_JS: &str = "(function(){ return {\"title\": document.title, \"classes\": Array.from(document.documentElement.classList).concat(Array.from(document.body.classList)).concat(Array.from(document.head.classList))} })()";
const MARKER_JS: &str = r#"
                    (function() {
                        const elem = document.createElement('div');
                        elem.id = "test_css_loaded";
                        document.head.append(elem);
                    })()
                    "#;
const REMOVE_ALL_JS: &str = "\n        (function() {\n            document.querySelectorAll('.css-loader-style').forEach(x => x.remove());\n        })()\n        ";

#[derive(Clone, Debug)]
struct Tab {
    id: String,
    session: String,
    title: String,
    url: String,
    classes: Vec<String>,
    initialized: bool,
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
            match fancy_regex::Regex::new(&format!("^({selector})")) {
                Ok(pattern) => pattern.is_match(&self.title).unwrap_or(false),
                Err(error) => {
                    static WARNED: LazyLock<std::sync::Mutex<HashSet<String>>> = LazyLock::new(Default::default);
                    if WARNED.lock().unwrap().insert(selector.to_owned()) {
                        log::warn!("Invalid title selector '{selector}': {error}");
                    }
                    false
                }
            }
        }
    }
}

struct Cdp {
    tx: mpsc::UnboundedSender<Message>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    events: broadcast::Sender<Value>,
    closed: watch::Receiver<bool>,
}

impl Cdp {
    async fn command(&self, method: &str, params: Value, session: Option<&str>, wait_for_response: bool) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (response_tx, response_rx) = oneshot::channel();
        if wait_for_response {
            self.pending.lock().await.insert(id, response_tx);
        }
        let mut command = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session { command["sessionId"] = Value::String(session.to_owned()); }
        if self.tx.send(Message::Text(command.to_string().into())).is_err() {
            self.pending.lock().await.remove(&id);
            return None;
        }
        if wait_for_response {
            match timeout(Duration::from_secs(5), response_rx).await {
                Ok(Ok(value)) => Some(value),
                _ => { self.pending.lock().await.remove(&id); None }
            }
        } else { None }
    }
}

#[derive(Default)]
pub struct Browser {
    tabs: Mutex<HashMap<String, Tab>>,
    connection: Mutex<Option<Arc<Cdp>>>,
}

impl Browser {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    pub fn start(self: &Arc<Self>, backend: Weak<Backend>) {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.health_loop(backend).await; });
    }

    async fn health_loop(self: Arc<Self>, backend: Weak<Backend>) {
        let client = match reqwest::Client::builder().timeout(Duration::from_secs(3)).build() {
            Ok(client) => client,
            Err(error) => { log::error!("CEF HTTP client failed: {error}"); return; }
        };
        loop {
            let connected = self.connection.lock().await.is_some();
            if !connected {
                match client.get("http://127.0.0.1:8080/json/version").send().await {
                    Ok(response) if response.status().is_success() => match response.json::<Value>().await {
                        Ok(value) => {
                            if let Some(url) = value.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                                match connect_async(url).await {
                                    Ok((stream, _)) => {
                                        log::info!("Connected to Steam Browser");
                                        self.connected(stream, backend.clone()).await;
                                    }
                                    Err(error) => log::debug!("CEF websocket connection failed: {error}"),
                                }
                            }
                        }
                        Err(error) => log::debug!("Invalid CEF version response: {error}"),
                    },
                    Ok(response) => log::debug!("/json/version returned {}", response.status()),
                    Err(error) => log::debug!("CEF version endpoint unavailable: {error}"),
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    async fn connected(self: &Arc<Self>, stream: WebSocketStream<MaybeTlsStream<TcpStream>>, backend: Weak<Backend>) {
        let (mut sink, mut source) = stream.split();
        let (tx, mut outgoing) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(500);
        let (closed_tx, closed) = watch::channel(false);
        let cdp = Arc::new(Cdp { tx, pending: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1), events, closed });
        *self.connection.lock().await = Some(Arc::clone(&cdp));

        let writer_closed = closed_tx.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                if sink.send(message).await.is_err() { break; }
            }
            let _ = writer_closed.send(true);
        });
        let reader_cdp = Arc::clone(&cdp);
        let reader = tokio::spawn(async move {
            while let Some(Ok(message)) = source.next().await {
                if let Message::Text(text) = message {
                    let Ok(value) = serde_json::from_str::<Value>(&text) else { continue; };
                    if let Some(id) = value.get("id").and_then(Value::as_u64) {
                        if let Some(sender) = reader_cdp.pending.lock().await.remove(&id) { let _ = sender.send(value); }
                    } else { let _ = reader_cdp.events.send(value); }
                } else if matches!(message, Message::Close(_)) { break; }
            }
            let _ = closed_tx.send(true);
        });

        let mut event_rx = cdp.events.subscribe();
        let _ = cdp.command("Target.setDiscoverTargets", json!({"discover": true}), None, false).await;
        self.attach_existing(&cdp).await;
        let mut closed_rx = cdp.closed.clone();
        let mut sanity = interval(Duration::from_secs(5));
        let mut health = interval(Duration::from_secs(3));
        sanity.tick().await;
        health.tick().await;
        loop {
            tokio::select! {
                event = event_rx.recv() => match event {
                    Ok(value) => self.handle_event(value, &cdp, backend.clone()).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => {},
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = sanity.tick() => self.sanity_check(&cdp, backend.clone()).await,
                _ = health.tick() => self.health_tabs(&cdp, backend.clone()).await,
                changed = closed_rx.changed() => if changed.is_err() || *closed_rx.borrow() { break; },
            }
        }
        self.tabs.lock().await.clear();
        *self.connection.lock().await = None;
        writer.abort();
        reader.abort();
    }

    async fn attach_existing(&self, cdp: &Arc<Cdp>) {
        if let Some(response) = cdp.command("Target.getTargets", json!({}), None, true).await {
            if let Some(targets) = response.pointer("/result/targetInfos").and_then(Value::as_array) {
                for target in targets {
                    if target.get("type").and_then(Value::as_str) == Some("page") {
                        let id = target.get("targetId").and_then(Value::as_str).unwrap_or_default();
                        let _ = cdp.command("Target.attachToTarget", json!({"targetId": id, "flatten": true}), None, false).await;
                    }
                }
            }
        }
    }

    async fn handle_event(&self, event: Value, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        match event.get("method").and_then(Value::as_str).unwrap_or_default() {
            "Target.targetCreated" => {
                if event.pointer("/params/targetInfo/type").and_then(Value::as_str) == Some("page") {
                    let id = event.pointer("/params/targetInfo/targetId").and_then(Value::as_str).unwrap_or_default();
                    if let Some(response) = cdp.command("Target.getTargets", json!({}), None, true).await {
                        let exists = response.pointer("/result/targetInfos").and_then(Value::as_array).is_some_and(|xs| xs.iter().any(|x| x.get("targetId").and_then(Value::as_str) == Some(id) && x.get("type").and_then(Value::as_str) == Some("page")));
                        if exists { let _ = cdp.command("Target.attachToTarget", json!({"targetId": id, "flatten": true}), None, false).await; }
                    }
                }
            }
            "Target.attachedToTarget" => self.attach_tab(event.pointer("/params").unwrap_or(&Value::Null).clone(), cdp, backend).await,
            "Target.detachedFromTarget" => {
                if let Some(id) = event.pointer("/params/targetId").and_then(Value::as_str) {
                    if let Some(tab) = self.tabs.lock().await.remove(id) { log::info!("Disconnected from tab: {}", tab.title); }
                }
            }
            "Target.targetInfoChanged" => {
                if let Some(info) = event.pointer("/params/targetInfo") {
                    let Some(id) = info.get("targetId").and_then(Value::as_str) else { return; };
                    let changed = {
                        let mut tabs = self.tabs.lock().await;
                        if let Some(tab) = tabs.get_mut(id) {
                            let changed = tab.title != info.get("title").and_then(Value::as_str).unwrap_or_default() || tab.url != info.get("url").and_then(Value::as_str).unwrap_or_default();
                            tab.title = info.get("title").and_then(Value::as_str).unwrap_or_default().to_owned();
                            tab.url = info.get("url").and_then(Value::as_str).unwrap_or_default().to_owned();
                            changed
                        } else { false }
                    };
                    if changed { if let Some(backend) = backend.upgrade() { backend.force_reinject(id).await; } }
                }
            }
            _ => {}
        }
    }

    async fn attach_tab(&self, params: Value, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let Some(info) = params.get("targetInfo") else { return; };
        let (Some(id), Some(session)) = (info.get("targetId").and_then(Value::as_str), params.get("sessionId").and_then(Value::as_str)) else { return; };
        if self.tabs.lock().await.contains_key(id) { return; }
        let tab = Tab {
            id: id.to_owned(), session: session.to_owned(),
            title: info.get("title").and_then(Value::as_str).unwrap_or_default().to_owned(), url: info.get("url").and_then(Value::as_str).unwrap_or_default().to_owned(),
            classes: Vec::new(), initialized: false, pending_add: IndexMap::new(), pending_remove: Vec::new(),
        };
        self.tabs.lock().await.insert(id.to_owned(), tab);
        let evaluated = evaluate(cdp, session, TAB_INIT_JS, true).await;
        let Some(value) = evaluated.and_then(|x| x.as_object().cloned()) else {
            log::warn!("Failed to connect to tab with id {id}");
            self.tabs.lock().await.remove(id);
            return;
        };
        {
            let mut tabs = self.tabs.lock().await;
            let Some(tab) = tabs.get_mut(id) else { return; };
            tab.title = value.get("title").and_then(Value::as_str).unwrap_or(&tab.title).to_owned();
            tab.classes = value.get("classes").and_then(Value::as_array).map(|x| x.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
            tab.initialized = true;
            log::info!("Connected to tab: {}", tab.title);
        }
        self.health_tab(id, cdp, backend).await;
    }

    async fn sanity_check(&self, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let Some(response) = cdp.command("Target.getTargets", json!({}), None, true).await else { return; };
        let Some(targets) = response.pointer("/result/targetInfos").and_then(Value::as_array) else { return; };
        let page_ids: Vec<_> = targets.iter().filter(|x| x.get("type").and_then(Value::as_str) == Some("page")).filter_map(|x| x.get("targetId").and_then(Value::as_str).map(str::to_owned)).collect();
        self.tabs.lock().await.retain(|id, tab| {
            let keep = page_ids.contains(id);
            if !keep { log::info!("Disconnected from tab: {}", tab.title); }
            keep
        });
        for target in targets.iter().filter(|x| x.get("type").and_then(Value::as_str) == Some("page")) {
            let Some(id) = target.get("targetId").and_then(Value::as_str) else { continue; };
            let mut changed = false;
            {
                let mut tabs = self.tabs.lock().await;
                if let Some(tab) = tabs.get_mut(id) {
                    let title = target.get("title").and_then(Value::as_str).unwrap_or_default();
                    let url = target.get("url").and_then(Value::as_str).unwrap_or_default();
                    changed = tab.title != title || tab.url != url;
                    tab.title = title.to_owned(); tab.url = url.to_owned();
                }
            }
            if changed { if let Some(backend) = backend.upgrade() { backend.force_reinject(id).await; } }
            else if !self.tabs.lock().await.contains_key(id) {
                let _ = cdp.command("Target.attachToTarget", json!({"targetId": id, "flatten": true}), None, false).await;
            }
        }
    }

    async fn health_tabs(&self, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let ids: Vec<String> = self.tabs.lock().await.keys().cloned().collect();
        for id in ids { self.health_tab(&id, cdp, backend.clone()).await; }
    }

    async fn health_tab(&self, id: &str, cdp: &Arc<Cdp>, backend: Weak<Backend>) {
        let Some((session, initialized)) = self.tabs.lock().await.get(id).map(|x| (x.session.clone(), x.initialized)) else { return; };
        if !initialized { return; }
        if evaluate(cdp, &session, "document.getElementById('test_css_loaded') != null", true).await == Some(Value::Bool(true)) { return; }
        let _ = evaluate(cdp, &session, MARKER_JS, true).await;
        if evaluate(cdp, &session, "document.getElementById('test_css_loaded') != null", true).await != Some(Value::Bool(true)) {
            log::error!("a variable was injected but cannot be found when retrieved");
            return;
        }
        if let Some(backend) = backend.upgrade() { backend.force_reinject(id).await; }
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
            if tab.pending_add.shift_remove(id).is_none() { tab.pending_remove.push(id.to_owned()); }
        }
    }

    pub async fn inject_on_tab(&self, target_id: &str, selector: &str, css: &str, id: &str) {
        let mut tabs = self.tabs.lock().await;
        if let Some(tab) = tabs.get_mut(target_id) {
            if tab.compare(selector) { tab.pending_add.insert(id.to_owned(), css.to_owned()); }
        }
    }

    pub async fn tab_matches(&self, target_id: &str, selector: &str) -> bool {
        self.tabs.lock().await.get(target_id).is_some_and(|x| x.compare(selector))
    }

    pub async fn commit_target(&self, id: &str, remove_all_first: bool) {
        let _ = self.commit_tab(id, 3, remove_all_first).await;
    }

    pub async fn commit_all(&self, remove_all_first: bool) {
        let ids: Vec<String> = self.tabs.lock().await.keys().cloned().collect();
        for id in ids { let _ = self.commit_tab(&id, 3, remove_all_first).await; }
    }

    async fn commit_tab(&self, id: &str, mut retry: usize, remove_all_first: bool) -> Value {
        let (session, adds, removes, title) = {
            let mut tabs = self.tabs.lock().await;
            let Some(tab) = tabs.get_mut(id) else { return json!({"success": true, "message": "Success"}); };
            if tab.pending_add.is_empty() && tab.pending_remove.is_empty() { return json!({"success": true, "message": "Success"}); }
            let adds = std::mem::take(&mut tab.pending_add);
            let removes = std::mem::take(&mut tab.pending_remove);
            (tab.session.clone(), adds, removes, tab.title.clone())
        };
        log::info!("Committing css transaction on {} +{} -{}", title, adds.len(), removes.len());
        let add = adds.iter().map(|(id, css)| json!({"id": id, "css": css})).collect::<Vec<_>>();
        let js_data = python_json_dump(&json!({"add": add, "remove": removes}));
        let extra = if remove_all_first { "document.querySelectorAll('.css-loader-style').forEach(x => x.remove());" } else { "" };
        let mut js = String::from("\n        (function() {\n            let css_data = ");
        js.push_str(&js_data);
        js.push_str(";\n\n            ");
        js.push_str(extra);
        js.push_str("\n\n            css_data.add.forEach(x => {\n                if (document.getElementById(x.id) !== null){\n                    return;\n                }\n\n                let style = document.createElement('style');\n\t            style.id = x.id;\n                style.classList.add('css-loader-style');\n\t            document.head.append(style);\n\t            style.textContent = x.css;\n            });\n            \n            css_data.remove.forEach(x => {\n                let style = document.getElementById(x);\n                style?.parentNode.removeChild(style);\n            });\n        })()\n        ");
        while retry > 0 {
            retry -= 1;
            let cdp = self.connection.lock().await.clone();
            if let Some(cdp) = cdp {
                if evaluate(&cdp, &session, &js, true).await.is_some() { return Value::String("Undefined".into()); }
            }
            log::warn!("Transaction failed! retrying in 0.2 seconds");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        json!({"success": false, "message": "Css Commit Retry Count Exceeded"})
    }

    pub async fn remove_all(&self) {
        let ids: Vec<String> = {
            let mut tabs = self.tabs.lock().await;
            tabs.values_mut().map(|tab| { tab.pending_add.clear(); tab.pending_remove.clear(); tab.id.clone() }).collect()
        };
        for id in ids {
            let Some(session) = self.tabs.lock().await.get(&id).map(|x| x.session.clone()) else { continue; };
            let mut retry = 3;
            while retry > 0 {
                retry -= 1;
                if let Some(cdp) = self.connection.lock().await.clone() {
                    if evaluate(&cdp, &session, REMOVE_ALL_JS, true).await.is_some() { break; }
                }
                log::warn!("Transaction failed! retrying in 0.2 seconds");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }

}

async fn evaluate(cdp: &Arc<Cdp>, session: &str, expression: &str, get_result: bool) -> Option<Value> {
    let response = cdp.command("Runtime.evaluate", json!({
        "expression": expression, "userGesture": true, "awaitPromise": false, "returnByValue": true
    }), Some(session), get_result).await?;
    if let Some(result) = response.pointer("/result/result") {
        if let Some(value) = result.get("value") { Some(value.clone()) }
        else if result.get("type").and_then(Value::as_str) == Some("undefined") { Some(Value::String("Undefined".into())) }
        else { None }
    } else { None }
}

