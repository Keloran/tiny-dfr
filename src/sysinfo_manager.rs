use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::{fs::MetadataExt, net::UnixStream};
use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::System;

use cosmic_client_toolkit::{
    sctk::{
        output::{OutputHandler, OutputState},
        registry::{ProvidesRegistryState, RegistryState},
    },
    toplevel_info::{ToplevelInfoHandler, ToplevelInfoState},
    wayland_client::{
        globals::registry_queue_init, protocol::wl_output, Connection, Proxy, QueueHandle,
    },
    wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1,
    workspace::{WorkspaceHandler, WorkspaceState},
};

const SYSTEM_STATS_REFRESH: Duration = Duration::from_secs(2);
const DESKTOP_INFO_REFRESH: Duration = Duration::from_millis(250);
const NOW_PLAYING_REFRESH: Duration = Duration::from_secs(1);

/// Configure the Hyprland IPC environment when tiny-dfr is started through sudo.
fn setup_hyprland_env() -> Option<String> {
    // Try environment variables first
    if env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() && env::var("XDG_RUNTIME_DIR").is_ok() {
        return hyprland_socket_from_env();
    }

    // Try SUDO_UID
    if let Ok(sudo_uid) = env::var("SUDO_UID") {
        let runtime_dir = format!("/run/user/{sudo_uid}");
        if let Some(signature) = find_hyprland_signature_in_runtime(&runtime_dir) {
            set_hyprland_env(&runtime_dir, &signature);
            println!("Hyprland detected via SUDO_UID runtime dir: {runtime_dir}");
            return hyprland_socket_from_env();
        }
    }

    // Try scanning /proc for Hyprland process
    if let Some((runtime_dir, signature)) = find_hyprland_env_in_proc() {
        set_hyprland_env(&runtime_dir, &signature);
        println!("Hyprland detected via process environment: {runtime_dir}");
        return hyprland_socket_from_env();
    }

    // Try all runtime directories
    if let Ok(runtime_users) = fs::read_dir("/run/user") {
        for runtime_user in runtime_users.flatten() {
            let runtime_dir = runtime_user.path();
            if let Some(signature) =
                find_hyprland_signature_in_runtime(runtime_dir.to_string_lossy().as_ref())
            {
                set_hyprland_env(runtime_dir.to_string_lossy().as_ref(), &signature);
                println!(
                    "Hyprland detected via runtime dir: {}",
                    runtime_dir.display()
                );
                return hyprland_socket_from_env();
            }
        }
    }

    // Try XDG_RUNTIME_DIR as last resort
    if let Ok(xdg_runtime) = env::var("XDG_RUNTIME_DIR") {
        if let Some(signature) = find_hyprland_signature_in_runtime(&xdg_runtime) {
            set_hyprland_env(&xdg_runtime, &signature);
            return hyprland_socket_from_env();
        }
    }

    None
}

fn current_desktop_contains(name: &str) -> bool {
    [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
    ]
    .iter()
    .filter_map(|key| env::var(key).ok())
    .any(|value| value.to_ascii_lowercase().contains(name))
}

fn is_cosmic_session() -> bool {
    current_desktop_contains("cosmic") || process_running("cosmic-comp")
}

fn process_running(process_name: &str) -> bool {
    let Ok(procs) = fs::read_dir("/proc") else {
        return false;
    };

    procs.flatten().any(|proc_entry| {
        let pid = proc_entry.file_name();
        if !pid.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            return false;
        }

        fs::read_to_string(proc_entry.path().join("comm"))
            .map(|comm| comm.trim().eq_ignore_ascii_case(process_name))
            .unwrap_or(false)
    })
}

fn process_uid(process_name: &str) -> Option<u32> {
    let procs = fs::read_dir("/proc").ok()?;

    for proc_entry in procs.flatten() {
        let pid = proc_entry.file_name();
        if !pid.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let comm = fs::read_to_string(proc_entry.path().join("comm")).unwrap_or_default();
        if comm.trim().eq_ignore_ascii_case(process_name) {
            return proc_entry.metadata().ok().map(|metadata| metadata.uid());
        }
    }

    None
}

