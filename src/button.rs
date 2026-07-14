use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface};
use chrono::{
    format::{Item as ChronoItem, StrftimeItems},
    Local, Locale,
};
use freedesktop_icons::lookup;
use input_linux::{uinput::UInputHandle, Key};
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};
use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::config::ButtonConfig;
use crate::input::toggle_keys;
use crate::notification_manager::NotificationManager;
use crate::slider::{Slider, SliderKind};
use crate::sysinfo_manager::SystemInfoManager;
use crate::weather::WeatherManager;
use crate::{DEFAULT_ICON_SIZE, LONG_PRESS_TIMEOUT};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    NotCharging,
    Charging,
    Low,
}

pub struct BatteryImages {
    plain: Vec<Handle>,
    charging: Vec<Handle>,
    bolt: Handle,
}

pub struct WeatherIcons {
    icons: HashMap<&'static str, Handle>,
}

impl WeatherIcons {
    pub fn load(theme: Option<&str>) -> WeatherIcons {
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
    pub fn pick(&self, name: &str) -> Option<(&'static str, Handle)> {
        self.icons
            .get_key_value(name)
            .or_else(|| self.icons.get_key_value("weather_cloudy"))
            .map(|(name, handle)| (*name, handle.clone()))
    }
}

#[derive(Eq, PartialEq, Copy, Clone)]
pub enum BatteryIconMode {
    Percentage,
    Icon,
    Both,
}

impl BatteryIconMode {
    pub fn should_draw_icon(self) -> bool {
        self != BatteryIconMode::Percentage
    }
    pub fn should_draw_text(self) -> bool {
        self != BatteryIconMode::Icon
    }
}

pub enum ButtonImage {
    Text(String),
    Svg(Handle),
    Bitmap(ImageSurface),
    Time(Vec<ChronoItem<'static>>, Locale),
    Battery(String, BatteryIconMode, BatteryImages, bool),
    LayerToggle(String, String),
    CpuUsage(Option<Handle>, bool),
    MemoryUsage(Option<Handle>, bool),
    ActiveWindow(Option<Handle>, bool),
    ActiveWorkspace(Option<Handle>, bool),
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
pub enum NotificationButton {
    Count,
    Back,
    Previous,
    Text,
    Next,
    Dnd,
}

pub fn notification_button_alias(name: &str) -> Option<NotificationButton> {
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

pub fn normalize_time_option(option: String) -> String {
    match option.as_str() {
        "24h" => "24hr".to_string(),
        "12h" => "12hr".to_string(),
        _ => option,
    }
}

pub fn usage_color(percent: f64) -> (f64, f64, f64) {
    match percent {
        p if p <= 10.0 => (1.0, 1.0, 1.0),
        p if p <= 25.0 => (1.0, 0.85, 0.0),
        p if p <= 55.0 => (1.0, 0.45, 0.0),
        p if p <= 75.0 => (0.55, 0.25, 1.0),
        _ => (1.0, 0.0, 0.0),
    }
}

pub fn battery_color(percent: u32) -> (f64, f64, f64) {
    match percent {
        0..=10 => (1.0, 0.0, 0.0),
        11..=25 => (0.55, 0.25, 1.0),
        26..=50 => (1.0, 0.45, 0.0),
        51..=75 => (0.0, 0.45, 1.0),
        _ => (0.0, 0.8, 0.0),
    }
}

pub fn battery_warning(capacity: u32, state: BatteryState, colorize: bool) -> bool {
    colorize && state != BatteryState::Charging && capacity <= 10
}

pub fn now_playing_icon_color(colorize: bool) -> Option<(f64, f64, f64)> {
    colorize.then_some((0.0, 0.8, 0.0))
}

pub fn workspace_icon_color(colorize: bool) -> Option<(f64, f64, f64)> {
    colorize.then_some((0.55, 0.25, 1.0))
}

pub fn weather_icon_color(icon: &str) -> Option<(f64, f64, f64)> {
    match icon {
        "weather_sunny" | "weather_moon" => Some((1.0, 0.85, 0.0)),
        _ => None,
    }
}

pub fn slider_icon_color(kind: SliderKind, percent: u32) -> Option<(f64, f64, f64)> {
    if kind == SliderKind::Volume {
        return None;
    }
    let t = (percent as f64 / 100.0).clamp(0.0, 1.0);
    Some((1.0, 1.0 - 0.55 * t, 1.0 - t))
}

pub struct Button {
    pub image: ButtonImage,
    pub changed: bool,
    pub active: bool,
    pub action: Vec<Key>,
    pub command: Option<String>,
    pub icon_width: f64,
    pub icon_height: f64,
    pub stacked: bool,
    pub font_size: Option<f64>,
    pub max_title_length: Option<usize>,
    pub toggle_target: Option<String>,
    pub hold_started: Option<Instant>,
}

// Unified rendering structures
pub enum ButtonContent {
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
        icon_color: Option<(f64, f64, f64)>,
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
    IconWithClippedText {
        icon: Handle,
        icon_color: Option<(f64, f64, f64)>,
        text: String,
    },
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
    pub fn get_content(
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
            ButtonImage::ActiveWindow(icon, colorize) => {
                if let Some(mgr) = sysinfo_mgr {
                    let (text, now_playing) = mgr.get_titlebar_text();
                    if now_playing && icon.is_some() {
                        ButtonContent::IconWithClippedText {
                            icon: icon.clone().unwrap(),
                            icon_color: now_playing_icon_color(*colorize),
                            text,
                        }
                    } else {
                        ButtonContent::ClippedText(text)
                    }
                } else {
                    ButtonContent::Empty
                }
            }
            ButtonImage::ActiveWorkspace(icon, colorize) => {
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
                            icon_color: workspace_icon_color(*colorize),
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
    pub fn render_content(
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
                icon_color,
                icon_size,
                text,
            } => {
                let icon_x =
                    button_left_edge + (button_width as f64 / 2.0 - icon_size / 2.0).round();
                let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();
                if let Some(color) = icon_color {
                    render_tinted_icon(c, &icon, icon_x, icon_y, icon_size, color);
                } else {
                    icon.render_document(c, &Rectangle::new(icon_x, icon_y, icon_size, icon_size))
                        .unwrap();
                }

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
            ButtonContent::IconWithClippedText {
                icon,
                icon_color,
                text,
            } => {
                let icon_size = DEFAULT_ICON_SIZE as f64;
                let spacing = 4.0;
                let extents = c.text_extents(&text).unwrap();
                let total_width = icon_size + spacing + extents.width();
                let start_x =
                    button_left_edge + (button_width as f64 / 2.0 - total_width / 2.0).round();
                let icon_y = y_shift + ((height as f64 - icon_size) / 2.0).round();

                c.save().unwrap();
                c.rectangle(
                    button_left_edge,
                    y_shift,
                    button_width as f64,
                    height as f64,
                );
                c.clip();

                if let Some(color) = icon_color {
                    render_tinted_icon(c, &icon, start_x, icon_y, icon_size, color);
                } else {
                    icon.render_document(c, &Rectangle::new(start_x, icon_y, icon_size, icon_size))
                        .unwrap();
                }
                c.move_to(
                    start_x + icon_size + spacing,
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(&text).unwrap();
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

pub fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(
        Handle::from_file(path).map_err(|_| anyhow!("failed to load image"))?,
    ))
}

pub fn render_tinted_icon(
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

pub fn try_load_png(path: impl AsRef<Path>, icon_width: i32, icon_height: i32) -> Result<ButtonImage> {
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

pub fn try_load_image(
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
        // Standard file icons. TINF_DFR_SHARE_DIR (used by the simulator and
        // checkout runs) is searched first so icons resolve without installing.
        let mut paths = Vec::new();
        if let Some(dir) = env::var_os("TINY_DFR_SHARE_DIR") {
            let dir = PathBuf::from(dir);
            paths.push(dir.join(format!("{name}.svg")));
            paths.push(dir.join(format!("{name}.png")));
        }
        paths.extend([
            PathBuf::from(format!("/etc/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/etc/tiny-dfr/{name}.png")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.png")),
        ]);
        locations = paths;
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

pub fn find_battery_device() -> Option<String> {
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

pub fn get_battery_state(battery: &str) -> (u32, BatteryState) {
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

pub fn get_power_profile() -> Option<String> {
    // The profile is read by spawning `powerprofilesctl`, which costs tens of
    // milliseconds. This runs during every battery draw, so cache it briefly:
    // without the cache a redraw loop (e.g. a pullout animation at 60fps) spends
    // all its time forking this subprocess and drops to a few frames a second.
    static CACHE: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);
    const TTL: Duration = Duration::from_secs(5);

    let mut cache = CACHE.lock().unwrap();
    if let Some((fetched_at, value)) = cache.as_ref() {
        if fetched_at.elapsed() < TTL {
            return value.clone();
        }
    }

    let fresh = std::process::Command::new("powerprofilesctl")
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
        });
    *cache = Some((Instant::now(), fresh.clone()));
    fresh
}

impl Button {
    pub fn with_config(cfg: ButtonConfig) -> Button {
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
                    log_line!(
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
                    log_line!(
                        "Unknown Weather option '{}'; expected current or forecast",
                        weather
                    );
                    Button::new_spacer()
                }
            }
        } else if matches!(button_name, Some("Time" | "time")) {
            let time = normalize_time_option(option.unwrap_or_else(|| "24hr".to_string()));
            Button::new_time(cfg.action, cfg.command, &time, cfg.locale.as_deref())
        } else if matches!(button_name, Some("pullout_toggle")) {
            let target = cfg.layer_toggle.clone().unwrap_or_default();
            let label = cfg.text.clone().unwrap_or_default();
            Button::new_layer_toggle(target, label)
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
            Button::new_active_window(cfg.action, cfg.theme, cfg.colorize)
        } else if matches!(button_name, Some("ActiveWorkspace" | "active_workspace")) {
            Button::new_active_workspace(
                cfg.action,
                cfg.command,
                cfg.icon,
                cfg.theme,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.colorize,
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
    pub fn new_slider(kind: SliderKind, theme: Option<&str>, colorize: bool) -> Button {
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
    pub fn new_weather_current(theme: Option<&str>, colorize: bool) -> Button {
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
    pub fn new_weather_forecast(day: usize, theme: Option<&str>, colorize: bool) -> Button {
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
    pub fn new_notification(
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
    pub fn new_spacer() -> Button {
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
    pub fn new_text(text: String, action: Vec<Key>) -> Button {
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
    pub fn new_layer_toggle(target: String, label: String) -> Button {
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
    pub fn new_icon(
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
    pub fn load_battery_image(icon: &str, theme: Option<impl AsRef<str>>) -> Handle {
        if let ButtonImage::Svg(svg) =
            try_load_image(icon, theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE).unwrap()
        {
            return svg;
        }
        panic!("failed to load icon");
    }
    pub fn new_battery(
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

    pub fn new_time(
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
    pub fn new_cpu_usage(
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
    pub fn new_memory_usage(
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
    pub fn new_active_window(
        action: Vec<Key>,
        theme: Option<impl AsRef<str>>,
        colorize: bool,
    ) -> Button {
        let icon = if let ButtonImage::Svg(svg) =
            try_load_image("play", theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE).unwrap()
        {
            svg
        } else {
            panic!("failed to load play icon");
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::ActiveWindow(Some(icon), colorize),
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
    pub fn new_active_workspace(
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
            image: ButtonImage::ActiveWorkspace(icon_handle, colorize),
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
    pub fn needs_faster_refresh(&self) -> bool {
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
            | ButtonImage::ActiveWindow(_, _)
            | ButtonImage::ActiveWorkspace(_, _)
            | ButtonImage::Notification(_, _, _) => true,
            _ => false,
        }
    }
    pub fn render(
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
    pub fn set_active<F>(&mut self, uinput: &mut UInputHandle<F>, active: bool)
    where
        F: AsRawFd,
    {
        // Debug: log all set_active calls
        let button_type = match &self.image {
            ButtonImage::CpuUsage(_, _) => "CPU",
            ButtonImage::MemoryUsage(_, _) => "Memory",
            ButtonImage::ActiveWorkspace(_, _) => "Workspace",
            ButtonImage::Battery(_, _, _, _) => "Battery",
            _ => "Other",
        };
        log_line!(
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
                        log_line!("TINY-DFR DEBUG: Executing command: {}", cmd_str);

                        // Build command with environment variables from current process
                        // If running as root and we have SUDO_UID, run command as that user
                        let mut command = if let Ok(sudo_uid) = std::env::var("SUDO_UID") {
                            if let Ok(sudo_user) = std::env::var("SUDO_USER") {
                                log_line!(
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

                        log_line!("TINY-DFR DEBUG: Environment: {}", env_debug);

                        match command.spawn() {
                            Ok(child) => {
                                log_line!(
                                    "TINY-DFR DEBUG: Command spawned with PID: {:?}",
                                    child.id()
                                );
                            }
                            Err(e) => {
                                log_line!("TINY-DFR DEBUG: Failed to spawn command: {}", e);
                            }
                        }
                    } else {
                        log_line!("TINY-DFR DEBUG: Button pressed but no command set");
                    }
                }
            }
        }
    }
    pub fn slider_kind(&self) -> Option<SliderKind> {
        match &self.image {
            ButtonImage::Slider { state, .. } => Some(state.kind()),
            _ => None,
        }
    }
    pub fn slider_refresh(&mut self) {
        if let ButtonImage::Slider { state, .. } = &mut self.image {
            if state.refresh() {
                self.changed = true;
            }
        }
    }
    pub fn set_slider_fraction(&mut self, frac: f64) {
        if let ButtonImage::Slider { state, .. } = &mut self.image {
            state.set_fraction(frac);
            self.changed = true;
        }
    }
    pub fn slider_commit(&mut self) {
        if let ButtonImage::Slider { state, .. } = &mut self.image {
            state.commit();
        }
    }
    pub fn slider_toggle_mute(&mut self) {
        if let ButtonImage::Slider { state, .. } = &mut self.image {
            state.toggle_mute();
            self.changed = true;
        }
    }
    pub fn layer_toggle_target(&self) -> Option<&str> {
        match &self.image {
            ButtonImage::LayerToggle(target, _) => Some(target),
            _ => self.toggle_target.as_deref(),
        }
    }
    pub fn notification_kind(&self) -> Option<NotificationButton> {
        match self.image {
            ButtonImage::Notification(kind, _, _) => Some(kind),
            _ => None,
        }
    }
    pub fn is_active_window(&self) -> bool {
        matches!(self.image, ButtonImage::ActiveWindow(_, _))
    }
    pub fn is_active_workspace(&self) -> bool {
        matches!(self.image, ButtonImage::ActiveWorkspace(_, _))
    }
    pub fn supports_hold(&self) -> bool {
        matches!(self.notification_kind(), Some(NotificationButton::Text))
    }
    pub fn start_hold(&mut self) {
        if self.supports_hold() {
            self.hold_started = Some(Instant::now());
            self.changed = true;
        }
    }
    pub fn clear_hold(&mut self) {
        if self.hold_started.is_some() {
            self.hold_started = None;
            self.changed = true;
        }
    }
    pub fn hold_progress(&self) -> Option<f64> {
        self.hold_started.map(|started| {
            (started.elapsed().as_secs_f64() / LONG_PRESS_TIMEOUT.as_secs_f64()).clamp(0.0, 1.0)
        })
    }
    // A button is interactive only if pressing it actually does something:
    // emits keys, runs a command, switches layers, or is a draggable slider.
    // Display-only buttons (time, weather, cpu/mem readouts, plain labels)
    // have nothing to do on press, so we ignore touches on them rather than
    // flashing a highlight that makes the button look broken.
    pub fn is_interactive(&self) -> bool {
        if matches!(self.image, ButtonImage::Slider { .. }) {
            return true;
        }
        if matches!(self.image, ButtonImage::Notification(_, _, _)) {
            return true;
        }
        if matches!(self.image, ButtonImage::ActiveWindow(_, _)) {
            return true;
        }
        !self.action.is_empty() || self.command.is_some() || self.layer_toggle_target().is_some()
    }
    pub fn is_visible(
        &self,
        sysinfo_mgr: Option<&SystemInfoManager>,
        notification_mgr: Option<&NotificationManager>,
    ) -> bool {
        match self.image {
            ButtonImage::ActiveWindow(_, _) | ButtonImage::ActiveWorkspace(_, _) => sysinfo_mgr
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
    pub fn set_background_color(&self, c: &Context, color: f64) {
        if let ButtonImage::Battery(battery, _, _, colorize) = &self.image {
            let (capacity, state) = get_battery_state(battery);
            if battery_warning(capacity, state, *colorize) {
                c.set_source_rgb(color, 0.0, 0.0);
            } else {
                match state {
                    BatteryState::NotCharging => c.set_source_rgb(color, color, color),
                    BatteryState::Charging => c.set_source_rgb(0.0, color, 0.0),
                    BatteryState::Low => c.set_source_rgb(color, color, color),
                }
            }
        } else if matches!(self.image, ButtonImage::LayerToggle(_, _)) {
            // Accent the pullout "<"/">" toggle so its edge is clear even when
            // the panel is drawn over another button. Brightens when pressed.
            c.set_source_rgb(0.0, (color + 0.35).min(1.0), (color + 0.55).min(1.0));
        } else {
            c.set_source_rgb(color, color, color);
        }
    }

    pub fn set_background_color_with_notifications(
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
