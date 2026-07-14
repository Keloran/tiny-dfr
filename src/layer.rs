use cairo::{Context, Surface};
use drm::control::ClipRect;
use std::time::{Duration, Instant};

use crate::button::{notification_button_alias, Button, ButtonContent, NotificationButton};
use crate::config::{ButtonConfig, Config};
use crate::notification_manager::NotificationManager;
use crate::pixel_shift::PIXEL_SHIFT_WIDTH_PX;
use crate::sysinfo_manager::SystemInfoManager;
use crate::weather::WeatherManager;
use crate::{BUTTON_COLOR_ACTIVE, BUTTON_COLOR_INACTIVE, BUTTON_SPACING_PX};

pub fn layer_index(layers: &[FunctionLayer], name: &str) -> Option<usize> {
    layers.iter().position(|layer| layer.name == name)
}

#[derive(Default)]
pub struct FunctionLayer {
    pub name: String,
    pub displays_time: bool,
    pub displays_battery: bool,
    pub displays_sysinfo: bool,
    pub displays_slider: bool,
    pub displays_weather: bool,
    pub displays_notifications: bool,
    pub buttons: Vec<(f64, Button)>,
    pub virtual_button_count: f64,
    pub faster_refresh: bool,
    // Set when this layer is a pullout panel: it draws over the right
    // `overlay_coverage` fraction of the `overlay_parent` layer, masking it.
    pub overlay_parent: Option<String>,
    pub overlay_coverage: f64,
    // How far a pullout panel has slid in from the right edge: 0.0 = fully
    // hidden (parent visible), 1.0 = fully covering its slice. Animated on
    // expand/collapse; ignored for non-overlay layers.
    pub overlay_reveal: f64,
    // Screen x (logical px) of the parent's `<` toggle button, captured when a
    // slide begins. The panel's leading edge starts here rather than at the far
    // right edge, so the panel appears to grow straight out of the toggle.
    // `None` falls back to the bar's right edge.
    pub overlay_slide_start_x: Option<f64>,
}

// A pullout panel slides in/out over up to this long. Kept well under the 2s
// ceiling so the reveal reads as a smooth glide rather than a lag.
pub const PULLOUT_ANIM: Duration = Duration::from_millis(300);

// Tracks an in-progress pullout slide. On expand the panel becomes active
// immediately and slides in; on collapse the panel stays active and slides out,
// handing back to its parent only once the animation finishes.
pub struct PulloutAnim {
    pub panel: usize,
    pub parent: usize,
    pub collapsing: bool,
    pub start: Instant,
}

impl PulloutAnim {
    // Reveal fraction (0..1) for the current instant. Linear, so the panel
    // starts widening on the very first frame rather than easing in (which reads
    // as a delay before anything moves).
    pub fn reveal(&self, now: Instant) -> f64 {
        let t = (now.saturating_duration_since(self.start).as_secs_f64()
            / PULLOUT_ANIM.as_secs_f64())
        .clamp(0.0, 1.0);
        if self.collapsing {
            1.0 - t
        } else {
            t
        }
    }
    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= PULLOUT_ANIM
    }
}

// Decide how switching from layer `from` to `target` should play out. A switch
// into a pullout panel slides it in; a switch from a panel back to its parent
// slides it out. Returns the layer that is active during the transition plus an
// optional animation to advance each frame. Non-pullout switches are instant.
pub fn begin_layer_switch(
    layers: &[FunctionLayer],
    from: usize,
    target: usize,
    now: Instant,
) -> (usize, Option<PulloutAnim>) {
    let parent_of = |idx: usize| {
        layers[idx]
            .overlay_parent
            .as_deref()
            .and_then(|name| layer_index(layers, name))
    };
    // Expand: target is a panel — it becomes active and slides in.
    if layers[target].is_overlay() {
        if let Some(parent) = parent_of(target) {
            return (
                target,
                Some(PulloutAnim {
                    panel: target,
                    parent,
                    collapsing: false,
                    start: now,
                }),
            );
        }
    }
    // Collapse: leaving a panel back to its own parent — slide out, then switch.
    if layers[from].is_overlay() && parent_of(from) == Some(target) {
        return (
            from,
            Some(PulloutAnim {
                panel: from,
                parent: target,
                collapsing: true,
                start: now,
            }),
        );
    }
    (target, None)
}

