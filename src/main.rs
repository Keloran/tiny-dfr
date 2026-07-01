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
    env,
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
mod notification_manager;
mod pixel_shift;
mod slider;
mod sysinfo_manager;
mod t2_usb;
mod weather;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use config::{ButtonConfig, Config};
use display::DrmBackend;
use notification_manager::NotificationManager;
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};
use slider::{Slider, SliderKind};
use sysinfo_manager::SystemInfoManager;
use t2_usb::{is_t2_macbook, T2TouchBar};
use weather::WeatherManager;

const BUTTON_SPACING_PX: i32 = 16;
const BUTTON_COLOR_INACTIVE: f64 = 0.200;
const BUTTON_COLOR_ACTIVE: f64 = 0.400;
const DEFAULT_ICON_SIZE: i32 = 48;
const TIMEOUT_MS: i32 = 10 * 1000;
const SLIDER_REFRESH_MS: i32 = 1000;
const DOUBLE_TAP_TIMEOUT: Duration = Duration::from_millis(300);
const LONG_PRESS_TIMEOUT: Duration = Duration::from_millis(600);
const EXIT_DRM_UNAVAILABLE: i32 = 75;
const DRM_OPEN_ATTEMPTS: u32 = 6;
const DRM_OPEN_RETRY_DELAY: Duration = Duration::from_secs(2);
const T2_USB_RECOVERY_ENV: &str = "TINY_DFR_T2_USB_RECOVERY";

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

struct WeatherIcons {
    icons: HashMap<&'static str, Handle>,
}

impl WeatherIcons {
    fn load(theme: Option<&str>) -> WeatherIcons {
        let mut icons = HashMap::new();
        for name in [
            "weather_sunny",
            "weather_cloudy",
            "weather_rainy",
            "weather_snowy",
            "weather_moon",
        ] {
            if let Ok(ButtonImage::Svg(handle)) =
                try_load_image(name, theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE)
            {
                icons.insert(name, handle);
            }
        }
        WeatherIcons { icons }
    }
    fn pick(&self, name: &str) -> Option<(&'static str, Handle)> {
        self.icons
            .get_key_value(name)
            .or_else(|| self.icons.get_key_value("weather_cloudy"))
            .map(|(name, handle)| (*name, handle.clone()))
    }
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
    Battery(String, BatteryIconMode, BatteryImages, bool),
    LayerToggle(String, String),
    CpuUsage(Option<Handle>, bool),
    MemoryUsage(Option<Handle>, bool),
    ActiveWindow,
    ActiveWorkspace(Option<Handle>),
    Slider {
        state: Slider,
        icon: Option<Handle>,
        muted_icon: Option<Handle>,
        colorize: bool,
    },
    WeatherCurrent(WeatherIcons, bool),
    WeatherForecast(usize, WeatherIcons, bool),
    Notification(NotificationButton, Option<Handle>, Option<Handle>),
    Spacer,
}

#[derive(Clone, Copy)]
enum NotificationButton {
    Count,
    Back,
    Previous,
    Text,
    Next,
    Dnd,
}

fn notification_button_alias(name: &str) -> Option<NotificationButton> {
    match name {
        "notifications" | "noticiations" => Some(NotificationButton::Count),
        "PreviousLayer" => Some(NotificationButton::Back),
        "PreviousNotification" => Some(NotificationButton::Previous),
        "NotificationContent" => Some(NotificationButton::Text),
        "NextNotification" => Some(NotificationButton::Next),
        "DnDNotification" => Some(NotificationButton::Dnd),
        _ => None,
    }
}

fn normalize_time_option(option: String) -> String {
    match option.as_str() {
        "24h" => "24hr".to_string(),
        "12h" => "12hr".to_string(),
        _ => option,
    }
}

fn usage_color(percent: f64) -> (f64, f64, f64) {
    match percent {
        p if p <= 10.0 => (1.0, 1.0, 1.0),
        p if p <= 25.0 => (1.0, 0.85, 0.0),
        p if p <= 55.0 => (1.0, 0.45, 0.0),
        p if p <= 75.0 => (0.55, 0.25, 1.0),
        _ => (1.0, 0.0, 0.0),
    }
}

fn battery_color(percent: u32) -> (f64, f64, f64) {
    match percent {
        0..=10 => (1.0, 0.0, 0.0),
        11..=25 => (0.55, 0.25, 1.0),
        26..=50 => (1.0, 0.45, 0.0),
        51..=75 => (0.0, 0.45, 1.0),
        _ => (0.0, 0.8, 0.0),
    }
}

fn weather_icon_color(icon: &str) -> Option<(f64, f64, f64)> {
    match icon {
        "weather_sunny" | "weather_moon" => Some((1.0, 0.85, 0.0)),
        _ => None,
    }
}

fn slider_icon_color(kind: SliderKind, percent: u32) -> Option<(f64, f64, f64)> {
    if kind == SliderKind::Volume {
        return None;
    }
    let t = (percent as f64 / 100.0).clamp(0.0, 1.0);
    Some((1.0, 1.0 - 0.55 * t, 1.0 - t))
}

fn layer_index(layers: &[FunctionLayer], name: &str) -> Option<usize> {
    layers.iter().position(|layer| layer.name == name)
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: Vec<Key>,
    command: Option<String>,
    icon_width: f64,
    icon_height: f64,
    stacked: bool,
    font_size: Option<f64>,
    max_title_length: Option<usize>,
    toggle_target: Option<String>,
    hold_started: Option<Instant>,
}

// Unified rendering structures
enum ButtonContent {
    // Simple text centered
    SimpleText(String),
    // Icon centered
    Icon(Handle, f64), // Handle and size
    // Bitmap centered
    Bitmap(ImageSurface, f64, f64), // Surface, width, height
    // Icon + single line of text (horizontal or stacked)
    IconWithText {
        icon: Handle,
        icon_size: f64,
        text: String,
    },
    ColoredIconWithText {
        icon: Handle,
        icon_size: f64,
        icon_color: (f64, f64, f64),
        overlay_icon: Option<Handle>,
        overlay_color: (f64, f64, f64),
        text: String,
    },
    IconWithCenteredText {
        icon: Handle,
        icon_size: f64,
        text: String,
    },
    // Icon + multiple lines of text (always stacked).
    // emphasize_first renders the first line 2pt larger (used for the weekday).
    IconWithMultilineText {
        icon: Handle,
        icon_size: f64,
        lines: Vec<String>,
        emphasize_first: bool,
    },
    ColoredIconWithMultilineText {
        icon: Handle,
        icon_size: f64,
        icon_color: (f64, f64, f64),
        overlay_icon: Option<Handle>,
        overlay_color: (f64, f64, f64),
        lines: Vec<String>,
        emphasize_first: bool,
    },
    // Clipped text (for window titles that might overflow)
    ClippedText(String),
    // Multiple centered lines of text (no icon)
    MultilineText(Vec<String>),
    // Slider with a live value indicator (brightness / backlight / volume)
    SliderBar {
        fraction: f64,
        muted: bool,
        percent: u32,
        icon_color: Option<(f64, f64, f64)>,
        icon: Option<Handle>,
        muted_icon: Option<Handle>,
    },
    // Nothing (spacer)
    Empty,
}

