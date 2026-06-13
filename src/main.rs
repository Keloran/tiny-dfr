use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface, Surface};
use chrono::{
    format::{Item as ChronoItem, StrftimeItems},
    Local, Locale, Timelike,
};
use drm::control::ClipRect;
use freedesktop_icons::lookup;
use input::{
    event::{
        device::DeviceEvent,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot},
        Event, EventTrait,
    },
    Device as InputDevice, Libinput, LibinputInterface,
};
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{input_event, input_id, timeval, uinput_setup};
use libc::{c_char, O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};
use nix::{
    errno::Errno,
    sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
};
use privdrop::PrivDrop;
use std::{
    cmp::min,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    os::{
        fd::{AsFd, AsRawFd},
        unix::{fs::OpenOptionsExt, io::OwnedFd},
    },
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use udev::MonitorBuilder;

mod backlight;
mod config;
mod display;
mod fonts;
mod pixel_shift;
mod sysinfo_manager;
mod t2_usb;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use config::{ButtonConfig, Config};
use display::DrmBackend;
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};
use sysinfo_manager::SystemInfoManager;

const BUTTON_SPACING_PX: i32 = 16;
const BUTTON_COLOR_INACTIVE: f64 = 0.200;
const BUTTON_COLOR_ACTIVE: f64 = 0.400;
const DEFAULT_ICON_SIZE: i32 = 48;
const TIMEOUT_MS: i32 = 10 * 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BatteryState {
    NotCharging,
    Charging,
    Low,
}

struct BatteryImages {
    plain: Vec<Handle>,
    charging: Vec<Handle>,
    bolt: Handle,
}

#[derive(Eq, PartialEq, Copy, Clone)]
enum BatteryIconMode {
    Percentage,
    Icon,
    Both,
}

impl BatteryIconMode {
    fn should_draw_icon(self) -> bool {
        self != BatteryIconMode::Percentage
    }
    fn should_draw_text(self) -> bool {
        self != BatteryIconMode::Icon
    }
}

enum ButtonImage {
    Text(String),
    Svg(Handle),
    Bitmap(ImageSurface),
    Time(Vec<ChronoItem<'static>>, Locale),
    Battery(String, BatteryIconMode, BatteryImages),
    LayerToggle(String),
    LayerToggleIcon(Handle),
    CpuUsage(Option<Handle>),
    MemoryUsage(Option<Handle>),
    ActiveWindow,
    ActiveWorkspace(Option<Handle>),
    Spacer,
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: Vec<Key>,
    command: Option<String>,
    icon_width: f64,
    icon_height: f64,
}

fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(
        Handle::from_file(path).map_err(|_| anyhow!("failed to load image"))?,
    ))
}

fn try_load_png(path: impl AsRef<Path>, icon_width: i32, icon_height: i32) -> Result<ButtonImage> {
    let mut file = File::open(path)?;
    let surf = ImageSurface::create_from_png(&mut file)?;
    if surf.height() == icon_height && surf.width() == icon_width {
        return Ok(ButtonImage::Bitmap(surf));
    }
    let resized = ImageSurface::create(Format::ARgb32, icon_width, icon_height).unwrap();
    let c = Context::new(&resized).unwrap();
    c.scale(
        icon_width as f64 / surf.width() as f64,
        icon_height as f64 / surf.height() as f64,
    );
    c.set_source_surface(surf, 0.0, 0.0).unwrap();
    c.set_antialias(Antialias::Best);
    c.paint().unwrap();
    Ok(ButtonImage::Bitmap(resized))
}

