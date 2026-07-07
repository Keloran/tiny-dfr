# tiny-dfr
The most basic dynamic function row daemon possible


## Dependencies
cairo, libinput, freetype, fontconfig, librsvg 2.59 or later, uinput enabled in kernel config

## Running Without Root

tiny-dfr needs access to the Touch Bar DRM/input devices, `/dev/uinput`, and
backlight brightness files under `/sys/class/backlight`. The packaged udev rules
in `etc/udev/rules.d` grant those devices to the `input` and `video` groups. The
keyboard-backlight slider additionally needs write access to the keyboard LED,
granted by `99-touchbar-leds.rules`.

Install the packaged rules and service file:

```sh
sudo install -Dm644 etc/udev/rules.d/99-touchbar-tiny-dfr.rules /etc/udev/rules.d/99-touchbar-tiny-dfr.rules
sudo install -Dm644 etc/udev/rules.d/99-touchbar-seat.rules /etc/udev/rules.d/99-touchbar-seat.rules
sudo install -Dm644 etc/udev/rules.d/99-touchbar-leds.rules /etc/udev/rules.d/99-touchbar-leds.rules
sudo install -Dm644 etc/udev/rules.d/99-uinput.rules /etc/udev/rules.d/99-uinput.rules
sudo install -Dm644 etc/systemd/system/tiny-dfr.service /etc/systemd/system/tiny-dfr.service
```

The weather buttons fetch from wttr.in, which needs network access. If your
service file hardens `RestrictAddressFamilies` (the packaged one does), install
the drop-in that re-enables `AF_INET`/`AF_INET6`. It merges with the main unit,
so it is safe even with a customized service file:

```sh
sudo install -Dm644 etc/systemd/system/tiny-dfr.service.d/network.conf /etc/systemd/system/tiny-dfr.service.d/network.conf
```

The notification buttons use `makoctl`, so they require the mako notification
daemon.

The battery button's power-profile toggle runs `powerprofilesctl` from the
service scope, which polkit does not treat as part of your active login
session, so the switch is denied by default. Install the packaged polkit rule
to authorize `wheel` members to switch profiles (no daemon restart needed):

```sh
sudo install -Dm644 etc/polkit-1/rules.d/49-tiny-dfr-power-profiles.rules /etc/polkit-1/rules.d/49-tiny-dfr-power-profiles.rules
```

Add your user to the required groups, then log out and back in:

```sh
sudo usermod -aG input,video "$USER"
```

Use a local systemd override to run the daemon as your user:

```sh
sudo systemctl edit tiny-dfr.service
```

Add:

```ini
[Service]
User=your-user
Group=your-user
SupplementaryGroups=input video
```

Set this in `/etc/tiny-dfr/config.toml` or your user config when running as your
own user:

```toml
DropPrivileges = false
```

Reload udev and systemd, then restart tiny-dfr:

```sh
sudo modprobe uinput
sudo udevadm control --reload-rules
sudo udevadm trigger
sudo systemctl daemon-reload
sudo systemctl restart tiny-dfr.service
```

If startup still fails, check the effective permissions:

```sh
ls -l /dev/uinput /dev/dri/card*
ls -l /sys/class/backlight/*/brightness
journalctl -u tiny-dfr.service -b
```

## Configuration

tiny-dfr loads configuration in this order, with later files overriding earlier
ones:

1. `/usr/share/tiny-dfr/config.toml`
2. `/etc/tiny-dfr/config.toml`
3. `$XDG_CONFIG_HOME/tiny-dfr/config.toml`, or `~/.config/tiny-dfr/config.toml`

When running tiny-dfr as your own user, prefer the user config path for personal
customisation. The path is resolved from the service user's environment, so the
systemd service should run as that same user. For example:

```sh
mkdir -p ~/.config/tiny-dfr
cp /etc/tiny-dfr/config.toml ~/.config/tiny-dfr/config.toml
```

Changes to `/etc/tiny-dfr/config.toml` or the user config file are reloaded while
tiny-dfr is running.

## Simulator (no hardware)

To run tiny-dfr on a machine without a Touch Bar, build with the `simulator`
feature and pass `--simulate`. It renders the configured layers into a normal
window and maps mouse clicks to touches, reusing the real drawing and
hit-testing code.

```sh
cargo run --features simulator -- --simulate
```

- Assets and config are loaded from the checkout's `share/tiny-dfr` (or from an
  installed `/usr/share/tiny-dfr`); set `TINY_DFR_SHARE_DIR` to override.
- Window size defaults to `1710x50`; set `TINY_DFR_SIM_SIZE=WxH` to change it.
  The buttons always stretch to fill the current window size, so resizing works.
- On a tiling WM the window gets tiled instead of shown as a thin strip. On
  Hyprland, float it to keep the Touch Bar shape:
  ```
  windowrulev2 = float, title:^(tiny-dfr simulator)$
  windowrulev2 = size 1710 50, title:^(tiny-dfr simulator)$
  windowrulev2 = center, title:^(tiny-dfr simulator)$
  ```
- Controls: click a button to tap it, number keys `1`-`9` switch layers, hold
  `LeftCtrl` for the FKeys layer, `Esc` quits.
- If `/dev/uinput` is writable the simulated buttons emit real key events (a
  functional on-screen Touch Bar); otherwise it runs preview-only and just logs
  presses to the console.

## License

tiny-dfr is licensed under the MIT license, as included in the [LICENSE](LICENSE) file.

* Copyright The Asahi Linux Contributors

Please see the Git history for authorship information.

tiny-dfr embeds Google's [material-design-icons](https://github.com/google/material-design-icons)
which are licensed under [Apache License Version 2.0](LICENSE.material)
Some icons are derivatives of material-icons, with edits made by kekrby.
