use anyhow::{anyhow, Result};
use std::fs::{self, File};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::thread;
use std::time::Duration;

const USBDEVFS_RESET: u64 = 21780;

/// T2 MacBook Touch Bar USB device information
pub struct T2TouchBar {
    /// Typical USB path for T2 Touch Bar
    /// Format: /sys/bus/usb/devices/{hub}-{port}
    usb_device_path: String,
}

impl T2TouchBar {
    /// Create a new T2TouchBar instance
    pub fn new() -> Self {
        T2TouchBar {
            // Common paths for T2 Touch Bar - we'll search for it
            usb_device_path: String::new(),
        }
    }

    /// Find the T2 Touch Bar USB device
    /// The Touch Bar typically appears as USB device 05ac:8302
    pub fn find_device(&mut self) -> Result<()> {
        let usb_devices_path = Path::new("/sys/bus/usb/devices");
        
        if !usb_devices_path.exists() {
            return Err(anyhow!("USB devices path does not exist"));
        }

        for entry in fs::read_dir(usb_devices_path)? {
            let entry = entry?;
            let path = entry.path();
            
            // Check if this is a USB device directory (not a USB interface)
            if !path.is_dir() {
                continue;
            }

            // Read vendor and product IDs
            let idvendor_path = path.join("idVendor");
            let idproduct_path = path.join("idProduct");
            
            if !idvendor_path.exists() || !idproduct_path.exists() {
                continue;
            }

            let vendor = fs::read_to_string(&idvendor_path)
                .unwrap_or_default()
                .trim()
                .to_string();
            let product = fs::read_to_string(&idproduct_path)
                .unwrap_or_default()
                .trim()
                .to_string();

            // Check if this is the Touch Bar (Apple 05ac:8302)
            if vendor == "05ac" && product == "8302" {
                self.usb_device_path = path.to_string_lossy().to_string();
                println!("Found T2 Touch Bar at: {}", self.usb_device_path);
                return Ok(());
            }
        }

        Err(anyhow!("T2 Touch Bar USB device not found (05ac:8302)"))
    }

    /// Reset the USB device to wake up the Touch Bar
    /// This is required after boot and after suspend/resume
    pub fn reset(&self) -> Result<()> {
        if self.usb_device_path.is_empty() {
            return Err(anyhow!("USB device path not set. Call find_device() first."));
        }

        let busnum_path = format!("{}/busnum", self.usb_device_path);
        let devnum_path = format!("{}/devnum", self.usb_device_path);

        let bus: u8 = fs::read_to_string(&busnum_path)?
            .trim()
            .parse()
            .map_err(|_| anyhow!("Failed to parse bus number"))?;

        let dev: u8 = fs::read_to_string(&devnum_path)?
            .trim()
            .parse()
            .map_err(|_| anyhow!("Failed to parse device number"))?;

        let usb_device = format!("/dev/bus/usb/{:03}/{:03}", bus, dev);
        println!("Resetting USB device: {}", usb_device);

        let file = File::options()
            .write(true)
            .open(&usb_device)
            .map_err(|e| anyhow!("Failed to open USB device {}: {}", usb_device, e))?;

        let fd = file.as_raw_fd();

        // Perform USB reset ioctl
        unsafe {
            let ret = libc::ioctl(fd, USBDEVFS_RESET, 0);
            if ret < 0 {
                return Err(anyhow!("USB reset ioctl failed: {}", 
                    std::io::Error::last_os_error()));
            }
        }

        println!("USB reset successful");
        Ok(())
    }

    /// Wait for the bce-vhci bus and Touch Bar to enumerate
    /// This can take up to 30 seconds after boot
    pub fn wait_for_enumeration(&mut self, timeout_secs: u64) -> Result<()> {
        let start = std::time::Instant::now();
        
        while start.elapsed().as_secs() < timeout_secs {
            if self.find_device().is_ok() {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(1));
            print!(".");
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }

        Err(anyhow!("Timeout waiting for T2 Touch Bar enumeration"))
    }

    /// Complete initialization sequence for T2 Touch Bar
    /// 1. Wait for enumeration
    /// 2. Reset USB device
    /// 3. Wait for kernel to bind appletbdrm
    pub fn initialize(&mut self) -> Result<()> {
        println!("Waiting for T2 Touch Bar enumeration...");
        self.wait_for_enumeration(30)?;

        println!("Performing USB reset...");
        // Store the original path before reset
        let original_path = self.usb_device_path.clone();
        
        if let Err(e) = self.reset() {
            eprintln!("Warning: USB reset failed: {}", e);
            eprintln!("Device may already be initialized, continuing anyway...");
        } else {
            println!("USB reset successful");
        }

        // Give the kernel extra time to re-enumerate and bind the driver
        // This is critical - the USB subsystem needs time to stabilize
        println!("Waiting for driver binding...");
        thread::sleep(Duration::from_secs(5));

        // Wait for DRM device to appear
        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < 15 {
            if Path::new("/dev/dri").exists() {
                // Check if any DRM card exists
                if let Ok(entries) = fs::read_dir("/dev/dri") {
                    for entry in entries.flatten() {
                        if entry.file_name().to_string_lossy().starts_with("card") {
                            println!("DRM device found: {}", entry.path().display());
                            // Give USB subsystem additional time to stabilize
                            // This prevents input devices from being in a stuck state
                            thread::sleep(Duration::from_secs(2));
                            return Ok(());
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(500));
        }

        eprintln!("Warning: DRM device did not appear after USB reset");
        eprintln!("Continuing anyway - you may need to manually restart tiny-dfr");
        Ok(()) // Don't fail - let the main loop try to continue
    }
}

/// Helper function to check if we're on a T2 MacBook
pub fn is_t2_macbook() -> bool {
    // Check for T2-specific indicators
    if let Ok(dmi) = fs::read_to_string("/sys/class/dmi/id/product_name") {
        let product = dmi.trim();
        // T2 MacBooks are typically MacBookPro15,x, MacBookPro16,x, MacBookAir8,x, MacBookAir9,x
        if product.starts_with("MacBookPro15")
            || product.starts_with("MacBookPro16")
            || product.starts_with("MacBookAir8")
            || product.starts_with("MacBookAir9")
        {
            return true;
        }
    }

    // Also check for the bce driver
    Path::new("/sys/bus/pci/drivers/bce").exists()
}