fn try_load_image(
    name: impl AsRef<str>,
    theme: Option<impl AsRef<str>>,
    icon_width: i32,
    icon_height: i32,
) -> Result<ButtonImage> {
    let name = name.as_ref();
    let locations;

    // Load list of candidate locations
    if let Some(theme) = theme {
        // Freedesktop icons
        let theme = theme.as_ref();
        let candidates = vec![
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .with_size(icon_height as u16)
                .force_svg()
                .find(),
            lookup(name)
                .with_cache()
                .with_theme(theme)
                .force_svg()
                .find(),
        ];

        // .flatten() removes `None` and unwraps `Some` values
        locations = candidates.into_iter().flatten().collect();
    } else {
        // Standard file icons
        locations = vec![
            PathBuf::from(format!("/etc/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/etc/tiny-dfr/{name}.png")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.png")),
        ];
    };

    // Try to load each candidate
    let mut last_err = anyhow!("no suitable icon path was found"); // in case locations is empty

    for location in locations {
        let result = match location.extension().and_then(|s| s.to_str()) {
            Some("png") => try_load_png(&location, icon_width, icon_height),
            Some("svg") => try_load_svg(
                location
                    .to_str()
                    .ok_or(anyhow!("image path is not unicode"))?,
            ),
            _ => Err(anyhow!("invalid file extension")),
        };

        match result {
            Ok(image) => return Ok(image),
            Err(err) => {
                last_err = err.context(format!("while loading path {}", location.display()));
            }
        };
    }

    // if function hasn't returned by now, all sources have been exhausted
    Err(last_err.context(format!("failed loading all possible paths for icon {name}")))
}

fn find_battery_device() -> Option<String> {
    let power_supply_path = "/sys/class/power_supply";
    if let Ok(entries) = fs::read_dir(power_supply_path) {
        for entry in entries.flatten() {
            let dev_path = entry.path();
            let type_path = dev_path.join("type");
            if let Ok(typ) = fs::read_to_string(&type_path) {
                if typ.trim() == "Battery" {
                    if let Some(name) = dev_path.file_name().and_then(|n| n.to_str()) {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

fn get_battery_state(battery: &str) -> (u32, BatteryState) {
    let status_path = format!("/sys/class/power_supply/{}/status", battery);
    let status = fs::read_to_string(&status_path).unwrap_or_else(|_| "Unknown".to_string());

    let capacity = {
        #[cfg(target_arch = "x86_64")]
        {
            let charge_now_path = format!("/sys/class/power_supply/{}/charge_now", battery);
            let charge_full_path = format!("/sys/class/power_supply/{}/charge_full", battery);
            let charge_now = fs::read_to_string(&charge_now_path)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok());
            let charge_full = fs::read_to_string(&charge_full_path)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok());
            match (charge_now, charge_full) {
                (Some(now), Some(full)) if full > 0.0 => ((now / full) * 100.0).round() as u32,
                _ => 100,
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let capacity_path = format!("/sys/class/power_supply/{}/capacity", battery);
            fs::read_to_string(&capacity_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(100)
        }
    };

    let status = match status.trim() {
        "Charging" | "Full" => BatteryState::Charging,
        "Discharging" if capacity < 10 => BatteryState::Low,
        _ => BatteryState::NotCharging,
    };
    (capacity, status)
}

fn get_power_profile() -> Option<String> {
    // Try to get current power profile from powerprofilesctl
    std::process::Command::new("powerprofilesctl")
        .arg("get")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| {
            // Abbreviate profile names
            match s.trim() {
                "power-saver" => "SAV".to_string(),
                "balanced" => "BAL".to_string(),
                "performance" => "PRF".to_string(),
                other => other.chars().take(3).collect::<String>().to_uppercase(),
            }
        })
}

impl Button {
    fn with_config(cfg: ButtonConfig) -> Button {
        if let Some(text) = cfg.text {
            Button::new_text(text, cfg.action)
        } else if let (Some(icon), Some(_)) = (&cfg.icon, &cfg.layer_toggle) {
            Button::new_layer_toggle_icon(
                icon,
                cfg.theme,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
            )
        } else if let Some(label) = cfg.layer_toggle {
            Button::new_layer_toggle(label)
        } else if let Some(time) = cfg.time {
            Button::new_time(cfg.action, &time, cfg.locale.as_deref())
        } else if let Some(battery_mode) = cfg.battery {
            if let Some(battery) = find_battery_device() {
                Button::new_battery(cfg.action, cfg.command, battery, battery_mode, cfg.theme)
            } else {
                Button::new_text("Battery N/A".to_string(), cfg.action)
            }
        } else if cfg.cpu_usage {
            Button::new_cpu_usage(
                cfg.action,
                cfg.command.clone(),
                cfg.icon.clone(),
                cfg.theme.clone(),
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
            )
        } else if cfg.memory_usage {
            Button::new_memory_usage(
                cfg.action,
                cfg.command,
                cfg.icon,
                cfg.theme,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
            )
        } else if cfg.active_window {
            Button::new_active_window(cfg.action)
        } else if cfg.active_workspace {
            Button::new_active_workspace(
                cfg.action,
                cfg.command,
                cfg.icon,
                cfg.theme,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
            )
        } else if let Some(icon) = cfg.icon {
            Button::new_icon(
                &icon,
                cfg.theme,
                cfg.action,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
            )
        } else {
            Button::new_spacer()
        }
    }
    fn new_spacer() -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::Spacer,
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
        }
    }
    fn new_text(text: String, action: Vec<Key>) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Text(text),
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
        }
    }
    fn new_layer_toggle(label: String) -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::LayerToggle(label),
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
        }
    }
    fn new_layer_toggle_icon(
        path: impl AsRef<str>,
        theme: Option<impl AsRef<str>>,
        icon_width: i32,
        icon_height: i32,
    ) -> Button {
        let image =
            try_load_image(path, theme, icon_width, icon_height).expect("failed to load icon");
        Button {
            action: vec![],
            image: match image {
                ButtonImage::Svg(handle) => ButtonImage::LayerToggleIcon(handle),
                ButtonImage::Bitmap(_) => {
                    panic!("Layer toggle icons must be SVG, not bitmap")
                }
                _ => panic!("Unexpected image type for layer toggle icon"),
            },
            command: None,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
            active: false,
            changed: false,
        }
    }
    fn new_icon(
        path: impl AsRef<str>,
        theme: Option<impl AsRef<str>>,
        action: Vec<Key>,
        icon_width: i32,
        icon_height: i32,
    ) -> Button {
        let image =
            try_load_image(path, theme, icon_width, icon_height).expect("failed to load icon");
        Button {
            action,
            image,
            command: None,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
            active: false,
            changed: false,
        }
    }
    fn load_battery_image(icon: &str, theme: Option<impl AsRef<str>>) -> Handle {
        if let ButtonImage::Svg(svg) =
            try_load_image(icon, theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE).unwrap()
        {
            return svg;
        }
        panic!("failed to load icon");
    }
    fn new_battery(
        action: Vec<Key>,
        command: Option<String>,
        battery: String,
        battery_mode: String,
        theme: Option<impl AsRef<str>>,
    ) -> Button {
        let bolt = Self::load_battery_image("bolt", theme.as_ref());
        let mut plain = Vec::new();
        let mut charging = Vec::new();
        for icon in [
            "battery_0_bar", "battery_1_bar", "battery_2_bar", "battery_3_bar",
            "battery_4_bar", "battery_5_bar", "battery_6_bar", "battery_full",
        ] {
            plain.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        for icon in [
            "battery_charging_20", "battery_charging_30", "battery_charging_50",
            "battery_charging_60", "battery_charging_80",
            "battery_charging_90", "battery_charging_full",
        ] {
            charging.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        let battery_mode = match battery_mode.as_str() {
            "icon" => BatteryIconMode::Icon,
            "percentage" => BatteryIconMode::Percentage,
            "both" => BatteryIconMode::Both,
            _ => panic!("invalid battery mode, accepted modes: icon, percentage, both"),
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Battery(
                battery,
                battery_mode,
                BatteryImages {
                    plain,
                    bolt,
                    charging,
                },
            ),
            command,
            icon_width: 0.0,
            icon_height: 0.0,
        }
    }

    fn new_time(action: Vec<Key>, format: &str, locale_str: Option<&str>) -> Button {
        let format_str = if format == "24hr" {
            "%H:%M    %a %-e %b"
        } else if format == "12hr" {
            "%-l:%M %p    %a %-e %b"
        } else {
            format
        };

        let format_items = match StrftimeItems::new(format_str).parse_to_owned() {
            Ok(s) => s,
            Err(e) => panic!("Invalid time format, consult the configuration file for examples of correct ones: {e:?}"),
        };

        let locale = locale_str
            .and_then(|l| Locale::try_from(l).ok())
            .unwrap_or(Locale::POSIX);
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Time(format_items, locale),
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
        }
    }
    fn new_cpu_usage(
        action: Vec<Key>,
        command: Option<String>,
        icon: Option<impl AsRef<str>>,
        theme: Option<impl AsRef<str>>,
        icon_width: i32,
        icon_height: i32,
    ) -> Button {
        let icon_handle = icon.and_then(|i| {
            try_load_image(i, theme, icon_width, icon_height)
                .ok()
                .and_then(|img| match img {
                    ButtonImage::Svg(handle) => Some(handle),
                    _ => None,
                })
        });
        let (w, h) = if icon_handle.is_some() {
            (icon_width as f64, icon_height as f64)
        } else {
            (0.0, 0.0)
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::CpuUsage(icon_handle),
            command,
            icon_width: w,
            icon_height: h,
        }
    }
    fn new_memory_usage(
        action: Vec<Key>,
        command: Option<String>,
        icon: Option<impl AsRef<str>>,
        theme: Option<impl AsRef<str>>,
        icon_width: i32,
        icon_height: i32,
    ) -> Button {
        let icon_handle = icon.and_then(|i| {
            try_load_image(i, theme, icon_width, icon_height)
                .ok()
                .and_then(|img| match img {
                    ButtonImage::Svg(handle) => Some(handle),
                    _ => None,
                })
        });
        let (w, h) = if icon_handle.is_some() {
            (icon_width as f64, icon_height as f64)
        } else {
            (0.0, 0.0)
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::MemoryUsage(icon_handle),
            command,
            icon_width: w,
            icon_height: h,
        }
    }
    fn new_active_window(action: Vec<Key>) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::ActiveWindow,
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
        }
    }
    fn new_active_workspace(
        action: Vec<Key>,
        command: Option<String>,
        icon: Option<impl AsRef<str>>,
        theme: Option<impl AsRef<str>>,
        icon_width: i32,
        icon_height: i32,
    ) -> Button {
        let icon_handle = icon.and_then(|i| {
            try_load_image(i, theme, icon_width, icon_height)
                .ok()
                .and_then(|img| match img {
                    ButtonImage::Svg(handle) => Some(handle),
                    _ => None,
                })
        });
        let (w, h) = if icon_handle.is_some() {
            (icon_width as f64, icon_height as f64)
        } else {
            (0.0, 0.0)
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::ActiveWorkspace(icon_handle),
            command,
            icon_width: w,
            icon_height: h,
        }
    }
    fn needs_faster_refresh(&self) -> bool {
        match &self.image {
            ButtonImage::Time(items, _) => items.iter().any(|item| {
                use chrono::format::{Item, Numeric};
                match item {
                    Item::Numeric(Numeric::Second, _)
                    | Item::Numeric(Numeric::Nanosecond, _)
                    | Item::Numeric(Numeric::Timestamp, _) => true,
                    _ => false,
                }
            }),
            ButtonImage::CpuUsage(_)
            | ButtonImage::MemoryUsage(_)
            | ButtonImage::ActiveWindow
            | ButtonImage::ActiveWorkspace(_) => true,
            _ => false,
        }
    }
    fn render(
        &self,
        c: &Context,
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
        sysinfo_mgr: Option<&SystemInfoManager>,
    ) {
        match &self.image {
            ButtonImage::Text(text) | ButtonImage::LayerToggle(text) => {
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(text).unwrap();
            }
            ButtonImage::Svg(svg) | ButtonImage::LayerToggleIcon(svg) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - self.icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();

                svg.render_document(c, &Rectangle::new(x, y, self.icon_width, self.icon_height))
                    .unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - self.icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();
                c.set_source_surface(surf, x, y).unwrap();
                c.rectangle(x, y, self.icon_width, self.icon_height);
                c.fill().unwrap();
            }
            ButtonImage::Time(format, locale) => {
                let current_time = Local::now();
                let formatted_time = current_time
                    .format_localized_with_items(format.iter(), *locale)
                    .to_string();
                let time_extents = c.text_extents(&formatted_time).unwrap();
                c.move_to(
                    button_left_edge
                        + (button_width as f64 / 2.0 - time_extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + time_extents.height() / 2.0).round(),
                );
                c.show_text(&formatted_time).unwrap();
            }
            ButtonImage::Battery(battery, battery_mode, icons) => {
                let (capacity, state) = get_battery_state(battery);
                let icon = if battery_mode.should_draw_icon() {
                    Some(match state {
                        BatteryState::Charging => match capacity {
                            0..=20 => &icons.charging[0],
                            21..=30 => &icons.charging[1],
                            31..=50 => &icons.charging[2],
                            51..=60 => &icons.charging[3],
                            61..=80 => &icons.charging[4],
                            81..=99 => &icons.charging[5],
                            _ => &icons.charging[6],
                        },
                        _ => match capacity {
                            0 => &icons.plain[0],
                            1..=20 => &icons.plain[1],
                            21..=30 => &icons.plain[2],
                            31..=50 => &icons.plain[3],
                            51..=60 => &icons.plain[4],
                            61..=80 => &icons.plain[5],
                            81..=99 => &icons.plain[6],
                            _ => &icons.plain[7],
                        },
                    })
                } else if state == BatteryState::Charging {
                    Some(&icons.bolt)
                } else {
                    None
                };
                let percent_str = if let Some(profile) = get_power_profile() {
                    format!("{:.0}% {}", capacity, profile)
                } else {
                    format!("{:.0}%", capacity)
                };
                let extents = c.text_extents(&percent_str).unwrap();
                let spacing = 3.0; // Spacing between icon and text
                let mut width = extents.width();
                let mut text_offset = 0.0;
                if let Some(svg) = icon {
                    if !battery_mode.should_draw_text() {
                        width = DEFAULT_ICON_SIZE as f64;
                    } else {
                        width += DEFAULT_ICON_SIZE as f64 + spacing;
                    }
                    text_offset = DEFAULT_ICON_SIZE as f64 + spacing;
                    let x = button_left_edge + (button_width as f64 / 2.0 - width / 2.0).round();
                    let y = y_shift + ((height as f64 - DEFAULT_ICON_SIZE as f64) / 2.0).round();

                    svg.render_document(
                        c,
                        &Rectangle::new(x, y, DEFAULT_ICON_SIZE as f64, DEFAULT_ICON_SIZE as f64),
                    )
                    .unwrap();
                }
                if battery_mode.should_draw_text() {
                    c.move_to(
                        button_left_edge
                            + (button_width as f64 / 2.0 - width / 2.0 + text_offset)
                                .round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&percent_str).unwrap();
                }
            }
            ButtonImage::CpuUsage(icon) => {
                if let Some(mgr) = sysinfo_mgr {
                    if let Some(svg) = icon {
                        // Icon + text layout
                        let icon_size = self.icon_width.min(self.icon_height);
                        let cpu_text = format!("{:.0}%", mgr.get_cpu_usage());
                        let extents = c.text_extents(&cpu_text).unwrap();
                        let spacing = 4.0;
                        let total_width = icon_size + spacing + extents.width();
                        let start_x = button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();
                        
                        let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                        svg.render_document(c, &Rectangle::new(start_x, icon_y, icon_size, icon_size))
                            .unwrap();
                        
                        c.move_to(
                            start_x + icon_size + spacing,
                            y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                        );
                        c.show_text(&cpu_text).unwrap();
                    } else {
                        // Text only (legacy)
                        let cpu_text = format!("CPU {:.1}%", mgr.get_cpu_usage());
                        let extents = c.text_extents(&cpu_text).unwrap();
                        c.move_to(
                            button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                            y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                        );
                        c.show_text(&cpu_text).unwrap();
                    }
                }
            }
            ButtonImage::MemoryUsage(icon) => {
                if let Some(mgr) = sysinfo_mgr {
                    if let Some(svg) = icon {
                        // Icon + text layout
                        let icon_size = self.icon_width.min(self.icon_height);
                        let (used, total) = mgr.get_memory_usage();
                        let mem_text = format!("{:.1}/{:.1}G", 
                            used as f64 / 1024.0 / 1024.0 / 1024.0,
                            total as f64 / 1024.0 / 1024.0 / 1024.0);
                        let extents = c.text_extents(&mem_text).unwrap();
                        let spacing = 4.0;
                        let total_width = icon_size + spacing + extents.width();
                        let start_x = button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();
                        
                        let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                        svg.render_document(c, &Rectangle::new(start_x, icon_y, icon_size, icon_size))
                            .unwrap();
                        
                        c.move_to(
                            start_x + icon_size + spacing,
                            y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                        );
                        c.show_text(&mem_text).unwrap();
                    } else {
                        // Text only (legacy)
                        let (used, total) = mgr.get_memory_usage();
                        let mem_text = format!("MEM {:.1}G/{:.1}G", used as f64 / 1024.0 / 1024.0 / 1024.0, total as f64 / 1024.0 / 1024.0 / 1024.0);
                        let extents = c.text_extents(&mem_text).unwrap();
                        c.move_to(
                            button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                            y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                        );
                        c.show_text(&mem_text).unwrap();
                    }
                }
            }
            ButtonImage::ActiveWindow => {
                if let Some(mgr) = sysinfo_mgr {
                    let window = mgr.get_active_window();
                    let extents = c.text_extents(&window).unwrap();
                    c.move_to(
                        button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&window).unwrap();
                }
            }
            ButtonImage::ActiveWorkspace(icon) => {
                if let Some(mgr) = sysinfo_mgr {
                    let workspace = mgr.get_active_workspace();
                    
                    if let Some(svg) = icon {
                        // Render icon + text layout (similar to battery rendering)
                        let icon_size = self.icon_width.min(self.icon_height);
                        let ws_text = if workspace.is_empty() {
                            "?".to_string()
                        } else {
                            workspace.clone()
                        };
                        let extents = c.text_extents(&ws_text).unwrap();
                        
                        // Calculate total width and spacing
                        let spacing = 4.0;
                        let total_width = icon_size + spacing + extents.width();
                        
                        // Center the entire (icon + text) block
                        let start_x = button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();
                        
                        // Render icon
                        let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                        svg.render_document(c, &Rectangle::new(start_x, icon_y, icon_size, icon_size))
                            .unwrap();
                        
                        // Render text (positioned using same method as battery)
                        c.move_to(
                            start_x + icon_size + spacing,
                            y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                        );
                        c.show_text(&ws_text).unwrap();
                    } else {
                        // Text only (legacy behavior)
                        let ws_text = format!("WS: {}", workspace);
                        let extents = c.text_extents(&ws_text).unwrap();
                        c.move_to(
                            button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                            y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                        );
                        c.show_text(&ws_text).unwrap();
                    }
                }
            }
            ButtonImage::Spacer => (),
        }
    }
    fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool)
    where
        F: AsRawFd,
    {
        if self.active != active {
            self.active = active;
            self.changed = true;

            if !matches!(
                self.image,
                ButtonImage::LayerToggle(_) | ButtonImage::LayerToggleIcon(_)
            ) {
                toggle_keys(uinput, &self.action, active as i32);
                
                // Execute command on button press (not release)
                if active && self.command.is_some() {
                    if let Some(cmd) = &self.command {
                        let _ = std::process::Command::new("sh")
                            .arg("-c")
                            .arg(cmd)
                            .spawn();
                    }
                }
            }
        }
    }
    fn is_layer_toggle(&self) -> bool {
        matches!(
            self.image,
            ButtonImage::LayerToggle(_) | ButtonImage::LayerToggleIcon(_)
        )
    }
    fn is_visible(&self, sysinfo_mgr: Option<&SystemInfoManager>) -> bool {
        match self.image {
            ButtonImage::ActiveWindow | ButtonImage::ActiveWorkspace(_) => sysinfo_mgr
                .map(|mgr| mgr.hyprland_available())
                .unwrap_or(false),
            ButtonImage::Spacer => false,
            _ => true,
        }
    }
    fn set_background_color(&self, c: &Context, color: f64) {
        if let ButtonImage::Battery(battery, _, _) = &self.image {
            let (_, state) = get_battery_state(battery);
            match state {
                BatteryState::NotCharging => c.set_source_rgb(color, color, color),
                BatteryState::Charging => c.set_source_rgb(0.0, color, 0.0),
                BatteryState::Low => c.set_source_rgb(color, 0.0, 0.0),
            }
        } else {
            c.set_source_rgb(color, color, color);
        }
    }
}

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    displays_battery: bool,
    displays_sysinfo: bool,
    buttons: Vec<(usize, Button)>,
    virtual_button_count: usize,
    faster_refresh: bool,
}

