use ::input::LibinputInterface;
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{input_event, timeval};
use libc::{O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use std::{
    env,
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, io::OwnedFd},
    },
    path::Path,
};

use crate::display::DrmBackend;
use crate::t2_usb::{is_t2_macbook, T2TouchBar};
use crate::{DRM_OPEN_ATTEMPTS, DRM_OPEN_RETRY_DELAY, T2_USB_RECOVERY_ENV};

pub(crate) struct Interface;

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

pub(crate) fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32)
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

pub(crate) fn toggle_keys<F>(uinput: &mut UInputHandle<F>, codes: &Vec<Key>, value: i32)
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

pub(crate) fn open_drm_backend(report_final_failure: bool) -> Option<DrmBackend> {
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

pub(crate) fn try_touchbar_usb_recovery() {
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