fn username_for_uid(uid: u32) -> Option<String> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;

    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let username = fields.next()?;
        fields.next()?;
        let entry_uid = fields.next()?.parse::<u32>().ok()?;
        (entry_uid == uid).then(|| username.to_string())
    })
}

pub fn desktop_user() -> Option<String> {
    env::var("SUDO_USER")
        .ok()
        .or_else(|| process_uid("cosmic-comp").and_then(username_for_uid))
        .or_else(|| process_uid("Hyprland").and_then(username_for_uid))
}

fn set_hyprland_env(runtime_dir: &str, signature: &str) {
    env::set_var("XDG_RUNTIME_DIR", runtime_dir);
    env::set_var("HYPRLAND_INSTANCE_SIGNATURE", signature);

    // Also try to detect and set WAYLAND_DISPLAY
    if let Some(display) = detect_wayland_display(runtime_dir) {
        env::set_var("WAYLAND_DISPLAY", display);
    }
}

fn detect_wayland_display(runtime_dir: &str) -> Option<String> {
    // Look for wayland-* socket files
    let runtime_path = Path::new(runtime_dir);
    if let Ok(entries) = fs::read_dir(runtime_path) {
        for entry in entries.flatten() {
            let filename = entry.file_name();
            let name = filename.to_string_lossy();
            if name.starts_with("wayland-") && !name.ends_with(".lock") {
                // Found a wayland socket, return it
                return Some(name.to_string());
            }
        }
    }
    None
}

fn hyprland_socket_from_env() -> Option<String> {
    let runtime_dir = env::var("XDG_RUNTIME_DIR").ok()?;
    let signature = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let socket = Path::new(&runtime_dir)
        .join("hypr")
        .join(signature)
        .join(".socket.sock");

    if socket.exists() {
        Some(socket.to_string_lossy().to_string())
    } else {
        None
    }
}

fn find_hyprland_signature_in_runtime(runtime_dir: &str) -> Option<String> {
    let hypr_dir = Path::new(runtime_dir).join("hypr");
    let instances = fs::read_dir(hypr_dir).ok()?;

    for instance in instances.flatten() {
        let instance_path = instance.path();
        if instance_path.join(".socket.sock").exists()
            || instance_path.join(".socket2.sock").exists()
        {
            return Some(instance.file_name().to_string_lossy().to_string());
        }
    }

    None
}

fn find_hyprland_env_in_proc() -> Option<(String, String)> {
    let procs = fs::read_dir("/proc").ok()?;

    for proc_entry in procs.flatten() {
        let pid = proc_entry.file_name();
        if !pid.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let comm = fs::read_to_string(proc_entry.path().join("comm")).unwrap_or_default();
        if !comm.trim().eq_ignore_ascii_case("Hyprland") {
            continue;
        }

        let environ = fs::read(proc_entry.path().join("environ")).ok()?;
        let mut runtime_dir = None;
        let mut signature = None;
        let mut wayland_display = None;

        for entry in environ.split(|b| *b == 0) {
            if let Some(value) = entry.strip_prefix(b"XDG_RUNTIME_DIR=") {
                runtime_dir = String::from_utf8(value.to_vec()).ok();
            } else if let Some(value) = entry.strip_prefix(b"HYPRLAND_INSTANCE_SIGNATURE=") {
                signature = String::from_utf8(value.to_vec()).ok();
            } else if let Some(value) = entry.strip_prefix(b"WAYLAND_DISPLAY=") {
                wayland_display = String::from_utf8(value.to_vec()).ok();
            }
        }

        if let (Some(runtime_dir), Some(signature)) = (runtime_dir, signature) {
            // Also set WAYLAND_DISPLAY if found
            if let Some(display) = wayland_display {
                env::set_var("WAYLAND_DISPLAY", display);
            }
            return Some((runtime_dir, signature));
        }
    }

    None
}