impl FunctionLayer {
    fn with_config(cfg: Vec<ButtonConfig>) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }

        let mut virtual_button_count = 0;
        let displays_time = cfg.iter().any(|cfg| cfg.time.is_some());
        let displays_battery = cfg.iter().any(|cfg| cfg.battery.is_some());
        let displays_sysinfo = cfg.iter().any(|cfg| {
            cfg.cpu_usage || cfg.memory_usage || cfg.active_window || cfg.active_workspace
        });
        let buttons = cfg
            .into_iter()
            .scan(&mut virtual_button_count, |state, cfg| {
                let i = **state;
                let mut stretch = cfg.stretch.unwrap_or(1);
                if stretch < 1 {
                    println!("Stretch value must be at least 1, setting to 1.");
                    stretch = 1;
                }
                **state += stretch;
                Some((i, Button::with_config(cfg)))
            })
            .collect::<Vec<_>>();
        let faster_refresh = buttons.iter().any(|(_, b)| b.needs_faster_refresh());
        FunctionLayer {
            displays_time,
            displays_battery,
            displays_sysinfo,
            buttons,
            virtual_button_count,
            faster_refresh,
        }
    }
    fn draw(
        &mut self,
        config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
        sysinfo_mgr: Option<&SystemInfoManager>,
    ) -> Vec<ClipRect> {
        let c = Context::new(surface).unwrap();
        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            Vec::new()
        };
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let pixel_shift_width = if config.enable_pixel_shift {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let virtual_button_width = ((width - pixel_shift_width as i32)
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            as f64
            / self.virtual_button_count as f64;
        let radius = 8.0f64;
        let bot = (height as f64) * 0.15;
        let top = (height as f64) * 0.85;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(32.0);

        for i in 0..self.buttons.len() {
            let end = if i + 1 < self.buttons.len() {
                self.buttons[i + 1].0
            } else {
                self.virtual_button_count
            };
            let (start, button) = &mut self.buttons[i];
            let start = *start;

            if !button.changed && !complete_redraw {
                continue;
            };

            let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor()
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();

            let color = if button.active {
                BUTTON_COLOR_ACTIVE
            } else if config.show_button_outlines {
                BUTTON_COLOR_INACTIVE
            } else {
                0.0
            };
            let button_visible = button.is_visible(sysinfo_mgr);
            if !complete_redraw {
                c.set_source_rgb(0.0, 0.0, 0.0);
                c.rectangle(
                    left_edge,
                    bot - radius,
                    button_width,
                    top - bot + radius * 2.0,
                );
                c.fill().unwrap();
            }
            if button_visible {
                button.set_background_color(&c, color);
                // draw box with rounded corners
                c.new_sub_path();
                let left = left_edge + radius;
                let right = (left_edge + button_width.ceil()) - radius;
                c.arc(
                    right,
                    bot,
                    radius,
                    (-90.0f64).to_radians(),
                    (0.0f64).to_radians(),
                );
                c.arc(
                    right,
                    top,
                    radius,
                    (0.0f64).to_radians(),
                    (90.0f64).to_radians(),
                );
                c.arc(
                    left,
                    top,
                    radius,
                    (90.0f64).to_radians(),
                    (180.0f64).to_radians(),
                );
                c.arc(
                    left,
                    bot,
                    radius,
                    (180.0f64).to_radians(),
                    (270.0f64).to_radians(),
                );
                c.close_path();
                c.fill().unwrap();
            }
            if button_visible {
                c.set_source_rgb(1.0, 1.0, 1.0);
                button.render(
                    &c,
                    height,
                    left_edge,
                    button_width.ceil() as u64,
                    pixel_shift_y,
                    sysinfo_mgr,
                );
            }

            button.changed = false;

            if !complete_redraw {
                modified_regions.push(ClipRect::new(
                    height as u16 - top as u16 - radius as u16,
                    left_edge as u16,
                    height as u16 - bot as u16 + radius as u16,
                    left_edge as u16 + button_width as u16,
                ));
            }
        }

        modified_regions
    }

    fn hit(&self, width: u16, height: u16, x: f64, y: f64, i: Option<usize>) -> Option<usize> {
        let virtual_button_width =
            (width as i32 - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32)) as f64
                / self.virtual_button_count as f64;

        let i = i.unwrap_or_else(|| {
            let virtual_i = (x / (width as f64 / self.virtual_button_count as f64)) as usize;
            self.buttons
                .iter()
                .position(|(start, _)| *start > virtual_i)
                .unwrap_or(self.buttons.len())
                - 1
        });
        if i >= self.buttons.len() {
            return None;
        }

        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };

        let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor();

        let button_width = virtual_button_width
            + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor();

        if x < left_edge
            || x > (left_edge + button_width)
            || y < 0.1 * height as f64
            || y > 0.9 * height as f64
        {
            return None;
        }

        Some(i)
    }
}

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;

        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32)
