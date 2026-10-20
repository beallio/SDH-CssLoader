mod browser;
pub mod model;
mod server;

use browser::Browser;
use decky_plugin::{Context, Env, serde_json::Value};
use model::{
    CSS_LOADER_VER, FLAG_KEEP_DEPENDENCIES, FLAG_PRESET, Inject, Theme, default_mappings,
    dependency_names, load_class_mappings, python_json_dump,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Map, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpStream as StdTcpStream},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, Weak},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, watch};

static BACKEND: OnceLock<Arc<Backend>> = OnceLock::new();
const MAX_MANIFEST_VERSION: u64 = 9;
const MAPPING_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

struct Core {
    root: PathBuf,
    user: String,
    steam: PathBuf,
    themes: Vec<Theme>,
    injects: Vec<Inject>,
    ordered_injects: Vec<usize>,
    errors: Vec<Value>,
    class_mappings: HashMap<String, String>,
    last_mapping_fetch: Option<Instant>,
    /// URL and ETag of the mappings now in `class_mappings`.
    mapping_etag: Option<(String, String)>,
    busy: bool,
    server_enabled: bool,
    watcher: Option<RecommendedWatcher>,
}

pub struct Backend {
    core: Mutex<Core>,
    mutations: Mutex<()>,
    mapping_fetch: Mutex<()>,
    browser: Arc<Browser>,
    ready: watch::Sender<bool>,
}

impl Backend {
    fn from_env(env: &Env) -> Result<Arc<Self>, String> {
        let home = env.home.clone().into_os_string();
        let home = if home.is_empty() {
            dirs_home()
        } else {
            PathBuf::from(home)
        };
        let decky_home = if env.decky_home.as_os_str().is_empty() {
            home.join("homebrew")
        } else {
            env.decky_home.clone()
        };
        let root = decky_home.join("themes");
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let steam = home.join(".steam/steam");
        let (ready, _) = watch::channel(false);
        Ok(Arc::new(Self {
            core: Mutex::new(Core {
                root,
                user: if env.user.is_empty() {
                    "user".into()
                } else {
                    env.user.clone()
                },
                steam,
                themes: Vec::new(),
                injects: Vec::new(),
                ordered_injects: Vec::new(),
                errors: Vec::new(),
                class_mappings: HashMap::new(),
                last_mapping_fetch: None,
                mapping_etag: None,
                busy: false,
                server_enabled: false,
                watcher: None,
            }),
            mutations: Mutex::new(()),
            mapping_fetch: Mutex::new(()),
            browser: Browser::new(),
            ready,
        }))
    }

    pub async fn open_for_tests(root: PathBuf, user: &str) -> Result<Arc<Self>, String> {
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let (ready, _) = watch::channel(true);
        let home = root.parent().unwrap_or(&root).to_path_buf();
        let backend = Arc::new(Self {
            core: Mutex::new(Core {
                root,
                user: user.to_owned(),
                steam: home.join(".steam/steam"),
                themes: Vec::new(),
                injects: Vec::new(),
                ordered_injects: Vec::new(),
                errors: Vec::new(),
                class_mappings: HashMap::new(),
                last_mapping_fetch: None,
                mapping_etag: None,
                busy: false,
                server_enabled: false,
                watcher: None,
            }),
            mutations: Mutex::new(()),
            mapping_fetch: Mutex::new(()),
            browser: Browser::new(),
            ready,
        });
        let mappings =
            load_class_mappings(&backend.core.lock().await.root.join("css_translations.json"))
                .unwrap_or_default();
        backend.core.lock().await.class_mappings = mappings;
        backend.load(false).await?;
        Ok(backend)
    }

