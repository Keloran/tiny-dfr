use crate::fonts::{FontConfig, Pattern};
use crate::FunctionLayer;
use cairo::FontFace;
use freetype::Library as FtLibrary;
use input_linux::Key;
use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor},
};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer,
};
use std::{collections::BTreeMap, env, fmt, fs::read_to_string, os::fd::AsFd, path::PathBuf};

const DEFAULT_CFG_PATH: &str = "/usr/share/tiny-dfr/config.toml";
const SYSTEM_CFG_PATH: &str = "/etc/tiny-dfr/config.toml";

// The bundled config lives in the share dir. Normally that's
// /usr/share/tiny-dfr, but the simulator (and anyone running from a checkout)
// can point at the repo's share/tiny-dfr via TINY_DFR_SHARE_DIR.
fn default_cfg_path() -> PathBuf {
    match env::var_os("TINY_DFR_SHARE_DIR") {
        Some(dir) => PathBuf::from(dir).join("config.toml"),
        None => PathBuf::from(DEFAULT_CFG_PATH),
    }
}

pub struct Config {
    pub default_layer: String,
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_face: FontFace,
    pub font_size: f64,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub double_press_switch_layers: u32,
    pub drop_privileges: bool,
    pub weather_location: Option<String>,
    pub weather_fahrenheit: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    default_layer: Option<String>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_template: Option<String>,
    font_size: Option<f64>,
    adaptive_brightness: Option<bool>,
    active_brightness: Option<u32>,
    double_press_switch_layers: Option<u32>,
    drop_privileges: Option<bool>,
    weather_location: Option<String>,
    weather_units: Option<String>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    system_info_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
    layers: Option<BTreeMap<String, Vec<ButtonConfig>>>,
}

impl ConfigProxy {
    fn merge(&mut self, other: ConfigProxy) {
        self.default_layer = other.default_layer.or(self.default_layer.take());
        self.show_button_outlines = other.show_button_outlines.or(self.show_button_outlines);
        self.enable_pixel_shift = other.enable_pixel_shift.or(self.enable_pixel_shift);
        self.font_template = other.font_template.or(self.font_template.take());
        self.font_size = other.font_size.or(self.font_size);
        self.adaptive_brightness = other.adaptive_brightness.or(self.adaptive_brightness);
        self.active_brightness = other.active_brightness.or(self.active_brightness);
        self.double_press_switch_layers = other
            .double_press_switch_layers
            .or(self.double_press_switch_layers);
        self.drop_privileges = other.drop_privileges.or(self.drop_privileges);
        self.weather_location = other.weather_location.or(self.weather_location.take());
        self.weather_units = other.weather_units.or(self.weather_units.take());
        self.primary_layer_keys = other.primary_layer_keys.or(self.primary_layer_keys.take());
        self.system_info_layer_keys = other
            .system_info_layer_keys
            .or(self.system_info_layer_keys.take());
        self.media_layer_keys = other.media_layer_keys.or(self.media_layer_keys.take());
        if let Some(layers) = other.layers {
            self.layers.get_or_insert_with(BTreeMap::new).extend(layers);
        }
    }
}