where
    F: AsRawFd,
{
    uinput
        .write(&[input_event {
            value,
            type_: ty as u16,
            code,
            time: timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        }])
        .unwrap();
}

fn toggle_keys<F>(uinput: &mut UInputHandle<F>, codes: &Vec<Key>, value: i32)
where
    F: AsRawFd,
{
    if codes.is_empty() {
        return;
    }
    for kc in codes {
        emit(uinput, EventKind::Key, *kc as u16, value);
    }
    emit(
        uinput,
        EventKind::Synchronize,
        SynchronizeKind::Report as u16,
        0,
    );
}

fn main() {
    // Check if we're on a T2 MacBook and initialize if needed
    // This can be disabled by setting TINY_DFR_SKIP_T2_INIT=1
    let skip_t2_init = std::env::var("TINY_DFR_SKIP_T2_INIT").unwrap_or_default() == "1";
    
    if !skip_t2_init && t2_usb::is_t2_macbook() {
        println!("Detected T2 MacBook - initializing Touch Bar...");
        let mut t2 = t2_usb::T2TouchBar::new();
        match t2.initialize() {
            Ok(_) => {
                println!("T2 Touch Bar initialized successfully");
                // Extra delay to ensure USB subsystem has fully stabilized
                // This prevents input devices from being grabbed in an inconsistent state
                println!("Allowing USB subsystem to stabilize...");
                std::thread::sleep(std::time::Duration::from_secs(3));
            },
            Err(e) => {
                eprintln!("Warning: T2 initialization failed: {}", e);
                eprintln!("Continuing anyway - the device may already be initialized");
            }
        }
    }

    let mut drm = DrmBackend::open_card().unwrap();
    let (height, width) = drm.mode().size();
    if panic::catch_unwind(AssertUnwindSafe(|| real_main(&mut drm))).is_ok() {
        return;
    }

    let crash_bitmap = include_bytes!("crash_bitmap.raw");
    let drew_crash_bitmap = match drm.map() {
        Ok(mut map) => {
            let data = map.as_mut();
            let mut wptr = 0;
            for byte in crash_bitmap {
                for i in 0..8 {
                    if wptr + 3 >= data.len() {
                        break;
                    }
                    let bit = ((byte >> i) & 0x1) == 0;
                    let color = if bit { 0xFF } else { 0x0 };
                    data[wptr] = color;
                    data[wptr + 1] = color;
                    data[wptr + 2] = color;
                    data[wptr + 3] = color;
                    wptr += 4;
                }
            }
            true
        }
        Err(_) => false,
    };
    if drew_crash_bitmap {
        let _ = drm.dirty(&[ClipRect::new(0, 0, height, width)]);
    }
    std::process::exit(1);
}