impl Button {
    // Get the content to render based on button type
    fn get_content(
        &self,
        sysinfo_mgr: Option<&SystemInfoManager>,
        weather_mgr: Option<&WeatherManager>,
        notification_mgr: Option<&NotificationManager>,
    ) -> ButtonContent {
        match &self.image {
            ButtonImage::Text(text) | ButtonImage::LayerToggle(_, text) => {
                ButtonContent::SimpleText(text.clone())
            }
            ButtonImage::Svg(svg) => {
                ButtonContent::Icon(svg.clone(), self.icon_width.min(self.icon_height))
            }
            ButtonImage::Bitmap(surf) => {
                ButtonContent::Bitmap(surf.clone(), self.icon_width, self.icon_height)
            }
            ButtonImage::Time(format, locale) => {
                let current_time = Local::now();
                let formatted_time = current_time
                    .format_localized_with_items(format.iter(), *locale)
                    .to_string();
                if self.stacked {
                    // Split on runs of 2+ whitespace so each group (e.g. the
                    // time and the date in the built-in "24hr"/"12hr" formats,
                    // which separate them with spaces) lands on its own line.
                    let lines: Vec<String> = formatted_time
                        .split("  ")
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect();
                    if lines.len() > 1 {
                        ButtonContent::MultilineText(lines)
                    } else {
                        ButtonContent::SimpleText(formatted_time)
                    }
                } else {
                    ButtonContent::SimpleText(formatted_time)
                }
            }
            ButtonImage::Battery(battery, battery_mode, icons, colorize) => {
                let (capacity, state) = get_battery_state(battery);
                let icon = if battery_mode.should_draw_icon() {
                    Some(match state {
                        BatteryState::Charging if !*colorize => match capacity {
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

                if self.stacked && battery_mode.should_draw_text() {
                    let percent_str = format!("{:.0}%", capacity);
                    let profile_str = get_power_profile().unwrap_or_default();
                    let lines = if profile_str.is_empty() {
                        vec![percent_str]
                    } else {
                        vec![percent_str, profile_str]
                    };

                    if let Some(icon_handle) = icon {
                        if *colorize {
                            ButtonContent::ColoredIconWithMultilineText {
                                icon: icon_handle.clone(),
                                icon_size: DEFAULT_ICON_SIZE as f64,
                                icon_color: battery_color(capacity),
                                overlay_icon: (state == BatteryState::Charging)
                                    .then(|| icons.bolt.clone()),
                                overlay_color: (1.0, 0.85, 0.0),
                                lines,
                                emphasize_first: false,
                            }
                        } else {
                            ButtonContent::IconWithMultilineText {
                                icon: icon_handle.clone(),
                                icon_size: DEFAULT_ICON_SIZE as f64,
                                lines,
                                emphasize_first: false,
                            }
                        }
                    } else {
                        ButtonContent::SimpleText(lines.join(" "))
                    }
                } else if let Some(icon_handle) = icon {
                    if battery_mode.should_draw_text() {
                        let text = if let Some(profile) = get_power_profile() {
                            format!("{:.0}% {}", capacity, profile)
                        } else {
                            format!("{:.0}%", capacity)
                        };
                        if *colorize {
                            ButtonContent::ColoredIconWithText {
                                icon: icon_handle.clone(),
                                icon_size: DEFAULT_ICON_SIZE as f64,
                                icon_color: battery_color(capacity),
                                overlay_icon: (state == BatteryState::Charging)
                                    .then(|| icons.bolt.clone()),
                                overlay_color: (1.0, 0.85, 0.0),
                                text,
                            }
                        } else {
                            ButtonContent::IconWithText {
                                icon: icon_handle.clone(),
                                icon_size: DEFAULT_ICON_SIZE as f64,
                                text,
                            }
                        }
                    } else {
                        ButtonContent::Icon(icon_handle.clone(), DEFAULT_ICON_SIZE as f64)
                    }
                } else {
                    let text = if let Some(profile) = get_power_profile() {
                        format!("{:.0}% {}", capacity, profile)
                    } else {
                        format!("{:.0}%", capacity)
                    };
                    ButtonContent::SimpleText(text)
                }
            }
            ButtonImage::CpuUsage(icon, colorize) => {
                if let Some(mgr) = sysinfo_mgr {
                    let usage = mgr.get_cpu_usage();
                    if let Some(svg) = icon {
                        let cpu_text = format!("{:.0}%", usage);
                        if *colorize {
                            ButtonContent::ColoredIconWithText {
                                icon: svg.clone(),
                                icon_size: self.icon_width.min(self.icon_height),
                                icon_color: usage_color(usage as f64),
                                overlay_icon: None,
                                overlay_color: (1.0, 1.0, 1.0),
                                text: cpu_text,
                            }
                        } else {
                            ButtonContent::IconWithText {
                                icon: svg.clone(),
                                icon_size: self.icon_width.min(self.icon_height),
                                text: cpu_text,
                            }
                        }
                    } else {
                        let cpu_text = format!("CPU {:.1}%", usage);
                        ButtonContent::SimpleText(cpu_text)
                    }
                } else {
                    ButtonContent::Empty
                }
            }
            ButtonImage::MemoryUsage(icon, colorize) => {
                if let Some(mgr) = sysinfo_mgr {
                    if let Some(svg) = icon {
                        let (used, total) = mgr.get_memory_usage();
                        let usage = if total > 0 {
                            (used as f64 / total as f64) * 100.0
                        } else {
                            0.0
                        };
                        if self.stacked {
                            let lines = vec![
                                format!("used: {:.1}G", used as f64 / 1024.0 / 1024.0 / 1024.0),
                                format!("total: {:.1}G", total as f64 / 1024.0 / 1024.0 / 1024.0),
                            ];
                            ButtonContent::IconWithMultilineText {
                                icon: svg.clone(),
                                icon_size: self.icon_width.min(self.icon_height),
                                lines,
                                emphasize_first: false,
                            }
                        } else {
                            let mem_text = format!(
                                "{:.1}/{:.1}G",
                                used as f64 / 1024.0 / 1024.0 / 1024.0,
                                total as f64 / 1024.0 / 1024.0 / 1024.0
                            );
                            if *colorize {
                                ButtonContent::ColoredIconWithText {
                                    icon: svg.clone(),
                                    icon_size: self.icon_width.min(self.icon_height),
                                    icon_color: usage_color(usage),
                                    overlay_icon: None,
                                    overlay_color: (1.0, 1.0, 1.0),
                                    text: mem_text,
                                }
                            } else {
                                ButtonContent::IconWithText {
                                    icon: svg.clone(),
                                    icon_size: self.icon_width.min(self.icon_height),
                                    text: mem_text,
                                }
                            }
                        }
                    } else {
                        let (used, total) = mgr.get_memory_usage();
                        let mem_text = format!(
                            "MEM {:.1}G/{:.1}G",
                            used as f64 / 1024.0 / 1024.0 / 1024.0,
                            total as f64 / 1024.0 / 1024.0 / 1024.0
                        );
                        ButtonContent::SimpleText(mem_text)
                    }
                } else {
                    ButtonContent::Empty
                }
            }
            ButtonImage::ActiveWindow => {
                if let Some(mgr) = sysinfo_mgr {
                    ButtonContent::ClippedText(mgr.get_active_window())
                } else {
                    ButtonContent::Empty
                }
            }
            ButtonImage::ActiveWorkspace(icon) => {
                if let Some(mgr) = sysinfo_mgr {
                    let workspace = mgr.get_active_workspace();
                    if let Some(svg) = icon {
                        let ws_text = if workspace.is_empty() {
                            "?".to_string()
                        } else {
                            workspace.clone()
                        };
                        ButtonContent::IconWithCenteredText {
                            icon: svg.clone(),
                            icon_size: self.icon_width.min(self.icon_height),
                            text: ws_text,
                        }
                    } else {
                        ButtonContent::SimpleText(format!("WS: {}", workspace))
                    }
                } else {
                    ButtonContent::Empty
                }
            }
            ButtonImage::Slider {
                state,
                icon,
                muted_icon,
                colorize,
            } => ButtonContent::SliderBar {
                fraction: state.value(),
                muted: state.muted(),
                percent: state.percent(),
                icon_color: colorize
                    .then(|| slider_icon_color(state.kind(), state.percent()))
                    .flatten(),
                icon: icon.clone(),
                muted_icon: muted_icon.clone(),
            },
            ButtonImage::WeatherCurrent(icons, colorize) => {
                if let Some(mgr) = weather_mgr {
                    let d = mgr.data();
                    if d.available {
                        let temp = d
                            .current_temp
                            .map(|t| format!("{:.0}{}", t, d.unit))
                            .unwrap_or_else(|| "--".to_string());
                        if let Some((icon_name, icon)) = icons.pick(&d.current_icon) {
                            if *colorize && weather_icon_color(icon_name).is_some() {
                                ButtonContent::ColoredIconWithText {
                                    icon,
                                    icon_size: DEFAULT_ICON_SIZE as f64,
                                    icon_color: weather_icon_color(icon_name).unwrap(),
                                    overlay_icon: None,
                                    overlay_color: (1.0, 1.0, 1.0),
                                    text: temp,
                                }
                            } else {
                                ButtonContent::IconWithText {
                                    icon,
                                    icon_size: DEFAULT_ICON_SIZE as f64,
                                    text: temp,
                                }
                            }
                        } else {
                            ButtonContent::SimpleText(temp)
                        }
                    } else {
                        ButtonContent::SimpleText("…".to_string())
                    }
                } else {
                    ButtonContent::Empty
                }
            }
            ButtonImage::WeatherForecast(day, icons, colorize) => {
                if let Some(mgr) = weather_mgr {
                    let d = mgr.data();
                    if let Some(forecast) = d.days.get(*day) {
                        let temps = format!(
                            "{}/{}{}",
                            forecast.tmax.round() as i64,
                            forecast.tmin.round() as i64,
                            d.unit
                        );
                        let mut lines = Vec::new();
                        if !forecast.weekday.is_empty() {
                            lines.push(forecast.weekday.clone());
                        }
                        lines.push(temps);
                        if !forecast.desc.is_empty() {
                            lines.push(forecast.desc.clone());
                        }
                        if let Some((icon_name, icon)) = icons.pick(&forecast.icon) {
                            if *colorize && weather_icon_color(icon_name).is_some() {
                                ButtonContent::ColoredIconWithMultilineText {
                                    icon,
                                    icon_size: DEFAULT_ICON_SIZE as f64,
                                    icon_color: weather_icon_color(icon_name).unwrap(),
                                    overlay_icon: None,
                                    overlay_color: (1.0, 1.0, 1.0),
                                    lines,
                                    emphasize_first: true,
                                }
                            } else {
                                ButtonContent::IconWithMultilineText {
                                    icon,
                                    icon_size: DEFAULT_ICON_SIZE as f64,
                                    lines,
                                    emphasize_first: true,
                                }
                            }
                        } else {
                            ButtonContent::MultilineText(lines)
                        }
                    } else {
                        ButtonContent::SimpleText("--".to_string())
                    }
                } else {
                    ButtonContent::Empty
                }
            }
            ButtonImage::Notification(kind, icon, active_icon) => match (kind, notification_mgr) {
                (NotificationButton::Count, Some(mgr)) => {
                    if let Some(icon) = icon {
                        ButtonContent::IconWithText {
                            icon: icon.clone(),
                            icon_size: DEFAULT_ICON_SIZE as f64,
                            text: mgr.count_text(),
                        }
                    } else {
                        ButtonContent::SimpleText(mgr.count_text())
                    }
                }
                (NotificationButton::Back, _) => icon
                    .as_ref()
                    .map(|icon| ButtonContent::Icon(icon.clone(), DEFAULT_ICON_SIZE as f64))
                    .unwrap_or_else(|| ButtonContent::SimpleText("<-".to_string())),
                (NotificationButton::Previous, _) => icon
                    .as_ref()
                    .map(|icon| ButtonContent::Icon(icon.clone(), DEFAULT_ICON_SIZE as f64))
                    .unwrap_or_else(|| ButtonContent::SimpleText("<".to_string())),
                (NotificationButton::Text, Some(mgr)) => {
                    ButtonContent::ClippedText(mgr.current_text())
                }
                (NotificationButton::Next, _) => icon
                    .as_ref()
                    .map(|icon| ButtonContent::Icon(icon.clone(), DEFAULT_ICON_SIZE as f64))
                    .unwrap_or_else(|| ButtonContent::SimpleText(">".to_string())),
                (NotificationButton::Dnd, Some(mgr)) => {
                    let icon = if mgr.dnd_enabled() {
                        active_icon.as_ref().or(icon.as_ref())
                    } else {
                        icon.as_ref()
                    };
                    icon.map(|icon| ButtonContent::Icon(icon.clone(), DEFAULT_ICON_SIZE as f64))
                        .unwrap_or_else(|| ButtonContent::SimpleText(mgr.dnd_text()))
                }
                _ => ButtonContent::Empty,
            },
            ButtonImage::Spacer => ButtonContent::Empty,
        }
    }

    // Unified render function that handles all button types
    fn render_content(
        &self,
        c: &Context,
        content: ButtonContent,
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
    ) {
        match content {
            ButtonContent::SimpleText(text) => {
                let extents = c.text_extents(&text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(&text).unwrap();
            }
            ButtonContent::Icon(svg, size) => {
                let x = button_left_edge + (button_width as f64 / 2.0 - size / 2.0).round();
                let y = y_shift + ((height as f64 - size) / 2.0).round();
                svg.render_document(c, &Rectangle::new(x, y, size, size))
                    .unwrap();
            }
            ButtonContent::Bitmap(surf, width, height_img) => {
                let x = button_left_edge + (button_width as f64 / 2.0 - width / 2.0).round();
                let y = y_shift + ((height as f64 - height_img) / 2.0).round();
                c.set_source_surface(&surf, x, y).unwrap();
                c.rectangle(x, y, width, height_img);
                c.fill().unwrap();
            }
            ButtonContent::IconWithText {
                icon,
                icon_size,
                text,
            } => {
                let extents = c.text_extents(&text).unwrap();
                let spacing = 4.0;

                if self.stacked {
                    // Stacked: icon on left, text on right
                    let total_width = icon_size + spacing + extents.width();
                    let start_x =
                        button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();

                    // Icon vertically centered
                    let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                    icon.render_document(c, &Rectangle::new(start_x, icon_y, icon_size, icon_size))
                        .unwrap();

                    // Text vertically centered, to the right of icon
                    c.move_to(
                        start_x + icon_size + spacing,
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&text).unwrap();
                } else {
                    // Horizontal: same layout
                    let total_width = icon_size + spacing + extents.width();
                    let start_x =
                        button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();

                    let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                    icon.render_document(c, &Rectangle::new(start_x, icon_y, icon_size, icon_size))
                        .unwrap();

                    c.move_to(
                        start_x + icon_size + spacing,
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&text).unwrap();
                }
            }
            ButtonContent::ColoredIconWithText {
                icon,
                icon_size,
                icon_color,
                overlay_icon,
                overlay_color,
                text,
            } => {
                let extents = c.text_extents(&text).unwrap();
                let spacing = 4.0;
                let total_width = icon_size + spacing + extents.width();
                let start_x =
                    button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();
                let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                render_tinted_icon(c, &icon, start_x, icon_y, icon_size, icon_color);
                if let Some(overlay_icon) = overlay_icon {
                    render_tinted_icon(c, &overlay_icon, start_x, icon_y, icon_size, overlay_color);
                }
                c.move_to(
                    start_x + icon_size + spacing,
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(&text).unwrap();
            }
            ButtonContent::IconWithCenteredText {
                icon,
                icon_size,
                text,
            } => {
                let icon_x =
                    button_left_edge + (button_width as f64 / 2.0 - icon_size / 2.0).round();
                let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                icon.render_document(c, &Rectangle::new(icon_x, icon_y, icon_size, icon_size))
                    .unwrap();

                let extents = c.text_extents(&text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(&text).unwrap();
            }
            ButtonContent::IconWithMultilineText {
                icon,
                icon_size,
                lines,
                emphasize_first,
            } => {
                let spacing = 4.0;
                let text_spacing = 2.0;
                // The current font size (from global or per-button config) is already
                // set on the context; optionally render the first line (the weekday)
                // 2pt larger so it's easier to read.
                let base_font_size = c.font_matrix().xx();
                let line_size = |i: usize| {
                    if emphasize_first && i == 0 {
                        base_font_size + 2.0
                    } else {
                        base_font_size
                    }
                };

                // Measure all lines
                let mut line_extents = Vec::new();
                let mut max_width: f64 = 0.0;
                let mut total_height: f64 = 0.0;

                for (i, line) in lines.iter().enumerate() {
                    c.set_font_size(line_size(i));
                    let extents = c.text_extents(line).unwrap();
                    max_width = max_width.max(extents.width());
                    total_height += extents.height();
                    if i < lines.len() - 1 {
                        total_height += text_spacing;
                    }
                    line_extents.push(extents);
                }

                let total_width = icon_size + spacing + max_width;
                let start_x =
                    button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();

                // Icon vertically centered
                let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                icon.render_document(c, &Rectangle::new(start_x, icon_y, icon_size, icon_size))
                    .unwrap();

                // Text block vertically centered
                let text_start_y = y_shift + (height as f64 / 2.0 - total_height / 2.0).round();
                let text_x = start_x + icon_size + spacing;

                let mut current_y = text_start_y;
                for (i, (line, extents)) in lines.iter().zip(line_extents.iter()).enumerate() {
                    c.set_font_size(line_size(i));
                    c.move_to(text_x, current_y + extents.height());
                    c.show_text(line).unwrap();
                    if i < lines.len() - 1 {
                        current_y += extents.height() + text_spacing;
                    }
                }
                c.set_font_size(base_font_size);
            }
            ButtonContent::ColoredIconWithMultilineText {
                icon,
                icon_size,
                icon_color,
                overlay_icon,
                overlay_color,
                lines,
                emphasize_first,
            } => {
                let spacing = 4.0;
                let text_spacing = 2.0;
                let base_font_size = c.font_matrix().xx();
                let line_size = |i: usize| {
                    if emphasize_first && i == 0 {
                        base_font_size + 2.0
                    } else {
                        base_font_size
                    }
                };
                let mut line_extents = Vec::new();
                let mut max_width: f64 = 0.0;
                let mut total_height: f64 = 0.0;
                for (i, line) in lines.iter().enumerate() {
                    c.set_font_size(line_size(i));
                    let extents = c.text_extents(line).unwrap();
                    max_width = max_width.max(extents.width());
                    total_height += extents.height();
                    if i < lines.len() - 1 {
                        total_height += text_spacing;
                    }
                    line_extents.push(extents);
                }
                let total_width = icon_size + spacing + max_width;
                let start_x =
                    button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();
                let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                render_tinted_icon(c, &icon, start_x, icon_y, icon_size, icon_color);
                if let Some(overlay_icon) = overlay_icon {
                    render_tinted_icon(c, &overlay_icon, start_x, icon_y, icon_size, overlay_color);
                }
                let text_start_y = y_shift + (height as f64 / 2.0 - total_height / 2.0).round();
                let text_x = start_x + icon_size + spacing;
                let mut current_y = text_start_y;
                for (i, (line, extents)) in lines.iter().zip(line_extents.iter()).enumerate() {
                    c.set_font_size(line_size(i));
                    c.move_to(text_x, current_y + extents.height());
                    c.show_text(line).unwrap();
                    if i < lines.len() - 1 {
                        current_y += extents.height() + text_spacing;
                    }
                }
                c.set_font_size(base_font_size);
            }
            ButtonContent::ClippedText(text) => {
                let extents = c.text_extents(&text).unwrap();

                // Save context and set up clipping
                c.save().unwrap();
                c.rectangle(
                    button_left_edge,
                    y_shift,
                    button_width as f64,
                    height as f64,
                );
                c.clip();

                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(&text).unwrap();

                // Restore context
                c.restore().unwrap();
            }
            ButtonContent::MultilineText(lines) => {
                let text_spacing = 2.0;
                let mut line_extents = Vec::new();
                let mut total_height = 0.0;
                for (i, line) in lines.iter().enumerate() {
                    let extents = c.text_extents(line).unwrap();
                    total_height += extents.height();
                    if i < lines.len() - 1 {
                        total_height += text_spacing;
                    }
                    line_extents.push(extents);
                }

                let mut current_y = y_shift + (height as f64 / 2.0 - total_height / 2.0).round();
                for (line, extents) in lines.iter().zip(line_extents.iter()) {
                    let x = button_left_edge
                        + (button_width as f64 / 2.0 - extents.width() / 2.0).round();
                    c.move_to(x, current_y + extents.height());
                    c.show_text(line).unwrap();
                    current_y += extents.height() + text_spacing;
                }
            }
            ButtonContent::SliderBar {
                fraction,
                muted,
                percent,
                icon_color,
                icon,
                muted_icon,
            } => {
                let pad = 14.0;
                let icon_size = 26.0;
                let cy = y_shift + height as f64 / 2.0;

                // Left icon: kind indicator, swapped for a muted glyph when muted.
                let left_icon = if muted {
                    muted_icon.as_ref().or(icon.as_ref())
                } else {
                    icon.as_ref()
                };
                let mut track_left = button_left_edge + pad;
                if let Some(svg) = left_icon {
                    let iy = cy - icon_size / 2.0;
                    if muted {
                        render_tinted_icon(c, svg, track_left, iy, icon_size, (1.0, 0.0, 0.0));
                    } else if let Some(color) = icon_color {
                        render_tinted_icon(c, svg, track_left, iy, icon_size, color);
                    } else {
                        svg.render_document(
                            c,
                            &Rectangle::new(track_left, iy, icon_size, icon_size),
                        )
                        .unwrap();
                    }
                    track_left += icon_size + 10.0;
                }

                // Percentage readout, right-aligned.
                let pct_text = format!("{}%", percent);
                let extents = c.text_extents(&pct_text).unwrap();
                let text_x = button_left_edge + button_width as f64 - pad - extents.width();
                let track_right = (text_x - 12.0).max(track_left + 10.0);
                let track_w = (track_right - track_left).max(10.0);

                let track_h = 6.0;
                let ty = cy - track_h / 2.0;

                // Track background.
                c.set_source_rgb(0.3, 0.3, 0.3);
                c.rectangle(track_left, ty, track_w, track_h);
                c.fill().unwrap();

                // Filled portion (dimmed when muted).
                let fill_level = if muted { 0.5 } else { 1.0 };
                let fill_w = (track_w * fraction).max(0.0);
                c.set_source_rgb(fill_level, fill_level, fill_level);
                c.rectangle(track_left, ty, fill_w, track_h);
                c.fill().unwrap();

                // Thumb indicator.
                let thumb_x = track_left + track_w * fraction;
                c.set_source_rgb(1.0, 1.0, 1.0);
                c.arc(thumb_x, cy, 8.0, 0.0, std::f64::consts::PI * 2.0);
                c.fill().unwrap();

                // Percentage text.
                c.set_source_rgb(1.0, 1.0, 1.0);
                c.move_to(text_x, cy + extents.height() / 2.0);
                c.show_text(&pct_text).unwrap();
            }
            ButtonContent::Empty => {}
        }
    }
}

fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(
        Handle::from_file(path).map_err(|_| anyhow!("failed to load image"))?,
    ))
}

fn render_tinted_icon(
    c: &Context,
    icon: &Handle,
    x: f64,
    y: f64,
    size: f64,
    (r, g, b): (f64, f64, f64),
) {
    let surface =
        ImageSurface::create(Format::ARgb32, size.ceil() as i32, size.ceil() as i32).unwrap();
    let icon_context = Context::new(&surface).unwrap();
    icon.render_document(&icon_context, &Rectangle::new(0.0, 0.0, size, size))
        .unwrap();

    c.save().unwrap();
    c.set_source_rgb(r, g, b);
    c.mask_surface(&surface, x, y).unwrap();
    c.restore().unwrap();
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
        let stacked = cfg.stacked;
        let font_size = cfg.font_size;
        let max_title_length = cfg.max_title_length;
        // A LayerToggle can be combined with any button content (e.g. a CPU
        // button that also switches layers). Capture it here and apply it to
        // whatever button we build below; a bare LayerToggle with no content
        // falls through to the dedicated toggle button at the end.
        let toggle_target = cfg.layer_toggle.clone();

        let button_name = cfg.button.as_deref();
        let option = cfg.option.clone();

        let mut button = if let Some(kind) = button_name.and_then(notification_button_alias) {
            let default_icon = match kind {
                NotificationButton::Count => Some("mail"),
                NotificationButton::Dnd => Some("bell-ring"),
                _ => None,
            };
            let icon = cfg.icon.as_deref().or(default_icon).and_then(|name| {
                try_load_image(
                    name,
                    cfg.theme.as_deref(),
                    DEFAULT_ICON_SIZE,
                    DEFAULT_ICON_SIZE,
                )
                .ok()
                .and_then(|img| match img {
                    ButtonImage::Svg(handle) => Some(handle),
                    _ => None,
                })
            });
            let active_icon = cfg
                .icon_active
                .as_deref()
                .or_else(|| matches!(kind, NotificationButton::Dnd).then_some("bell-off"))
                .and_then(|name| {
                    try_load_image(
                        name,
                        cfg.theme.as_deref(),
                        DEFAULT_ICON_SIZE,
                        DEFAULT_ICON_SIZE,
                    )
                    .ok()
                    .and_then(|img| match img {
                        ButtonImage::Svg(handle) => Some(handle),
                        _ => None,
                    })
                });
            Button::new_notification(kind, icon, active_icon)
        } else if matches!(button_name, Some("Slider" | "slider")) {
            match option.as_deref().and_then(SliderKind::parse) {
                Some(kind) => Button::new_slider(kind, cfg.theme.as_deref(), cfg.colorize),
                None => {
                    eprintln!(
                        "Unknown Slider option '{}'; expected display_brightness, keyboard_backlight, or volume",
                        option.unwrap_or_default()
                    );
                    Button::new_spacer()
                }
            }
        } else if matches!(button_name, Some("Weather" | "weather")) {
            match option.as_deref().unwrap_or("current") {
                "current" => Button::new_weather_current(cfg.theme.as_deref(), cfg.colorize),
                "forecast" => Button::new_weather_forecast(
                    cfg.weather_day.unwrap_or(0),
                    cfg.theme.as_deref(),
                    cfg.colorize,
                ),
                weather => {
                    eprintln!(
                        "Unknown Weather option '{}'; expected current or forecast",
                        weather
                    );
                    Button::new_spacer()
                }
            }
        } else if matches!(button_name, Some("Time" | "time")) {
            let time = normalize_time_option(option.unwrap_or_else(|| "24hr".to_string()));
            Button::new_time(cfg.action, cfg.command, &time, cfg.locale.as_deref())
        } else if let Some(text) = cfg.text {
            Button::new_text(text, cfg.action)
        } else if matches!(button_name, Some("Battery" | "battery")) {
            let battery_mode = option.unwrap_or_else(|| "percentage".to_string());
            if let Some(battery) = find_battery_device() {
                Button::new_battery(
                    cfg.action,
                    cfg.command,
                    battery,
                    battery_mode,
                    cfg.theme,
                    cfg.colorize,
                )
            } else {
                Button::new_text("Battery N/A".to_string(), cfg.action)
            }
        } else if matches!(button_name, Some("CpuUsage" | "CPU" | "cpu")) {
            Button::new_cpu_usage(
                cfg.action,
                cfg.command.clone(),
                cfg.icon.clone(),
                cfg.theme.clone(),
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.colorize,
            )
        } else if matches!(button_name, Some("MemoryUsage" | "Memory" | "memory")) {
            Button::new_memory_usage(
                cfg.action,
                cfg.command,
                cfg.icon,
                cfg.theme,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.colorize,
            )
        } else if matches!(button_name, Some("ActiveWindow" | "active_window")) {
            Button::new_active_window(cfg.action)
        } else if matches!(button_name, Some("ActiveWorkspace" | "active_workspace")) {
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
        } else if let Some(target) = cfg.layer_toggle {
            // Bare layer toggle with no other content; label it with the target.
            let label = target.clone();
            Button::new_layer_toggle(target, label)
        } else {
            Button::new_spacer()
        };

        button.stacked = stacked;
        button.font_size = font_size;
        button.max_title_length = max_title_length;
        button.toggle_target = toggle_target;
        button
    }
    fn new_slider(kind: SliderKind, theme: Option<&str>, colorize: bool) -> Button {
        let (icon_name, muted_name) = match kind {
            SliderKind::DisplayBrightness => ("brightness_low", None),
            SliderKind::KeyboardBacklight => ("backlight_low", None),
            SliderKind::Volume => ("volume_down", Some("volume_off")),
        };
        let load = |name: &str| -> Option<Handle> {
            match try_load_image(name, theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE) {
                Ok(ButtonImage::Svg(handle)) => Some(handle),
                _ => None,
            }
        };
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::Slider {
                state: Slider::new(kind),
                icon: load(icon_name),
                muted_icon: muted_name.and_then(load),
                colorize,
            },
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
        }
    }
    fn new_weather_current(theme: Option<&str>, colorize: bool) -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::WeatherCurrent(WeatherIcons::load(theme), colorize),
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
        }
    }
    fn new_weather_forecast(day: usize, theme: Option<&str>, colorize: bool) -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::WeatherForecast(day, WeatherIcons::load(theme), colorize),
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
        }
    }
    fn new_notification(
        kind: NotificationButton,
        icon: Option<Handle>,
        active_icon: Option<Handle>,
    ) -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::Notification(kind, icon, active_icon),
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
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
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
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
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
        }
    }
    fn new_layer_toggle(target: String, label: String) -> Button {
        Button {
            action: vec![],
            active: false,
            changed: false,
            image: ButtonImage::LayerToggle(target, label),
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
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
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
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
        colorize: bool,
    ) -> Button {
        let bolt = Self::load_battery_image("bolt", theme.as_ref());
        let mut plain = Vec::new();
        let mut charging = Vec::new();
        for icon in [
            "battery_0_bar",
            "battery_1_bar",
            "battery_2_bar",
            "battery_3_bar",
            "battery_4_bar",
            "battery_5_bar",
            "battery_6_bar",
            "battery_full",
        ] {
            plain.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        for icon in [
            "battery_charging_20",
            "battery_charging_30",
            "battery_charging_50",
            "battery_charging_60",
            "battery_charging_80",
            "battery_charging_90",
            "battery_charging_full",
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
                colorize,
            ),
            command,
            icon_width: 0.0,
            icon_height: 0.0,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
        }
    }

    fn new_time(
        action: Vec<Key>,
        command: Option<String>,
        format: &str,
        locale_str: Option<&str>,
    ) -> Button {
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
            command,
            icon_width: 0.0,
            icon_height: 0.0,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
        }
    }
    fn new_cpu_usage(
        action: Vec<Key>,
        command: Option<String>,
        icon: Option<impl AsRef<str>>,
        theme: Option<impl AsRef<str>>,
        icon_width: i32,
        icon_height: i32,
        colorize: bool,
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
            image: ButtonImage::CpuUsage(icon_handle, colorize),
            command,
            icon_width: w,
            icon_height: h,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
        }
    }
    fn new_memory_usage(
        action: Vec<Key>,
        command: Option<String>,
        icon: Option<impl AsRef<str>>,
        theme: Option<impl AsRef<str>>,
        icon_width: i32,
        icon_height: i32,
        colorize: bool,
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
            image: ButtonImage::MemoryUsage(icon_handle, colorize),
            command,
            icon_width: w,
            icon_height: h,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
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
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
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
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
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
            ButtonImage::CpuUsage(_, _)
            | ButtonImage::MemoryUsage(_, _)
            | ButtonImage::ActiveWindow
            | ButtonImage::ActiveWorkspace(_)
            | ButtonImage::Notification(_, _, _) => true,
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
        weather_mgr: Option<&WeatherManager>,
        notification_mgr: Option<&NotificationManager>,
    ) {
        // Get the content for this button
        let content = self.get_content(sysinfo_mgr, weather_mgr, notification_mgr);

        // Render the content using the unified rendering function
        self.render_content(c, content, height, button_left_edge, button_width, y_shift);
    }
    fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool)
    where
        F: AsRawFd,
    {
        // Debug: log all set_active calls
        let button_type = match &self.image {
            ButtonImage::CpuUsage(_, _) => "CPU",
            ButtonImage::MemoryUsage(_, _) => "Memory",
            ButtonImage::ActiveWorkspace(_) => "Workspace",
            ButtonImage::Battery(_, _, _, _) => "Battery",
            _ => "Other",
        };
        eprintln!(
            "TINY-DFR DEBUG: set_active called: button={}, active={}, has_command={}",
            button_type,
            active,
            self.command.is_some()
        );

        if self.active != active {
            self.active = active;
            self.changed = true;

            if !matches!(self.image, ButtonImage::LayerToggle(_, _)) {
                toggle_keys(uinput, &self.action, active as i32);

                // Execute command on button press (not release)
                if active {
                    if let Some(cmd_str) = &self.command {
                        eprintln!("TINY-DFR DEBUG: Executing command: {}", cmd_str);

                        // Build command with environment variables from current process
                        // If running as root and we have SUDO_UID, run command as that user
                        let mut command = if let Ok(sudo_uid) = std::env::var("SUDO_UID") {
                            if let Ok(sudo_user) = std::env::var("SUDO_USER") {
                                eprintln!(
                                    "TINY-DFR DEBUG: Running command as user {} (UID {})",
                                    sudo_user, sudo_uid
                                );
                                let mut cmd = std::process::Command::new("sudo");
                                cmd.arg("-u")
                                    .arg(&sudo_user)
                                    .arg("sh")
                                    .arg("-c")
                                    .arg(cmd_str);
                                cmd
                            } else {
                                let mut cmd = std::process::Command::new("sh");
                                cmd.arg("-c").arg(cmd_str);
                                cmd
                            }
                        } else {
                            let mut cmd = std::process::Command::new("sh");
                            cmd.arg("-c").arg(cmd_str);
                            cmd
                        };

                        // Pass through Wayland/Hyprland environment if available
                        let mut env_debug = String::new();
                        if let Ok(val) = std::env::var("WAYLAND_DISPLAY") {
                            env_debug.push_str(&format!("WAYLAND_DISPLAY={} ", val));
                            command.env("WAYLAND_DISPLAY", val);
                        } else {
                            env_debug.push_str("WAYLAND_DISPLAY=<not set> ");
                        }
                        if let Ok(val) = std::env::var("XDG_RUNTIME_DIR") {
                            env_debug.push_str(&format!("XDG_RUNTIME_DIR={} ", val));
                            command.env("XDG_RUNTIME_DIR", val);
                        } else {
                            env_debug.push_str("XDG_RUNTIME_DIR=<not set> ");
                        }
                        if let Ok(val) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
                            env_debug.push_str(&format!("HYPRLAND_INSTANCE_SIGNATURE={} ", val));
                            command.env("HYPRLAND_INSTANCE_SIGNATURE", val);
                        } else {
                            env_debug.push_str("HYPRLAND_INSTANCE_SIGNATURE=<not set> ");
                        }

                        eprintln!("TINY-DFR DEBUG: Environment: {}", env_debug);

                        match command.spawn() {
                            Ok(child) => {
                                eprintln!(
                                    "TINY-DFR DEBUG: Command spawned with PID: {:?}",
                                    child.id()
                                );
                            }
                            Err(e) => {
                                eprintln!("TINY-DFR DEBUG: Failed to spawn command: {}", e);
                            }
                        }
                    } else {
                        eprintln!("TINY-DFR DEBUG: Button pressed but no command set");
                    }
                }
            }
        }
    }
    fn slider_kind(&self) -> Option<SliderKind> {
        match &self.image {
            ButtonImage::Slider { state, .. } => Some(state.kind()),
            _ => None,
        }
    }
    fn slider_refresh(&mut self) {
        if let ButtonImage::Slider { state, .. } = &mut self.image {
            if state.refresh() {
                self.changed = true;
            }
        }
    }
    fn set_slider_fraction(&mut self, frac: f64) {
        if let ButtonImage::Slider { state, .. } = &mut self.image {
            state.set_fraction(frac);
            self.changed = true;
        }
    }
    fn slider_commit(&mut self) {
        if let ButtonImage::Slider { state, .. } = &mut self.image {
            state.commit();
        }
    }
    fn slider_toggle_mute(&mut self) {
        if let ButtonImage::Slider { state, .. } = &mut self.image {
            state.toggle_mute();
            self.changed = true;
        }
    }
    fn layer_toggle_target(&self) -> Option<&str> {
        match &self.image {
            ButtonImage::LayerToggle(target, _) => Some(target),
            _ => self.toggle_target.as_deref(),
        }
    }
    fn notification_kind(&self) -> Option<NotificationButton> {
        match self.image {
            ButtonImage::Notification(kind, _, _) => Some(kind),
            _ => None,
        }
    }
    fn supports_hold(&self) -> bool {
        matches!(self.notification_kind(), Some(NotificationButton::Text))
    }
    fn start_hold(&mut self) {
        if self.supports_hold() {
            self.hold_started = Some(Instant::now());
            self.changed = true;
        }
    }
    fn clear_hold(&mut self) {
        if self.hold_started.is_some() {
            self.hold_started = None;
            self.changed = true;
        }
    }
    fn hold_progress(&self) -> Option<f64> {
        self.hold_started.map(|started| {
            (started.elapsed().as_secs_f64() / LONG_PRESS_TIMEOUT.as_secs_f64()).clamp(0.0, 1.0)
        })
    }
    // A button is interactive only if pressing it actually does something:
    // emits keys, runs a command, switches layers, or is a draggable slider.
    // Display-only buttons (time, weather, cpu/mem readouts, plain labels)
    // have nothing to do on press, so we ignore touches on them rather than
    // flashing a highlight that makes the button look broken.
    fn is_interactive(&self) -> bool {
        if matches!(self.image, ButtonImage::Slider { .. }) {
            return true;
        }
        if matches!(self.image, ButtonImage::Notification(_, _, _)) {
            return true;
        }
        !self.action.is_empty() || self.command.is_some() || self.layer_toggle_target().is_some()
    }
    fn is_visible(
        &self,
        sysinfo_mgr: Option<&SystemInfoManager>,
        notification_mgr: Option<&NotificationManager>,
    ) -> bool {
        match self.image {
            ButtonImage::ActiveWindow | ButtonImage::ActiveWorkspace(_) => sysinfo_mgr
                .map(|mgr| mgr.desktop_info_available())
                .unwrap_or(false),
            ButtonImage::Notification(
                NotificationButton::Previous | NotificationButton::Next,
                _,
                _,
            ) => notification_mgr
                .map(|mgr| mgr.can_navigate())
                .unwrap_or(false),
            ButtonImage::Spacer => false,
            _ => true,
        }
    }
    fn set_background_color(&self, c: &Context, color: f64) {
        if let ButtonImage::Battery(battery, _, _, _) = &self.image {
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

    fn set_background_color_with_notifications(
        &self,
        c: &Context,
        color: f64,
        notification_mgr: Option<&NotificationManager>,
    ) {
        if matches!(self.notification_kind(), Some(NotificationButton::Dnd))
            && notification_mgr
                .map(|mgr| mgr.dnd_enabled())
                .unwrap_or(false)
        {
            c.set_source_rgb(color, 0.0, 0.0);
        } else {
            self.set_background_color(c, color);
        }
    }
}

#[derive(Default)]
pub struct FunctionLayer {
    name: String,
    displays_time: bool,
    displays_battery: bool,
    displays_sysinfo: bool,
    displays_slider: bool,
    displays_weather: bool,
    displays_notifications: bool,
    buttons: Vec<(f64, Button)>,
    virtual_button_count: f64,
    faster_refresh: bool,
}

impl FunctionLayer {
    fn button_spans(&self, notification_mgr: Option<&NotificationManager>) -> Vec<(f64, f64)> {
        let hide_notification_nav = self.displays_notifications
            && notification_mgr
                .map(|mgr| !mgr.can_navigate())
                .unwrap_or(false);
        let notification_nav_width = if hide_notification_nav {
            self.buttons
                .iter()
                .enumerate()
                .filter_map(|(i, (_, button))| {
                    matches!(
                        button.notification_kind(),
                        Some(NotificationButton::Previous | NotificationButton::Next)
                    )
                    .then(|| self.button_width_units(i))
                })
                .sum()
        } else {
            0.0
        };

        let mut cursor = 0.0;
        self.buttons
            .iter()
            .enumerate()
            .map(|(i, (_, button))| {
                let mut width = self.button_width_units(i);
                if hide_notification_nav {
                    match button.notification_kind() {
                        Some(NotificationButton::Previous | NotificationButton::Next) => {
                            width = 0.0
                        }
                        Some(NotificationButton::Text) => width += notification_nav_width,
                        _ => {}
                    }
                }
                let start = cursor;
                cursor += width;
                (start, cursor)
            })
            .collect()
    }

    fn button_width_units(&self, i: usize) -> f64 {
        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };
        end - start
    }

    fn with_config(name: impl Into<String>, cfg: Vec<ButtonConfig>) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }

        let mut virtual_button_count = 0.0;
        let has_button = |names: &[&str]| {
            cfg.iter()
                .filter_map(|cfg| cfg.button.as_deref())
                .any(|button| names.contains(&button))
        };
        let displays_time = has_button(&["Time", "time"]);
        let displays_battery = has_button(&["Battery", "battery"]);
        let displays_sysinfo = has_button(&[
            "CpuUsage",
            "CPU",
            "cpu",
            "MemoryUsage",
            "Memory",
            "memory",
            "ActiveWindow",
            "active_window",
            "ActiveWorkspace",
            "active_workspace",
        ]);
        let displays_slider = has_button(&["Slider", "slider"]);
        let displays_weather = has_button(&["Weather", "weather"]);
        let displays_notifications = cfg.iter().any(|cfg| {
            cfg.button
                .as_deref()
                .and_then(notification_button_alias)
                .is_some()
        });
        let buttons = cfg
            .into_iter()
            .scan(&mut virtual_button_count, |state, cfg| {
                let i = **state;
                let mut stretch = cfg.stretch.unwrap_or(1.0);
                if stretch < 0.5 {
                    println!("Stretch value must be at least 0.5, setting to 0.5.");
                    stretch = 0.5;
                }
                **state += stretch;
                Some((i, Button::with_config(cfg)))
            })
            .collect::<Vec<_>>();
        let faster_refresh = buttons.iter().any(|(_, b)| b.needs_faster_refresh());
        FunctionLayer {
            name: name.into(),
            displays_time,
            displays_battery,
            displays_sysinfo,
            displays_slider,
            displays_weather,
            displays_notifications,
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
        weather_mgr: Option<&WeatherManager>,
        notification_mgr: Option<&NotificationManager>,
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
        let spans = self.button_spans(notification_mgr);
        let visible_buttons = spans.iter().filter(|(start, end)| end > start).count();
        let total_spacing = BUTTON_SPACING_PX as f64 * visible_buttons.saturating_sub(1) as f64;
        let virtual_button_width =
            (width as f64 - pixel_shift_width as f64 - total_spacing) / self.virtual_button_count;
        let radius = 8.0f64;
        let bot = (height as f64) * 0.15;
        let top = (height as f64) * 0.85;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(config.font_size);

        for i in 0..self.buttons.len() {
            let (start, end) = spans[i];
            let visible_i = spans[..i].iter().filter(|(start, end)| end > start).count();
            let (_, button) = &mut self.buttons[i];

            if !button.changed && !complete_redraw {
                continue;
            };
            if end <= start {
                continue;
            }

            let left_edge = (start * virtual_button_width
                + visible_i as f64 * BUTTON_SPACING_PX as f64)
                .floor()
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = ((end - start) * virtual_button_width).floor();

            let color = if button.active {
                BUTTON_COLOR_ACTIVE
            } else if config.show_button_outlines {
                BUTTON_COLOR_INACTIVE
            } else {
                0.0
            };
            let button_visible = button.is_visible(sysinfo_mgr, notification_mgr);
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
                button.set_background_color_with_notifications(&c, color, notification_mgr);
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
                if let Some(progress) = button.hold_progress() {
                    c.set_source_rgb(0.55, 0.55, 0.55);
                    c.rectangle(
                        left_edge,
                        bot - radius,
                        button_width * progress,
                        top - bot + radius * 2.0,
                    );
                    c.fill().unwrap();
                }
            }
            if button_visible {
                c.set_source_rgb(1.0, 1.0, 1.0);
                // Set per-button font size if specified, otherwise use global config
                if let Some(fs) = button.font_size {
                    c.set_font_size(fs);
                } else {
                    c.set_font_size(config.font_size);
                }
                button.render(
                    &c,
                    height,
                    left_edge,
                    button_width.ceil() as u64,
                    pixel_shift_y,
                    sysinfo_mgr,
                    weather_mgr,
                    notification_mgr,
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

    fn hit(
        &self,
        width: u16,
        height: u16,
        x: f64,
        y: f64,
        i: Option<usize>,
        notification_mgr: Option<&NotificationManager>,
    ) -> Option<usize> {
        let spans = self.button_spans(notification_mgr);
        let visible_buttons = spans.iter().filter(|(start, end)| end > start).count();
        let total_spacing = BUTTON_SPACING_PX as f64 * visible_buttons.saturating_sub(1) as f64;
        let virtual_button_width = (width as f64 - total_spacing) / self.virtual_button_count;

        let i = i.unwrap_or_else(|| {
            spans
                .iter()
                .enumerate()
                .position(|(idx, (start, end))| {
                    if end <= start {
                        return false;
                    }
                    let visible_i = spans[..idx]
                        .iter()
                        .filter(|(start, end)| end > start)
                        .count();
                    let left =
                        start * virtual_button_width + visible_i as f64 * BUTTON_SPACING_PX as f64;
                    let right = left + (end - start) * virtual_button_width;
                    x >= left && x <= right
                })
                .unwrap_or(self.buttons.len())
        });
        if i >= self.buttons.len() {
            return None;
        }

        let (start, end) = spans[i];
        if end <= start {
            return None;
        }
        let visible_i = spans[..i].iter().filter(|(start, end)| end > start).count();

        let left_edge =
            (start * virtual_button_width + visible_i as f64 * BUTTON_SPACING_PX as f64).floor();

        let button_width = ((end - start) * virtual_button_width).floor();

        if x < left_edge
            || x > (left_edge + button_width)
            || y < 0.1 * height as f64
            || y > 0.9 * height as f64
        {
            return None;
        }

        // Ignore presses on buttons that have no action to perform so they
        // don't flash an active highlight and appear broken.
        if !self.buttons[i].1.is_interactive() {
            return None;
        }

        if !self.buttons[i].1.is_visible(None, notification_mgr) {
            return None;
        }

        Some(i)
    }

    // Fraction (0.0..=1.0) of the touch x-position across button `i`'s width.
    fn slider_fraction(&self, width: u16, i: usize, x: f64) -> f64 {
        let total_spacing = BUTTON_SPACING_PX as f64 * self.buttons.len().saturating_sub(1) as f64;
        let virtual_button_width = (width as f64 - total_spacing) / self.virtual_button_count;

        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };

        let left_edge =
            (start * virtual_button_width + i as f64 * BUTTON_SPACING_PX as f64).floor();
        let button_width = ((end - start) * virtual_button_width).floor();

        ((x - left_edge) / button_width).clamp(0.0, 1.0)
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

fn open_drm_backend(report_final_failure: bool) -> Option<DrmBackend> {
    let mut last_error = None;

    for attempt in 1..=DRM_OPEN_ATTEMPTS {
        match DrmBackend::open_card() {
            Ok(drm) => return Some(drm),
            Err(err) => {
                last_error = Some(err);
                if attempt < DRM_OPEN_ATTEMPTS {
                    eprintln!(
                        "Touch Bar DRM device unavailable, retrying ({attempt}/{DRM_OPEN_ATTEMPTS})"
                    );
                    std::thread::sleep(DRM_OPEN_RETRY_DELAY);
                }
            }
        }
    }

    if let Some(err) = last_error {
        eprintln!("Touch Bar DRM device unavailable after retries: {err}");
    }
    if report_final_failure {
        eprintln!(
            "Not restarting automatically; fix the DRM owner/seat and restart tiny-dfr.service"
        );
    }
    None
}

fn try_touchbar_usb_recovery() {
    let recovery_enabled = env::var(T2_USB_RECOVERY_ENV)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if !recovery_enabled {
        eprintln!(
            "Touch Bar DRM device unavailable; skipping T2 USB reset. Set {T2_USB_RECOVERY_ENV}=1 to enable recovery."
        );
        return;
    }

    if !is_t2_macbook() {
        eprintln!("Touch Bar DRM device unavailable and this does not look like a T2 MacBook");
        return;
    }

    eprintln!("Touch Bar DRM device unavailable; trying T2 Touch Bar USB recovery");
    let mut touchbar = T2TouchBar::new();
    if let Err(err) = touchbar.initialize() {
        eprintln!("T2 Touch Bar USB recovery failed: {err}");
    }
}

fn main() {
    let mut drm = match open_drm_backend(false) {
        Some(drm) => drm,
        None => {
            try_touchbar_usb_recovery();
            let Some(drm) = open_drm_backend(true) else {
                std::process::exit(EXIT_DRM_UNAVAILABLE);
            };
            drm
        }
    };
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
    let weather_mgr = WeatherManager::new(cfg.weather_location.clone(), cfg.weather_fahrenheit);
    let mut last = Instant::now();

    if cfg.drop_privileges {
        // Drop privileges after opening devices. Use the desktop user when
        // available so Wayland compositors accept desktop-info IPC clients.
        let groups = ["input", "video"];
        let user = sysinfo_manager::desktop_user().unwrap_or_else(|| "nobody".to_string());

        PrivDrop::default()
            .user(&user)
            .group_list(&groups)
            .apply()
            .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));
    }

    let sysinfo_mgr = SystemInfoManager::new();
    let mut notification_mgr = NotificationManager::new();

    let mut surface =
        ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut normal_layer = layer_index(&layers, &cfg.default_layer).unwrap_or(0);
    let mut notification_return_layer = normal_layer;
    let mut active_layer = normal_layer;
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
    let mut last_volume_tap: Option<Instant> = None;
    let mut last_redraw_ts = if layers[active_layer].faster_refresh {
        Local::now().second()
    } else {
        Local::now().minute()
    };
    loop {
        if cfg_mgr.update_config(&mut cfg, &mut layers, width) {
            normal_layer = layer_index(&layers, &cfg.default_layer).unwrap_or(0);
            active_layer = normal_layer;
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
                if let ButtonImage::Battery(_, _, _, _) = button.1.image {
                    button.1.changed = true;
                }
            }
        }

        if layers[active_layer].displays_sysinfo {
            for button in &mut layers[active_layer].buttons {
                match button.1.image {
                    ButtonImage::CpuUsage(_, _)
                    | ButtonImage::MemoryUsage(_, _)
                    | ButtonImage::ActiveWindow
                    | ButtonImage::ActiveWorkspace(_) => {
                        button.1.changed = true;
                    }
                    _ => {}
                }
            }
        }

        if layers[active_layer].displays_slider {
            for button in &mut layers[active_layer].buttons {
                button.1.slider_refresh();
            }
            // Poll often enough that external changes appear live.
            next_timeout_ms = min(next_timeout_ms, SLIDER_REFRESH_MS);
        }

        if layers[active_layer].displays_weather {
            for button in &mut layers[active_layer].buttons {
                if matches!(
                    button.1.image,
                    ButtonImage::WeatherCurrent(_, _) | ButtonImage::WeatherForecast(_, _, _)
                ) {
                    button.1.changed = true;
                }
            }
        }

        if layers[active_layer].displays_notifications {
            if notification_mgr.refresh() {
                needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, 1000);
        }

        if layers[active_layer]
            .buttons
            .iter()
            .any(|(_, button)| button.hold_started.is_some())
        {
            for (_, button) in &mut layers[active_layer].buttons {
                if button.hold_started.is_some() {
                    button.changed = true;
                }
            }
            next_timeout_ms = min(next_timeout_ms, 50);
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
            let weather_mgr_ref = if layers[active_layer].displays_weather {
                Some(&weather_mgr)
            } else {
                None
            };
            let notification_mgr_ref = if layers[active_layer].displays_notifications {
                Some(&notification_mgr)
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
                weather_mgr_ref,
                notification_mgr_ref,
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
                        if cfg.double_press_switch_layers > 0
                            && layers.len() == 2
                            && key.key_state() == KeyState::Pressed
                        {
                            if last.elapsed()
                                < Duration::from_millis(cfg.double_press_switch_layers.into())
                            {
                                layers.swap(0, 1);
                            }
                            last = Instant::now();
                        }
                        let new_layer = match key.key_state() {
                            KeyState::Pressed => {
                                layer_index(&layers, "FKeys").unwrap_or(normal_layer)
                            }
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
                            let notification_mgr_ref =
                                if layers[active_layer].displays_notifications {
                                    Some(&notification_mgr)
                                } else {
                                    None
                                };
                            if let Some(btn) = layers[active_layer].hit(
                                width,
                                height,
                                x,
                                y,
                                None,
                                notification_mgr_ref,
                            ) {
                                touches.insert(dn.seat_slot(), (active_layer, btn, Instant::now()));
                                match layers[active_layer].buttons[btn].1.slider_kind() {
                                    Some(kind) => {
                                        // Double-tap the volume slider to toggle mute.
                                        let mut muted = false;
                                        if kind == SliderKind::Volume {
                                            let now = Instant::now();
                                            if last_volume_tap
                                                .map(|t| now.duration_since(t) < DOUBLE_TAP_TIMEOUT)
                                                .unwrap_or(false)
                                            {
                                                layers[active_layer].buttons[btn]
                                                    .1
                                                    .slider_toggle_mute();
                                                last_volume_tap = None;
                                                muted = true;
                                            } else {
                                                last_volume_tap = Some(now);
                                            }
                                        }
                                        if !muted {
                                            let frac =
                                                layers[active_layer].slider_fraction(width, btn, x);
                                            layers[active_layer].buttons[btn]
                                                .1
                                                .set_slider_fraction(frac);
                                        }
                                    }
                                    None => {
                                        layers[active_layer].buttons[btn]
                                            .1
                                            .set_active(&mut uinput, true);
                                        layers[active_layer].buttons[btn].1.start_hold();
                                    }
                                }
                            }
                        }
                        TouchEvent::Motion(mtn) => {
                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            if !touches.contains_key(&mtn.seat_slot()) {
                                continue;
                            }

                            let (layer, btn, _) = *touches.get(&mtn.seat_slot()).unwrap();
                            if layers[layer].buttons[btn].1.slider_kind().is_some() {
                                // Any drag cancels a pending double-tap.
                                last_volume_tap = None;
                                let frac = layers[layer].slider_fraction(width, btn, x);
                                layers[layer].buttons[btn].1.set_slider_fraction(frac);
                            } else {
                                let notification_mgr_ref =
                                    if layers[active_layer].displays_notifications {
                                        Some(&notification_mgr)
                                    } else {
                                        None
                                    };
                                let hit = layers[active_layer]
                                    .hit(width, height, x, y, Some(btn), notification_mgr_ref)
                                    .is_some();
                                layers[layer].buttons[btn].1.set_active(&mut uinput, hit);
                                if !hit {
                                    layers[layer].buttons[btn].1.clear_hold();
                                }
                            }
                        }
                        TouchEvent::Up(up) => {
                            if !touches.contains_key(&up.seat_slot()) {
                                continue;
                            }
                            let (layer, btn, down_at) = *touches.get(&up.seat_slot()).unwrap();
                            if layers[layer].buttons[btn].1.slider_kind().is_some() {
                                layers[layer].buttons[btn].1.slider_commit();
                                touches.remove(&up.seat_slot());
                                continue;
                            }
                            let layer_toggle_target = layers[layer].buttons[btn]
                                .1
                                .layer_toggle_target()
                                .map(str::to_string);
                            let notification_kind =
                                layers[layer].buttons[btn].1.notification_kind();
                            layers[layer].buttons[btn].1.set_active(&mut uinput, false);
                            layers[layer].buttons[btn].1.clear_hold();
                            touches.remove(&up.seat_slot());
                            match notification_kind {
                                Some(NotificationButton::Back) => {
                                    normal_layer = notification_return_layer;
                                    active_layer = normal_layer;
                                    needs_complete_redraw = true;
                                }
                                Some(NotificationButton::Previous) => {
                                    notification_mgr.previous();
                                    needs_complete_redraw = true;
                                }
                                Some(NotificationButton::Text) => {
                                    if down_at.elapsed() >= LONG_PRESS_TIMEOUT {
                                        notification_mgr.dismiss_current();
                                    } else {
                                        notification_mgr.invoke_current();
                                    }
                                    needs_complete_redraw = true;
                                }
                                Some(NotificationButton::Next) => {
                                    notification_mgr.next();
                                    needs_complete_redraw = true;
                                }
                                Some(NotificationButton::Dnd) => {
                                    notification_mgr.toggle_dnd();
                                    needs_complete_redraw = true;
                                }
                                Some(NotificationButton::Count) | None => {}
                            }
                            if let Some(target) = layer_toggle_target {
                                if let Some(target_layer) = layer_index(&layers, &target) {
                                    if layers[target_layer].displays_notifications {
                                        notification_return_layer = normal_layer;
                                    }
                                    normal_layer = target_layer;
                                    active_layer = normal_layer;
                                    needs_complete_redraw = true;
                                } else {
                                    eprintln!("LayerToggle target '{}' does not exist", target);
                                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_button(image: ButtonImage) -> Button {
        Button {
            image,
            changed: false,
            active: false,
            action: vec![],
            command: None,
            icon_width: 0.0,
            icon_height: 0.0,
            stacked: false,
            font_size: None,
            max_title_length: None,
            toggle_target: None,
            hold_started: None,
        }
    }

    fn notification_layer() -> FunctionLayer {
        FunctionLayer {
            name: "Notifications".to_string(),
            displays_time: false,
            displays_battery: false,
            displays_sysinfo: false,
            displays_slider: false,
            displays_weather: false,
            displays_notifications: true,
            buttons: vec![
                (
                    0.0,
                    test_button(ButtonImage::Notification(
                        NotificationButton::Previous,
                        None,
                        None,
                    )),
                ),
                (
                    1.0,
                    test_button(ButtonImage::Notification(
                        NotificationButton::Text,
                        None,
                        None,
                    )),
                ),
                (
                    9.0,
                    test_button(ButtonImage::Notification(
                        NotificationButton::Next,
                        None,
                        None,
                    )),
                ),
                (
                    10.0,
                    test_button(ButtonImage::Notification(
                        NotificationButton::Dnd,
                        None,
                        None,
                    )),
                ),
            ],
            virtual_button_count: 11.0,
            faster_refresh: false,
        }
    }

    #[test]
    fn time_option_aliases_normalize() {
        assert_eq!(normalize_time_option("24h".to_string()), "24hr");
        assert_eq!(normalize_time_option("12h".to_string()), "12hr");
        assert_eq!(normalize_time_option("%H:%M".to_string()), "%H:%M");
    }

    #[test]
    fn notification_aliases_map_to_buttons() {
        assert!(matches!(
            notification_button_alias("notifications"),
            Some(NotificationButton::Count)
        ));
        assert!(matches!(
            notification_button_alias("PreviousLayer"),
            Some(NotificationButton::Back)
        ));
        assert!(matches!(
            notification_button_alias("DnDNotification"),
            Some(NotificationButton::Dnd)
        ));
        assert!(notification_button_alias("nope").is_none());
    }

    #[test]
    fn usage_color_thresholds_match_configured_ranges() {
        assert_eq!(usage_color(10.0), (1.0, 1.0, 1.0));
        assert_eq!(usage_color(11.0), (1.0, 0.85, 0.0));
        assert_eq!(usage_color(25.0), (1.0, 0.85, 0.0));
        assert_eq!(usage_color(26.0), (1.0, 0.45, 0.0));
        assert_eq!(usage_color(55.0), (1.0, 0.45, 0.0));
        assert_eq!(usage_color(56.0), (0.55, 0.25, 1.0));
        assert_eq!(usage_color(75.0), (0.55, 0.25, 1.0));
        assert_eq!(usage_color(76.0), (1.0, 0.0, 0.0));
    }

    #[test]
    fn battery_color_thresholds_match_configured_ranges() {
        assert_eq!(battery_color(10), (1.0, 0.0, 0.0));
        assert_eq!(battery_color(11), (0.55, 0.25, 1.0));
        assert_eq!(battery_color(25), (0.55, 0.25, 1.0));
        assert_eq!(battery_color(26), (1.0, 0.45, 0.0));
        assert_eq!(battery_color(50), (1.0, 0.45, 0.0));
        assert_eq!(battery_color(51), (0.0, 0.45, 1.0));
        assert_eq!(battery_color(75), (0.0, 0.45, 1.0));
        assert_eq!(battery_color(76), (0.0, 0.8, 0.0));
    }

    #[test]
    fn weather_colorizes_only_sun_moon_and_rain() {
        assert_eq!(weather_icon_color("weather_sunny"), Some((1.0, 0.85, 0.0)));
        assert_eq!(weather_icon_color("weather_moon"), Some((1.0, 0.85, 0.0)));
        assert_eq!(weather_icon_color("weather_rainy"), None);
        assert_eq!(weather_icon_color("weather_cloudy"), None);
    }

    #[test]
    fn brightness_slider_icons_blend_from_white_to_orange() {
        assert_eq!(
            slider_icon_color(SliderKind::DisplayBrightness, 0),
            Some((1.0, 1.0, 1.0))
        );
        let orange = slider_icon_color(SliderKind::KeyboardBacklight, 100).unwrap();
        assert!((orange.0 - 1.0).abs() < f64::EPSILON);
        assert!((orange.1 - 0.45).abs() < 0.0001);
        assert!((orange.2 - 0.0).abs() < f64::EPSILON);
        assert_eq!(slider_icon_color(SliderKind::Volume, 100), None);
    }

    #[test]
    fn notification_nav_space_goes_to_content_until_multiple_notifications() {
        let layer = notification_layer();

        let empty = NotificationManager::with_count(0);
        assert_eq!(
            layer.button_spans(Some(&empty)),
            vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 11.0)]
        );

        let one = NotificationManager::with_count(1);
        assert_eq!(
            layer.button_spans(Some(&one)),
            vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 11.0)]
        );

        let two = NotificationManager::with_count(2);
        assert_eq!(
            layer.button_spans(Some(&two)),
            vec![(0.0, 1.0), (1.0, 9.0), (9.0, 10.0), (10.0, 11.0)]
        );
    }

    #[test]
    fn fractional_stretch_is_preserved_in_layer_spans() {
        let layer = FunctionLayer {
            name: "Fractional".to_string(),
            displays_time: false,
            displays_battery: false,
            displays_sysinfo: false,
            displays_slider: false,
            displays_weather: false,
            displays_notifications: false,
            buttons: vec![
                (0.0, test_button(ButtonImage::Text("a".to_string()))),
                (0.5, test_button(ButtonImage::Text("b".to_string()))),
                (2.0, test_button(ButtonImage::Text("c".to_string()))),
            ],
            virtual_button_count: 3.0,
            faster_refresh: false,
        };

        assert_eq!(
            layer.button_spans(None),
            vec![(0.0, 0.5), (0.5, 2.0), (2.0, 3.0)]
        );
    }
}