pub struct SystemInfo {
    cpu_usage: f32,
    memory_usage: u64,
    memory_total: u64,
    active_window: String,
    active_workspace: String,
    now_playing: String,
    show_now_playing: bool,
    desktop_info_available: bool,
    last_update: Instant,
}

impl SystemInfo {
    fn new() -> Self {
        SystemInfo {
            cpu_usage: 0.0,
            memory_usage: 0,
            memory_total: 0,
            active_window: String::new(),
            active_workspace: String::new(),
            now_playing: String::new(),
            show_now_playing: true,
            desktop_info_available: false,
            last_update: Instant::now(),
        }
    }

    fn update_system_stats(&mut self, cpu_usage: f32, memory_usage: u64, memory_total: u64) {
        self.cpu_usage = cpu_usage;
        self.memory_usage = memory_usage;
        self.memory_total = memory_total;
        self.last_update = Instant::now();
    }

    fn update_desktop_info(
        &mut self,
        active_window: String,
        active_workspace: String,
        desktop_info_available: bool,
    ) {
        self.active_window = active_window;
        self.active_workspace = active_workspace;
        self.desktop_info_available = desktop_info_available;
        self.last_update = Instant::now();
    }

    fn update_now_playing(&mut self, now_playing: String) {
        if self.now_playing.is_empty() && !now_playing.is_empty() {
            self.show_now_playing = true;
        }
        self.now_playing = now_playing;
        self.last_update = Instant::now();
    }

    fn titlebar_text(&self) -> String {
        if self.show_now_playing && !self.now_playing.is_empty() {
            self.now_playing.clone()
        } else {
            self.active_window.clone()
        }
    }

    fn toggle_titlebar_text(&mut self) -> bool {
        if self.now_playing.is_empty() {
            return false;
        }
        self.show_now_playing = !self.show_now_playing;
        true
    }
}

fn current_now_playing() -> String {
    let status = Command::new("playerctl")
        .arg("-s")
        .arg("status")
        .output()
        .ok()
        .and_then(|output| output.status.success().then_some(output.stdout))
        .and_then(|stdout| String::from_utf8(stdout).ok())
        .unwrap_or_default();
    if status.trim() != "Playing" {
        return String::new();
    }

    let metadata = Command::new("playerctl")
        .arg("-s")
        .arg("metadata")
        .arg("--format")
        .arg("{{artist}} - {{title}}")
        .output()
        .ok()
        .and_then(|output| output.status.success().then_some(output.stdout))
        .and_then(|stdout| String::from_utf8(stdout).ok())
        .unwrap_or_default();
    clean_now_playing(metadata.trim())
}

fn clean_now_playing(text: &str) -> String {
    text.trim_matches(|c: char| c.is_whitespace() || c == '-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_playing_text_trims_empty_artist_separator() {
        assert_eq!(clean_now_playing("Artist - Track"), "Artist - Track");
        assert_eq!(clean_now_playing(" - Track"), "Track");
        assert_eq!(clean_now_playing("Artist - "), "Artist");
    }

    #[test]
    fn titlebar_text_toggles_between_now_playing_and_window() {
        let mut info = SystemInfo::new();
        info.update_desktop_info("Window".to_string(), String::new(), true);
        assert_eq!(info.titlebar_text(), "Window");
        assert!(!info.toggle_titlebar_text());

        info.update_now_playing("Artist - Track".to_string());
        assert_eq!(info.titlebar_text(), "Artist - Track");
        assert!(info.toggle_titlebar_text());
        assert_eq!(info.titlebar_text(), "Window");
    }

    #[test]
    fn new_now_playing_replaces_window_after_empty_state() {
        let mut info = SystemInfo::new();
        info.update_desktop_info("Window".to_string(), String::new(), true);
        info.update_now_playing("Track".to_string());
        assert!(info.toggle_titlebar_text());
        assert_eq!(info.titlebar_text(), "Window");

        info.update_now_playing(String::new());
        info.update_now_playing("New Track".to_string());
        assert_eq!(info.titlebar_text(), "New Track");
    }
}