    async fn wait_ready(&self) {
        let mut receiver = self.ready.subscribe();
        while !*receiver.borrow() {
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }

    async fn initialize(self: &Arc<Self>) -> Result<(), String> {
        {
            let mut core = self.core.lock().await;
            let mapping_path = core.root.join("css_translations.json");
            match load_class_mappings(&mapping_path) {
                Ok(mappings) => core.class_mappings = mappings,
                Err(error) if mapping_path.exists() => {
                    log::warn!("Failed to load css translations from local file: {error}")
                }
                _ => log::info!("Failed to get css translations from local file"),
            }
        }
        log::info!("Initializing css loader; max manifest version: {CSS_LOADER_VER}");
        self.create_steam_symlink().await;
        self.load(false).await?;
        let settings = {
            let core = self.core.lock().await;
            (
                store_or_file_config(&core.root, "watch"),
                store_or_file_config(&core.root, "server"),
                core.themes.len(),
                core.injects.iter().filter(|x| x.enabled).count(),
            )
        };
        if settings.0 {
            let _ = self.toggle_watch_state(true, false).await;
        } else {
            log::info!("Not observing themes folder for file changes");
        }
        log::info!(
            "Initialized css loader. Found {} themes. Total {} injects, {} injected",
            settings.2,
            self.core.lock().await.injects.len(),
            settings.3
        );
        if settings.1 {
            let _ = self.enable_server().await;
        }
        let backend = Arc::downgrade(self);
        tokio::spawn(async move {
            fetch_mappings_periodically(backend).await;
        });
        self.browser.start(Arc::downgrade(self));
        Ok(())
    }

    async fn create_steam_symlink(&self) {
        let core = self.core.lock().await;
        let destination = core.steam.join("steamui/themes_custom");
        if !destination.exists()
            && let Err(error) = std::os::unix::fs::symlink(&core.root, &destination)
        {
            log::warn!("Failed to create Steam theme symlink: {error}");
        }
    }

    async fn load(&self, inject_now: bool) -> Result<(), String> {
        let mut core = self.core.lock().await;
        load_core(&mut core, &self.browser, inject_now).await
    }

    async fn reset(&self, silent: bool) -> Value {
        {
            let mut core = self.core.lock().await;
            while core.busy {
                drop(core);
                tokio::time::sleep(Duration::from_millis(100)).await;
                core = self.core.lock().await;
            }
            core.busy = true;
        }
        if !silent {
            self.browser.remove_all().await;
        }
        let result = self.load(true).await;
        if let Err(error) = result {
            log::error!("Reset failed: {error}");
        }
        if silent {
            self.browser.commit_all(true).await;
        } else {
            self.browser.commit_all(false).await;
        }
        let errors = {
            let mut core = self.core.lock().await;
            core.busy = false;
            core.errors.clone()
        };
        json!({"fails": errors})
    }

    async fn enable_server(self: &Arc<Self>) -> Value {
        {
            let mut core = self.core.lock().await;
            if core.server_enabled {
                return result(false, "Nothing to do!");
            }
            core.server_enabled = true;
            let cef_flag = core.steam.join(".cef-enable-remote-debugging");
            if !cef_flag.exists()
                && let Err(error) = fs::write(cef_flag, [])
            {
                log::warn!("Failed to create steam cef flag. {error}");
            }
        }
        server::start(Arc::downgrade(self));
        result(true, "Success")
    }

    async fn toggle_watch_state(self: &Arc<Self>, enable: bool, only_this_session: bool) -> Value {
        let mut core = self.core.lock().await;
        if enable && core.watcher.is_none() {
            let (sender, mut receiver) =
                tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();
            let mut watcher = match notify::recommended_watcher(move |event| {
                let _ = sender.send(event);
            }) {
                Ok(watcher) => watcher,
                Err(error) => return result(false, error.to_string()),
            };
            if let Err(error) = watcher.watch(&core.root, RecursiveMode::Recursive) {
                return result(false, error.to_string());
            }
            core.watcher = Some(watcher);
            if !only_this_session {
                let _ = store_write(&core.root, "watch", "1");
            }
            log::info!("Observing themes folder for file changes");
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                let mut last = Instant::now() - Duration::from_secs(2);
                while let Some(event) = receiver.recv().await {
                    let Ok(event) = event else {
                        continue;
                    };
                    if !event.kind.is_modify() || event.paths.iter().any(|path| path.is_dir()) {
                        continue;
                    }
                    if !event.paths.iter().any(|path| {
                        path.to_string_lossy().ends_with(".css")
                            || path.to_string_lossy().ends_with("theme.json")
                    }) {
                        continue;
                    }
                    let now = Instant::now();
                    if now.duration_since(last) <= Duration::from_secs(1) {
                        continue;
                    }
                    if let Some(backend) = weak.upgrade() {
                        if backend.core.lock().await.busy {
                            continue;
                        }
                        last = now;
                        log::info!("Reloading themes due to FS event");
                        let _ = backend.reset(true).await;
                    } else {
                        break;
                    }
                }
            });
            result(true, "Success")
        } else if core.watcher.is_some() && !enable {
            log::info!("Stopping observer");
            core.watcher.take();
            if !only_this_session {
                let _ = store_write(&core.root, "watch", "0");
            }
            result(true, "Success")
        } else {
            result(false, "Nothing to do!")
        }
    }

    async fn set_theme_state(
        &self,
        name: &str,
        state: bool,
        set_deps: bool,
        set_deps_value: bool,
    ) -> Value {
        let _mutation = self.mutations.lock().await;
        let mut core = self.core.lock().await;
        let Some(index) = core.themes.iter().position(|x| x.name == name) else {
            return result(false, format!("Did not find theme {name}"));
        };
        let outcome = if state {
            enable_theme(
                &mut core,
                &self.browser,
                index,
                set_deps,
                set_deps_value,
                &[],
                0,
            )
            .await
        } else {
            let keep_dependencies = core.themes[index]
                .flags
                .iter()
                .any(|x| x == FLAG_KEEP_DEPENDENCIES);
            disable_theme(&mut core, &self.browser, index, keep_dependencies, 0).await
        };
        drop(core);
        self.browser.commit_all(false).await;
        result(outcome.0, &outcome.1)
    }

    async fn set_patch(&self, theme_name: &str, patch_name: &str, value: &str) -> Value {
        let _mutation = self.mutations.lock().await;
        let mut core = self.core.lock().await;
        let Some(theme_index) = core.themes.iter().position(|x| x.name == theme_name) else {
            return result(false, format!("Did not find theme '{theme_name}'"));
        };
        let Some(patch_index) = core.themes[theme_index]
            .patches
            .iter()
            .position(|x| x.name == patch_name)
        else {
            return result(
                false,
                format!("Did not find patch '{patch_name}' for theme '{theme_name}'"),
            );
        };
        if core.themes[theme_index].patches[patch_index].value == value {
            return result(true, "Already injected");
        }
        if core.themes[theme_index].patches[patch_index]
            .options
            .contains_key(value)
        {
            core.themes[theme_index].patches[patch_index].value = value.to_owned();
        }
        if core.themes[theme_index].enabled {
            remove_patch(&mut core, &self.browser, theme_index, patch_index).await;
            let _ = inject_patch(&mut core, &self.browser, theme_index, patch_index, true).await;
        }
        let user = core.user.clone();
        let _ = core.themes[theme_index].save(&user);
        drop(core);
        self.browser.commit_all(false).await;
        result(true, "Success")
    }

    async fn set_component(
        &self,
        theme_name: &str,
        patch_name: &str,
        component_name: &str,
        value: &str,
    ) -> Value {
        let _mutation = self.mutations.lock().await;
        let mut core = self.core.lock().await;
        let Some(theme_index) = core.themes.iter().position(|x| x.name == theme_name) else {
            return result(false, format!("Did not find theme '{theme_name}'"));
        };
        let Some(patch_index) = core.themes[theme_index]
            .patches
            .iter()
            .position(|x| x.name == patch_name)
        else {
            return result(
                false,
                format!("Did not find patch '{patch_name}' for theme '{theme_name}'"),
            );
        };
        let Some(component_index) = core.themes[theme_index].patches[patch_index]
            .components
            .iter()
            .position(|x| x.name == component_name)
        else {
            return result(
                false,
                format!("Failed to find component '{component_name}'"),
            );
        };
        let inject_id =
            core.themes[theme_index].patches[patch_index].components[component_index].inject_id;
        core.themes[theme_index].patches[patch_index].components[component_index].value =
            value.to_owned();
        if let Err(error) = update_component(&mut core, theme_index, patch_index, component_index) {
            return result(false, &error);
        }
        if core.injects[inject_id].enabled
            && let Err(error) = inject_one(&mut core, &self.browser, inject_id, true).await
        {
            return result(false, &error);
        }
        let user = core.user.clone();
        let _ = core.themes[theme_index].save(&user);
        drop(core);
        self.browser.commit_all(false).await;
        result(true, "Success")
    }

    async fn delete_theme(&self, name: &str) -> Value {
        let mut core = self.core.lock().await;
        let Some(index) = core.themes.iter().position(|x| x.name == name) else {
            return result(false, format!("Could not find theme {name}"));
        };
        if core.themes[index].bundled {
            return result(false, "Can't delete a bundled theme");
        }
        remove_theme(&mut core, &self.browser, index).await;
        let path = core.themes[index].theme_path.clone();
        if let Err(error) = fs::remove_dir_all(path) {
            return result(false, error.to_string());
        }
        core.themes.remove(index);
        rebuild_inject_indices(&mut core);
        result(true, "Success")
    }

    async fn generate_preset(&self, name: &str, selected: Option<&[String]>) -> Value {
        let mut core = self.core.lock().await;
        let mut dependencies = Map::new();
        for theme in &core.themes {
            let include = selected.map_or(theme.enabled, |names| names.contains(&theme.name));
            if include && !theme.flags.iter().any(|x| x == FLAG_PRESET) {
                let mut patches = Map::new();
                for patch in &theme.patches {
                    patches.insert(patch.name.clone(), patch.get_value());
                }
                dependencies.insert(theme.name.clone(), Value::Object(patches));
            }
        }
        generate_preset_core(&mut core, name, dependencies)
    }

    async fn upload_theme(&self, name: &str, base_url: &str, token: &str) -> Value {
        let theme_path = {
            let core = self.core.lock().await;
            let Some(theme) = core.themes.iter().find(|x| x.name == name) else {
                return result(false, format!("Could not find theme {name}"));
            };
            theme.theme_path.clone()
        };
        match upload_theme_archive(&theme_path, base_url, token).await {
            Ok(data) => json!({"success": true, "message": data}),
            Err(error) => result(false, &error),
        }
    }

    async fn download_theme(&self, id: &str, base_url: &str) -> Result<Value, String> {
        let (root, local, no_deps) = {
            let core = self.core.lock().await;
            (
                core.root.clone(),
                core.themes
                    .iter()
                    .map(|x| x.name.clone())
                    .collect::<Vec<_>>(),
                store_or_file_config(&core.root, "no_deps_install"),
            )
        };
        match install_theme(id, base_url, &root, &local, no_deps).await {
            Ok(()) => Ok(result(true, "Success")),
            Err(error)
                if error
                    == "Manifest version of themedb entry is unsupported by this version of CSS_Loader" =>
            {
                Err(error)
            }
            Err(error) => Ok(result(false, error)),
        }
    }

    async fn fetch_mappings(&self) -> Value {
        let _ = fetch_class_mappings(self, true).await;
        result(true, "Success")
    }

    async fn force_reinject(&self, target_id: &str) {
        let _mutation = self.mutations.lock().await;
        let mut core = self.core.lock().await;
        let active: Vec<(usize, Vec<String>)> = core
            .ordered_injects
            .iter()
            .copied()
            .filter_map(|i| {
                core.injects
                    .get(i)
                    .filter(|inject| inject.enabled)
                    .map(|inject| (i, inject.selectors.clone()))
            })
            .collect();
        for (inject_id, selectors) in active {
            let css = match ensure_css(&mut core, inject_id) {
                Ok(css) => css,
                Err(error) => {
                    log::warn!("{error}");
                    continue;
                }
            };
            for selector in selectors {
                if self.browser.tab_matches(target_id, &selector).await {
                    let uuid = uuid::Uuid::new_v4().to_string();
                    self.browser
                        .inject_on_tab(target_id, &selector, &css, &uuid)
                        .await;
                    core.injects[inject_id]
                        .uuids
                        .entry(selector)
                        .or_default()
                        .push(uuid);
                }
            }
        }
        drop(core);
        self.browser.commit_target(target_id, true).await;
    }

    async fn class_mapping_refresh(&self) -> bool {
        fetch_class_mappings(self, false).await
    }
}