fn array_or_single<'de, D>(deserializer: D) -> Result<Vec<Key>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ArrayOrSingle;

    impl<'de> Visitor<'de> for ArrayOrSingle {
        type Value = Vec<Key>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("string or array of strings")
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<Key>, E> {
            if value == "DnD" {
                return Ok(vec![]);
            }
            Ok(vec![Deserialize::deserialize(
                de::value::BorrowedStrDeserializer::new(value),
            )?])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Vec<Key>, A::Error> {
            Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))
        }
    }

    deserializer.deserialize_any(ArrayOrSingle)
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ButtonConfig {
    pub button: Option<String>,
    pub option: Option<String>,
    #[serde(alias = "Svg")]
    pub icon: Option<String>,
    pub icon_active: Option<String>,
    pub text: Option<String>,
    pub theme: Option<String>,
    pub locale: Option<String>,
    pub layer_toggle: Option<String>,
    pub weather_day: Option<usize>,
    #[serde(deserialize_with = "array_or_single", default)]
    pub action: Vec<Key>,
    pub command: Option<String>,
    pub stretch: Option<f64>,
    pub icon_width: Option<i32>,
    pub icon_height: Option<i32>,
    #[serde(default)]
    pub stacked: bool,
    #[serde(default)]
    pub colorize: bool,
    pub font_size: Option<f64>,
    pub max_title_length: Option<usize>,
    pub children: Option<Vec<ButtonConfig>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_inherit_parent_colorize() {
        let parent = ButtonConfig {
            layer_toggle: Some("Child".to_string()),
            colorize: true,
            children: Some(vec![ButtonConfig {
                button: Some("Weather".to_string()),
                ..ButtonConfig::default()
            }]),
            ..ButtonConfig::default()
        };
        let mut layers = Vec::new();

        collect_child_layers(&[parent], &mut layers);

        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].0, "Child");
        assert!(layers[0].1[0].colorize);
    }

    #[test]
    fn pullout_child_layer_toggle_target_is_collected() {
        // A pullout child that opens its own sub-layer (LayerToggle + Children).
        let sub_toggle = ButtonConfig {
            text: Some("open".to_string()),
            layer_toggle: Some("SubLayer".to_string()),
            children: Some(vec![ButtonConfig {
                text: Some("inner".to_string()),
                ..ButtonConfig::default()
            }]),
            ..ButtonConfig::default()
        };
        let keys = vec![
            ButtonConfig {
                text: Some("title".to_string()),
                ..ButtonConfig::default()
            },
            ButtonConfig {
                button: Some("pullout".to_string()),
                children: Some(vec![sub_toggle]),
                ..ButtonConfig::default()
            },
        ];

        // load_config collects child layers BEFORE expanding pullouts.
        let mut child_layers = Vec::new();
        collect_child_layers(&keys, &mut child_layers);
        let (_collapsed, _pullout) = expand_pullout("SystemInfo", keys);

        assert!(
            child_layers.iter().any(|(name, _)| name == "SubLayer"),
            "pullout child's LayerToggle target 'SubLayer' was never collected, \
             so pressing it does nothing"
        );
    }

    #[test]
    fn pullout_splits_into_collapsed_and_expanded() {
        let text = |t: &str| ButtonConfig {
            text: Some(t.to_string()),
            ..ButtonConfig::default()
        };
        let keys = vec![
            text("esc"),
            text("title"),
            ButtonConfig {
                button: Some("pullout".to_string()),
                option: Some("2".to_string()),
                children: Some(vec![text("weather"), text("cpu"), text("battery"), text("date")]),
                ..ButtonConfig::default()
            },
        ];

        let (collapsed, pullout) = expand_pullout("SystemInfo", keys);
        let p = pullout.unwrap();

        // collapsed parent renders normally: esc, title, "<", battery, date
        let labels: Vec<_> = collapsed.iter().map(|b| b.text.clone().unwrap()).collect();
        assert_eq!(labels, ["esc", "title", "<", "battery", "date"]);
        assert_eq!(collapsed[2].layer_toggle.as_deref(), Some("SystemInfo__pullout"));

        // panel overlays the parent: ">" toggle + all children, no parent buttons
        assert_eq!(p.name, "SystemInfo__pullout");
        assert_eq!(p.parent, "SystemInfo");
        let labels: Vec<_> = p.keys.iter().map(|b| b.text.clone().unwrap()).collect();
        assert_eq!(labels, [">", "weather", "cpu", "battery", "date"]);
        assert_eq!(p.keys[0].layer_toggle.as_deref(), Some("SystemInfo"));
    }
}

const PULLOUT_SUFFIX: &str = "__pullout";
const DEFAULT_PULLOUT_VISIBLE: usize = 2;
const DEFAULT_PULLOUT_COVERAGE: f64 = 0.66;

fn pullout_toggle(label: &str, target: String) -> ButtonConfig {
    // "pullout_toggle" makes with_config build a LayerToggle button (accented
    // background) rather than a plain text button, so the "<"/">" stands out
    // when the panel is drawn on top of the parent.
    ButtonConfig {
        button: Some("pullout_toggle".to_string()),
        text: Some(label.to_string()),
        layer_toggle: Some(target),
        ..ButtonConfig::default()
    }
}

// A pullout panel that overlays the right portion of its parent layer.
pub struct Pullout {
    pub name: String,             // panel layer name ("<parent>__pullout")
    pub keys: Vec<ButtonConfig>,  // ">" toggle + all children
    pub parent: String,           // parent layer name, drawn underneath
    pub coverage: f64,            // fraction of the bar the panel covers, from the right
}