fn get_hyprland_info() -> (String, String, bool) {
    let Some(socket) = setup_hyprland_env() else {
        return (String::new(), String::new(), false);
    };

    let active_window = match hyprctl_json(&socket, "j/activewindow") {
        Ok(value) => value
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|title| !title.trim().is_empty())
            .or_else(|| value.get("class").and_then(|v| v.as_str()))
            .map(|title| title.trim().to_string())
            .unwrap_or_else(|| "Desktop".to_string()),
        Err(err) => {
            println!("Hyprland active-window query failed: {}", err);
            return (String::new(), String::new(), false);
        }
    };

    let active_workspace = match hyprctl_json(&socket, "j/activeworkspace") {
        Ok(value) => value
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string())
            .or_else(|| value.get("id").map(|id| id.to_string()))
            .unwrap_or_default(),
        Err(err) => {
            println!("Hyprland workspace query failed: {}", err);
            return (active_window, String::new(), false);
        }
    };

    (active_window, active_workspace, true)
}

fn setup_cosmic_env() {
    if env::var("XDG_RUNTIME_DIR").is_ok() && env::var("WAYLAND_DISPLAY").is_ok() {
        return;
    }

    if let Ok(sudo_uid) = env::var("SUDO_UID") {
        let runtime_dir = format!("/run/user/{sudo_uid}");
        if let Some(display) = detect_wayland_display(&runtime_dir) {
            env::set_var("XDG_RUNTIME_DIR", runtime_dir);
            env::set_var("WAYLAND_DISPLAY", display);
            return;
        }
    }

    if let Some(uid) = process_uid("cosmic-comp") {
        let runtime_dir = format!("/run/user/{uid}");
        if let Some(display) = detect_wayland_display(&runtime_dir) {
            env::set_var("XDG_RUNTIME_DIR", runtime_dir);
            env::set_var("WAYLAND_DISPLAY", display);
            return;
        }
    }

    if let Some((runtime_dir, wayland_display)) = find_wayland_env_in_proc("cosmic-comp") {
        env::set_var("XDG_RUNTIME_DIR", runtime_dir);
        env::set_var("WAYLAND_DISPLAY", wayland_display);
    }
}

fn find_wayland_env_in_proc(process_name: &str) -> Option<(String, String)> {
    let procs = fs::read_dir("/proc").ok()?;

    for proc_entry in procs.flatten() {
        let pid = proc_entry.file_name();
        if !pid.to_string_lossy().chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let comm = fs::read_to_string(proc_entry.path().join("comm")).unwrap_or_default();
        if !comm.trim().eq_ignore_ascii_case(process_name) {
            continue;
        }

        let environ = fs::read(proc_entry.path().join("environ")).ok()?;
        let mut runtime_dir = None;
        let mut wayland_display = None;

        for entry in environ.split(|b| *b == 0) {
            if let Some(value) = entry.strip_prefix(b"XDG_RUNTIME_DIR=") {
                runtime_dir = String::from_utf8(value.to_vec()).ok();
            } else if let Some(value) = entry.strip_prefix(b"WAYLAND_DISPLAY=") {
                wayland_display = String::from_utf8(value.to_vec()).ok();
            }
        }

        if let (Some(runtime_dir), Some(wayland_display)) = (runtime_dir, wayland_display) {
            return Some((runtime_dir, wayland_display));
        }
    }

    None
}

struct CosmicAppData {
    info: Arc<Mutex<SystemInfo>>,
    output_state: OutputState,
    registry_state: RegistryState,
    toplevel_info_state: ToplevelInfoState,
    workspace_state: WorkspaceState,
}