pub async fn run(context: Context) -> Result<(), String> {
    let backend = Backend::from_env(context.env())?;
    let _ = BACKEND.set(Arc::clone(&backend));
    let initialized = backend.initialize().await;
    backend.ready.send_replace(true);
    initialized
}

pub async fn rpc(
    _context: Context,
    args: Vec<Value>,
    method: &'static str,
) -> Result<Value, String> {
    let backend = BACKEND
        .get()
        .cloned()
        .ok_or_else(|| "Plugin is still initializing".to_owned())?;
    backend.wait_ready().await;
    dispatch(&backend, method, args).await
}

async fn dispatch(backend: &Arc<Backend>, method: &str, args: Vec<Value>) -> Result<Value, String> {
    match method {
        "fetch_theme_path" => {
            require_count(method, &args, 0, 0)?;
            Ok(Value::String(
                backend
                    .core
                    .lock()
                    .await
                    .root
                    .to_string_lossy()
                    .into_owned(),
            ))
        }
        "get_last_load_errors" => {
            require_count(method, &args, 0, 0)?;
            Ok(json!({"fails": backend.core.lock().await.errors.clone()}))
        }
        "get_themes" => {
            require_count(method, &args, 0, 0)?;
            let core = backend.core.lock().await;
            Ok(Value::Array(
                core.themes
                    .iter()
                    .map(|x| x.to_dict(&core.injects))
                    .collect(),
            ))
        }
        "reset" => {
            require_count(method, &args, 0, 0)?;
            Ok(backend.reset(false).await)
        }
        "enable_server" => {
            require_count(method, &args, 0, 0)?;
            Ok(backend.enable_server().await)
        }
        "get_server_state" => {
            require_count(method, &args, 0, 0)?;
            Ok(Value::Bool(backend.core.lock().await.server_enabled))
        }
        "get_watch_state" => {
            require_count(method, &args, 0, 0)?;
            Ok(Value::Bool(backend.core.lock().await.watcher.is_some()))
        }
        "get_backend_version" => {
            require_count(method, &args, 0, 0)?;
            Ok(json!(CSS_LOADER_VER))
        }
        "dummy_function" => {
            require_count(method, &args, 0, 0)?;
            Ok(Value::Bool(true))
        }
        "fetch_class_mappings" => {
            require_count(method, &args, 0, 0)?;
            Ok(backend.fetch_mappings().await)
        }
        "is_standalone" => {
            require_count(method, &args, 0, 0)?;
            Ok(Value::Bool(false))
        }
        "set_theme_state" => {
            require_count(method, &args, 2, 4)?;
            let name = string_arg(method, &args, 0, "name")?;
            let state = bool_arg(method, &args, 1, "state")?;
            let deps = args.get(2).map_or(Ok(true), |x| as_bool(x, "set_deps"))?;
            let value = args
                .get(3)
                .map_or(Ok(true), |x| as_bool(x, "set_deps_value"))?;
            Ok(backend.set_theme_state(name, state, deps, value).await)
        }
        "set_patch_of_theme" => {
            require_count(method, &args, 3, 3)?;
            Ok(backend
                .set_patch(
                    string_arg(method, &args, 0, "themeName")?,
                    string_arg(method, &args, 1, "patchName")?,
                    string_arg(method, &args, 2, "value")?,
                )
                .await)
        }
        "set_component_of_theme_patch" => {
            require_count(method, &args, 4, 4)?;
            Ok(backend
                .set_component(
                    string_arg(method, &args, 0, "themeName")?,
                    string_arg(method, &args, 1, "patchName")?,
                    string_arg(method, &args, 2, "componentName")?,
                    string_arg(method, &args, 3, "value")?,
                )
                .await)
        }
        "download_theme_from_url" => {
            require_count(method, &args, 2, 2)?;
            backend
                .download_theme(
                    string_arg(method, &args, 0, "id")?,
                    string_arg(method, &args, 1, "url")?,
                )
                .await
        }
        "delete_theme" => {
            require_count(method, &args, 1, 1)?;
            Ok(backend
                .delete_theme(string_arg(method, &args, 0, "themeName")?)
                .await)
        }
        "store_read" => {
            require_count(method, &args, 1, 1)?;
            let core = backend.core.lock().await;
            Ok(Value::String(
                store_read(&core.root, string_arg(method, &args, 0, "key")?)
                    .map_err(|e| e.to_string())?,
            ))
        }
        "store_write" => {
            require_count(method, &args, 2, 2)?;
            let core = backend.core.lock().await;
            store_write(
                &core.root,
                string_arg(method, &args, 0, "key")?,
                string_arg(method, &args, 1, "val")?,
            )
            .map_err(|e| e.to_string())?;
            Ok(result(true, "Success"))
        }
        "toggle_watch_state" => {
            require_count(method, &args, 0, 2)?;
            let enable = args.first().map_or(Ok(true), |x| as_bool(x, "enable"))?;
            let only = args
                .get(1)
                .map_or(Ok(false), |x| as_bool(x, "only_this_session"))?;
            Ok(backend.toggle_watch_state(enable, only).await)
        }
        "generate_preset_theme" => {
            require_count(method, &args, 1, 1)?;
            Ok(backend
                .generate_preset(string_arg(method, &args, 0, "name")?, None)
                .await)
        }
        "generate_preset_theme_from_theme_names" => {
            require_count(method, &args, 2, 2)?;
            let selected = args[1]
                .as_array()
                .ok_or_else(|| "themeNames must be a list".to_owned())?
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "themeNames must contain strings".to_owned())
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(backend
                .generate_preset(string_arg(method, &args, 0, "name")?, Some(&selected))
                .await)
        }
        "upload_theme" => {
            require_count(method, &args, 3, 3)?;
            Ok(backend
                .upload_theme(
                    string_arg(method, &args, 0, "name")?,
                    string_arg(method, &args, 1, "base_url")?,
                    string_arg(method, &args, 2, "bearer_token")?,
                )
                .await)
        }
        _ => Err(format!("Plugin has no method {method}")),
    }
}