// A pullout marker ({ Button = "pullout", Option = "N", Stretch = C, Children = [...] })
// leaves the collapsed layer looking normal: the marker is replaced in place by a
// "<" toggle plus the trailing N children (so the parent renders as usual). Pressing
// "<" switches to a separate panel layer ("<name>__pullout") that draws a ">" toggle
// plus all children over the right `C` fraction of the parent, masking it. Only the
// first marker in a layer is used. Returns the collapsed keys and, if a marker was
// found, the panel.
fn expand_pullout(name: &str, keys: Vec<ButtonConfig>) -> (Vec<ButtonConfig>, Option<Pullout>) {
    let Some(m) = keys
        .iter()
        .position(|b| matches!(b.button.as_deref(), Some("pullout" | "Pullout")))
    else {
        return (keys, None);
    };
    if keys[m + 1..]
        .iter()
        .any(|b| matches!(b.button.as_deref(), Some("pullout" | "Pullout")))
    {
        eprintln!("Layer '{name}' has multiple pullout markers; only the first is used");
    }
    let children = keys[m].children.clone().unwrap_or_default();
    let visible = keys[m]
        .option
        .as_deref()
        .and_then(|o| o.parse::<usize>().ok())
        .unwrap_or(DEFAULT_PULLOUT_VISIBLE)
        .min(children.len());
    let coverage = keys[m]
        .stretch
        .unwrap_or(DEFAULT_PULLOUT_COVERAGE)
        .clamp(0.25, 0.95);
    let expanded_name = format!("{name}{PULLOUT_SUFFIX}");

    let mut collapsed = keys[..m].to_vec();
    collapsed.push(pullout_toggle("<", expanded_name.clone()));
    collapsed.extend_from_slice(&children[children.len() - visible..]);
    collapsed.extend_from_slice(&keys[m + 1..]);

    let mut panel = vec![pullout_toggle(">", name.to_string())];
    panel.extend(children);

    (
        collapsed,
        Some(Pullout {
            name: expanded_name,
            keys: panel,
            parent: name.to_string(),
            coverage,
        }),
    )
}

fn collect_child_layers(buttons: &[ButtonConfig], layers: &mut Vec<(String, Vec<ButtonConfig>)>) {
    for button in buttons {
        if let (Some(target), Some(children)) = (&button.layer_toggle, &button.children) {
            let mut children = children.clone();
            if button.colorize {
                for child in &mut children {
                    child.colorize = true;
                }
            }
            // Descend first so a LayerToggle nested inside these children (or
            // inside a pullout further down) also gets its layer created.
            collect_child_layers(&children, layers);
            layers.push((target.clone(), children));
        } else if let Some(children) = &button.children {
            // A pullout marker holds its buttons in `children` without a
            // `layer_toggle` of its own. expand_pullout later turns those into
            // an overlay panel, but any LayerToggle among them still needs its
            // target layer collected here, or pressing it does nothing.
            collect_child_layers(children, layers);
        }
    }
}

fn load_font(name: &str) -> FontFace {
    let fontconfig = FontConfig::new();
    let mut pattern = Pattern::new(name);
    fontconfig.perform_substitutions(&mut pattern);
    let pat_match = match fontconfig.match_pattern(&pattern) {
        Ok(pat) => pat,
        Err(_) => panic!("Unable to find specified font. If you are using the default config, make sure you have at least one font installed")
    };
    let file_name = pat_match.get_file_name();
    let file_idx = pat_match.get_font_index();
    let ft_library = FtLibrary::init().unwrap();
    let face = ft_library.new_face(file_name, file_idx).unwrap();
    FontFace::create_from_ft(&face).unwrap()
}

fn user_config_path() -> Option<PathBuf> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config_home).join("tiny-dfr/config.toml"));
    }

    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/tiny-dfr/config.toml"))
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(SYSTEM_CFG_PATH)];
    if let Some(user_path) = user_config_path() {
        paths.push(user_path);
    }
    paths
}

