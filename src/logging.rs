use std::sync::atomic::{AtomicBool, Ordering};

// Global logging switch, controlled by the `Logging` config key. Defaults to on
// so that early-boot and config-load diagnostics are visible before the config
// (which may turn logging off) has been read; load_config() then applies the
// configured value on every load and reload.
static LOGGING_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_logging_enabled(enabled: bool) {
    LOGGING_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn logging_enabled() -> bool {
    LOGGING_ENABLED.load(Ordering::Relaxed)
}

// Drop-in replacement for eprintln! that only emits when logging is enabled, so
// the daemon stays quiet in the journal unless `Logging = true` is configured.
macro_rules! log_line {
    ($($arg:tt)*) => {
        if $crate::logging::logging_enabled() {
            eprintln!($($arg)*);
        }
    };
}