pub async fn dispatch_http(
    backend: &Arc<Backend>,
    method: &str,
    kwargs: &Map<String, Value>,
) -> Result<Value, String> {
    let definitions: &[(&str, Value, bool)] = match method {
        "fetch_theme_path"
        | "get_last_load_errors"
        | "get_themes"
        | "reset"
        | "enable_server"
        | "get_server_state"
        | "get_watch_state"
        | "get_backend_version"
        | "dummy_function"
        | "fetch_class_mappings"
        | "is_standalone" => &[],
        "set_theme_state" => &[
            ("name", Value::Null, true),
            ("state", Value::Null, true),
            ("set_deps", Value::Bool(true), false),
            ("set_deps_value", Value::Bool(true), false),
        ],
        "set_patch_of_theme" => &[
            ("themeName", Value::Null, true),
            ("patchName", Value::Null, true),
            ("value", Value::Null, true),
        ],
        "set_component_of_theme_patch" => &[
            ("themeName", Value::Null, true),
            ("patchName", Value::Null, true),
            ("componentName", Value::Null, true),
            ("value", Value::Null, true),
        ],
        "download_theme_from_url" => &[("id", Value::Null, true), ("url", Value::Null, true)],
        "delete_theme" => &[("themeName", Value::Null, true)],
        "store_read" => &[("key", Value::Null, true)],
        "store_write" => &[("key", Value::Null, true), ("val", Value::Null, true)],
        "toggle_watch_state" => &[
            ("enable", Value::Bool(true), false),
            ("only_this_session", Value::Bool(false), false),
        ],
        "generate_preset_theme" => &[("name", Value::Null, true)],
        "generate_preset_theme_from_theme_names" => &[
            ("name", Value::Null, true),
            ("themeNames", Value::Null, true),
        ],
        "upload_theme" => &[
            ("name", Value::Null, true),
            ("base_url", Value::Null, true),
            ("bearer_token", Value::Null, true),
        ],
        _ => return Err(format!("Plugin has no method {method}")),
    };
    if let Some(name) = kwargs
        .keys()
        .find(|key| !definitions.iter().any(|(known, _, _)| key == known))
    {
        return Err(format!(
            "{method}() got an unexpected keyword argument '{name}'"
        ));
    }
    let mut args = Vec::with_capacity(definitions.len());
    for (name, default, required) in definitions {
        match kwargs.get(*name) {
            Some(value) => args.push(value.clone()),
            None if *required => return Err(format!("missing required argument: '{name}'")),
            None => args.push(default.clone()),
        }
    }
    dispatch(backend, method, args).await
}

pub(crate) fn result(success: bool, message: impl std::fmt::Display) -> Value {
    json!({"success": success, "message": message.to_string()})
}

fn require_count(method: &str, args: &[Value], min: usize, max: usize) -> Result<(), String> {
    if args.len() < min {
        return Err(format!("{method}() missing required positional arguments"));
    }
    if args.len() > max {
        return Err(format!(
            "{method}() takes {max} positional arguments but {} were given",
            args.len()
        ));
    }
    Ok(())
}
fn string_arg<'a>(
    method: &str,
    args: &'a [Value],
    index: usize,
    name: &str,
) -> Result<&'a str, String> {
    args.get(index)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{method}() argument '{name}' must be str"))
}
fn bool_arg(method: &str, args: &[Value], index: usize, name: &str) -> Result<bool, String> {
    args.get(index)
        .ok_or_else(|| format!("{method}() missing argument '{name}'"))
        .and_then(|x| as_bool(x, name))
}
fn as_bool(value: &Value, name: &str) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{name} must be bool"))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn store_path(root: &Path) -> PathBuf {
    root.join("STORE")
}
fn read_store(root: &Path) -> std::io::Result<indexmap::IndexMap<String, String>> {
    let mut items = indexmap::IndexMap::new();
    let text = match fs::read_to_string(store_path(root)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(items),
        Err(error) => return Err(error),
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            items.insert(key.to_owned(), value.to_owned());
        }
    }
    Ok(items)
}
pub fn store_read(root: &Path, key: &str) -> std::io::Result<String> {
    Ok(read_store(root)?.get(key).cloned().unwrap_or_default())
}
pub fn store_write(root: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let mut items = read_store(root)?;
    items.insert(key.to_owned(), value.replace('\n', ""));
    let contents = items
        .iter()
        .map(|(key, val)| format!("{key}:{val}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(store_path(root), contents)
}
fn store_or_file_config(root: &Path, key: &str) -> bool {
    if root.join(key.to_uppercase()).exists() {
        return true;
    }
    matches!(
        store_read(root, key).unwrap_or_default().as_str(),
        "True" | "1"
    )
}

async fn load_core(
    core: &mut Core,
    browser: &Arc<Browser>,
    inject_now: bool,
) -> Result<(), String> {
    log::info!("Loading themes...");
    core.themes.clear();
    core.injects.clear();
    core.ordered_injects.clear();
    let mappings = default_mappings();
    core.errors.clear();
    let mut entries: Vec<_> = fs::read_dir(&core.root)
        .map_err(|e| e.to_string())?
        .collect();
    // read_dir order depends on the filesystem; equal-priority themes must inject in a stable order.
    entries.sort_by_key(|entry| entry.as_ref().ok().map(fs::DirEntry::file_name));
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!("Could not read theme directory entry: {error}");
                continue;
            }
        };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let arena_start = core.injects.len();
        match Theme::parse(
            &path,
            &path,
            &core.root,
            &core.user,
            &mappings,
            &mut core.injects,
        ) {
            Ok(mut theme) => {
                let names: HashSet<String> = core.themes.iter().map(|x| x.name.clone()).collect();
                if names.contains(&theme.name) {
                    for suffix in 0..5 {
                        let candidate = format!("{}_{}", theme.name, suffix);
                        if !names.contains(&candidate) {
                            if theme.display_name.is_none() {
                                theme.display_name = Some(theme.name.clone());
                            }
                            theme.name = candidate;
                            break;
                        }
                    }
                }
                if core.themes.iter().any(|x| x.name == theme.name) {
                    core.injects.truncate(arena_start);
                    continue;
                }
                log::info!("Found theme {}", theme.name);
                core.themes.push(theme);
            }
            Err(error) => {
                core.injects.truncate(arena_start);
                log::warn!(
                    "Failed parsing '{}': {error}",
                    entry.file_name().to_string_lossy()
                );
                core.errors
                    .push(json!([entry.file_name().to_string_lossy(), error]));
            }
        }
    }
    let mut scores = HashMap::new();
    for index in 0..core.themes.len() {
        score_theme(core, index, &mut scores, &mut Vec::new(), 0);
    }
    core.themes
        .sort_by_key(|x| scores.get(&x.name).copied().unwrap_or(x.priority));
    for index in 0..core.themes.len() {
        load_theme_config(core, browser, index, inject_now).await;
    }
    core.ordered_injects = core.themes.iter().flat_map(Theme::all_inject_ids).collect();
    core.themes
        .sort_by(|a, b| a.get_display_name().cmp(b.get_display_name()));
    Ok(())
}

