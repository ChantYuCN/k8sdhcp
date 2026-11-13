# pxe-installer (Rust)

One static binary that installs and manages a **UEFI PXE server** (dnsmasq DHCP/DNS/TFTP + Apache + NFS)
that **deploys a Clonezilla disk image** to target hosts. Targets PXE boot Clonezilla Live, restore the
image onto their disk, get their own hostname / machine-id / SSH host keys, and report back. Hosts that
already finished are skipped; MAC -> IP bindings supported. The CLI is std-only; the optional GUI uses the
`gtk4` crate.

```text
PXE server                                   target (UEFI, PXE first)
  /home/partimag/<image>  --NFS (ro)-->       GRUB -> Clonezilla Live (kernel+initrd via TFTP; live system
  clonezilla/filesystem.squashfs --HTTP-->      into RAM via HTTP (tmpfs) or mounted over NFS (nfs mode))
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

## What `install` does, and the files you copy by hand
`install` installs the packages (dnsmasq, apache2, nfs-kernel-server, curl, unzip and the Clonezilla tools
`clonezilla partclone pigz zstd`), creates the directories and leaves the services stopped. **It downloads
nothing.** At the end (in the terminal and in the GUI log) it lists the files you copy by hand and where they
go, using your configured paths. The default ones:

| File you obtain | Copy it to |
| --- | --- |
| Ubuntu netboot tarball (e.g. `ubuntu-26.04.1-netboot-amd64.tar.gz` from releases.ubuntu.com) | the **content** of its `amd64/` folder (`bootx64.efi`, `grubx64.efi`, `grub/` ...) -> the TFTP root `/var/lib/tftpboot/` |
| Clonezilla Live amd64 zip (e.g. `clonezilla-live-3.3.3-37-amd64.zip` from clonezilla.org) | `live/vmlinuz` and `live/initrd.img` -> `/var/lib/tftpboot/clonezilla/` (always) |
| | `live/filesystem.squashfs` -> `/var/www/html/clonezilla/` (deploy way **tmpfs**) |
| | the **whole** zip content -> `LIVE_ROOT`, `/srv/nfs/clonezilla` (deploy way **nfs**) |

```
tar -xzf ubuntu-*-netboot-amd64.tar.gz
sudo cp -r amd64/* /var/lib/tftpboot/
unzip clonezilla-live-*-amd64.zip -d czlive
sudo cp czlive/live/vmlinuz czlive/live/initrd.img /var/lib/tftpboot/clonezilla/
sudo cp czlive/live/filesystem.squashfs /var/www/html/clonezilla/        # tmpfs way
sudo cp -a czlive/. /srv/nfs/clonezilla/                                  # nfs way (whole zip content)
sudo pxe-ctl grub     # after `start`, and after every copy into the TFTP root (see below)
```
Copy the files for the deploy way(s) you use; `pxe-ctl mode tmpfs|nfs` switches between them. The netboot
tarball ships its own `grub/grub.cfg`, which overwrites ours when you copy it after `start`: run
`sudo pxe-ctl grub` again, it rewrites our menu and fixes the file permissions. `install` and `start`
print a note for every file that is still missing.

## What it does NOT do: the image
The Clonezilla image is never created or copied for you: capture it on the source disk (see
[clonezilla-pxe-clone-guide.md](clonezilla-pxe-clone-guide.md)) or copy it into the image folder:
```
sudo pxe-ctl clone nvme0n2                  # capture disk nvme0n2 of THIS server as IMAGE_NAME and export it
# the same by hand:
sudo ocs-sr -q2 -c -j2 -z9p -sfsck -senc -p true savedisk cri-golden nvme0n2   # -> /home/partimag/cri-golden/
sudo pxe-ctl nfs                            # (re-)export /home/partimag read-only over NFS
```
`pxe-ctl clone <disk> [--yes] [--force]` (also the **Clone Image** button and its disk field on the GUI's
Manage tab) runs exactly that `ocs-sr` command and then re-exports `IMAGE_DIR` over NFS. Before it starts it
refuses a disk that is not a whole disk, is mounted or holds swap (an OS disk that is running must be imaged
from Clonezilla Live instead), and an `IMAGE_NAME` that already exists (delete it, or use `--force` / the
"Overwrite" checkbox, which removes the old image first). The disk itself is only read. `--yes` runs without
the confirmation prompt (`-batch` instead of `-c`; the GUI always uses it). Only one clone runs at a time, and
`IMAGE_DIR` has to be `/home/partimag`, where `ocs-sr` saves images.
**The target disk is erased.** Which image and disk are used: `IMAGE_NAME` (default `cri-golden`) and
`TARGET_DISK` (default `nvme0n1`) in the config. By default (`AUTO_INSTALL=1`) the restore entry is the
default PXE menu entry: a PXE-booted host is erased and restored without any question (use
`ONLY_KNOWN_HOSTS=1` so only bound hosts are served). Set `AUTO_INSTALL=0` and re-run `start` for a menu
that starts an interactive Clonezilla Live, where the restore entry has to be selected by hand (safer for
first tests).

## Deploy way: tmpfs or NFS root
How a target gets the Clonezilla Live system itself (the image is always read over NFS):

| `ROOT_MODE` | Live system | Target RAM | Needs on the server |
| --- | --- | --- | --- |
| `tmpfs` (default) | `live/filesystem.squashfs` is downloaded over HTTP into the target's RAM; the root is a tmpfs overlay (`fetch=` in `grub.cfg`) | about 4 GB or more | the squashfs in `<web-root>/clonezilla/` |
| `nfs` | the target mounts the extracted Clonezilla Live zip over NFS as a diskless root (`netboot=nfs nfsroot=` in `grub.cfg`) | less (no RAM copy of the system) | the whole zip extracted into `LIVE_ROOT` (default `/srv/nfs/clonezilla`), exported read-only |

Choose it with `ROOT_MODE=` / `--root-mode`, the radio buttons on the GUI's Install tab, or switch at any time
without restarting anything:
```
sudo pxe-ctl mode            # show the current way
sudo pxe-ctl mode nfs        # or: tmpfs   (also the Manage tab buttons)
```
The switch rewrites `grub.cfg`, updates the runtime config and adjusts the NFS exports (the live root is only
exported in nfs mode). Hosts that are already booted are not affected; the next PXE boot uses the new way.
The NFS way depends on the server for the whole restore and uses NFSv3 (mountd is pinned to port 20048 and
opened in ufw). It has not been tested against every Clonezilla Live version: if the target does not boot,
look at `cat /boot.log` in the initramfs and fall back to `tmpfs`.

## GUI
```
./target/release/pxe-installer-gui
```
Install tab: network/DHCP, default hostname, image name, target disk, deploy way (1 tmpfs / 2 NFS) ->
**Install** (packages, directories, services stopped) -> **copy the netboot files and Clonezilla Live by
hand** to the folders the log lists -> **Start**. It validates with the same code as the CLI, writes a mode-600 temp config, runs
`pxe-installer start --config ...` as root (directly, via passwordless `sudo -n`, or else a pkexec prompt)
and streams the output with a progress bar; the temp file is deleted afterwards. Manage tab: host status,
installed hosts, bindings, re-export images, switch the deploy way, reinstall / mark installed / bind / unbind.

## CLI
```
pxe-installer print-config > pxe.conf       # edit it (IMAGE_NAME, TARGET_DISK, network)
sudo ./pxe-installer install --config pxe.conf                  # packages + directories; lists the files to copy by hand
sudo ./pxe-installer start   --config pxe.conf --dry-run        # show everything it would do/write
sudo ./pxe-installer start   --config pxe.conf

sudo pxe-ctl status                          # per-host: installed / pending / waiting
sudo pxe-ctl list | reinstall <mac> | done <mac>
sudo pxe-ctl binds | bind <mac> <ip> [name] | unbind <mac|ip>
sudo pxe-ctl grub | nfs | mode [tmpfs|nfs] | clone <disk> [--yes] [--force] | stop
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
NFS export of `IMAGE_DIR` (and `LIVE_ROOT` in nfs mode; read-only, PXE subnet) -> start services, ufw rules,
checks.

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
