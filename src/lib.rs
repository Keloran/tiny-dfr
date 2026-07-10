use cairo::{Format, ImageSurface};
use chrono::{Local, Timelike};
use drm::control::ClipRect;
use ::input::{
    event::{
        device::DeviceEvent,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot},
        Event, EventTrait,
    },
    Device as InputDevice, Libinput,
};
use input_linux::{uinput::UInputHandle, EventKind, Key};
use input_linux_sys::{input_id, uinput_setup};
use libc::c_char;
use nix::{
    errno::Errno,
    sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
};
use privdrop::PrivDrop;
use std::{
    cmp::min,
    collections::HashMap,
    fs::OpenOptions,
    os::fd::AsFd,
    panic::{self, AssertUnwindSafe},
    time::{Duration, Instant},
};
use udev::MonitorBuilder;

mod backlight;
mod button;
mod config;
mod display;
mod fonts;
mod input;
mod layer;
mod notification_manager;
mod pixel_shift;
#[cfg(feature = "simulator")]
mod simulator;
mod slider;
mod sysinfo_manager;
mod t2_usb;
mod weather;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use display::DrmBackend;
use crate::input::{open_drm_backend, try_touchbar_usb_recovery, Interface};
pub use notification_manager::NotificationManager;
use pixel_shift::PixelShiftManager;
pub use slider::SliderKind;
use sysinfo_manager::SystemInfoManager;
use weather::WeatherManager;

pub use button::*;
pub use config::ButtonConfig;
pub use layer::*;

pub const BUTTON_SPACING_PX: i32 = 16;
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

