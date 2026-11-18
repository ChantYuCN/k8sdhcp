# pxe-installer (Rust)

One static binary that installs and manages a **UEFI PXE server** (dnsmasq DHCP/TFTP + Apache + NFS)
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

## What `install` does, and the files it unpacks
`install` installs the packages (dnsmasq, apache2, nfs-kernel-server, curl, unzip and the Clonezilla tools
`clonezilla partclone pigz zstd`), creates the directories and leaves the services stopped. **It downloads nothing.** As its **last step** it unpacks the two files you put on the server beforehand:

| Put it in (setting) | `install` then does |
| --- | --- |
| `netboot/ubuntu-*-netboot-amd64.tar.gz` (`NETBOOT_DIR`) | `tar -xzf` into a scratch folder, `cp -r amd64/.` into the TFTP root |
| `livesystem/clonezilla-live-*-amd64.zip` (`LIVESYSTEM_DIR`) | `unzip` of `live/vmlinuz` + `live/initrd.img` into `<tftp-root>/clonezilla/`, and for the deploy way (`ROOT_MODE`): `live/filesystem.squashfs` into `<web-root>/clonezilla/` (tmpfs) or the whole zip into `LIVE_ROOT` (nfs) |

The two folders are **relative** by default, so they travel with the app: put `netboot/` and `livesystem/`
in the same folder as `pxe-installer` / `pxe-installer-gui` (if not there, the current folder and then
`/boot/netboot`, `/boot/livesystem` are tried; set an absolute path to pin one). `start` remembers the
resolved paths for `pxe-ctl import`.

Then it writes `grub.cfg` and fixes the permissions (what `sudo pxe-ctl grub` does; the netboot tarball ships
its own `grub.cfg`). If several files match, the highest version is used. A missing file is a note, not an
error. `start` unpacks the live system again if the deploy way chosen there needs files that are not there
yet, and `sudo pxe-ctl import` repeats the whole step (for example after putting a newer zip into the
folder). `pxe-ctl mode` does the same for the new deploy way.

If a file is not in those folders, copy it by hand to where it goes (the log lists the missing ones, using
your configured paths). The default targets:

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
`pxe-ctl clone <disk> [--image NAME] [--yes] [--force]` (also the **Clone Image** button with its disk and image-name fields on the GUI's
Manage tab, which opens a terminal window for it) works **interactively on the terminal**, in two stages, and
then re-exports `IMAGE_DIR` over NFS:
1. `fsck -f` on every filesystem of the disk, one after the other. fsck asks its own questions, so a problem
   can be fixed right there; if one remains you choose: run fsck again, open a shell to fix it by hand,
   continue anyway, or abort. (A damaged filesystem would otherwise end up in the golden image.)
2. exactly the `ocs-sr` command above (with `-c`), with its own prompts on the same terminal.

Before it starts it refuses a disk that is not a whole disk, is mounted or holds swap (an OS disk that is
running must be imaged from Clonezilla Live instead). If `IMAGE_NAME` already exists it asks whether to replace
it (the GUI shows a message box asking whether to overwrite; `--force` answers yes); the old image is only deleted after fsck succeeded. The
disk itself is only read. `--yes` runs without any prompt (read-only `fsck -f -n` that aborts on the first
problem, `ocs-sr -batch`) for scripts. Only one clone runs at a time, and `IMAGE_DIR` has to be
`/home/partimag`, where `ocs-sr` saves images.
**The target disk is erased.** Which image and disk are used: `IMAGE_NAME` (default `cri-golden`) and
`TARGET_DISK` (default `nvme0n1`) in the config. By default (`AUTO_INSTALL=1`) the restore entry is the
default PXE menu entry: a PXE-booted host is erased and restored without any question (use
`ONLY_KNOWN_HOSTS=1` so only bound hosts are served). Set `AUTO_INSTALL=0` and re-run `start` for a menu
that starts an interactive Clonezilla Live, where the restore entry has to be selected by hand (safer for
first tests).
The restore entry is fully unattended from the PXE menu to the final reboot: GRUB picks it after the 5 s
timeout, Clonezilla Live skips the keyboard/language questions (`ocs_live_keymap=NONE`, `ocs_lang`), runs
`ocs-sr ... -batch ... restoredisk` (no confirmation, no "press Enter"), then `post.sh` and `reboot`.

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
Install tab: network/DHCP, default hostname, the Clonezilla image (a drop-down of the folders in `/home/partimag`, with a Refresh button), target disk (the deploy way, tmpfs or NFS, is switched on the Manage tab and Start keeps it) ->
**Install** (packages, directories, then the netboot tarball in `netboot/` and the Clonezilla Live zip
in `livesystem/` next to the program are unpacked; services stopped) -> **Start**. It validates with the same code as the CLI, writes a mode-600 temp config, runs
`pxe-installer start --config ...` as root (directly, via passwordless `sudo -n`, or else a pkexec prompt)
and streams the output with a progress bar; the temp file is deleted afterwards. Manage tab: host status,
installed hosts, bindings, re-export images, switch the deploy way, reinstall / mark installed / bind / unbind.

