use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::UNIX_EPOCH,
};

static CLASS_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\.[_a-zA-Z]+[_a-zA-Z0-9-]*)").unwrap());
static CLASS_ATTR_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(\[class[*^|~]=\"[_a-zA-Z0-9-]*\"\])"#).unwrap());

pub const CSS_LOADER_VER: u64 = 9;
pub const FLAG_KEEP_DEPENDENCIES: &str = "KEEP_DEPENDENCIES";
pub const FLAG_PRESET: &str = "PRESET";

#[derive(Clone, Debug)]
pub struct Inject {
    pub path: PathBuf,
    pub selectors: Vec<String>,
    pub inline_css: Option<String>,
    pub css: Option<String>,
    pub enabled: bool,
    pub uuids: HashMap<String, Vec<String>>,
}

impl Inject {
    fn new(path: PathBuf, selectors: Vec<String>, inline_css: Option<String>) -> Self {
        let uuids = selectors.iter().map(|x| (x.clone(), Vec::new())).collect();
        Self {
            path,
            selectors,
            inline_css: inline_css.clone(),
            css: inline_css,
            enabled: false,
            uuids,
        }
    }

    pub fn load(&mut self, mappings: &HashMap<String, String>) -> Result<(), String> {
        if self.css.is_some() {
            return Ok(());
        }
        // Python reads CSS in text mode, which turns \r\n and lone \r into \n.
        let source = fs::read_to_string(&self.path)
            .map_err(|e| e.to_string())?
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let mut css = CLASS_PATTERN
            .replace_all(&source, |captures: &regex::Captures<'_>| {
                let token = &captures[0][1..];
                mappings
                    .get(token)
                    .map(|x| format!(".{x}"))
                    .unwrap_or_else(|| captures[0].to_owned())
            })
            .into_owned();
        css = css.replace('\\', "\\\\").replace('`', "\\`");
        css = CLASS_ATTR_PATTERN
            .replace_all(&css, |captures: &regex::Captures<'_>| {
                let matched = &captures[0];
                let token = &matched[9..matched.len() - 2];
                mappings
                    .get(token)
                    .map(|replacement| {
                        format!(
                            "{}{}{}",
                            &matched[..9],
                            replacement,
                            &matched[matched.len() - 2..]
                        )
                    })
                    .unwrap_or_else(|| matched.to_owned())
            })
            .into_owned();
        self.css = Some(css.replace('\\', "\\\\").replace('`', "\\`"));
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Component {
    pub name: String,
    pub kind: String,
    pub on: String,
    pub value: String,
    pub css_variable: String,
    pub inject_id: usize,
}

impl Component {
    pub fn to_dict(&self) -> Value {
        json!({"name": self.name, "type": self.kind, "on": self.on, "value": self.value})
    }
}

#[derive(Clone, Debug)]
pub struct Patch {
    pub name: String,
    pub default: String,
    pub value: String,
    pub kind: String,
    pub options: IndexMap<String, Vec<usize>>,
    pub injects: Vec<usize>,
    pub components: Vec<Component>,
}

impl Patch {
    pub fn get_value(&self) -> Value {
        if self.components.is_empty() {
            Value::String(self.value.clone())
        } else {
            let mut components = Map::new();
            for component in &self.components {
                components.insert(
                    component.name.clone(),
                    Value::String(component.value.clone()),
                );
            }
            json!({"value": self.value, "components": components})
        }
    }

    pub fn to_dict(&self) -> Value {
        json!({
            "name": self.name,
            "default": self.default,
            "value": self.value,
            "options": self.options.keys().cloned().collect::<Vec<_>>(),
            "type": self.kind,
            "components": self.components.iter().map(Component::to_dict).collect::<Vec<_>>()
        })
    }

    pub fn check_value(&mut self) {
        if !self.options.contains_key(&self.value) {
            self.value.clone_from(&self.default);
        }
        if !["dropdown", "checkbox", "slider", "none"].contains(&self.kind.as_str()) {
            self.kind = "dropdown".to_owned();
        }
        if self.kind == "checkbox"
            && !(self.options.contains_key("No") && self.options.contains_key("Yes"))
        {
            self.kind = "dropdown".to_owned();
        }
    }

    pub fn set_value(
        &mut self,
        value: &Value,
        arena: &mut [Inject],
        root: Option<&Path>,
    ) -> Result<(), String> {
        if let Some(text) = value.as_str() {
            self.value = text.to_owned();
            return Ok(());
        }
        if let Some(object) = value.as_object() {
            if let Some(text) = object.get("value").and_then(Value::as_str) {
                self.value = text.to_owned();
            }
            if let Some(components) = object.get("components") {
                let components = components.as_object().ok_or_else(|| "???".to_owned())?;
                for component in &mut self.components {
                    if let Some(value) = components.get(&component.name).and_then(Value::as_str) {
                        component.value = value.to_owned();
                        let injector = &mut arena[component.inject_id];
                        let _ = generate_component(component, injector, root);
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub theme_path: PathBuf,
    pub config_path: PathBuf,
    pub name: String,
    pub display_name: Option<String>,
    pub id: String,
    pub version: String,
    pub author: String,
    pub require: u64,
    pub flags: Vec<String>,
    pub tab_mappings: IndexMap<String, Vec<String>>,
    pub dependencies: Value,
    pub bundled: bool,
    pub enabled: bool,
    pub priority: i64,
    pub created: Option<f64>,
    pub modified: Option<f64>,
    pub injects: Vec<usize>,
    pub patches: Vec<Patch>,
}

impl Theme {
    pub fn parse(
        theme_path: &Path,
        config_path: &Path,
        theme_root: &Path,
        user: &str,
        mappings: &IndexMap<String, Vec<String>>,
        arena: &mut Vec<Inject>,
    ) -> Result<Self, String> {
        let mut priority = 0;
        if let Ok(text) = fs::read_to_string(theme_path.join("PRIORITY")) {
            priority = text
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .parse()
                .unwrap_or(0);
        }
        let config_name = if user == "root" {
            "config_ROOT.json"
        } else {
            "config_USER.json"
        };
        let config_file = config_path.join(config_name);
        let modified = metadata_time(&config_file);
        let manifest_file = theme_path.join("theme.json");
        let manifest_time = metadata_time(&manifest_file);
        let manifest = match fs::read(&manifest_file) {
            Ok(bytes) => Some(serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())?)
                .filter(|x| !x.is_null()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        let mut theme = Self {
            theme_path: theme_path.to_path_buf(),
            config_path: config_path.to_path_buf(),
            name: String::new(),
            display_name: None,
            id: String::new(),
            version: "v1.0".into(),
            author: String::new(),
            require: 1,
            flags: Vec::new(),
            tab_mappings: IndexMap::new(),
            dependencies: json!({}),
            bundled: theme_path != config_path,
            enabled: false,
            priority,
            created: manifest_time,
            modified,
            injects: Vec::new(),
            patches: Vec::new(),
        };
        let Some(manifest) = manifest else {
            if theme_path.join("theme.css").exists() {
                theme.name = basename(theme_path);
                theme.id.clone_from(&theme.name);
                theme.dependencies = json!([]);
                let id = add_file_inject(
                    arena,
                    theme_path.join("theme.css"),
                    vec![".*".into()],
                    mappings,
                    &theme.tab_mappings,
                )?;
                theme.injects.push(id);
                return Ok(theme);
            }
            let sfp_files = [
                (
                    "libraryroot.custom.css",
                    vec![
                        "desktop".into(),
                        "desktopoverlay".into(),
                        "desktopcontextmenu".into(),
                    ],
                ),
                (
                    "bigpicture.custom.css",
                    vec!["bigpicture".into(), "bigpictureoverlay".into()],
                ),
                ("friends.custom.css", vec!["desktopchat".into()]),
                ("webkit.css", vec!["store".into()]),
            ];
            for (filename, tabs) in sfp_files {
                let file = theme_path.join(filename);
                if file.exists() {
                    let id = add_file_inject(arena, file, tabs, mappings, &theme.tab_mappings)?;
                    theme.injects.push(id);
                }
            }
            if theme.injects.is_empty() {
                return Err("Folder does not look like a theme?".into());
            }
            theme.name = basename(theme_path);
            theme.id.clone_from(&theme.name);
            theme.dependencies = json!([]);
            return Ok(theme);
        };

        theme.name = required_string(&manifest, "name")?;
        theme.display_name = manifest
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        theme.id = manifest
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&theme.name)
            .to_owned();
        theme.version = manifest
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("v1.0")
            .to_owned();
        theme.author = manifest
            .get("author")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        theme.require = match manifest.get("manifest_version") {
            None => 1,
            Some(Value::Number(n)) => n
                .as_u64()
                .ok_or_else(|| "invalid literal for int()".to_owned())?,
            Some(Value::String(s)) => s
                .parse::<u64>()
                .map_err(|_| format!("invalid literal for int() with base 10: '{s}'"))?,
            Some(_) => {
                return Err(
                    "int() argument must be a string, a bytes-like object or a real number".into(),
                );
            }
        };
        if theme.require > CSS_LOADER_VER {
            return Err(format!(
                "A newer version of the CssLoader is required to load this theme (Read manifest version {} but only up to {} is supported)",
                theme.require, CSS_LOADER_VER
            ));
        }
        if let Some(flags) = manifest.get("flags") {
            let flags = flags
                .as_array()
                .ok_or_else(|| "'NoneType' object is not iterable".to_owned())?;
            theme.flags = flags
                .iter()
                .map(|x| {
                    x.as_str()
                        .ok_or_else(|| "'NoneType' object has no attribute 'upper'".to_owned())
                        .map(str::to_uppercase)
                })
                .collect::<Result<_, _>>()?;
        }
        if let Some(tabs) = manifest.get("tabs").and_then(Value::as_object) {
            for (name, values) in tabs {
                let values = values
                    .as_array()
                    .ok_or_else(|| format!("'{}' object is not iterable", python_type(values)))?;
                theme.tab_mappings.insert(
                    name.clone(),
                    values
                        .iter()
                        .map(|x| x.as_str().unwrap_or_default().to_owned())
                        .collect(),
                );
            }
        }
        theme.dependencies = manifest
            .get("dependencies")
            .cloned()
            .unwrap_or_else(|| json!({}));

        if let Some(inject) = manifest.get("inject") {
            let entries = inject
                .as_object()
                .ok_or_else(|| "'NoneType' object is not iterable".to_owned())?;
            for (key, tabs) in entries {
                let tabs = string_list(tabs)?;
                if key.starts_with("--") {
                    let value = tabs.first().cloned().unwrap_or_default();
                    let selectors = expand_tabs(
                        tabs.get(1..).unwrap_or_default(),
                        &theme.tab_mappings,
                        mappings,
                    )?;
                    let css = format!(":root {{ {key}: {value}; }}");
                    let id = add_inline_inject(arena, selectors, css);
                    theme.injects.push(id);
                } else {
                    let id = add_file_inject(
                        arena,
                        theme_path.join(key),
                        tabs,
                        mappings,
                        &theme.tab_mappings,
                    )?;
                    theme.injects.push(id);
                }
            }
        }
        if let Some(patches) = manifest.get("patches") {
            let patch_obj = patches
                .as_object()
                .ok_or_else(|| "'NoneType' object is not iterable".to_owned())?;
            for (name, data) in patch_obj {
                theme.patches.push(parse_patch(
                    &theme, name, data, theme_root, mappings, arena,
                )?);
            }
        }
        Ok(theme)
    }

    pub fn get_display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    pub fn to_dict(&self, arena: &[Inject]) -> Value {
        let dependencies = dependency_names(&self.dependencies);
        let _ = arena;
        json!({
            "id": self.id, "name": self.name, "display_name": self.get_display_name(),
            "version": self.version, "author": self.author, "enabled": self.enabled,
            "patches": self.patches.iter().map(Patch::to_dict).collect::<Vec<_>>(),
            "bundled": self.bundled, "require": self.require, "dependencies": dependencies,
            "flags": self.flags, "created": self.created, "modified": self.modified
        })
    }

    pub fn config_value(&self) -> Value {
        let mut config = Map::new();
        config.insert("active".into(), Value::Bool(self.enabled));
        for patch in &self.patches {
            config.insert(patch.name.clone(), patch.get_value());
        }
        Value::Object(config)
    }

    pub fn save(&mut self, user: &str) -> Result<(), String> {
        fs::create_dir_all(&self.config_path).map_err(|e| e.to_string())?;
        let file = self.config_path.join(if user == "root" {
            "config_ROOT.json"
        } else {
            "config_USER.json"
        });
        fs::write(&file, python_json_dump(&self.config_value())).map_err(|e| e.to_string())?;
        self.modified = Some(now_seconds());
        Ok(())
    }

    pub fn all_inject_ids(&self) -> Vec<usize> {
        let mut ids = self.injects.clone();
        for patch in &self.patches {
            ids.extend(patch.injects.iter().copied());
        }
        ids
    }
}

fn parse_patch(
    theme: &Theme,
    name: &str,
    data: &Value,
    root: &Path,
    mappings: &IndexMap<String, Vec<String>>,
    arena: &mut Vec<Inject>,
) -> Result<Patch, String> {
    let object = data
        .as_object()
        .ok_or_else(|| "'NoneType' object is not iterable".to_owned())?;
    let default = object
        .get("default")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let version_two = object.contains_key("values");
    let mut option_data: IndexMap<String, Value> = IndexMap::new();
    if version_two {
        let values = object
            .get("values")
            .and_then(Value::as_object)
            .ok_or_else(|| "'NoneType' object is not iterable".to_owned())?;
        for (key, value) in values {
            option_data.insert(key.clone(), value.clone());
        }
    } else {
        for (key, value) in object {
            if key != "default" {
                option_data.insert(key.clone(), value.clone());
            }
        }
    }
    if option_data.is_empty() {
        return Err(format!(
            "In patch '{name}' there is less than 1 value present"
        ));
    }
    let default = default.unwrap_or_else(|| option_data.keys().next().cloned().unwrap_or_default());
    if !option_data.contains_key(&default) {
        return Err(format!(
            "In patch '{name}', '{default}' does not exist as a patch option"
        ));
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("dropdown")
        .to_owned();
    let mut patch = Patch {
        name: name.to_owned(),
        default: default.clone(),
        value: default,
        kind,
        options: IndexMap::new(),
        injects: Vec::new(),
        components: Vec::new(),
    };
    for (option, inject_data) in option_data {
        let entries = inject_data
            .as_object()
            .ok_or_else(|| "'NoneType' object is not iterable".to_owned())?;
        let mut ids = Vec::new();
        for (key, tabs) in entries {
            let tabs = string_list(tabs)?;
            let id = if key.starts_with("--") {
                let value = tabs.first().cloned().unwrap_or_default();
                add_inline_inject(
                    arena,
                    expand_tabs(
                        tabs.get(1..).unwrap_or_default(),
                        &theme.tab_mappings,
                        mappings,
                    )?,
                    format!(":root {{ {key}: {value}; }}"),
                )
            } else {
                add_file_inject(
                    arena,
                    theme.theme_path.join(key),
                    tabs,
                    mappings,
                    &theme.tab_mappings,
                )?
            };
            ids.push(id);
        }
        patch.injects.extend(ids.iter().copied());
        patch.options.insert(option, ids);
    }
    if let Some(components) = object.get("components") {
        let components = components
            .as_array()
            .ok_or_else(|| "'NoneType' object is not iterable".to_owned())?;
        for source in components {
            let name = required_string(source, "name")?;
            let kind = required_string(source, "type")?;
            if kind != "color-picker" && kind != "image-picker" {
                return Err(format!("Unknown component type '{kind}'"));
            }
            let mut default = required_string(source, "default")?;
            if kind == "color-picker" && !valid_color(&default) {
                default = "#FFFFFF".into();
            }
            if kind == "image-picker" {
                validate_image(&default, root)?;
            }
            let on = required_string(source, "on")?;
            if !patch.options.contains_key(&on) {
                return Err("Component references non-existent value".into());
            }
            let mut variable = required_string(source, "css_variable")?;
            if !variable.starts_with("--") {
                variable = format!("--{variable}");
            }
            let tabs = string_list(source.get("tabs").ok_or_else(|| "'tabs'".to_owned())?)?;
            let selectors = expand_tabs(&tabs, &theme.tab_mappings, mappings)?;
            let inject_id = add_inline_inject(arena, selectors, String::new());
            let component = Component {
                name,
                kind,
                on: on.clone(),
                value: default,
                css_variable: variable,
                inject_id,
            };
            let _ = generate_component(&component, &mut arena[inject_id], Some(root));
            patch.options.get_mut(&on).unwrap().push(inject_id);
            patch.components.push(component);
            patch.injects.push(inject_id);
        }
    }
    patch.check_value();
    Ok(patch)
}

pub(crate) fn generate_component(
    component: &Component,
    inject: &mut Inject,
    root: Option<&Path>,
) -> Result<(), String> {
    if component.css_variable.contains(';') || component.value.contains(';') {
        return Err("???".into());
    }
    let css = if component.kind == "color-picker" {
        let rgb = if component.value.starts_with('#') {
            parse_hex_rgb(&component.value)
        } else if (component.value.starts_with("hsla(") || component.value.starts_with("hsl("))
            && component.value.ends_with(')')
        {
            parse_hsl(&component.value)
        } else {
            None
        };
        if let Some((r, g, b)) = rgb {
            format!(
                ":root {{ {}: {}; {}_r: {}; {}_g: {}; {}_b: {}; {}_rgb: {}, {}, {}; }}",
                component.css_variable,
                component.value,
                component.css_variable,
                r,
                component.css_variable,
                g,
                component.css_variable,
                b,
                component.css_variable,
                r,
                g,
                b
            )
        } else {
            format!(
                ":root {{ {}: {}; }}",
                component.css_variable, component.value
            )
        }
    } else {
        if let Some(root) = root {
            validate_image(&component.value, root)?;
        }
        let value = component.value.replace(' ', "%20").replace('\\', "/");
        format!(
            ":root {{ {}: url(/themes_custom/{}) }}",
            component.css_variable, value
        )
    };
    inject.css = Some(css);
    Ok(())
}

fn parse_hex_rgb(value: &str) -> Option<(i32, i32, i32)> {
    let digits = value.strip_prefix('#')?;
    if ![3, 4, 6, 8].contains(&digits.len()) || !digits.chars().all(|x| x.is_ascii_hexdigit()) {
        return None;
    }
    if digits.len() < 6 {
        Some((
            i32::from_str_radix(&digits[0..1], 16).ok()?,
            i32::from_str_radix(&digits[1..2], 16).ok()?,
            i32::from_str_radix(&digits[2..3], 16).ok()?,
        ))
    } else {
        Some((
            i32::from_str_radix(&digits[0..2], 16).ok()?,
            i32::from_str_radix(&digits[2..4], 16).ok()?,
            i32::from_str_radix(&digits[4..6], 16).ok()?,
        ))
    }
}

fn parse_hsl(value: &str) -> Option<(i32, i32, i32)> {
    let inside = value.get(value.find('(')? + 1..value.len().checked_sub(1)?)?;
    let parts: Vec<_> = inside.split(',').collect();
    if parts.len() < 3 {
        return None;
    }
    let h: f64 = parts[0].trim().parse().ok()?;
    let s: f64 = parts[1].trim().strip_suffix('%')?.parse::<f64>().ok()? / 100.0;
    let l: f64 = parts[2].trim().strip_suffix('%')?.parse::<f64>().ok()? / 100.0;
    let h = h / 360.0;
    if s == 0.0 {
        let gray = (l * 100.0) as i32;
        return Some((gray, gray, gray));
    }
    let m2 = if l <= 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let m1 = 2.0 * l - m2;
    let mask = |mut hue: f64| -> f64 {
        hue = hue.rem_euclid(1.0);
        if hue < 1.0 / 6.0 {
            m1 + (m2 - m1) * hue * 6.0
        } else if hue < 0.5 {
            m2
        } else if hue < 2.0 / 3.0 {
            m1 + (m2 - m1) * (2.0 / 3.0 - hue) * 6.0
        } else {
            m1
        }
    };
    Some((
        (mask(h + 1.0 / 3.0) * 255.0) as i32,
        (mask(h) * 255.0) as i32,
        (mask(h - 1.0 / 3.0) * 255.0) as i32,
    ))
}

fn expand_tabs_inner(
    tabs: &[String],
    theme_tabs: &IndexMap<String, Vec<String>>,
    mapping: &IndexMap<String, Vec<String>>,
    stack: &mut Vec<String>,
    result: &mut Vec<String>,
) -> Result<(), String> {
    let actual = if tabs.is_empty() {
        theme_tabs.get("default").map(Vec::as_slice).unwrap_or(&[])
    } else {
        tabs
    };
    for tab in actual {
        if let Some(children) = theme_tabs.get(tab).or_else(|| mapping.get(tab)) {
            if stack.contains(tab) {
                return Err(format!(
                    "Cyclic tab alias: {}",
                    stack
                        .iter()
                        .chain(std::iter::once(tab))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ));
            }
            stack.push(tab.clone());
            expand_tabs_inner(children, theme_tabs, mapping, stack, result)?;
            stack.pop();
        } else {
            result.push(tab.clone());
        }
    }
    Ok(())
}

pub fn expand_tabs(
    tabs: &[String],
    theme_tabs: &IndexMap<String, Vec<String>>,
    mapping: &IndexMap<String, Vec<String>>,
) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    expand_tabs_inner(tabs, theme_tabs, mapping, &mut Vec::new(), &mut result)?;
    Ok(result)
}

fn valid_color(value: &str) -> bool {
    value.starts_with('#')
        && [4, 5, 7, 9].contains(&value.len())
        && value[1..].chars().all(|x| x.is_ascii_hexdigit())
}

fn validate_image(value: &str, root: &Path) -> Result<(), String> {
    if value.trim().starts_with('/') {
        return Err("Image Picker path cannot be absolute".into());
    }
    if value.split('/').any(|x| x.trim() == "..") {
        return Err("Going back in a relative path is not allowed".into());
    }
    if !root.join(value).exists() {
        return Err("Image Picker specified image does not exist".into());
    }
    Ok(())
}

fn add_file_inject(
    arena: &mut Vec<Inject>,
    path: PathBuf,
    tabs: Vec<String>,
    mapping: &IndexMap<String, Vec<String>>,
    theme_tabs: &IndexMap<String, Vec<String>>,
) -> Result<usize, String> {
    let selectors = expand_tabs(&tabs, theme_tabs, mapping)?;
    let inject = Inject::new(path, selectors, None);
    let id = arena.len();
    arena.push(inject);
    Ok(id)
}

fn add_inline_inject(arena: &mut Vec<Inject>, selectors: Vec<String>, css: String) -> usize {
    let id = arena.len();
    arena.push(Inject::new(PathBuf::new(), selectors, Some(css)));
    id
}

pub fn default_mappings() -> IndexMap<String, Vec<String>> {
    [
        ("desktop", vec!["Steam|SteamLibraryWindow"]),
        ("desktopchat", vec!["!friendsui-container"]),
        (
            "desktoppopup",
            vec![
                "OverlayBrowser_Browser",
                "SP Overlay:.*",
                "notificationtoasts_.*",
                "SteamBrowser_Find",
                "OverlayTab\\d+_Find",
                "!ModalDialogPopup",
                "!FullModalOverlay",
            ],
        ),
        ("desktopoverlay", vec!["desktoppopup"]),
        ("desktopcontextmenu", vec![".*Menu", ".*Supernav"]),
        (
            "bigpicture",
            vec!["~Valve Steam Gamepad/default~", "~Valve%20Steam%20Gamepad~"],
        ),
        ("bigpictureoverlay", vec!["QuickAccess", "MainMenu"]),
        (
            "store",
            vec![
                "~https://store.steampowered.com~",
                "~https://steamcommunity.com~",
            ],
        ),
        ("SP", vec!["bigpicture"]),
        ("Steam Big Picture Mode", vec!["bigpicture"]),
        ("MainMenu", vec!["MainMenu.*"]),
        ("MainMenu_.*", vec!["MainMenu"]),
        ("QuickAccess", vec!["QuickAccess.*"]),
        ("QuickAccess_.*", vec!["QuickAccess"]),
        ("Steam", vec!["desktop"]),
        ("SteamLibraryWindow", vec!["desktop"]),
        ("All", vec!["bigpicture", "bigpictureoverlay"]),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.into_iter().map(str::to_owned).collect()))
    .collect()
}

pub fn load_class_mappings(path: &Path) -> Result<HashMap<String, String>, String> {
    let value: Value = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "CSS translations are not a JSON object".to_owned())?;
    if object.is_empty() {
        return Err("CSS translations are empty".to_owned());
    }
    let mut result = HashMap::new();
    for (uid, versions) in object {
        let values = versions
            .as_array()
            .ok_or_else(|| format!("CSS translation '{uid}' is not a list"))?;
        let latest = values
            .last()
            .and_then(Value::as_str)
            .ok_or_else(|| format!("CSS translation '{uid}' has no latest mapping"))?;
        for old in values.iter().take(values.len().saturating_sub(1)) {
            let old = old
                .as_str()
                .ok_or_else(|| format!("CSS translation '{uid}' contains a non-string class"))?;
            result.insert(old.to_owned(), latest.to_owned());
        }
    }
    Ok(result)
}

pub fn dependency_names(dependencies: &Value) -> Vec<String> {
    match dependencies {
        Value::Object(values) => values.keys().cloned().collect(),
        Value::Array(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Value::String(value) => value.chars().map(|x| x.to_string()).collect(),
        _ => Vec::new(),
    }
}

pub fn python_json_dump(value: &Value) -> String {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(x) => out.push_str(if *x { "true" } else { "false" }),
            Value::Number(x) => out.push_str(&x.to_string()),
            Value::String(x) => write_string(x, out),
            Value::Array(values) => {
                out.push('[');
                for (i, x) in values.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write(x, out);
                }
                out.push(']');
            }
            Value::Object(values) => {
                out.push('{');
                for (i, (key, val)) in values.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_string(key, out);
                    out.push_str(": ");
                    write(val, out);
                }
                out.push('}');
            }
        }
    }
    fn write_string(value: &str, out: &mut String) {
        out.push('"');
        for character in value.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c <= '\u{1f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
                c if c as u32 > 0x7f => {
                    let code = c as u32;
                    if code <= 0xffff {
                        out.push_str(&format!("\\u{code:04x}"));
                    } else {
                        let code = code - 0x10000;
                        let high = 0xd800 + (code >> 10);
                        let low = 0xdc00 + (code & 0x3ff);
                        out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
                    }
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    let mut output = String::new();
    write(value, &mut output);
    output
}

fn required_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("'{key}'"))
}
fn string_list(value: &Value) -> Result<Vec<String>, String> {
    let values = value
        .as_array()
        .ok_or_else(|| format!("'{}' object is not iterable", python_type(value)))?;
    values
        .iter()
        .map(|x| {
            x.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "expected string".into())
        })
        .collect()
}
fn python_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}
fn basename(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
fn metadata_time(path: &Path) -> Option<f64> {
    let time = fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    Some(time.as_secs_f64())
}
pub fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_cyclic_tab_aliases() {
        let aliases = [("desktop", vec!["desktop"])]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.into_iter().map(str::to_owned).collect()))
            .collect();
        assert!(
            expand_tabs(&["desktop".into()], &IndexMap::new(), &aliases)
                .unwrap_err()
                .contains("Cyclic tab alias")
        );
    }

    #[test]
    fn negative_hue_wraps_and_fractional_hsl_parses() {
        assert_eq!(parse_hsl("hsl(0, 50%, 50%)"), Some((191, 63, 63)));
        assert!(parse_hsl("hsl(180, 50.5%, 40%)").is_some());
    }

    #[test]
    fn css_line_endings_match_python_text_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.css");
        fs::write(&path, "a {\r\n  color: red;\r}\r\n").unwrap();
        let mut inject = Inject::new(path, vec!["desktop".into()], None);
        inject.load(&HashMap::new()).unwrap();
        assert_eq!(inject.css.as_deref(), Some("a {\n  color: red;\n}\n"));
    }
}
