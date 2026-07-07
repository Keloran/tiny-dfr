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
    auto_add_esc_key: Option<bool>,
    show_esc_key: Option<bool>,
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
        self.auto_add_esc_key = other.auto_add_esc_key.or(self.auto_add_esc_key);
        self.show_esc_key = other.show_esc_key.or(self.show_esc_key);
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
            layers.push((target.clone(), children));
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

fn load_config(width: u16) -> (Config, Vec<FunctionLayer>) {
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

    let show_esc = base
        .show_esc_key
        .unwrap_or_else(|| width >= 2170 && base.auto_add_esc_key.unwrap_or(true));
    if show_esc {
        for layer in [
            &mut system_info_layer_keys,
            &mut primary_layer_keys,
            &mut media_layer_keys,
        ] {
            layer.insert(
                0,
                ButtonConfig {
                    button: None,
                    option: None,
                    icon: None,
                    icon_active: None,
                    text: Some("esc".into()),
                    theme: None,
                    action: vec![Key::Esc],
                    command: None,
                    stretch: None,
                    locale: None,
                    layer_toggle: None,
                    weather_day: None,
                    icon_width: None,
                    icon_height: None,
                    stacked: false,
                    colorize: false,
                    font_size: None,
                    max_title_length: None,
                    children: None,
                },
            );
        }
    }
    let mut layers = vec![
        FunctionLayer::with_config("SystemInfo", system_info_layer_keys),
        FunctionLayer::with_config("FKeys", primary_layer_keys),
        FunctionLayer::with_config("Media", media_layer_keys),
    ];
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
