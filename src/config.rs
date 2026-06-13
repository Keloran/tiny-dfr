use crate::fonts::{FontConfig, Pattern};
use crate::FunctionLayer;
use anyhow::Error;
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
use std::{fmt, fs::read_to_string, os::fd::AsFd};

const USER_CFG_PATH: &str = "/etc/tiny-dfr/config.toml";

pub struct Config {
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_face: FontFace,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub double_press_switch_layers: u32,
    pub drop_privileges: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    default_layer: Option<String>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_template: Option<String>,
    adaptive_brightness: Option<bool>,
    active_brightness: Option<u32>,
    double_press_switch_layers: Option<u32>,
    auto_add_esc_key: Option<bool>,
    show_esc_key: Option<bool>,
    drop_privileges: Option<bool>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    system_info_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
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

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ButtonConfig {
    #[serde(alias = "Svg")]
    pub icon: Option<String>,
    pub text: Option<String>,
    pub theme: Option<String>,
    pub time: Option<String>,
    pub battery: Option<String>,
    pub locale: Option<String>,
    pub layer_toggle: Option<String>,
    #[serde(default)]
    pub cpu_usage: bool,
    #[serde(default)]
    pub memory_usage: bool,
    #[serde(default)]
    pub active_window: bool,
    #[serde(default)]
    pub active_workspace: bool,
    #[serde(deserialize_with = "array_or_single", default)]
    pub action: Vec<Key>,
    pub command: Option<String>,
    pub stretch: Option<usize>,
    pub icon_width: Option<i32>,
    pub icon_height: Option<i32>,
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

fn load_config(width: u16) -> (Config, Vec<FunctionLayer>) {
    let mut base =
        toml::from_str::<ConfigProxy>(&read_to_string("/usr/share/tiny-dfr/config.toml").unwrap())
            .unwrap();
    let user = read_to_string(USER_CFG_PATH)
        .map_err::<Error, _>(|e| e.into())
        .and_then(|r| Ok(toml::from_str::<ConfigProxy>(&r)?));
    if let Ok(user) = user {
        base.default_layer = user.default_layer.or(base.default_layer);
        base.show_button_outlines = user.show_button_outlines.or(base.show_button_outlines);
        base.enable_pixel_shift = user.enable_pixel_shift.or(base.enable_pixel_shift);
        base.font_template = user.font_template.or(base.font_template);
        base.adaptive_brightness = user.adaptive_brightness.or(base.adaptive_brightness);
        base.system_info_layer_keys = user.system_info_layer_keys.or(base.system_info_layer_keys);
        base.primary_layer_keys = user.primary_layer_keys.or(base.primary_layer_keys);
        base.media_layer_keys = user.media_layer_keys.or(base.media_layer_keys);
        base.active_brightness = user.active_brightness.or(base.active_brightness);
        base.double_press_switch_layers = user.double_press_switch_layers.or(base.double_press_switch_layers);
        base.auto_add_esc_key = user.auto_add_esc_key.or(base.auto_add_esc_key);
        base.show_esc_key = user.show_esc_key.or(base.show_esc_key);
        base.drop_privileges = user.drop_privileges.or(base.drop_privileges);
    };
    let mut primary_layer_keys = base.primary_layer_keys.unwrap();
    
    // Handle configs with only old-style MediaLayerKeys (no SystemInfoLayerKeys)
    let (mut system_info_layer_keys, mut media_layer_keys) = match (base.system_info_layer_keys, base.media_layer_keys) {
        (Some(sysinfo), Some(media)) => (sysinfo, media),
        (Some(sysinfo), None) => (sysinfo.clone(), sysinfo), // SystemInfo exists, use it for both
        (None, Some(media)) => (media.clone(), media), // Only MediaLayerKeys, use it for both (old config)
        (None, None) => panic!("Config must have either SystemInfoLayerKeys or MediaLayerKeys defined"),
    };
    let show_esc = base
        .show_esc_key
        .unwrap_or_else(|| width >= 2170 && base.auto_add_esc_key.unwrap_or(true));
    if show_esc {
        for layer in [&mut system_info_layer_keys, &mut primary_layer_keys, &mut media_layer_keys] {
            layer.insert(
                0,
                ButtonConfig {
                    icon: None,
                    text: Some("esc".into()),
                    theme: None,
                    action: vec![Key::Esc],
                    command: None,
                    stretch: None,
                    time: None,
                    locale: None,
                    battery: None,
                    layer_toggle: None,
                    cpu_usage: false,
                    memory_usage: false,
                    active_window: false,
                    active_workspace: false,
                    icon_width: None,
                    icon_height: None,
                },
            );
        }
    }
    let system_info_layer = FunctionLayer::with_config(system_info_layer_keys);
    let fkey_layer = FunctionLayer::with_config(primary_layer_keys);
    let media_layer = FunctionLayer::with_config(media_layer_keys);
    
    // Determine layer order based on default layer setting
    let default_layer = base.default_layer.as_deref().unwrap_or("SystemInfo");
    let layers = match default_layer {
        "SystemInfo" => vec![system_info_layer, fkey_layer, media_layer],
        "FKeys" => vec![fkey_layer, system_info_layer, media_layer],
        "Media" => vec![media_layer, fkey_layer, system_info_layer],
        _ => {
            eprintln!("Warning: Invalid DefaultLayer '{}', using SystemInfo", default_layer);
            vec![system_info_layer, fkey_layer, media_layer]
        }
    };
    let cfg = Config {
        show_button_outlines: base.show_button_outlines.unwrap(),
        enable_pixel_shift: base.enable_pixel_shift.unwrap(),
        adaptive_brightness: base.adaptive_brightness.unwrap(),
        font_face: load_font(&base.font_template.unwrap()),
        active_brightness: base.active_brightness.unwrap(),
        double_press_switch_layers: base.double_press_switch_layers.unwrap(),
        drop_privileges: base.drop_privileges.unwrap_or(true),
    };
    (cfg, layers)
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    watch_desc: Option<WatchDescriptor>,
}

fn arm_inotify(inotify_fd: &Inotify) -> Option<WatchDescriptor> {
    let flags = AddWatchFlags::IN_MOVED_TO | AddWatchFlags::IN_CLOSE | AddWatchFlags::IN_ONESHOT;
    match inotify_fd.add_watch(USER_CFG_PATH, flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

impl ConfigManager {
    pub fn new() -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let watch_desc = arm_inotify(&inotify_fd);
        ConfigManager {
            inotify_fd,
            watch_desc,
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
        if self.watch_desc.is_none() {
            self.watch_desc = arm_inotify(&self.inotify_fd);
            return false;
        }
        match self.inotify_fd.read_events() {
            Err(Errno::EAGAIN) => false,
            r => self.handle_events(cfg, layers, width, r),
        }
    }
    #[cold]
    fn handle_events(&mut self, cfg: &mut Config, layers: &mut Vec<FunctionLayer>, width: u16, evts: Result<Vec<InotifyEvent>, Errno>) -> bool {
        let mut ret = false;
        for evt in evts.unwrap() {
            if Some(evt.wd) != self.watch_desc {
                continue;
            }
            let parts = load_config(width);
            *cfg = parts.0;
            *layers = parts.1;
            ret = true;
            self.watch_desc = arm_inotify(&self.inotify_fd);
        }
        ret
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}
