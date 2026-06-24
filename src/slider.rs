use crate::backlight::{find_display_backlight, find_keyboard_backlight};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    time::{Duration, Instant},
};

// How often the volume backend is polled for live updates while a slider layer
// is shown. Brightness/backlight values come from sysfs and are read every tick.
const VOLUME_POLL_INTERVAL: Duration = Duration::from_millis(700);
// While dragging the volume slider, coalesce writes to the audio backend so we
// don't spawn a flood of `wpctl` processes.
const VOLUME_SET_INTERVAL: Duration = Duration::from_millis(60);
// Never let the main display go fully black via the slider.
const DISPLAY_MIN_FRACTION: f64 = 0.02;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SliderKind {
    DisplayBrightness,
    KeyboardBacklight,
    Volume,
}

impl SliderKind {
    pub fn parse(s: &str) -> Option<SliderKind> {
        match s.to_ascii_lowercase().as_str() {
            "display_brightness" | "brightness" | "display" => Some(SliderKind::DisplayBrightness),
            "keyboard_backlight" | "backlight" | "keyboard" | "illum" => {
                Some(SliderKind::KeyboardBacklight)
            }
            "volume" | "audio" | "sound" => Some(SliderKind::Volume),
            _ => None,
        }
    }
}

pub struct Slider {
    kind: SliderKind,
    path: Option<PathBuf>,
    max: u32,
    value: f64, // 0.0..=1.0
    muted: bool,
    last_poll: Option<Instant>,
    last_set: Option<Instant>,
    children: Vec<Child>,
}

impl Slider {
    pub fn new(kind: SliderKind) -> Slider {
        let (path, max) = match kind {
            SliderKind::DisplayBrightness => Self::sysfs_init(find_display_backlight().ok()),
            SliderKind::KeyboardBacklight => Self::sysfs_init(find_keyboard_backlight().ok()),
            SliderKind::Volume => (None, 0),
        };
        let mut slider = Slider {
            kind,
            path,
            max,
            value: 0.0,
            muted: false,
            last_poll: None,
            last_set: None,
            children: Vec::new(),
        };
        slider.refresh();
        slider
    }

    fn sysfs_init(path: Option<PathBuf>) -> (Option<PathBuf>, u32) {
        match path {
            Some(p) => {
                let max = read_u32(&p.join("max_brightness")).unwrap_or(0);
                (Some(p), max)
            }
            None => (None, 0),
        }
    }

    pub fn kind(&self) -> SliderKind {
        self.kind
    }
    pub fn value(&self) -> f64 {
        self.value
    }
    pub fn muted(&self) -> bool {
        self.muted
    }
    pub fn percent(&self) -> u32 {
        (self.value * 100.0).round() as u32
    }

    /// Re-read the current hardware value. Returns true if the displayed state
    /// changed and the button should be redrawn.
    pub fn refresh(&mut self) -> bool {
        self.reap();
        let (value, muted) = match self.kind {
            SliderKind::Volume => {
                if let Some(last) = self.last_poll {
                    if last.elapsed() < VOLUME_POLL_INTERVAL {
                        return false;
                    }
                }
                self.last_poll = Some(Instant::now());
                match query_volume() {
                    Some(v) => v,
                    None => return false,
                }
            }
            _ => {
                let Some(path) = &self.path else {
                    return false;
                };
                let cur = read_u32(&path.join("brightness")).unwrap_or(0);
                let frac = if self.max > 0 {
                    cur as f64 / self.max as f64
                } else {
                    0.0
                };
                (frac, false)
            }
        };
        let changed = (value - self.value).abs() > 0.001 || muted != self.muted;
        self.value = value.clamp(0.0, 1.0);
        self.muted = muted;
        changed
    }

    /// Set the slider to a fraction (0.0..=1.0) and write it to the hardware.
    pub fn set_fraction(&mut self, frac: f64) {
        self.reap();
        let frac = frac.clamp(0.0, 1.0);
        match self.kind {
            SliderKind::Volume => {
                self.value = frac;
                if frac > 0.0 {
                    self.muted = false;
                }
                let now = Instant::now();
                let due = self
                    .last_set
                    .map_or(true, |t| now.duration_since(t) >= VOLUME_SET_INTERVAL);
                if due {
                    self.apply_volume();
                    self.last_set = Some(now);
                }
            }
            SliderKind::DisplayBrightness | SliderKind::KeyboardBacklight => {
                let frac = if self.kind == SliderKind::DisplayBrightness {
                    frac.max(DISPLAY_MIN_FRACTION)
                } else {
                    frac
                };
                self.value = frac;
                if let Some(path) = &self.path {
                    let val = (frac * self.max as f64).round() as u32;
                    let _ = fs::write(path.join("brightness"), format!("{}\n", val));
                }
            }
        }
    }

    /// Apply the final value once a drag ends (ensures coalesced volume writes
    /// don't drop the last position).
    pub fn commit(&mut self) {
        if self.kind == SliderKind::Volume {
            self.apply_volume();
            self.last_set = Some(Instant::now());
        }
    }

    pub fn toggle_mute(&mut self) {
        if self.kind != SliderKind::Volume {
            return;
        }
        self.spawn(user_command("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"));
        self.muted = !self.muted;
        self.last_poll = Some(Instant::now());
    }

    fn apply_volume(&mut self) {
        let pct = (self.value * 100.0).round() as u32;
        self.spawn(user_command(&format!(
            "wpctl set-volume @DEFAULT_AUDIO_SINK@ {}%",
            pct
        )));
    }

    fn spawn(&mut self, mut command: Command) {
        match command.spawn() {
            Ok(child) => self.children.push(child),
            Err(e) => eprintln!("TINY-DFR: failed to spawn slider command: {e}"),
        }
    }

    /// Reap any finished child processes so volume drags don't leave zombies.
    fn reap(&mut self) {
        self.children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

fn read_u32(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn query_volume() -> Option<(f64, bool)> {
    let out = user_command("wpctl get-volume @DEFAULT_AUDIO_SINK@")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Output looks like "Volume: 0.42" or "Volume: 0.42 [MUTED]".
    let s = String::from_utf8_lossy(&out.stdout);
    let muted = s.contains("[MUTED]");
    let frac = s
        .split_whitespace()
        .find_map(|tok| tok.parse::<f64>().ok())?;
    Some((frac.min(1.0), muted))
}

/// Build a command that runs as the logged-in user with their Wayland/PipeWire
/// session environment, mirroring how button commands are executed.
pub fn user_command(cmd_str: &str) -> Command {
    let mut command = if let Ok(sudo_user) = env::var("SUDO_USER") {
        let mut cmd = Command::new("sudo");
        cmd.arg("-u")
            .arg(&sudo_user)
            .arg("sh")
            .arg("-c")
            .arg(cmd_str);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(cmd_str);
        cmd
    };
    for var in [
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "HYPRLAND_INSTANCE_SIGNATURE",
    ] {
        if let Ok(val) = env::var(var) {
            command.env(var, val);
        }
    }
    // Best-effort: derive the runtime dir if it isn't in our environment. The
    // daemon usually runs as the logged-in user under systemd with a minimal
    // env, so fall back to /run/user/<uid> so wpctl can find the PipeWire socket.
    if env::var_os("XDG_RUNTIME_DIR").is_none() {
        let uid = env::var("SUDO_UID").unwrap_or_else(|_| unsafe { libc::geteuid() }.to_string());
        command.env("XDG_RUNTIME_DIR", format!("/run/user/{}", uid));
    }
    command
}
