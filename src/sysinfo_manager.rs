use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::System;

const SYSTEM_STATS_REFRESH: Duration = Duration::from_secs(2);
const HYPRLAND_REFRESH: Duration = Duration::from_millis(250);

/// Configure the Hyprland IPC environment when tiny-dfr is started through sudo.
fn setup_hyprland_env() -> Option<String> {
    if env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() && env::var("XDG_RUNTIME_DIR").is_ok() {
        return hyprland_socket_from_env();
    }

    if let Ok(sudo_uid) = env::var("SUDO_UID") {
        let runtime_dir = format!("/run/user/{sudo_uid}");
        if let Some(signature) = find_hyprland_signature_in_runtime(&runtime_dir) {
            set_hyprland_env(&runtime_dir, &signature);
            println!("Hyprland detected via SUDO_UID runtime dir: {runtime_dir}");
            return hyprland_socket_from_env();
        }
    }

    if let Some((runtime_dir, signature)) = find_hyprland_env_in_proc() {
        set_hyprland_env(&runtime_dir, &signature);
        println!("Hyprland detected via process environment: {runtime_dir}");
        return hyprland_socket_from_env();
    }

    if let Ok(runtime_users) = fs::read_dir("/run/user") {
        for runtime_user in runtime_users.flatten() {
            let runtime_dir = runtime_user.path();
            if let Some(signature) = find_hyprland_signature_in_runtime(runtime_dir.to_string_lossy().as_ref()) {
                set_hyprland_env(runtime_dir.to_string_lossy().as_ref(), &signature);
                println!("Hyprland detected via runtime dir: {}", runtime_dir.display());
                return hyprland_socket_from_env();
            }
        }
    }

    if let Ok(xdg_runtime) = env::var("XDG_RUNTIME_DIR") {
        if let Some(signature) = find_hyprland_signature_in_runtime(&xdg_runtime) {
            set_hyprland_env(&xdg_runtime, &signature);
            return hyprland_socket_from_env();
        }
    }

    None
}

fn set_hyprland_env(runtime_dir: &str, signature: &str) {
    env::set_var("XDG_RUNTIME_DIR", runtime_dir);
    env::set_var("HYPRLAND_INSTANCE_SIGNATURE", signature);
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
        if instance_path.join(".socket.sock").exists() || instance_path.join(".socket2.sock").exists() {
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

        for entry in environ.split(|b| *b == 0) {
            if let Some(value) = entry.strip_prefix(b"XDG_RUNTIME_DIR=") {
                runtime_dir = String::from_utf8(value.to_vec()).ok();
            } else if let Some(value) = entry.strip_prefix(b"HYPRLAND_INSTANCE_SIGNATURE=") {
                signature = String::from_utf8(value.to_vec()).ok();
            }
        }

        if let (Some(runtime_dir), Some(signature)) = (runtime_dir, signature) {
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
    hyprland_available: bool,
    last_update: Instant,
}

impl SystemInfo {
    fn new() -> Self {
        let hyprland_available = setup_hyprland_env().is_some();

        SystemInfo {
            cpu_usage: 0.0,
            memory_usage: 0,
            memory_total: 0,
            active_window: String::new(),
            active_workspace: String::new(),
            hyprland_available,
            last_update: Instant::now(),
        }
    }

    fn update(
        &mut self,
        cpu_usage: f32,
        memory_usage: u64,
        memory_total: u64,
        active_window: String,
        active_workspace: String,
        hyprland_available: bool,
    ) {
        self.cpu_usage = cpu_usage;
        self.memory_usage = memory_usage;
        self.memory_total = memory_total;
        self.active_window = active_window;
        self.active_workspace = active_workspace;
        self.hyprland_available = hyprland_available;
        self.last_update = Instant::now();
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
            .map(|title| title.trim().chars().take(25).collect())
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

        // Spawn background thread to update system info periodically
        thread::spawn(move || {
            let mut sys = System::new_all();
            let mut last_system_refresh = Instant::now() - SYSTEM_STATS_REFRESH;
            let mut cpu_usage = 0.0;
            let mut memory_usage = 0;
            let mut memory_total = 0;
            thread::sleep(Duration::from_millis(200));

            loop {
                if last_system_refresh.elapsed() >= SYSTEM_STATS_REFRESH {
                    sys.refresh_cpu_all();
                    sys.refresh_memory();
                    cpu_usage = sys.global_cpu_usage();
                    memory_usage = sys.used_memory();
                    memory_total = sys.total_memory();
                    last_system_refresh = Instant::now();
                }

                let (active_window, active_workspace, hyprland_available) = get_hyprland_info();

                info_clone.lock().unwrap().update(
                    cpu_usage,
                    memory_usage,
                    memory_total,
                    active_window,
                    active_workspace,
                    hyprland_available,
                );

                thread::sleep(HYPRLAND_REFRESH);
            }
        });

        SystemInfoManager { info }
    }

    pub fn get_cpu_usage(&self) -> f32 {
        self.info.lock().unwrap().cpu_usage
    }

    pub fn get_memory_usage(&self) -> (u64, u64) {
        let info = self.info.lock().unwrap();
        (info.memory_usage, info.memory_total)
    }

    pub fn get_active_window(&self) -> String {
        self.info.lock().unwrap().active_window.clone()
    }

    pub fn get_active_workspace(&self) -> String {
        self.info.lock().unwrap().active_workspace.clone()
    }

    pub fn hyprland_available(&self) -> bool {
        self.info.lock().unwrap().hyprland_available
    }

    pub fn should_refresh(&self) -> bool {
        self.info.lock().unwrap().last_update.elapsed() > Duration::from_secs(1)
    }
}