fn score_theme(
    core: &Core,
    index: usize,
    scores: &mut HashMap<String, i64>,
    stack: &mut Vec<String>,
    depth: usize,
) {
    if depth > core.themes.len() {
        return;
    }
    let name = core.themes[index].name.clone();
    if stack.contains(&name) {
        return;
    }
    scores
        .entry(name.clone())
        .or_insert(core.themes[index].priority);
    stack.push(name);
    for dependency in dependency_names(&core.themes[index].dependencies) {
        if let Some(dependency_index) = core.themes.iter().position(|x| x.name == dependency) {
            score_theme(core, dependency_index, scores, stack, depth + 1);
            *scores.entry(dependency).or_default() -= 1;
        }
    }
    stack.pop();
}

async fn load_theme_config(
    core: &mut Core,
    browser: &Arc<Browser>,
    theme_index: usize,
    inject_now: bool,
) {
    let config_path = core.themes[theme_index]
        .config_path
        .join(if core.user == "root" {
            "config_ROOT.json"
        } else {
            "config_USER.json"
        });
    let Ok(bytes) = fs::read(config_path) else {
        return;
    };
    let Ok(config) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    let mut active = false;
    if let Some(object) = config.as_object() {
        for (key, value) in object {
            if key == "active" && value.as_bool() == Some(true) {
                active = true;
            } else if let Some(patch_index) = core.themes[theme_index]
                .patches
                .iter()
                .position(|x| x.name == *key)
            {
                let root = core.root.clone();
                let themes = &mut core.themes;
                let injects = &mut core.injects;
                let _ =
                    themes[theme_index].patches[patch_index].set_value(value, injects, Some(&root));
            }
        }
    }
    if active {
        let _ = inject_theme(core, browser, theme_index, inject_now).await;
    }
}

async fn inject_theme(
    core: &mut Core,
    browser: &Arc<Browser>,
    theme_index: usize,
    inject_now: bool,
) -> Result<(), String> {
    let injects = core.themes[theme_index].injects.clone();
    for inject_id in injects {
        let _ = inject_one(core, browser, inject_id, inject_now).await;
    }
    let mut patch_index = 0;
    while patch_index < core.themes[theme_index].patches.len() {
        inject_patch(core, browser, theme_index, patch_index, inject_now).await?;
        patch_index += 1;
    }
    core.themes[theme_index].enabled = true;
    let user = core.user.clone();
    let _ = core.themes[theme_index].save(&user);
    Ok(())
}

async fn inject_patch(
    core: &mut Core,
    browser: &Arc<Browser>,
    theme_index: usize,
    patch_index: usize,
    inject_now: bool,
) -> Result<(), String> {
    core.themes[theme_index].patches[patch_index].check_value();
    let value = core.themes[theme_index].patches[patch_index].value.clone();
    let ids = core.themes[theme_index].patches[patch_index]
        .options
        .get(&value)
        .cloned()
        .unwrap_or_default();
    for id in ids {
        if let Err(error) = inject_one(core, browser, id, inject_now).await {
            log::warn!("{error}");
        }
    }
    Ok(())
}

async fn inject_one(
    core: &mut Core,
    browser: &Arc<Browser>,
    inject_id: usize,
    inject_now: bool,
) -> Result<(), String> {
    if !inject_now {
        core.injects[inject_id].enabled = true;
        return Ok(());
    }
    let selectors = core.injects[inject_id].selectors.clone();
    let old = core.injects[inject_id].uuids.clone();
    for selector in &selectors {
        if let Some(ids) = old.get(selector) {
            for id in ids {
                browser.remove_css(selector, id).await;
            }
        }
    }
    let css = ensure_css(core, inject_id)?;
    let mut uuids = HashMap::new();
    for selector in selectors {
        let id = uuid::Uuid::new_v4().to_string();
        browser.inject_css(&selector, &css, &id).await;
        uuids.entry(selector).or_insert_with(Vec::new).push(id);
    }
    core.injects[inject_id].uuids = uuids;
    core.injects[inject_id].enabled = true;
    Ok(())
}

async fn remove_one(
    core: &mut Core,
    browser: &Arc<Browser>,
    inject_id: usize,
) -> Result<(), String> {
    let selectors = core.injects[inject_id].selectors.clone();
    for selector in selectors {
        let ids = core.injects[inject_id]
            .uuids
            .get(&selector)
            .cloned()
            .unwrap_or_default();
        // Page-scoped reinjects record IDs only for selectors that matched an open page.
        if ids.is_empty() {
            continue;
        }
        for id in ids {
            browser.remove_css(&selector, &id).await;
        }
        core.injects[inject_id].uuids.insert(selector, Vec::new());
    }
    core.injects[inject_id].enabled = false;
    Ok(())
}

async fn remove_patch(
    core: &mut Core,
    browser: &Arc<Browser>,
    theme_index: usize,
    patch_index: usize,
) {
    let ids = core.themes[theme_index].patches[patch_index]
        .injects
        .clone();
    for id in ids {
        let _ = remove_one(core, browser, id).await;
    }
}

async fn remove_theme(core: &mut Core, browser: &Arc<Browser>, theme_index: usize) {
    let ids = core.themes[theme_index].all_inject_ids();
    for id in ids {
        let _ = remove_one(core, browser, id).await;
    }
    core.themes[theme_index].enabled = false;
    let user = core.user.clone();
    if let Err(error) = core.themes[theme_index].save(&user) {
        log::warn!("Failed saving theme state: {error}");
    }
}

async fn enable_theme(
    core: &mut Core,
    browser: &Arc<Browser>,
    theme_index: usize,
    mut set_deps: bool,
    set_deps_value: bool,
    ignored: &[String],
    depth: usize,
) -> (bool, String) {
    if depth > core.themes.len() {
        return (true, "Success".into());
    }
    if set_deps {
        let mut dependencies = dependency_names(&core.themes[theme_index].dependencies);
        let mut ignored_next = ignored.to_vec();
        ignored_next.extend(dependencies.iter().cloned());
        if core.themes[theme_index]
            .flags
            .iter()
            .any(|x| x == FLAG_PRESET)
        {
            set_deps = false;
        }
        let mut scores = HashMap::new();
        for index in 0..core.themes.len() {
            score_theme(core, index, &mut scores, &mut Vec::new(), 0);
        }
        dependencies.sort_by_key(|name| scores.get(name).copied().unwrap_or(0));
        for dependency_name in dependencies {
            if ignored.contains(&dependency_name) {
                continue;
            }
            let Some(dependency_index) = core.themes.iter().position(|x| x.name == dependency_name)
            else {
                continue;
            };
            if set_deps_value {
                if core.themes[dependency_index].enabled {
                    remove_theme(core, browser, dependency_index).await;
                }
                let value = core.themes[theme_index]
                    .dependencies
                    .get(&dependency_name)
                    .cloned()
                    .unwrap_or(Value::Null);
                if let Some(patches) = value.as_object() {
                    for (patch_name, patch_value) in patches {
                        if let Some(patch_index) = core.themes[dependency_index]
                            .patches
                            .iter()
                            .position(|x| x.name == *patch_name)
                        {
                            let root = core.root.clone();
                            let themes = &mut core.themes;
                            let injects = &mut core.injects;
                            let _ = themes[dependency_index].patches[patch_index].set_value(
                                patch_value,
                                injects,
                                Some(&root),
                            );
                        }
                    }
                }
            }
            let _ = Box::pin(enable_theme(
                core,
                browser,
                dependency_index,
                set_deps,
                set_deps_value,
                &ignored_next,
                depth + 1,
            ))
            .await;
        }
    }
    match inject_theme(core, browser, theme_index, true).await {
        Ok(()) => (true, "Success".into()),
        Err(error) => (false, error),
    }
}