// Two distinct elements of the same slice, mutably.
pub fn two_mut<T>(slice: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    assert_ne!(a, b);
    if a < b {
        let (l, r) = slice.split_at_mut(b);
        (&mut l[a], &mut r[0])
    } else {
        let (l, r) = slice.split_at_mut(a);
        (&mut r[0], &mut l[b])
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LayoutItemKind {
    Button(usize),
    NotificationAction(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutItem {
    start: f64,
    end: f64,
    kind: LayoutItemKind,
}

impl FunctionLayer {
    pub fn layout_items(&self, notification_mgr: Option<&NotificationManager>) -> Vec<LayoutItem> {
        let hide_notification_nav = self.displays_notifications
            && notification_mgr
                .map(|mgr| !mgr.can_navigate())
                .unwrap_or(false);
        let action_count = notification_mgr
            .map(|mgr| mgr.current_actions().len())
            .unwrap_or(0);
        let action_width = action_count as f64 * 2.0;
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
        let mut items = Vec::new();
        for (i, (_, button)) in self.buttons.iter().enumerate() {
            let mut width = self.button_width_units(i);
            if hide_notification_nav {
                match button.notification_kind() {
                    Some(NotificationButton::Previous | NotificationButton::Next) => width = 0.0,
                    Some(NotificationButton::Text) => width += notification_nav_width,
                    _ => {}
                }
            }
            if matches!(button.notification_kind(), Some(NotificationButton::Text)) {
                width = (width - action_width).max(0.5);
            }
            let start = cursor;
            cursor += width;
            items.push(LayoutItem {
                start,
                end: cursor,
                kind: LayoutItemKind::Button(i),
            });
            if matches!(button.notification_kind(), Some(NotificationButton::Text)) {
                for action_index in 0..action_count {
                    let start = cursor;
                    cursor += 2.0;
                    items.push(LayoutItem {
                        start,
                        end: cursor,
                        kind: LayoutItemKind::NotificationAction(action_index),
                    });
                }
            }
        }
        items
    }

    pub fn button_spans(&self, notification_mgr: Option<&NotificationManager>) -> Vec<(f64, f64)> {
        self.layout_items(notification_mgr)
            .into_iter()
            .map(|item| (item.start, item.end))
            .collect()
    }

    pub fn button_width_units(&self, i: usize) -> f64 {
        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };
        end - start
    }

    pub fn with_config(name: impl Into<String>, cfg: Vec<ButtonConfig>) -> FunctionLayer {
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
                    log_line!("Stretch value must be at least 0.5, setting to 0.5.");
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
            overlay_parent: None,
            overlay_coverage: 0.0,
            overlay_reveal: 1.0,
            overlay_slide_start_x: None,
        }
    }
    pub fn set_overlay(&mut self, parent: String, coverage: f64) {
        self.overlay_parent = Some(parent);
        self.overlay_coverage = coverage;
    }
    pub fn is_overlay(&self) -> bool {
        self.overlay_parent.is_some()
    }
    // Left x (logical pixels) where a pullout panel begins; 0 for normal layers.
    pub fn overlay_origin(&self, width: f64) -> f64 {
        if self.is_overlay() {
            width * (1.0 - self.overlay_coverage)
        } else {
            0.0
        }
    }
    pub fn draw(
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
        let items = self.layout_items(notification_mgr);
        let visible_buttons = items.iter().filter(|item| item.end > item.start).count();
        let total_spacing = BUTTON_SPACING_PX as f64 * visible_buttons.saturating_sub(1) as f64;
        // Pullout panels lay out within the right slice of the bar instead of
        // the full width; everything below offsets by origin_x and packs into
        // region_w so the parent stays visible to the left.
        let overlay = self.is_overlay();
        let origin_x = self.overlay_origin(width as f64);
        let region_w = width as f64 - origin_x;
        // A panel slides in from the bar's right edge: its contents are shifted
        // right by `slide_offset` and translate leftward into their final
        // positions as reveal goes 0 -> 1. The toggle button, being the panel's
        // leftmost item, rides the leading edge next to the divider, so it looks
        // like the button walks left as the panel grows; the rightmost content
        // (e.g. the workspace icon) enters from the far edge first. On collapse
        // the whole thing slides back out to the right the same way.
        let reveal = if overlay {
            self.overlay_reveal.clamp(0.0, 1.0)
        } else {
            1.0
        };
        // The slide starts at the parent's `<` toggle (captured when the anim
        // began) so the panel grows straight out of that button; without it we
        // fall back to the far right edge. Distance is clamped to the panel
        // slice so a toggle sitting outside it can't push content off-screen.
        let covered_right = origin_x + region_w;
        let slide_start_x = self
            .overlay_slide_start_x
            .filter(|_| overlay)
            .unwrap_or(covered_right)
            .clamp(origin_x, covered_right);
        let slide_offset = (1.0 - reveal) * (slide_start_x - origin_x);
        let covered_left = origin_x + slide_offset;
        let virtual_button_width =
            (region_w - pixel_shift_width as f64 - total_spacing) / self.virtual_button_count;
        let radius = 8.0f64;
        let bot = (height as f64) * 0.15;
        let top = (height as f64) * 0.85;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            if overlay {
                // Mask only the covered part of the parent, with the divider
                // tracking the moving left edge; the parent shows through to the
                // left of `covered_left` until the panel widens over it.
                c.set_source_rgb(0.0, 0.0, 0.0);
                c.rectangle(covered_left, 0.0, covered_right - covered_left, height as f64);
                c.fill().unwrap();
                c.set_source_rgb(0.3, 0.3, 0.3);
                c.rectangle(covered_left, bot - radius, 2.0, top - bot + radius * 2.0);
                c.fill().unwrap();
            } else {
                c.set_source_rgb(0.0, 0.0, 0.0);
                c.paint().unwrap();
            }
        }
        // Confine the panel's buttons to the revealed width while animating.
        if overlay && reveal < 1.0 {
            c.rectangle(covered_left, 0.0, covered_right - covered_left, height as f64);
            c.clip();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(config.font_size);

        for (i, item) in items.iter().enumerate() {
            let visible_i = items[..i]
                .iter()
                .filter(|item| item.end > item.start)
                .count();
            let button_index = match item.kind {
                LayoutItemKind::Button(button_index) => Some(button_index),
                LayoutItemKind::NotificationAction(_) => None,
            };
            let button_changed = button_index
                .map(|button_index| self.buttons[button_index].1.changed)
                .unwrap_or(true);

            if !button_changed && !complete_redraw {
                continue;
            };
            if item.end <= item.start {
                continue;
            }

            let left_edge = (item.start * virtual_button_width
                + visible_i as f64 * BUTTON_SPACING_PX as f64)
                .floor()
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64
                + origin_x
                + slide_offset;

            let button_width = ((item.end - item.start) * virtual_button_width).floor();

            let active = button_index
                .map(|button_index| self.buttons[button_index].1.active)
                .unwrap_or(false);

            let color = if active {
                BUTTON_COLOR_ACTIVE
            } else if config.show_button_outlines {
                BUTTON_COLOR_INACTIVE
            } else {
                0.0
            };
            let button_visible = button_index
                .map(|button_index| {
                    self.buttons[button_index]
                        .1
                        .is_visible(sysinfo_mgr, notification_mgr)
                })
                .unwrap_or(true);
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
                if let Some(button_index) = button_index {
                    self.buttons[button_index]
                        .1
                        .set_background_color_with_notifications(&c, color, notification_mgr);
                } else {
                    c.set_source_rgb(color, color, color);
                }
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
                if let Some(progress) = button_index
                    .and_then(|button_index| self.buttons[button_index].1.hold_progress())
                {
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
                if let Some(button_index) = button_index {
                    let button = &self.buttons[button_index].1;
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
                } else if let Some((button, mgr)) = self.buttons.first().zip(notification_mgr) {
                    if let LayoutItemKind::NotificationAction(action_index) = item.kind {
                        if let Some(action) = mgr.current_actions().get(action_index) {
                            c.set_font_size(config.font_size);
                            button.1.render_content(
                                &c,
                                ButtonContent::ClippedText(action.label.clone()),
                                height,
                                left_edge,
                                button_width.ceil() as u64,
                                pixel_shift_y,
                            );
                        }
                    }
                }
            }

            if let Some(button_index) = button_index {
                self.buttons[button_index].1.changed = false;
            }

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

    // Screen x (logical px) of the left edge of button `button_index` as it is
    // currently laid out, or None if it isn't visible. Used to anchor a pullout
    // slide to the toggle that launched it.
    pub fn button_left_edge(
        &self,
        width: u16,
        button_index: usize,
        notification_mgr: Option<&NotificationManager>,
    ) -> Option<f64> {
        let origin_x = self.overlay_origin(width as f64);
        let items = self.layout_items(notification_mgr);
        let visible_buttons = items.iter().filter(|item| item.end > item.start).count();
        let total_spacing = BUTTON_SPACING_PX as f64 * visible_buttons.saturating_sub(1) as f64;
        let virtual_button_width =
            (width as f64 - origin_x - total_spacing) / self.virtual_button_count;
        let idx = items.iter().position(|item| {
            matches!(item.kind, LayoutItemKind::Button(b) if b == button_index)
        })?;
        let item = items[idx];
        if item.end <= item.start {
            return None;
        }
        let visible_i = items[..idx]
            .iter()
            .filter(|item| item.end > item.start)
            .count();
        Some(
            (item.start * virtual_button_width + visible_i as f64 * BUTTON_SPACING_PX as f64)
                .floor()
                + origin_x,
        )
    }

    pub fn hit(
        &self,
        width: u16,
        height: u16,
        x: f64,
        y: f64,
        i: Option<usize>,
        notification_mgr: Option<&NotificationManager>,
    ) -> Option<usize> {
        // Touches left of a pullout panel belong to the parent layer, not the
        // panel; the caller re-runs hit() against the parent for those.
        let origin_x = self.overlay_origin(width as f64);
        if self.is_overlay() && i.is_none() && x < origin_x {
            return None;
        }
        let items = self.layout_items(notification_mgr);
        let visible_buttons = items.iter().filter(|item| item.end > item.start).count();
        let total_spacing = BUTTON_SPACING_PX as f64 * visible_buttons.saturating_sub(1) as f64;
        let virtual_button_width =
            (width as f64 - origin_x - total_spacing) / self.virtual_button_count;

        let i = if let Some(slot) = i {
            items
                .iter()
                .position(|item| match item.kind {
                    LayoutItemKind::Button(button_index) => button_index == slot,
                    LayoutItemKind::NotificationAction(action_index) => {
                        self.buttons.len() + action_index == slot
                    }
                })
                .unwrap_or(items.len())
        } else {
            items
                .iter()
                .enumerate()
                .position(|(idx, item)| {
                    if item.end <= item.start {
                        return false;
                    }
                    let visible_i = items[..idx]
                        .iter()
                        .filter(|item| item.end > item.start)
                        .count();
                    let left = item.start * virtual_button_width
                        + visible_i as f64 * BUTTON_SPACING_PX as f64
                        + origin_x;
                    let right = left + (item.end - item.start) * virtual_button_width;
                    x >= left && x <= right
                })
                .unwrap_or(items.len())
        };
        if i >= items.len() {
            return None;
        }

        let item = items[i];
        if item.end <= item.start {
            return None;
        }
        let visible_i = items[..i]
            .iter()
            .filter(|item| item.end > item.start)
            .count();

        let left_edge = (item.start * virtual_button_width
            + visible_i as f64 * BUTTON_SPACING_PX as f64)
            .floor()
            + origin_x;

        let button_width = ((item.end - item.start) * virtual_button_width).floor();

        if x < left_edge
            || x > (left_edge + button_width)
            || y < 0.1 * height as f64
            || y > 0.9 * height as f64
        {
            return None;
        }

        // Ignore presses on buttons that have no action to perform so they
        // don't flash an active highlight and appear broken.
        match item.kind {
            LayoutItemKind::Button(button_index) => {
                if !self.buttons[button_index].1.is_interactive() {
                    return None;
                }
                // ActiveWindow / ActiveWorkspace visibility depends on
                // desktop-info state that hit() can't see (it passes None for
                // the SystemInfoManager, so the is_visible() check below would
                // treat them as hidden and drop the touch). They already passed
                // the is_interactive() gate above, so honor the press.
                if self.buttons[button_index].1.is_active_window()
                    || self.buttons[button_index].1.is_active_workspace()
                {
                    return Some(button_index);
                }
                if !self.buttons[button_index]
                    .1
                    .is_visible(None, notification_mgr)
                {
                    return None;
                }
                Some(button_index)
            }
            LayoutItemKind::NotificationAction(action_index) => {
                Some(self.buttons.len() + action_index)
            }
        }
    }

    // Fraction (0.0..=1.0) of the touch x-position across button `i`'s width.
    pub fn slider_fraction(&self, width: u16, i: usize, x: f64) -> f64 {
        let origin_x = self.overlay_origin(width as f64);
        let total_spacing = BUTTON_SPACING_PX as f64 * self.buttons.len().saturating_sub(1) as f64;
        let virtual_button_width =
            (width as f64 - origin_x - total_spacing) / self.virtual_button_count;

        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };

        let left_edge =
            (start * virtual_button_width + i as f64 * BUTTON_SPACING_PX as f64).floor() + origin_x;
        let button_width = ((end - start) * virtual_button_width).floor();

        ((x - left_edge) / button_width).clamp(0.0, 1.0)
    }
}
