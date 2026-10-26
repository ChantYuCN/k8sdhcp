# pxe-installer (Rust)

One static binary that installs and manages a **UEFI PXE server** (dnsmasq DHCP/DNS/TFTP + Apache for the
ISO) and unattended-installs Ubuntu 25.04. Hosts that already finished installing are skipped; MAC -> IP
bindings supported. The CLI is std-only; the optional GUI uses the `gtk4` crate.

## Build (Linux or WSL with Rust)
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
sudo apt install libgtk-4-dev build-essential pkg-config curl  # GUI only
./build.sh               # tests, static CLI, and the GTK 4 GUI
./build.sh --cli-only    # CLI only (std only, no dependencies)
```
Outputs: `target/x86_64-unknown-linux-musl/release/pxe-installer` (static CLI) and
`target/release/pxe-installer-gui` (GUI; keep it in the same folder as `pxe-installer`).

## GUI
```
./target/release/pxe-installer-gui
```
Install tab: network/DHCP, target credentials, MAC->IP bindings (one `mac,ip[,hostname]` per line) -> **Install**.
It validates with the same code as the CLI, writes a mode-600 temp config, runs
`pkexec pxe-installer install --config ...` and streams the output with a progress bar; the temp file is
deleted afterwards. Manage tab: host status, installed hosts, bindings, reinstall / mark installed / bind / unbind.

## Use
```
pxe-installer print-config > pxe.conf       # edit it (set TARGET_PASSWORD!)
sudo ./pxe-installer install --config pxe.conf --dry-run   # show everything it would do/write
sudo ./pxe-installer install --config pxe.conf

sudo pxe-ctl status                          # per-host: installed / pending / waiting
sudo pxe-ctl list | reinstall <mac> | done <mac>
sudo pxe-ctl binds | bind <mac> <ip> [name] | unbind <mac|ip>
```
Flags override the config file (`--iface`, `--server-ip`, `--host mac,ip,name`, `--static-ip` ...); see
`pxe-installer --help`. The password comes only from the config file or `PXE_TARGET_PASSWORD`.

## How duplicate installs are prevented
The autoinstall's last step calls `http://SERVER/cgi-bin/pxe-done?mac=..`, which runs
`pxe-installer cgi-done` and creates `/var/lib/tftpboot/done/<mac>`. `grub.cfg` exits to the local disk
when that file exists. A failed install has no marker and retries.

## What `install` does
apt packages -> (optional netplan static IP) -> free port 53 -> `/etc/dnsmasq.conf` + MAC bindings ->
netboot image to TFTP root + `grub.cfg` -> copies itself to `/usr/local/sbin/pxe-installer`, writes
`pxe-ctl` wrapper -> ISO, autoinstall `user-data`, CGI and Apache conf -> start services, ufw rules, checks.

## Layout
```
src/lib.rs       shared library (used by both binaries)
src/gui.rs      GTK 4 app (feature `gui`)
src/config.rs   settings, config-file/flag parsing, validation
src/render.rs   templates for dnsmasq.conf, grub.cfg, user-data, netplan, apache conf
src/install.rs  the install steps (honours --dry-run)
src/ctl.rs      pxe-ctl commands, status, CGI callback
src/util.rs     command/file helpers
```
(`../pxe-installer-gui` is the earlier Python prototype of the GUI and is superseded by `src/gui.rs`.)