async fn disable_theme(
    core: &mut Core,
    browser: &Arc<Browser>,
    theme_index: usize,
    keep_dependencies: bool,
    depth: usize,
) -> (bool, String) {
    if depth > core.themes.len() {
        return (true, "Success".into());
    }
    remove_theme(core, browser, theme_index).await;
    if keep_dependencies {
        return (true, "Success".into());
    }
    let dependencies = dependency_names(&core.themes[theme_index].dependencies);
    for name in dependencies {
        let Some(dependency_index) = core.themes.iter().position(|x| x.name == name) else {
            continue;
        };
        let used = core.themes.iter().any(|theme| {
            theme.enabled
                && dependency_names(&theme.dependencies)
                    .iter()
                    .any(|x| x == &name)
        });
        if !used {
            let result = Box::pin(disable_theme(
                core,
                browser,
                dependency_index,
                false,
                depth + 1,
            ))
            .await;
            if !result.0 {
                return result;
            }
        }
    }
    (true, "Success".into())
}

fn ensure_css(core: &mut Core, inject_id: usize) -> Result<String, String> {
    core.injects[inject_id].load(&core.class_mappings)?;
    Ok(core.injects[inject_id].css.clone().unwrap_or_default())
}

fn update_component(
    core: &mut Core,
    theme_index: usize,
    patch_index: usize,
    component_index: usize,
) -> Result<(), String> {
    let Core {
        themes,
        injects,
        root,
        ..
    } = core;
    let component = themes[theme_index].patches[patch_index].components[component_index].clone();
    model::generate_component(&component, &mut injects[component.inject_id], Some(root))
}

fn rebuild_inject_indices(core: &mut Core) {
    let mut live = HashSet::new();
    for theme in &core.themes {
        live.extend(theme.injects.iter().copied());
        for patch in &theme.patches {
            live.extend(patch.injects.iter().copied());
        }
    }
    let old = std::mem::take(&mut core.injects);
    let mut indices = HashMap::new();
    for (old_index, inject) in old.into_iter().enumerate() {
        if live.contains(&old_index) {
            indices.insert(old_index, core.injects.len());
            core.injects.push(inject);
        }
    }
    for theme in &mut core.themes {
        for id in &mut theme.injects {
            *id = indices[id];
        }
        for patch in &mut theme.patches {
            for id in &mut patch.injects {
                *id = indices[id];
            }
            for ids in patch.options.values_mut() {
                for id in ids {
                    *id = indices[id];
                }
            }
            for component in &mut patch.components {
                component.inject_id = indices[&component.inject_id];
            }
        }
    }
    core.ordered_injects = core.themes.iter().flat_map(Theme::all_inject_ids).collect();
}

fn generate_preset_core(
    core: &mut Core,
    requested_name: &str,
    dependencies: Map<String, Value>,
) -> Value {
    let mut display_name = requested_name.to_owned();
    if display_name.ends_with(".profile") {
        display_name.truncate(display_name.len() - 8);
    }
    let mut name = format!("{display_name}.profile");
    if let Some(existing) = core.themes.iter().find(|x| x.name == name) {
        if !existing.flags.iter().any(|x| x == FLAG_PRESET) {
            return result(false, format!("Theme '{name}' already exists"));
        }
    } else if let Some(existing) = core.themes.iter().find(|x| x.name == display_name) {
        if existing.flags.iter().any(|x| x == FLAG_PRESET) {
            name.clone_from(&existing.name);
        } else {
            return result(false, format!("Theme '{display_name}' already exists"));
        }
    }
    let path = core.root.join(&name);
    if let Err(error) = fs::create_dir_all(&path) {
        return result(false, error.to_string());
    }
    let manifest = json!({"display_name": display_name, "name": name, "manifest_version": MAX_MANIFEST_VERSION, "flags": [FLAG_PRESET], "dependencies": dependencies});
    if let Err(error) = fs::write(path.join("theme.json"), python_json_dump(&manifest)) {
        return result(false, error.to_string());
    }
    if let Some(theme) = core.themes.iter_mut().find(|x| x.name == name) {
        log::info!("Updating dependencies for {name}");
        theme.dependencies = manifest["dependencies"].clone();
    }
    result(true, "Success")
}