impl CosmicAppData {
    fn publish(&mut self) {
        let active_workspace = self
            .workspace_state
            .workspaces()
            .find(|workspace| format!("{:?}", workspace.state).contains("Active"))
            .map(|workspace| {
                if workspace.name.is_empty() {
                    workspace
                        .id
                        .clone()
                        .unwrap_or_else(|| workspace.handle.id().to_string())
                } else {
                    workspace.name.clone()
                }
            })
            .unwrap_or_default();

        let active_workspace_handle = self
            .workspace_state
            .workspaces()
            .find(|workspace| format!("{:?}", workspace.state).contains("Active"))
            .map(|workspace| workspace.handle.clone());

        let active_toplevel = self
            .toplevel_info_state
            .toplevels()
            .find(|toplevel| format!("{:?}", toplevel.state).contains("Activated"))
            .or_else(|| {
                active_workspace_handle.as_ref().and_then(|workspace| {
                    self.toplevel_info_state
                        .toplevels()
                        .find(|toplevel| toplevel.workspace.contains(workspace))
                })
            });

        let active_window = active_toplevel
            .map(|toplevel| {
                if !toplevel.title.trim().is_empty() {
                    toplevel.title.trim().to_string()
                } else if !toplevel.app_id.trim().is_empty() {
                    toplevel.app_id.trim().to_string()
                } else {
                    "Desktop".to_string()
                }
            })
            .unwrap_or_else(|| "Desktop".to_string());

        self.info
            .lock()
            .unwrap()
            .update_desktop_info(active_window, active_workspace, true);
    }
}

impl ProvidesRegistryState for CosmicAppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    cosmic_client_toolkit::sctk::registry_handlers!(OutputState);
}

impl OutputHandler for CosmicAppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ToplevelInfoHandler for CosmicAppData {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }

    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        self.publish();
    }

    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        self.publish();
    }

    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) {
        self.publish();
    }

    fn info_done(&mut self, _: &Connection, _: &QueueHandle<Self>) {
        self.publish();
    }
}

impl WorkspaceHandler for CosmicAppData {
    fn workspace_state(&mut self) -> &mut WorkspaceState {
        &mut self.workspace_state
    }

    fn done(&mut self) {
        self.publish();
    }
}

fn run_cosmic_info_loop(
    info: Arc<Mutex<SystemInfo>>,
    desktop_probe_tx: &mut Option<mpsc::Sender<()>>,
) -> anyhow::Result<()> {
    setup_cosmic_env();

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();
    let registry_state = RegistryState::new(&globals);
    let Some(toplevel_info_state) = ToplevelInfoState::try_new(&registry_state, &qh) else {
        return Err(anyhow::anyhow!(
            "Cosmic toplevel info protocol is unavailable"
        ));
    };

    let mut app_data = CosmicAppData {
        info,
        output_state: OutputState::new(&globals, &qh),
        workspace_state: WorkspaceState::new(&registry_state, &qh),
        toplevel_info_state,
        registry_state,
    };

    notify_desktop_probe(desktop_probe_tx);

    loop {
        event_queue.blocking_dispatch(&mut app_data)?;
    }
}

fn notify_desktop_probe(notifier: &mut Option<mpsc::Sender<()>>) {
    if let Some(notifier) = notifier.take() {
        let _ = notifier.send(());
    }
}

cosmic_client_toolkit::sctk::delegate_output!(CosmicAppData);
cosmic_client_toolkit::sctk::delegate_registry!(CosmicAppData);
cosmic_client_toolkit::delegate_toplevel_info!(CosmicAppData);
cosmic_client_toolkit::delegate_workspace!(CosmicAppData);

fn hyprctl_json(socket: &str, command: &str) -> anyhow::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(command.as_bytes())?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    Ok(serde_json::from_str(&response)?)
}

pub struct SystemInfoManager {
    info: Arc<Mutex<SystemInfo>>,
}

