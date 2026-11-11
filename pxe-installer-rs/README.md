# pxe-installer (Rust)

One static binary that installs and manages a **UEFI PXE server** (dnsmasq DHCP/DNS/TFTP + Apache + NFS)
that **deploys a Clonezilla disk image** to target hosts. Targets PXE boot Clonezilla Live, restore the
image onto their disk, get their own hostname / machine-id / SSH host keys, and report back. Hosts that
already finished are skipped; MAC -> IP bindings supported. The CLI is std-only; the optional GUI uses the
`gtk4` crate.

```text
PXE server                                   target (UEFI, PXE first)
  /home/partimag/<image>  --NFS (ro)-->       GRUB -> Clonezilla Live (kernel+initrd via TFTP,
  clonezilla/filesystem.squashfs --HTTP-->      live filesystem via HTTP)
  cgi-bin/pxe-seed/<mac>/post.sh --HTTP-->    ocs-sr restoredisk <image> <disk>
  cgi-bin/pxe-done?mac=...      <--HTTP--     post.sh: hostname, machine-id, SSH keys -> pxe-done -> reboot
```

## Build (Linux or WSL with Rust)

Install Rust SDK.
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Compile and build the binary.
```
sudo apt install libgtk-4-dev build-essential pkg-config curl  # GUI only
./build.sh               # tests, static CLI, and the GTK 4 GUI
./build.sh --cli-only    # CLI only (std only, no dependencies)
```
Outputs: `target/x86_64-unknown-linux-musl/release/pxe-installer` (static CLI) and
`target/release/pxe-installer-gui` (GUI; keep it in the same folder as `pxe-installer`).

## What it does NOT do
The installer installs/configures dnsmasq, Apache and NFS. It never downloads or creates the boot files,
Clonezilla Live or the image - you place them yourself (`start` prints these steps and notes what is missing):
```
# 1. UEFI boot files from the Ubuntu netboot tarball (amd64/*) into the TFTP root
sudo cp -r amd64/* /var/lib/tftpboot/
# 2. Clonezilla Live (clonezilla-live-*-amd64.zip, folder live/)
sudo cp live/vmlinuz live/initrd.img        /var/lib/tftpboot/clonezilla/
sudo cp live/filesystem.squashfs            /var/www/html/clonezilla/
sudo pxe-ctl grub                           # re-apply our grub.cfg (the tarball ships its own) + permissions
# 3. the image: made with ocs-sr on the source disk (see clonezilla-pxe-clone-guide.md), or copied here
sudo ocs-sr -q2 -c -j2 -z9p -sfsck -senc -p true savedisk cri-golden nvme0n2   # -> /home/partimag/cri-golden/
sudo pxe-ctl nfs                            # (re-)export /home/partimag read-only over NFS
```
**The target disk is erased.** Which image and disk are used: `IMAGE_NAME` (default `cri-golden`) and
`TARGET_DISK` (default `nvme0n1`) in the config. The restore entry is the default PXE menu entry
(`AUTO_INSTALL=1`); with `AUTO_INSTALL=0` the default is an interactive Clonezilla Live.

## GUI
```
./target/release/pxe-installer-gui
```
Install tab: network/DHCP, default hostname, image name, target disk -> **Install** (packages, services
stopped) then **Start**. It validates with the same code as the CLI, writes a mode-600 temp config, runs
`pxe-installer start --config ...` as root (directly, via passwordless `sudo -n`, or else a pkexec prompt)
and streams the output with a progress bar; the temp file is deleted afterwards. Manage tab: host status,
installed hosts, bindings, re-export images, reinstall / mark installed / bind / unbind.

