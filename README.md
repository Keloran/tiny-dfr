# tiny-dfr
The most basic dynamic function row daemon possible


## Dependencies
cairo, libinput, freetype, fontconfig, librsvg 2.59 or later, uinput enabled in kernel config

## Running Without Root

tiny-dfr needs access to the Touch Bar DRM/input devices, `/dev/uinput`, and
backlight brightness files under `/sys/class/backlight`. The packaged udev rules
in `etc/udev/rules.d` grant those devices to the `input` and `video` groups.

Install the packaged rules and service file:

```sh
sudo install -Dm644 etc/udev/rules.d/99-touchbar-tiny-dfr.rules /etc/udev/rules.d/99-touchbar-tiny-dfr.rules
sudo install -Dm644 etc/udev/rules.d/99-touchbar-seat.rules /etc/udev/rules.d/99-touchbar-seat.rules
sudo install -Dm644 etc/udev/rules.d/99-uinput.rules /etc/udev/rules.d/99-uinput.rules
sudo install -Dm644 etc/systemd/system/tiny-dfr.service /etc/systemd/system/tiny-dfr.service
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

## License

tiny-dfr is licensed under the MIT license, as included in the [LICENSE](LICENSE) file.

* Copyright The Asahi Linux Contributors

Please see the Git history for authorship information.

tiny-dfr embeds Google's [material-design-icons](https://github.com/google/material-design-icons)
which are licensed under [Apache License Version 2.0](LICENSE.material)
Some icons are derivatives of material-icons, with edits made by kekrby.