impl SystemInfoManager {
    pub fn new() -> Self {
        let info = Arc::new(Mutex::new(SystemInfo::new()));
        let info_clone = Arc::clone(&info);
        let desktop_info = Arc::clone(&info);
        let (desktop_probe_tx, desktop_probe_rx) = mpsc::channel();

        // Spawn background thread to update system stats periodically
        thread::spawn(move || {
            let mut sys = System::new_all();
            let mut last_system_refresh = Instant::now() - SYSTEM_STATS_REFRESH;
            thread::sleep(Duration::from_millis(200));

            loop {
                if last_system_refresh.elapsed() >= SYSTEM_STATS_REFRESH {
                    sys.refresh_cpu_all();
                    sys.refresh_memory();
                    info_clone.lock().unwrap().update_system_stats(
                        sys.global_cpu_usage(),
                        sys.used_memory(),
                        sys.total_memory(),
                    );
                    last_system_refresh = Instant::now();
                }

                thread::sleep(Duration::from_millis(250));
            }
        });

        let now_playing_info = Arc::clone(&info);
        thread::spawn(move || loop {
            now_playing_info
                .lock()
                .unwrap()
                .update_now_playing(current_now_playing());
            thread::sleep(NOW_PLAYING_REFRESH);
        });

        thread::spawn(move || {
            let prefer_cosmic = is_cosmic_session();
            let mut last_cosmic_error = None;
            let mut desktop_probe_tx = Some(desktop_probe_tx);

            loop {
                if prefer_cosmic {
                    desktop_info.lock().unwrap().update_desktop_info(
                        String::new(),
                        String::new(),
                        false,
                    );

                    if let Err(err) =
                        run_cosmic_info_loop(Arc::clone(&desktop_info), &mut desktop_probe_tx)
                    {
                        notify_desktop_probe(&mut desktop_probe_tx);
                        let err = err.to_string();
                        if last_cosmic_error.as_deref() != Some(err.as_str()) {
                            println!("Cosmic desktop info unavailable: {err}");
                            last_cosmic_error = Some(err);
                        }
                        thread::sleep(Duration::from_secs(2));
                    }

                    continue;
                }

                loop {
                    let (active_window, active_workspace, hyprland_available) = get_hyprland_info();
                    if !hyprland_available {
                        break;
                    }

                    desktop_info.lock().unwrap().update_desktop_info(
                        active_window,
                        active_workspace,
                        hyprland_available,
                    );
                    notify_desktop_probe(&mut desktop_probe_tx);

                    thread::sleep(DESKTOP_INFO_REFRESH);
                }

                desktop_info.lock().unwrap().update_desktop_info(
                    String::new(),
                    String::new(),
                    false,
                );

                if let Err(err) =
                    run_cosmic_info_loop(Arc::clone(&desktop_info), &mut desktop_probe_tx)
                {
                    notify_desktop_probe(&mut desktop_probe_tx);
                    let err = err.to_string();
                    if last_cosmic_error.as_deref() != Some(err.as_str()) {
                        println!("Cosmic desktop info unavailable: {err}");
                        last_cosmic_error = Some(err);
                    }
                    thread::sleep(Duration::from_secs(2));
                }
            }
        });

        let _ = desktop_probe_rx.recv_timeout(Duration::from_secs(2));

        SystemInfoManager { info }
    }

    pub fn get_cpu_usage(&self) -> f32 {
        self.info.lock().unwrap().cpu_usage
    }

    pub fn get_memory_usage(&self) -> (u64, u64) {
        let info = self.info.lock().unwrap();
        (info.memory_usage, info.memory_total)
    }

    pub fn get_titlebar_text(&self) -> (String, bool) {
        let info = self.info.lock().unwrap();
        let now_playing = info.show_now_playing && !info.now_playing.is_empty();
        (info.titlebar_text(), now_playing)
    }

    pub fn toggle_titlebar_text(&self) -> bool {
        self.info.lock().unwrap().toggle_titlebar_text()
    }

    pub fn get_active_workspace(&self) -> String {
        self.info.lock().unwrap().active_workspace.clone()
    }

    pub fn desktop_info_available(&self) -> bool {
        self.info.lock().unwrap().desktop_info_available
    }
}