fn real_main(drm: &mut DrmBackend) {
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm.fb_info().unwrap().size();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    let mut backlight = BacklightManager::new();
    let mut cfg_mgr = ConfigManager::new();
    let (mut cfg, mut layers) = cfg_mgr.load_config(width);
    let mut pixel_shift = PixelShiftManager::new();
    let sysinfo_mgr = SystemInfoManager::new();
    let mut last = Instant::now();

    if cfg.drop_privileges {
        // drop privileges to input and video group
        let groups = ["input", "video"];

        PrivDrop::default()
            .user("nobody")
            .group_list(&groups)
            .apply()
            .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));
    }

    let mut surface =
        ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut active_layer = 0;
    let mut normal_layer = 0;
    let mut needs_complete_redraw = true;

    let mut input_tb = Libinput::new_with_udev(Interface);
    let mut input_main = Libinput::new_with_udev(Interface);
    input_tb.udev_assign_seat("seat-touchbar").unwrap();
    input_main.udev_assign_seat("seat0").unwrap();
    let udev_monitor = MonitorBuilder::new()
        .unwrap()
        .match_subsystem("power_supply")
        .unwrap()
        .listen()
        .unwrap();
    let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();
    epoll
        .add(input_main.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0))
        .unwrap();
    epoll
        .add(input_tb.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 1))
        .unwrap();
    epoll
        .add(cfg_mgr.fd(), EpollEvent::new(EpollFlags::EPOLLIN, 2))
        .unwrap();
    epoll
        .add(&udev_monitor, EpollEvent::new(EpollFlags::EPOLLIN, 3))
        .unwrap();
    uinput.set_evbit(EventKind::Key).unwrap();
    for k in Key::iter() {
        uinput.set_keybit(k).unwrap();
    }
    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput
        .dev_setup(&uinput_setup {
            id: input_id {
                bustype: 0x19,
                vendor: 0x1209,
                product: 0x316E,
                version: 1,
            },
            ff_effects_max: 0,
            name: dev_name_c,
        })
        .unwrap();
    uinput.dev_create().unwrap();

    let mut digitizer: Option<InputDevice> = None;
    let mut touches = HashMap::new();
    let mut last_redraw_ts = if layers[active_layer].faster_refresh {
        Local::now().second()
    } else {
        Local::now().minute()
    };
    loop {
        if cfg_mgr.update_config(&mut cfg, &mut layers, width) {
            active_layer = 0;
            normal_layer = 0;
            needs_complete_redraw = true;
        }

        let now = Local::now();
        let ms_left = ((60 - now.second()) * 1000) as i32;
        let mut next_timeout_ms = min(ms_left, TIMEOUT_MS);

        if cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = pixel_shift.update();
            if pixel_shift_needs_redraw {
                needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, pixel_shift_next_timeout_ms);
        }

        let current_ts = if layers[active_layer].faster_refresh {
            Local::now().second()
        } else {
            Local::now().minute()
        };
        if layers[active_layer].displays_time && (current_ts != last_redraw_ts) {
            needs_complete_redraw = true;
            last_redraw_ts = current_ts;
        }
        if layers[active_layer].displays_battery {
            for button in &mut layers[active_layer].buttons {
                if let ButtonImage::Battery(_, _, _) = button.1.image {
                    button.1.changed = true;
                }
            }
        }
        
        if layers[active_layer].displays_sysinfo {
            for button in &mut layers[active_layer].buttons {
                match button.1.image {
                    ButtonImage::CpuUsage(_)
                    | ButtonImage::MemoryUsage(_)
                    | ButtonImage::ActiveWindow
                    | ButtonImage::ActiveWorkspace(_) => {
                        button.1.changed = true;
                    }
                    _ => {}
                }
            }
        }

        if needs_complete_redraw || layers[active_layer].buttons.iter().any(|b| b.1.changed) {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let sysinfo_mgr_ref = if layers[active_layer].displays_sysinfo {
                Some(&sysinfo_mgr)
            } else {
                None
            };
            let clips = layers[active_layer].draw(
                &cfg,
                width as i32,
                height as i32,
                &surface,
                shift,
                needs_complete_redraw,
                sysinfo_mgr_ref,
            );
            let data = surface.data().unwrap();
            drm.map().unwrap().as_mut()[..data.len()].copy_from_slice(&data);
            drm.dirty(&clips).unwrap();
            needs_complete_redraw = false;
        }

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.unwrap(),
        };

        _ = udev_monitor.iter().last();

        input_tb.dispatch().unwrap();
        input_main.dispatch().unwrap();
        for event in &mut input_tb.clone().chain(input_main.clone()) {
            backlight.process_event(&event);
            match event {
                Event::Device(DeviceEvent::Added(evt)) => {
                    let dev = evt.device();
                    if dev.name().contains(" Touch Bar") {
                        digitizer = Some(dev);
                    }
                }
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    if key.key() == Key::Fn as u32 {
                        if cfg.double_press_switch_layers > 0 && layers.len() == 2 && key.key_state() == KeyState::Pressed {
                            if last.elapsed() < Duration::from_millis(cfg.double_press_switch_layers.into()) {
                                layers.swap(0, 1);
                            }
                            last = Instant::now();
                        }
                        let new_layer = match key.key_state() {
                            KeyState::Pressed => 1,
                            KeyState::Released => normal_layer,
                        };
                        if active_layer != new_layer {
                            active_layer = new_layer;
                            needs_complete_redraw = true;
                        }
                    }
                }
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer || backlight.current_bl() == 0 {
                        continue;
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(width as u32);
                            let y = dn.y_transformed(height as u32);
                            if let Some(btn) = layers[active_layer].hit(width, height, x, y, None) {
                                touches.insert(dn.seat_slot(), (active_layer, btn));
                                layers[active_layer].buttons[btn]
                                    .1
                                    .set_active(&mut uinput, true);
                            }
                        }
                        TouchEvent::Motion(mtn) => {
                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            if !touches.contains_key(&mtn.seat_slot()) {
                                continue;
                            }

                            let (layer, btn) = *touches.get(&mtn.seat_slot()).unwrap();
                            let hit = layers[active_layer]
                                .hit(width, height, x, y, Some(btn))
                                .is_some();
                            layers[layer].buttons[btn].1.set_active(&mut uinput, hit);
                        }
                        TouchEvent::Up(up) => {
                            if !touches.contains_key(&up.seat_slot()) {
                                continue;
                            }
                            let (layer, btn) = *touches.get(&up.seat_slot()).unwrap();
                            let is_layer_toggle = layers[layer].buttons[btn].1.is_layer_toggle();
                            layers[layer].buttons[btn].1.set_active(&mut uinput, false);
                            touches.remove(&up.seat_slot());
                            if is_layer_toggle && layers.len() > 2 {
                                normal_layer = if normal_layer == 2 { 0 } else { 2 };
                                active_layer = normal_layer;
                                needs_complete_redraw = true;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        backlight.update_backlight(&cfg);
    }
}
