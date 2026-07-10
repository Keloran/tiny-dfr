// On-screen Touch Bar simulator.
//
// Runs the real rendering and hit-testing code (config::ConfigManager,
// FunctionLayer::draw / hit / set_active) against a plain framebuffer window
// instead of the DRM Touch Bar, so tiny-dfr can be exercised on a machine
// without the hardware. Mouse maps to touch. If /dev/uinput is writable the
// simulated buttons emit real key events (a functional on-screen Touch Bar);
// otherwise it runs preview-only and just logs/highlights presses.
//
// Enabled with `--features simulator`, run with `tiny-dfr --simulate`.

use crate::{layer_index, ConfigManager, NotificationManager, SystemInfoManager, WeatherManager};
use cairo::{Format, ImageSurface};
use input_linux::{uinput::UInputHandle, EventKind, Key};
use input_linux_sys::{input_id, uinput_setup};
use libc::c_char;
use minifb::{Key as MKey, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
};

const DEFAULT_WIDTH: usize = 1710;
const DEFAULT_HEIGHT: usize = 50;

// Resolve the bundled assets so the simulator works from a checkout without
// installing to /usr/share. Honors an existing TINY_DFR_SHARE_DIR, otherwise
// probes the repo layout and the installed location.
fn ensure_share_dir() {
    if std::env::var_os("TINY_DFR_SHARE_DIR").is_some() {
        return;
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("share/tiny-dfr"));
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("share/tiny-dfr"));
    candidates.push(PathBuf::from("/usr/share/tiny-dfr"));
    for dir in candidates {
        if dir.join("config.toml").exists() {
            std::env::set_var("TINY_DFR_SHARE_DIR", dir);
            return;
        }
    }
}

// Parse TINY_DFR_SIM_SIZE=WxH, falling back to the defaults.
fn window_size() -> (usize, usize) {
    if let Ok(spec) = std::env::var("TINY_DFR_SIM_SIZE") {
        if let Some((w, h)) = spec.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.trim().parse(), h.trim().parse()) {
                return (w, h);
            }
        }
        eprintln!("Ignoring invalid TINY_DFR_SIM_SIZE='{spec}' (expected WxH)");
    }
    (DEFAULT_WIDTH, DEFAULT_HEIGHT)
}

// A UInputHandle usable by Button::set_active. When /dev/uinput is available we
// register a real virtual keyboard so presses actually type; otherwise we sink
// the events into /dev/null so set_active stays a no-op instead of panicking.
fn open_uinput() -> (UInputHandle<File>, bool) {
    if let Ok(file) = OpenOptions::new().write(true).open("/dev/uinput") {
        let uinput = UInputHandle::new(file);
        let setup = (|| -> std::io::Result<()> {
            uinput.set_evbit(EventKind::Key)?;
            for k in Key::iter() {
                uinput.set_keybit(k)?;
            }
            let mut name = [0 as c_char; 80];
            for (dst, src) in name.iter_mut().zip(b"tiny-dfr Simulator".iter()) {
                *dst = *src as c_char;
            }
            uinput.dev_setup(&uinput_setup {
                id: input_id {
                    bustype: 0x19,
                    vendor: 0x1209,
                    product: 0x316E,
                    version: 1,
                },
                ff_effects_max: 0,
                name,
            })?;
            uinput.dev_create()?;
            Ok(())
        })();
        if setup.is_ok() {
            return (uinput, true);
        }
    }
    let null = OpenOptions::new().write(true).open("/dev/null").unwrap();
    (UInputHandle::new(null), false)
}

// Blit the cairo surface into minifb's 0RGB u32 buffer, rotating back to a
// horizontal screen orientation.
//
// draw() bakes in the Touch Bar's 90° panel rotation, so it renders onto a
// surface held in the panel's native (tall-narrow) orientation: its width is
// the logical height `ch` and its height is the logical width `cw`. A logical
// point (x, y) lands at device pixel (ch - y, x). We invert that here so the
// output window shows buttons left-to-right like a normal display.
fn blit(surface: &mut ImageSurface, buf: &mut [u32], cw: usize, ch: usize) {
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().unwrap();
    for y in 0..ch {
        let col = ch - 1 - y; // surface column for this window row
        for x in 0..cw {
            let idx = x * stride + col * 4; // surface row = x
            // little-endian ARGB32 bytes are [B, G, R, A]
            buf[y * cw + x] = u32::from_le_bytes([data[idx], data[idx + 1], data[idx + 2], 0]);
        }
    }
}