## CLI
```
pxe-installer print-config > pxe.conf       # edit it (IMAGE_NAME, TARGET_DISK, network)
sudo ./pxe-installer install --config pxe.conf                  # packages + directories, services stopped
sudo ./pxe-installer start   --config pxe.conf --dry-run        # show everything it would do/write
sudo ./pxe-installer start   --config pxe.conf

sudo pxe-ctl status                          # per-host: installed / pending / waiting
sudo pxe-ctl list | reinstall <mac> | done <mac>
sudo pxe-ctl binds | bind <mac> <ip> [name] | unbind <mac|ip>
sudo pxe-ctl grub | nfs | stop
  # bindings are stored as `dhcp-host=mac,ip,name` lines in /etc/dnsmasq.d/pxe-hosts.conf (loaded via the global conf-dir);
  # bind/unbind restart dnsmasq because dhcp-host lines are not re-read on SIGHUP
```
Flags override the config file (`--iface`, `--server-ip`, `--host mac,ip,name`, `--static-ip`,
`--image-name`, `--image-dir`, `--target-disk` ...); see `pxe-installer --help`. Settings of older
(autoinstall / ISO based) versions (`TARGET_USER`, `TARGET_PASSWORD`, `ISO_NAME`, `NFS_ROOT`) are accepted
and ignored.

## Per-MAC hostname after the restore
A restored disk is an exact copy of the source, so it must not keep the source's identity. After the
restore, Clonezilla fetches `http://SERVER/cgi-bin/pxe-seed/<its MAC>/post.sh` (`grub.cfg` passes
`${net_default_mac}`). The CGI (`pxe-installer cgi-seed`) generates a script for that MAC that finds the
restored root filesystem, sets the **hostname bound to that MAC** (`pxe-ctl bind <mac> <ip> <hostname>` /
Manage tab; a MAC without a bound hostname gets `TARGET_HOSTNAME`), empties `/etc/machine-id`, regenerates
the SSH host keys, removes the PXE server's own `60-pxe.yaml` netplan file, and finally calls `pxe-done`.
So bind first, then PXE boot the host. Check the cloned OS for other network files that pin interface
names or MACs (for example cloud-init's) and extend the script if needed (`POST_SCRIPT` in `src/render.rs`).

## How duplicate restores are prevented
The post-restore script's last step calls `http://SERVER/cgi-bin/pxe-done?mac=..`, which runs
`pxe-installer cgi-done` and creates `/var/lib/tftpboot/done/<mac>`. `grub.cfg` exits to the local disk
when that file exists. A failed restore (or a failed `post.sh`) leaves no marker and retries on the next
PXE boot. Use `ONLY_KNOWN_HOSTS=1` so only hosts you bound get a DHCP answer at all.

## What `start` does
(optional netplan static IP) -> free port 53 -> `/etc/dnsmasq.d/pxe.conf` (global `/etc/dnsmasq.conf` is
kept, only ensured to load `conf-dir=/etc/dnsmasq.d`) + MAC bindings -> `grub.cfg` (Clonezilla entries)
-> copies itself to `/usr/local/sbin/pxe-installer`, writes the `pxe-ctl` wrapper -> CGIs and Apache conf ->
NFS export of `IMAGE_DIR` (read-only, PXE subnet) -> start services, ufw rules, checks.

## Security
PXE, TFTP, NFS and the callbacks are unauthenticated: use an isolated provisioning network with no other
DHCP server. The CGIs only accept requests from the PXE subnet; a MAC check is a safety check, not
protection against a spoofed MAC.

## Layout
```
src/lib.rs       shared library (used by both binaries)
src/gui.rs      GTK 4 app (feature `gui`)
src/config.rs   settings, config-file/flag parsing, validation
src/render.rs   templates for dnsmasq.conf, grub.cfg (Clonezilla), post.sh, netplan, apache conf
src/install.rs  the install steps: packages, dnsmasq, Apache, NFS export, services (honours --dry-run)
src/ctl.rs      pxe-ctl commands, status, CGI callbacks (pxe-done, pxe-seed)
src/util.rs     command/file helpers
```
(`../pxe-installer-gui` is the earlier Python prototype of the GUI and is superseded by `src/gui.rs`.)