pub fn run() {
    #[cfg(feature = "simulator")]
    if std::env::args().any(|a| a == "--simulate" || a == "--sim") {
        simulator::run();
        return;
    }

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
    let mut pullout_anim: Option<PulloutAnim> = None;
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
                    | ButtonImage::ActiveWindow(_, _)
                    | ButtonImage::ActiveWorkspace(_, _) => {
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

        // Advance a running pullout slide: update the reveal, keep redrawing at
        // ~60fps until it settles, then hand a finished collapse back to the
        // parent layer.
        if let Some(anim) = &pullout_anim {
            let now = Instant::now();
            layers[anim.panel].overlay_reveal = anim.reveal(now);
            needs_complete_redraw = true;
            if anim.done(now) {
                if anim.collapsing {
                    normal_layer = anim.parent;
                    active_layer = anim.parent;
                }
                layers[anim.panel].overlay_reveal = 1.0;
                pullout_anim = None;
            } else {
                next_timeout_ms = min(next_timeout_ms, 16);
            }
        }

        // A pullout panel composites over its parent, so redraw the whole thing
        // whenever either changes rather than tracking partial regions.
        if layers[active_layer].is_overlay()
            && layers[active_layer].buttons.iter().any(|b| b.1.changed)
        {
            needs_complete_redraw = true;
        }
        if needs_complete_redraw || layers[active_layer].buttons.iter().any(|b| b.1.changed) {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let clips = if let Some(parent_idx) = layers[active_layer]
                .overlay_parent
                .clone()
                .and_then(|name| layer_index(&layers, &name))
                .filter(|&idx| idx != active_layer)
            {
                // Draw the parent as the base, then the panel over its right side.
                let (parent, panel) = two_mut(&mut layers, parent_idx, active_layer);
                parent.draw(
                    &cfg,
                    width as i32,
                    height as i32,
                    &surface,
                    shift,
                    true,
                    Some(&sysinfo_mgr),
                    Some(&weather_mgr),
                    Some(&notification_mgr),
                );
                panel.draw(
                    &cfg,
                    width as i32,
                    height as i32,
                    &surface,
                    shift,
                    true,
                    Some(&sysinfo_mgr),
                    Some(&weather_mgr),
                    Some(&notification_mgr),
                );
                // Both draws are full-frame complete redraws that each return a
                // whole-screen clip. Passing both to drm.dirty() flushes the
                // entire Touch Bar framebuffer twice per redraw; on real T2
                // hardware that double full-frame transfer overruns the
                // appletbdrm/apple-bce USB pipe and wedges the controller (the
                // simulator ignores clips, so it never showed up there). The
                // composite is already on the surface, so emit exactly one
                // full-frame damage rect.
                vec![ClipRect::new(0, 0, height, width)]
            } else {
                let sysinfo_mgr_ref = layers[active_layer].displays_sysinfo.then_some(&sysinfo_mgr);
                let weather_mgr_ref = layers[active_layer].displays_weather.then_some(&weather_mgr);
                let notification_mgr_ref = layers[active_layer]
                    .displays_notifications
                    .then_some(&notification_mgr);
                layers[active_layer].draw(
                    &cfg,
                    width as i32,
                    height as i32,
                    &surface,
                    shift,
                    needs_complete_redraw,
                    sysinfo_mgr_ref,
                    weather_mgr_ref,
                    notification_mgr_ref,
                )
            };
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
                        // Ignore new touches mid-slide; positions are in flux.
                        TouchEvent::Down(_) if pullout_anim.is_some() => {}
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(width as u32);
                            let y = dn.y_transformed(height as u32);
                            let notification_mgr_ref =
                                if layers[active_layer].displays_notifications {
                                    Some(&notification_mgr)
                                } else {
                                    None
                                };
                            // On a pullout panel, touches to the left of the
                            // panel fall through to the parent layer underneath.
                            let mut hit_layer = active_layer;
                            let mut hit = layers[active_layer]
                                .hit(width, height, x, y, None, notification_mgr_ref);
                            if hit.is_none() {
                                if let Some(parent_idx) = layers[active_layer]
                                    .overlay_parent
                                    .clone()
                                    .and_then(|name| layer_index(&layers, &name))
                                {
                                    let parent_notif = layers[parent_idx]
                                        .displays_notifications
                                        .then_some(&notification_mgr);
                                    hit = layers[parent_idx]
                                        .hit(width, height, x, y, None, parent_notif);
                                    if hit.is_some() {
                                        hit_layer = parent_idx;
                                    }
                                }
                            }
                            if let Some(btn) = hit {
                                touches.insert(dn.seat_slot(), (hit_layer, btn, Instant::now()));
                                if btn >= layers[hit_layer].buttons.len() {
                                    needs_complete_redraw = true;
                                    continue;
                                }
                                match layers[hit_layer].buttons[btn].1.slider_kind() {
                                    Some(kind) => {
                                        // Double-tap the volume slider to toggle mute.
                                        let mut muted = false;
                                        if kind == SliderKind::Volume {
                                            let now = Instant::now();
                                            if last_volume_tap
                                                .map(|t| now.duration_since(t) < DOUBLE_TAP_TIMEOUT)
                                                .unwrap_or(false)
                                            {
                                                layers[hit_layer].buttons[btn]
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
                                                layers[hit_layer].slider_fraction(width, btn, x);
                                            layers[hit_layer].buttons[btn]
                                                .1
                                                .set_slider_fraction(frac);
                                        }
                                    }
                                    None => {
                                        layers[hit_layer].buttons[btn]
                                            .1
                                            .set_active(&mut uinput, true);
                                        layers[hit_layer].buttons[btn].1.start_hold();
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
                            if btn >= layers[layer].buttons.len() {
                                continue;
                            }
                            if layers[layer].buttons[btn].1.slider_kind().is_some() {
                                // Any drag cancels a pending double-tap.
                                last_volume_tap = None;
                                let frac = layers[layer].slider_fraction(width, btn, x);
                                layers[layer].buttons[btn].1.set_slider_fraction(frac);
                            } else {
                                let notification_mgr_ref = layers[layer]
                                    .displays_notifications
                                    .then_some(&notification_mgr);
                                let hit = layers[layer]
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
                            if btn >= layers[layer].buttons.len() {
                                notification_mgr.invoke_action(btn - layers[layer].buttons.len());
                                touches.remove(&up.seat_slot());
                                needs_complete_redraw = true;
                                continue;
                            }
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
                            let active_window = layers[layer].buttons[btn].1.is_active_window();
                            layers[layer].buttons[btn].1.set_active(&mut uinput, false);
                            layers[layer].buttons[btn].1.clear_hold();
                            touches.remove(&up.seat_slot());
                            if active_window && sysinfo_mgr.toggle_titlebar_text() {
                                needs_complete_redraw = true;
                                continue;
                            }
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
                                    // Pullout expand/collapse slides; everything
                                    // else switches instantly. A collapse keeps
                                    // normal_layer until the slide-out finishes.
                                    let (active_now, anim) = begin_layer_switch(
                                        &layers,
                                        active_layer,
                                        target_layer,
                                        Instant::now(),
                                    );
                                    let collapsing =
                                        anim.as_ref().map(|a| a.collapsing).unwrap_or(false);
                                    if !collapsing {
                                        normal_layer = target_layer;
                                    }
                                    if let Some(a) = &anim {
                                        // Anchor the slide to the `<` toggle
                                        // just tapped so the panel grows out of
                                        // it. On collapse we keep the value from
                                        // the expand (the `>` we tap now sits at
                                        // the panel's left edge, not the `<`).
                                        if !a.collapsing {
                                            let start_x = layers[layer]
                                                .button_left_edge(width, btn, None);
                                            layers[a.panel].overlay_slide_start_x = start_x;
                                        }
                                        layers[a.panel].overlay_reveal =
                                            a.reveal(Instant::now());
                                    }
                                    active_layer = active_now;
                                    pullout_anim = anim;
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