fn load_config(_width: u16) -> (Config, Vec<FunctionLayer>) {
    let mut base =
        toml::from_str::<ConfigProxy>(&read_to_string(default_cfg_path()).unwrap()).unwrap();
    for path in config_paths() {
        match read_to_string(&path).and_then(|contents| {
            toml::from_str::<ConfigProxy>(&contents)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
        }) {
            Ok(config) => base.merge(config),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!("Failed to load config {}: {err}", path.display()),
        }
    }
    let mut primary_layer_keys = base.primary_layer_keys.unwrap();

    // Handle configs with only old-style MediaLayerKeys (no SystemInfoLayerKeys)
    let (mut system_info_layer_keys, mut media_layer_keys) =
        match (base.system_info_layer_keys, base.media_layer_keys) {
            (Some(sysinfo), Some(media)) => (sysinfo, media),
            (Some(sysinfo), None) => (sysinfo.clone(), sysinfo), // SystemInfo exists, use it for both
            (None, Some(media)) => (media.clone(), media), // Only MediaLayerKeys, use it for both (old config)
            (None, None) => {
                panic!("Config must have either SystemInfoLayerKeys or MediaLayerKeys defined")
            }
        };
    let mut extra_layers = base.layers.unwrap_or_default();
    let mut child_layers = Vec::new();
    collect_child_layers(&primary_layer_keys, &mut child_layers);
    collect_child_layers(&system_info_layer_keys, &mut child_layers);
    collect_child_layers(&media_layer_keys, &mut child_layers);
    for keys in extra_layers.values() {
        collect_child_layers(keys, &mut child_layers);
    }
    extra_layers.extend(child_layers);

    // Expand pullout markers: the parent keeps its collapsed buttons; each
    // marker also yields an overlay panel layer.
    let mut pullouts: Vec<Pullout> = Vec::new();
    for (name, keys) in [
        ("SystemInfo", &mut system_info_layer_keys),
        ("FKeys", &mut primary_layer_keys),
        ("Media", &mut media_layer_keys),
    ] {
        let (collapsed, pullout) = expand_pullout(name, std::mem::take(keys));
        *keys = collapsed;
        pullouts.extend(pullout);
    }
    for (name, keys) in extra_layers.iter_mut() {
        let (collapsed, pullout) = expand_pullout(name, std::mem::take(keys));
        *keys = collapsed;
        pullouts.extend(pullout);
    }

    let mut layers = vec![
        FunctionLayer::with_config("SystemInfo", system_info_layer_keys),
        FunctionLayer::with_config("FKeys", primary_layer_keys),
        FunctionLayer::with_config("Media", media_layer_keys),
    ];
    for p in pullouts {
        let mut layer = FunctionLayer::with_config(p.name, p.keys);
        layer.set_overlay(p.parent, p.coverage);
        layers.push(layer);
    }
    for (name, keys) in extra_layers {
        if matches!(name.as_str(), "SystemInfo" | "FKeys" | "Media") {
            let key_name = match name.as_str() {
                "SystemInfo" => "SystemInfoLayerKeys",
                "FKeys" => "PrimaryLayerKeys",
                "Media" => "MediaLayerKeys",
                _ => unreachable!(),
            };
            eprintln!(
                "Warning: Layers.{} ignored; use {} for built-in layers",
                name, key_name
            );
            continue;
        }
        layers.push(FunctionLayer::with_config(name, keys));
    }
    let default_layer = base.default_layer.as_deref().unwrap_or("SystemInfo");
    if !layers.iter().any(|layer| layer.name == default_layer) {
        eprintln!(
            "Warning: Invalid DefaultLayer '{}', using SystemInfo",
            default_layer
        );
    }
    let cfg = Config {
        default_layer: if layers.iter().any(|layer| layer.name == default_layer) {
            default_layer.to_string()
        } else {
            "SystemInfo".to_string()
        },
        show_button_outlines: base.show_button_outlines.unwrap(),
        enable_pixel_shift: base.enable_pixel_shift.unwrap(),
        adaptive_brightness: base.adaptive_brightness.unwrap(),
        font_face: load_font(&base.font_template.unwrap()),
        font_size: base.font_size.unwrap_or(32.0),
        active_brightness: base.active_brightness.unwrap(),
        double_press_switch_layers: base.double_press_switch_layers.unwrap(),
        drop_privileges: base.drop_privileges.unwrap_or(true),
        weather_location: base.weather_location,
        weather_fahrenheit: base
            .weather_units
            .map(|u| {
                matches!(
                    u.to_ascii_lowercase().as_str(),
                    "fahrenheit" | "imperial" | "f"
                )
            })
            .unwrap_or(false),
    };
    (cfg, layers)
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    watch_descs: Vec<WatchDescriptor>,
}

fn arm_inotify(inotify_fd: &Inotify) -> Vec<WatchDescriptor> {
    let flags = AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CLOSE | AddWatchFlags::IN_ONESHOT;
    config_paths()
        .into_iter()
        .filter_map(|path| match inotify_fd.add_watch(&path, flags) {
            Ok(wd) => Some(wd),
            Err(Errno::ENOENT) => None,
            e => Some(e.unwrap()),
        })
        .collect()
}

impl ConfigManager {
    pub fn new() -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let watch_descs = arm_inotify(&inotify_fd);
        ConfigManager {
            inotify_fd,
            watch_descs,
        }
    }
    pub fn load_config(&self, width: u16) -> (Config, Vec<FunctionLayer>) {
        load_config(width)
    }
    pub fn update_config(
        &mut self,
        cfg: &mut Config,
        layers: &mut Vec<FunctionLayer>,
        width: u16,
    ) -> bool {
        if self.watch_descs.is_empty() {
            self.watch_descs = arm_inotify(&self.inotify_fd);
            return false;
        }
        match self.inotify_fd.read_events() {
            Err(Errno::EAGAIN) => false,
            r => self.handle_events(cfg, layers, width, r),
        }
    }
    #[cold]
    fn handle_events(
        &mut self,
        cfg: &mut Config,
        layers: &mut Vec<FunctionLayer>,
        width: u16,
        evts: Result<Vec<InotifyEvent>, Errno>,
    ) -> bool {
        let mut ret = false;
        for evt in evts.unwrap() {
            if !self.watch_descs.iter().any(|wd| *wd == evt.wd) {
                continue;
            }
            let parts = load_config(width);
            *cfg = parts.0;
            *layers = parts.1;
            ret = true;
            self.watch_descs = arm_inotify(&self.inotify_fd);
        }
        ret
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}