fn create_zip_directory(
    zip: &mut zip::ZipWriter<fs::File>,
    root: &Path,
    directory: &Path,
) -> Result<(), String> {
    use zip::write::SimpleFileOptions;
    let entries = fs::read_dir(directory).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            create_zip_directory(zip, root, &path)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        zip.start_file(
            relative,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .map_err(|e| e.to_string())?;
        let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        zip.write_all(&bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn upload_theme_archive(
    theme_path: &Path,
    base_url: &str,
    token: &str,
) -> Result<Value, String> {
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let archive_path = temp.path().join("theme.zip");
    let file = fs::File::create(&archive_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    create_zip_directory(&mut zip, theme_path, theme_path)?;
    zip.finish().map_err(|e| e.to_string())?;
    let bytes = fs::read(archive_path).map_err(|e| e.to_string())?;
    let url = format!("{}blobs", ensure_slash(base_url));
    let client = reqwest::Client::builder()
        .user_agent(format!("SDH-CSSLoader/{CSS_LOADER_VER}"))
        .build()
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(bytes).file_name("theme.zip"),
    );
    let response = client
        .post(url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!("Invalid status code {}", response.status()));
    }
    response.json().await.map_err(|e| e.to_string())
}

async fn install_theme(
    id: &str,
    base_url: &str,
    root: &Path,
    local_themes: &[String],
    no_deps: bool,
) -> Result<(), String> {
    let base_url = ensure_slash(base_url);
    let client = reqwest::Client::builder()
        .user_agent(format!("SDH-CSSLoader/{CSS_LOADER_VER}"))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(format!("{base_url}themes/{id}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!("Invalid status code {}", response.status()));
    }
    let data: Value = response.json().await.map_err(|e| e.to_string())?;
    let version = data
        .get("manifestVersion")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if version > CSS_LOADER_VER {
        return Err(
            "Manifest version of themedb entry is unsupported by this version of CSS_Loader".into(),
        );
    }
    let blob_id = data
        .pointer("/download/id")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing download id".to_owned())?;
    let download_url = format!("{base_url}blobs/{blob_id}");
    log::info!("Downloading {download_url}");
    let response = client
        .get(&download_url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!(
            "Got {} code from '{download_url}'",
            response.status()
        ));
    }
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    extract_zip(&bytes, root)?;
    if !no_deps && let Some(dependencies) = data.get("dependencies").and_then(Value::as_array) {
        for dependency in dependencies {
            if let (Some(name), Some(dependency_id)) = (
                dependency.get("name").and_then(Value::as_str),
                dependency.get("id").and_then(Value::as_str),
            ) && !local_themes.iter().any(|x| x == name)
                && let Err(error) = Box::pin(install_theme(
                    dependency_id,
                    &base_url,
                    root,
                    local_themes,
                    no_deps,
                ))
                .await
                && error
                    == "Manifest version of themedb entry is unsupported by this version of CSS_Loader"
            {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn extract_zip(bytes: &[u8], root: &Path) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|e| e.to_string())?;
        let Some(relative) = entry.enclosed_name() else {
            return Err("ZIP entry escapes target directory".into());
        };
        let destination = root.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(destination).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut output = fs::File::create(destination).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut output).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn ensure_slash(base_url: &str) -> String {
    if base_url.ends_with('/') {
        base_url.to_owned()
    } else {
        format!("{base_url}/")
    }
}

fn mapping_refresh_due(last_success: Option<Instant>, now: Instant) -> bool {
    last_success.is_none_or(|last| now.duration_since(last) >= MAPPING_REFRESH_INTERVAL)
}

fn save_class_mappings(root: &Path, content: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut candidate = tempfile::NamedTempFile::new_in(root).map_err(|e| e.to_string())?;
    candidate.write_all(content).map_err(|e| e.to_string())?;
    let mappings = load_class_mappings(candidate.path())?;
    candidate
        .persist(root.join("css_translations.json"))
        .map_err(|e| e.error.to_string())?;
    Ok(mappings)
}

async fn fetch_mappings_periodically(backend: Weak<Backend>) {
    loop {
        if let Some(backend) = backend.upgrade() {
            let _ = backend.class_mapping_refresh().await;
        } else {
            return;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn fetch_class_mappings(backend: &Backend, force: bool) -> bool {
    let _fetch = backend.mapping_fetch.lock().await;
    let (root, beta_override, steam, last_fetch, etag) = {
        let mut core = backend.core.lock().await;
        if force {
            core.last_mapping_fetch = None;
        }
        (
            core.root.clone(),
            store_read(&core.root, "beta_translations").unwrap_or_default(),
            core.steam.clone(),
            core.last_mapping_fetch,
            core.mapping_etag.clone(),
        )
    };
    if !mapping_refresh_due(last_fetch, Instant::now()) {
        return false;
    }
    let check_connectivity = true;
    if check_connectivity {
        match tokio::task::spawn_blocking(|| {
            StdTcpStream::connect_timeout(
                &SocketAddr::from(([8, 8, 8, 8], 53)),
                Duration::from_secs(3),
            )
        })
        .await
        {
            Ok(Ok(_)) => (),
            _ => {
                log::info!("No internet connection. Not fetching css translations");
                return false;
            }
        }
    }
    let beta = match fs::read_to_string(steam.join("package/beta")) {
        Ok(branch) => !["steamdeck_stable"].contains(&branch.trim()),
        Err(_) => false,
    };
    let translations_url =
        if ((beta_override.trim().is_empty() || beta_override == "-1" || beta_override == "auto")
            && beta)
            || beta_override == "1"
            || beta_override == "true"
        {
            "https://beallio.github.io/steamui-watch/beta.json"
        } else {
            "https://beallio.github.io/steamui-watch/stable.json"
        };
    log::info!("Fetching CSS mappings from {translations_url}");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            log::warn!("Failed to fetch css translations from server [Error]: {error}");
            return false;
        }
    };
    let fetched = async {
        let mut request = client.get(translations_url);
        if let Some((_, etag)) = etag.as_ref().filter(|(url, _)| url == translations_url) {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(format!("status {}", response.status()));
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(|value| (translations_url.to_owned(), value.to_owned()));
        let content = response.bytes().await.map_err(|e| e.to_string())?;
        if content.iter().all(u8::is_ascii_whitespace) {
            return Err("Empty response".to_owned());
        }
        save_class_mappings(&root, &content).map(|mappings| Some((mappings, etag)))
    }
    .await;
    match fetched {
        Ok(None) => {
            backend.core.lock().await.last_mapping_fetch = Some(Instant::now());
            log::info!("CSS translations on the server are unchanged");
            true
        }
        Ok(Some((mappings, etag))) => {
            let changed = {
                let mut core = backend.core.lock().await;
                let changed = core.class_mappings != mappings;
                core.class_mappings = mappings;
                core.last_mapping_fetch = Some(Instant::now());
                core.mapping_etag = etag;
                changed
            };
            log::info!("Fetched css translations from server");
            if changed {
                let _ = backend.reset(true).await;
            }
            true
        }
        Err(error) => {
            log::warn!("Failed to fetch css translations from server [Error]: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn rejected_mapping_download_keeps_the_working_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("css_translations.json");
        let original = br#"{"id":["old","current"]}"#;
        fs::write(&cache, original).unwrap();
        let invalid: &[&[u8]] = &[
            b"{broken",
            b"{}",
            b"[]",
            b"null",
            br#"{"id":"class"}"#,
            br#"{"id":[]}"#,
            br#"{"id":[3,"latest"]}"#,
            br#"{"id":["old",false]}"#,
        ];
        for content in invalid {
            assert!(save_class_mappings(dir.path(), content).is_err());
            assert_eq!(fs::read(&cache).unwrap(), original);
        }
        assert_eq!(
            load_class_mappings(&cache).unwrap(),
            HashMap::from([("old".to_owned(), "current".to_owned())])
        );
    }

    #[test]
    fn valid_mapping_download_replaces_the_cache_and_translations() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("css_translations.json");
        fs::write(&cache, br#"{"id":["old","previous"]}"#).unwrap();
        let updated = br#"{"id":["old","middle","latest"]}"#;
        assert_eq!(
            save_class_mappings(dir.path(), updated).unwrap(),
            HashMap::from([
                ("old".to_owned(), "latest".to_owned()),
                ("middle".to_owned(), "latest".to_owned()),
            ])
        );
        assert_eq!(fs::read(&cache).unwrap(), updated);
    }

    #[test]
    fn mapping_refresh_resumes_at_the_hourly_deadline() {
        let fetched = Instant::now();
        assert!(mapping_refresh_due(None, fetched));
        assert!(!mapping_refresh_due(Some(fetched), fetched));
        assert!(!mapping_refresh_due(
            Some(fetched),
            fetched + MAPPING_REFRESH_INTERVAL - Duration::from_nanos(1)
        ));
        assert!(mapping_refresh_due(
            Some(fetched),
            fetched + MAPPING_REFRESH_INTERVAL
        ));
        assert!(mapping_refresh_due(
            Some(fetched),
            fetched + MAPPING_REFRESH_INTERVAL + Duration::from_secs(1)
        ));
    }

    #[test]
    fn python_json_matches_default_dump_format() {
        assert_eq!(
            python_json_dump(&json!({"title": "é", "nested": [true, "x"]})),
            r#"{"title": "\u00e9", "nested": [true, "x"]}"#
        );
    }

    #[test]
    fn store_round_trips_first_colon_and_preserves_insert_order() {
        let dir = tempfile::tempdir().unwrap();
        store_write(dir.path(), "one", "a:b").unwrap();
        store_write(dir.path(), "two", "x\ny").unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("STORE")).unwrap(),
            "one:a:b\ntwo:xy"
        );
        assert_eq!(store_read(dir.path(), "one").unwrap(), "a:b");
    }

    #[tokio::test]
    async fn http_dispatch_rejects_unknown_keyword_before_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Backend::open_for_tests(dir.path().join("themes"), "test")
            .await
            .unwrap();
        let kwargs = [
            ("name".to_owned(), json!("x")),
            ("state".to_owned(), json!(true)),
            ("set_dep".to_owned(), json!(false)),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            dispatch_http(&backend, "set_theme_state", &kwargs)
                .await
                .unwrap_err(),
            "set_theme_state() got an unexpected keyword argument 'set_dep'"
        );
    }

    #[tokio::test]
    async fn reload_replaces_prior_load_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("themes");
        fs::create_dir_all(root.join("broken")).unwrap();
        fs::write(root.join("broken/theme.json"), r#"{"manifest_version":9}"#).unwrap();
        let backend = Backend::open_for_tests(root.clone(), "test").await.unwrap();
        assert_eq!(backend.core.lock().await.errors.len(), 1);
        fs::remove_dir_all(root.join("broken")).unwrap();
        load_core(&mut *backend.core.lock().await, &backend.browser, false)
            .await
            .unwrap();
        assert!(backend.core.lock().await.errors.is_empty());
    }

    #[tokio::test]
    async fn recursive_disable_ignores_dependency_keep_flags() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/themes");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("themes");
        copy_tree(&fixture, &root).unwrap();
        let backend = Backend::open_for_tests(root, "test").await.unwrap();
        let mut core = backend.core.lock().await;
        let a = 0;
        let b = 1;
        let c = 2;
        core.themes[a].name = "A".into();
        core.themes[b].name = "B".into();
        core.themes[c].name = "C".into();
        core.themes[a].dependencies = json!({"B": null});
        core.themes[b].dependencies = json!({"C": null});
        core.themes[b].flags.push(FLAG_KEEP_DEPENDENCIES.into());
        core.themes[a].enabled = true;
        core.themes[b].enabled = true;
        core.themes[c].enabled = true;
        assert!(
            disable_theme(&mut core, &backend.browser, a, false, 0)
                .await
                .0
        );
        assert!(!core.themes[a].enabled && !core.themes[b].enabled && !core.themes[c].enabled);
    }

    #[tokio::test]
    async fn patch_continues_after_individual_inject_failure() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/themes");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("themes");
        copy_tree(&fixture, &root).unwrap();
        let backend = Backend::open_for_tests(root.clone(), "test").await.unwrap();
        let mut core = backend.core.lock().await;
        let theme_index = 0;
        let make_inject = |path: PathBuf| Inject {
            path,
            selectors: vec![".*".into()],
            inline_css: None,
            css: None,
            enabled: false,
            uuids: HashMap::new(),
        };
        let bad = core.injects.len();
        core.injects.push(make_inject(root.join("missing.css")));
        let good = core.injects.len();
        fs::write(root.join("good.css"), "ok").unwrap();
        core.injects.push(make_inject(root.join("good.css")));
        let patch_index = core.themes[theme_index].patches.len();
        core.themes[theme_index].patches.push(model::Patch {
            name: "test".into(),
            default: "selected".into(),
            value: "selected".into(),
            kind: "dropdown".into(),
            options: [("selected".into(), vec![bad, good])].into_iter().collect(),
            injects: vec![bad, good],
            components: Vec::new(),
        });
        assert!(
            inject_patch(&mut core, &backend.browser, theme_index, patch_index, true)
                .await
                .is_ok()
        );
        assert!(core.injects[good].enabled);
    }

    #[tokio::test]
    async fn remove_skips_selectors_without_injected_styles() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/themes");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("themes");
        copy_tree(&fixture, &root).unwrap();
        let backend = Backend::open_for_tests(root.clone(), "test").await.unwrap();
        let mut core = backend.core.lock().await;
        let inject = core.injects.len();
        core.injects.push(Inject {
            path: root.join("unused.css"),
            selectors: vec!["~not-open~".into(), "~open~".into()],
            inline_css: Some("x".into()),
            css: Some("x".into()),
            enabled: true,
            uuids: [("~open~".to_owned(), vec!["live".to_owned()])].into(),
        });
        remove_one(&mut core, &backend.browser, inject)
            .await
            .unwrap();
        assert!(!core.injects[inject].enabled);
        assert!(core.injects[inject].uuids["~open~"].is_empty());
    }
    #[test]
    fn rejects_zip_traversal() {
        let mut bytes = std::io::Cursor::new(Vec::new());
        {
            let mut archive = zip::ZipWriter::new(&mut bytes);
            archive
                .start_file("../escape", zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(b"unsafe").unwrap();
            archive.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            extract_zip(bytes.get_ref(), dir.path()).unwrap_err(),
            "ZIP entry escapes target directory"
        );
        assert!(!dir.path().parent().unwrap().join("escape").exists());
    }
    fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let target = destination.join(entry.file_name());
            if entry.path().is_dir() {
                copy_tree(&entry.path(), &target)?;
            } else {
                fs::copy(entry.path(), target)?;
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn permanent_fixtures_match_python_golden_outputs() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/themes");
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("themes");
        copy_tree(&fixture, &root).unwrap();
        let backend = Backend::open_for_tests(root.clone(), "cssfixture")
            .await
            .unwrap();
        assert_eq!(
            backend.set_theme_state("Theme Six", true, true, true).await["success"],
            true
        );
        assert_eq!(
            backend
                .set_theme_state("Theme Three", true, true, true)
                .await["success"],
            true
        );
        assert_eq!(
            backend.set_patch("Theme Two", "Density", "compact").await["success"],
            true
        );
        assert_eq!(
            backend.set_patch("Theme Three", "Enabled", "Yes").await["success"],
            true
        );
        assert_eq!(
            backend
                .set_component("Theme Nine", "Accent", "Brand", "#a1b2c3")
                .await["success"],
            true
        );
        assert_eq!(
            backend
                .set_component("Theme Nine", "Accent", "Wallpaper", "wallpaper.png")
                .await["success"],
            true
        );
        store_write(&root, "fixtureKey", "value:with:colons").unwrap();
        store_write(&root, "multiline", "line1\nline2").unwrap();
        assert_eq!(
            backend.generate_preset("Parity Profile", None).await["success"],
            true
        );

        let mut core = backend.core.lock().await;
        assert_eq!(
            core.themes
                .iter()
                .filter(|x| (1..=9).contains(&x.require))
                .map(|x| x.require)
                .collect::<HashSet<_>>(),
            (1..=9).collect()
        );
        let themes = core
            .themes
            .iter()
            .map(|theme| {
                let mut value = theme.to_dict(&core.injects);
                value["created"] = Value::Null;
                value["modified"] = Value::Null;
                value
            })
            .collect::<Vec<_>>();
        let mut css_by_tab = Map::new();
        for inject_id in core.ordered_injects.clone() {
            if !core.injects[inject_id].enabled {
                continue;
            }
            let css = ensure_css(&mut core, inject_id).unwrap();
            let selectors = core.injects[inject_id].selectors.clone();
            for selector in selectors {
                css_by_tab
                    .entry(selector)
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .unwrap()
                    .push(Value::String(css.clone()));
            }
        }
        let errors = core.errors.clone();
        drop(core);

        let golden: Value = serde_json::from_slice(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/python_parity.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(Value::Array(themes), golden["themes"]);
        assert_eq!(Value::Array(errors), golden["fails"]);
        assert_eq!(Value::Object(css_by_tab), golden["css_by_tab"]);
        for name in [
            "version-1",
            "version-2",
            "version-3",
            "version-6",
            "version-9",
        ] {
            let path = root.join(name).join("config_USER.json");
            assert_eq!(
                fs::read_to_string(path).unwrap(),
                golden["configs"][name].as_str().unwrap()
            );
        }
        assert_eq!(
            fs::read_to_string(root.join("STORE")).unwrap(),
            golden["store"].as_str().unwrap()
        );
        assert_eq!(
            fs::read_to_string(root.join("Parity Profile.profile/theme.json")).unwrap(),
            golden["preset_theme_json"].as_str().unwrap()
        );
    }
}