## CLI
```
pxe-installer print-config > pxe.conf       # edit it (IMAGE_NAME, TARGET_DISK, network)
sudo ./pxe-installer install --config pxe.conf                  # packages + directories + unpacks NETBOOT_DIR / LIVESYSTEM_DIR
sudo ./pxe-installer start   --config pxe.conf --dry-run        # show everything it would do/write
sudo ./pxe-installer start   --config pxe.conf

sudo pxe-ctl status                          # per-host: installed / pending / waiting + last deploy time
sudo pxe-ctl times                           # deploy time of every MAC (all runs, plus deploys still running)
sudo pxe-ctl list | reinstall <mac> | done <mac>
sudo pxe-ctl binds | bind <mac> <ip> [name] | unbind <mac|ip>
sudo pxe-ctl grub | import | nfs | mode [tmpfs|nfs] | clone <disk> [--image NAME] [--yes] [--force] | stop
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
**Static IP:** if the MAC has a binding (`pxe-ctl bind <mac> <ip>`), the script also moves the image's files in
`/etc/netplan/` into `/etc/netplan/backup/` and writes `/etc/netplan/01-pxe-server-mac-binding.yaml` with that
address (`<ip>/<PREFIX>`, `dhcp4: false`, interface `eth0`, no gateway/DNS), so the installed OS keeps its address
without a DHCP server. A MAC without a binding keeps the network config of the image.
So bind first, then PXE boot the host. Check the cloned OS for other network files that pin interface
names or MACs (for example cloud-init's) and extend the script if needed (`POST_SCRIPT` in `src/render.rs`).

## How duplicate restores are prevented
The post-restore script's last step calls `http://SERVER/cgi-bin/pxe-done?mac=..`, which runs
`pxe-installer cgi-done` and creates `/var/lib/tftpboot/done/<mac>`. `grub.cfg` exits to the local disk
when that file exists. A failed restore (or a failed `post.sh`) leaves no marker and retries on the next
PXE boot. Use `ONLY_KNOWN_HOSTS=1` so only hosts you bound get a DHCP answer at all.

## Deploy time per MAC
The restore menu entry calls `http://SERVER/cgi-bin/pxe-seed/<mac>/start` (kernel argument `ocs_prerun1`) as
soon as Clonezilla Live is up, and `pxe-done` is called at the very end. The server records the difference:
`pxe-ctl times` (Manage tab: **Deploy times**) lists every finished deploy as MAC, start, finish and elapsed
time, plus deploys that started but have not finished; `pxe-ctl status` shows the latest one per host. The
history is appended to `/var/lib/tftpboot/done/deploy-times.csv` (`mac,started,finished,elapsed_seconds`) and
survives `pxe-ctl reinstall`. The elapsed time covers the restore, `post.sh` and the callback; it does **not**
include the firmware / DHCP / TFTP phase or, in the `tmpfs` deploy way, the download of
`filesystem.squashfs`, all of which happen before Clonezilla can call the server. A restarted deploy restarts
the clock; a deploy without a recorded start (for example after an upgrade, with an old `grub.cfg`) is logged
with empty start and elapsed. Re-run `start` (or `pxe-ctl grub`) once so the new `grub.cfg` is written.

## What `start` does
(optional netplan static IP) -> free port 53 -> `/etc/dnsmasq.d/pxe.conf` (global `/etc/dnsmasq.conf` is
kept, only ensured to load `conf-dir=/etc/dnsmasq.d`) + MAC bindings -> `grub.cfg` (Clonezilla entries)
-> copies itself to `/usr/local/sbin/pxe-installer`, writes the `pxe-ctl` wrapper -> CGIs and Apache conf ->
NFS export of `IMAGE_DIR` (and `LIVE_ROOT` in nfs mode; read-only, PXE subnet) -> start services, ufw rules,
checks.

## DHCP and TFTP: dnsmasq
DHCP, TFTP and DNS forwarding are served by **dnsmasq** (one service, `dnsmasq`). The PXE settings are generated
into `/etc/dnsmasq.d/pxe.conf` (`/etc/dnsmasq.conf` stays the global file and only has to load `conf-dir`): the
pool `DHCP_START`-`DHCP_END`, `LEASE`, the router/DNS options, `dhcp-ignore=tag:!known` with `ONLY_KNOWN_HOSTS`,
the TFTP root, and a UEFI x64 match (option 93 = 7 or 9) that hands out `bootx64.efi`. The MAC bindings are in
the hosts file (`/etc/dnsmasq.d/pxe-hosts.conf`) as `dhcp-host=mac,ip[,name]` lines; `pxe-ctl bind / unbind`
rewrite it and restart dnsmasq (only when it is running; leases are kept in `/var/lib/misc/dnsmasq.leases`, what
`pxe-ctl status` reads). Watch requests with `journalctl -fu dnsmasq`.

Coming from the Kea + tftpd-hpa build: run **Install** and **Start** again. `start` stops and disables Kea and
tftpd-hpa (only when their config is the one this tool generated; the packages and files stay), and takes over
the MAC bindings of that build (`/etc/pxe-installer-hosts.conf` or `/etc/kea/pxe-hosts.conf`), rewriting them with
the `dhcp-host=` prefix dnsmasq needs. Do not run another DHCP or TFTP server on the same network.

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