pub fn run() {
    ensure_share_dir();
    let (w, h) = window_size();

    let cfg_mgr = ConfigManager::new();
    let (cfg, mut layers) = cfg_mgr.load_config(w as u16);

    let (mut uinput, functional) = open_uinput();
    if functional {
        println!("tiny-dfr simulator: /dev/uinput open — button presses will emit real keys.");
    } else {
        println!("tiny-dfr simulator: no /dev/uinput access — preview only (keys are logged, not sent).");
        println!("  Grant access (e.g. install 99-uinput.rules / run as a member of the input group) to make it functional.");
    }

    // Info managers, mirroring real_main; only passed to draw when a layer needs them.
    let sysinfo_mgr = SystemInfoManager::new();
    let weather_mgr = WeatherManager::new(cfg.weather_location.clone(), cfg.weather_fahrenheit);
    let mut notification_mgr = NotificationManager::new();

    let normal_layer = layer_index(&layers, &cfg.default_layer).unwrap_or(0);
    let fkeys_layer = layer_index(&layers, "FKeys");
    let mut selected_layer = normal_layer;

    println!("Layers: {}", layer_list(&layers));
    println!("Controls: click = tap · number keys 1-{} switch layer · hold LeftCtrl = FKeys · Esc quits", layers.len());

    let mut window = Window::new(
        "tiny-dfr simulator",
        w,
        h,
        WindowOptions {
            resize: true,
            // Fill whatever size the window manager gives us (Hyprland tiles
            // it), stretching the buffer if it can't be matched exactly.
            scale_mode: ScaleMode::Stretch,
            ..WindowOptions::default()
        },
    )
    .expect("failed to open simulator window");
    window.set_target_fps(60);

    // Render at the window's live size so buttons always span the full width,
    // regardless of Hyprland tiling or HiDPI scaling.
    let (mut cw, mut ch) = (w, h);
    // Surface is allocated transposed (panel-native): width = ch, height = cw.
    let mut surface = ImageSurface::create(Format::ARgb32, ch as i32, cw as i32).unwrap();
    let mut buf = vec![0u32; cw * ch];

    // Currently-held press: (layer, button index, is_slider).
    let mut pressed: Option<(usize, usize, bool)> = None;
    let mut prev_mouse_down = false;

    while window.is_open() && !window.is_key_down(MKey::Escape) {
        // Resize the render surface to the current window size.
        let (nw, nh) = window.get_size();
        if nw > 0 && nh > 0 && (nw, nh) != (cw, ch) {
            cw = nw;
            ch = nh;
            surface = ImageSurface::create(Format::ARgb32, ch as i32, cw as i32).unwrap();
            buf = vec![0u32; cw * ch];
        }

        // Digit keys pick a layer.
        for (i, key) in DIGIT_KEYS.iter().enumerate() {
            if i < layers.len() && window.is_key_pressed(*key, minifb::KeyRepeat::No) {
                selected_layer = i;
            }
        }
        let active = if window.is_key_down(MKey::LeftCtrl) {
            fkeys_layer.unwrap_or(selected_layer)
        } else {
            selected_layer
        };

        // Keep live buttons (sliders, notifications) up to date.
        if layers[active].displays_slider {
            for (_, button) in &mut layers[active].buttons {
                button.slider_refresh();
            }
        }
        if layers[active].displays_notifications {
            notification_mgr.refresh();
        }

        // A pullout panel draws over its parent; render the parent first.
        if let Some(parent_idx) = layers[active]
            .overlay_parent
            .clone()
            .and_then(|name| layer_index(&layers, &name))
            .filter(|&idx| idx != active)
        {
            let (parent, panel) = crate::two_mut(&mut layers, parent_idx, active);
            for l in [&mut *parent, &mut *panel] {
                l.draw(
                    &cfg,
                    cw as i32,
                    ch as i32,
                    &surface,
                    (0.0, 0.0),
                    true,
                    Some(&sysinfo_mgr),
                    Some(&weather_mgr),
                    Some(&notification_mgr),
                );
            }
        } else {
            let sysinfo_ref = layers[active].displays_sysinfo.then_some(&sysinfo_mgr);
            let weather_ref = layers[active].displays_weather.then_some(&weather_mgr);
            let notif_ref = layers[active]
                .displays_notifications
                .then_some(&notification_mgr);
            layers[active].draw(
                &cfg,
                cw as i32,
                ch as i32,
                &surface,
                (0.0, 0.0),
                true,
                sysinfo_ref,
                weather_ref,
                notif_ref,
            );
        }
        blit(&mut surface, &mut buf, cw, ch);
        window.update_with_buffer(&buf, cw, ch).unwrap();

        // Mouse → touch.
        let mouse_down = window.get_mouse_down(MouseButton::Left);
        let pos = window.get_mouse_pos(MouseMode::Clamp);

        if mouse_down && !prev_mouse_down {
            if let Some((mx, my)) = pos {
                let notif = layers[active]
                    .displays_notifications
                    .then_some(&notification_mgr);
                // Clicks left of a pullout panel fall through to the parent.
                let mut hit_layer = active;
                let mut hit =
                    layers[active].hit(cw as u16, ch as u16, mx as f64, my as f64, None, notif);
                if hit.is_none() {
                    if let Some(parent_idx) = layers[active]
                        .overlay_parent
                        .clone()
                        .and_then(|name| layer_index(&layers, &name))
                    {
                        let pn = layers[parent_idx]
                            .displays_notifications
                            .then_some(&notification_mgr);
                        hit = layers[parent_idx].hit(cw as u16, ch as u16, mx as f64, my as f64, None, pn);
                        if hit.is_some() {
                            hit_layer = parent_idx;
                        }
                    }
                }
                if let Some(btn) = hit {
                    if btn < layers[hit_layer].buttons.len() {
                        if let Some(kind) = layers[hit_layer].buttons[btn].1.slider_kind() {
                            let frac = layers[hit_layer].slider_fraction(cw as u16, btn, mx as f64);
                            layers[hit_layer].buttons[btn].1.set_slider_fraction(frac);
                            log_press(&format!("slider {kind:?}"), functional);
                            pressed = Some((hit_layer, btn, true));
                        } else {
                            let label = button_label(&layers[hit_layer].buttons[btn].1);
                            layers[hit_layer].buttons[btn].1.set_active(&mut uinput, true);
                            log_press(&label, functional);
                            pressed = Some((hit_layer, btn, false));
                        }
                    }
                }
            }
        } else if mouse_down {
            // Drag a held slider.
            if let Some((layer, btn, true)) = pressed {
                if let Some((mx, _)) = pos {
                    let frac = layers[layer].slider_fraction(cw as u16, btn, mx as f64);
                    layers[layer].buttons[btn].1.set_slider_fraction(frac);
                }
            }
        } else if prev_mouse_down {
            // Release.
            if let Some((layer, btn, is_slider)) = pressed.take() {
                if is_slider {
                    layers[layer].buttons[btn].1.slider_commit();
                } else {
                    layers[layer].buttons[btn].1.set_active(&mut uinput, false);
                    // A layer toggle (e.g. the pullout "<"/">" or a settings
                    // icon) switches the selected layer on release.
                    if let Some(target) = layers[layer].buttons[btn]
                        .1
                        .layer_toggle_target()
                        .map(str::to_string)
                    {
                        if let Some(idx) = layer_index(&layers, &target) {
                            selected_layer = idx;
                        }
                    }
                }
            }
        }
        prev_mouse_down = mouse_down;
    }
}

const DIGIT_KEYS: [MKey; 9] = [
    MKey::Key1,
    MKey::Key2,
    MKey::Key3,
    MKey::Key4,
    MKey::Key5,
    MKey::Key6,
    MKey::Key7,
    MKey::Key8,
    MKey::Key9,
];

fn layer_list(layers: &[crate::FunctionLayer]) -> String {
    layers
        .iter()
        .enumerate()
        .map(|(i, l)| format!("[{}] {}", i + 1, l.name))
        .collect::<Vec<_>>()
        .join("  ")
}

// A short description of what a press does, for the console log.
fn button_label(button: &crate::Button) -> String {
    if !button.action.is_empty() {
        format!("{:?}", button.action)
    } else if let Some(cmd) = &button.command {
        format!("command `{cmd}`")
    } else {
        "(no action)".to_string()
    }
}

fn log_press(label: &str, functional: bool) {
    if functional {
        println!("press: {label}");
    } else {
        println!("press (preview): {label}");
    }
}
