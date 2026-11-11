# Clone a Disk to a Target Host with Clonezilla over an Isolated PXE Network

## Purpose and Scope

Clone the disk `/dev/nvme0n2` of the source host `192.168.5.1` (which is also the PXE server) onto
`/dev/nvme0n1` of the target host `192.168.5.20`. The target boots Clonezilla Live over PXE and restores a
compressed partclone image from an NFS share on the PXE server. `pxe-installer` configures the PXE, NFS and
callback services and generates the PXE menu and the post-restore script; you supply the Clonezilla Live
files and the image.

```text
SOURCE / PXE SERVER 192.168.5.1                       TARGET 192.168.5.20
  /dev/nvme0n2
     |  ocs-sr savedisk (partclone + zstd)
     v
  /home/partimag/cri-golden  --- NFS (read-only) --->  UEFI PXE -> GRUB -> Clonezilla Live
                                                         ocs-sr restoredisk -> /dev/nvme0n1
  cgi-bin/pxe-seed/<mac>/post.sh  --- HTTP --------->    post.sh: hostname, machine-id, SSH keys
  cgi-bin/pxe-done?mac=...        <-- HTTP ----------    reboot into the cloned OS
```

Why this approach: partclone copies only the used blocks and reads the image sequentially, which avoids the
small random reads that made the earlier squashfs-over-NFS install slow. On gigabit, expect roughly 100 MB/s
at best.

**Status:** this code and these commands have not been run against your hardware (the project was edited
without a Rust toolchain, so run `./build.sh` first and fix any compile error). Test on disposable disks and
check every Clonezilla option against `ocs-sr --help` for the version you download.

## 1. Warnings

- **The target's `/dev/nvme0n1` is erased.** Confirm the device on the target with
  `lsblk -d -o NAME,SIZE,SERIAL,MODEL`. Do not rely on the name `nvme0n1` alone.
- The target disk must be **at least as large as the source's used partitions**.
- A clone is an identical copy. It carries the source's hostname, `machine-id`, SSH host keys and network
  config. The generated `post.sh` resets hostname, `machine-id` and SSH host keys and removes
  `/etc/netplan/60-pxe.yaml` (the PXE server's static IP). Other files that pin names or MACs (for example
  cloud-init's netplan file) are not touched: check them.
- Use an **isolated provisioning network**. PXE, TFTP, NFS and the callbacks are unauthenticated.
- Keep other DHCP servers off this network (turn off VirtualBox's own DHCP server if you test in VMs).

## 2. Configure and Start the PXE Server

```bash
pxe-installer print-config > pxe.conf
```

Edit `pxe.conf` (the keys that matter for this guide):

```ini
IFACE=enp2s0
SERVER_IP=192.168.5.1
PREFIX=24
DHCP_START=192.168.5.100
DHCP_END=192.168.5.200
SET_STATIC_IP=1
ONLY_KNOWN_HOSTS=1
AUTO_INSTALL=0                    # first tests: the default PXE entry is interactive Clonezilla
TARGET_HOSTNAME=ubuntu-node       # hostname of a host that has none bound to its MAC
IMAGE_NAME=cri-golden             # folder inside IMAGE_DIR
IMAGE_DIR=/home/partimag
TARGET_DISK=nvme0n1               # ERASED on the target
HOST=<target-mac>,192.168.5.20,node20
```

Then:

```bash
sudo ./pxe-installer install --config pxe.conf               # packages + directories, services stopped
sudo ./pxe-installer start   --config pxe.conf --dry-run     # review
sudo ./pxe-installer start   --config pxe.conf
```

`start` writes the dnsmasq config, the PXE menu (`grub.cfg`), the CGIs, and the read-only NFS export of
`IMAGE_DIR`, and opens ports 53, 67, 69, 80, 111, 2049 and 20048 in ufw when it is active. It prints which
files are still missing. The binding gives the target the IP `192.168.5.20` and the hostname `node20`.

## 3. Capture the Image from /dev/nvme0n2

Install Clonezilla's tools on the server so the disk can be captured without rebooting it:

```bash
sudo apt install clonezilla partclone pigz zstd
```

The source disk must be **idle and unmounted**:

```bash
lsblk /dev/nvme0n2        # MOUNTPOINTS column must be empty
swapon --show             # must not list nvme0n2
```

If the server boots its OS from `nvme0n2`, you cannot image it while it runs. Boot Clonezilla Live from USB
on the server instead and choose `savedisk`.

```bash
sudo ocs-sr -q2 -c -j2 -z9p -sfsck -senc -p true savedisk cri-golden nvme0n2
sudo pxe-ctl nfs          # re-export IMAGE_DIR and check that the image exists
```

| Option | Meaning |
| --- | --- |
| `-q2` | use partclone |
| `-z9p` | parallel zstd compression |
| `-c` | ask for confirmation first |
| `-p true` | take no action when finished |

The image is written to `/home/partimag/cri-golden/`. Optionally verify it with `ocs-chkimg`.

## 4. Put the Boot Files and Clonezilla Live in Place

Download the Clonezilla Live **amd64 zip** from clonezilla.org and the Ubuntu netboot tarball (only the
UEFI boot files from it are used: `bootx64.efi`, `grubx64.efi`, `grub/`).

```bash
sudo cp -r amd64/* /var/lib/tftpboot/                      # from the netboot tarball
cd ~/cz && unzip clonezilla-live-*-amd64.zip -d czlive
sudo cp czlive/live/vmlinuz czlive/live/initrd.img /var/lib/tftpboot/clonezilla/
sudo cp czlive/live/filesystem.squashfs /var/www/html/clonezilla/
sudo chmod 644 /var/www/html/clonezilla/filesystem.squashfs
sudo pxe-ctl grub          # re-apply our grub.cfg (the tarball ships its own) and file permissions
```

The kernel and initrd go through TFTP. The target downloads `filesystem.squashfs` over HTTP into RAM, so
give the target at least 4 GiB of RAM.

## 5. What the Tool Generates

**The PXE menu** (`/var/lib/tftpboot/grub/grub.cfg`, from `src/render.rs`; do not edit it by hand,
`pxe-ctl grub` and `start` rewrite it). It has three entries and the guard at the top:

- the guard: a host whose marker `/done/<mac>` exists leaves GRUB and boots its local disk;
- entry 0: Clonezilla Live, interactive (nothing is changed automatically);
- entry 1: **Restore `IMAGE_NAME` to `/dev/TARGET_DISK` (ERASES THE DISK)**: runs
  `ocs-sr -g auto -e1 auto -e2 -batch -r -j2 -scr -p true restoredisk <image> <disk>` without questions,
  then `ocs_postrun1..3`: download `post.sh`, run it, reboot;
- entry 2: boot from the local disk.

`AUTO_INSTALL=1` makes entry 1 the default, `AUTO_INSTALL=0` makes entry 0 the default. Both Clonezilla
entries mount `SERVER_IP:IMAGE_DIR` read-only on `/home/partimag` through `ocs_prerun`.

**The post-restore script** is generated per MAC by the `pxe-seed` CGI at
`http://SERVER/cgi-bin/pxe-seed/<mac>/post.sh`. It finds the restored root filesystem on the target disk,
then: sets the hostname bound to that MAC (or `TARGET_HOSTNAME`), empties `/etc/machine-id`, removes the old
SSH host keys and regenerates them in a chroot, removes `/etc/netplan/60-pxe.yaml`, and finally calls
`pxe-done?mac=<mac>`. If any step fails, the callback is not made, so the host gets no marker and is
restored again on its next PXE boot. Look at the generated script with:

```bash
curl http://192.168.5.1/cgi-bin/pxe-seed/<target-mac>/post.sh
```

To change what happens after the restore, edit `POST_SCRIPT` in `src/render.rs` and rebuild.

## 6. Run and Verify

1. First test: keep `AUTO_INSTALL=0` and select the restore entry by hand on the target console. Watch the
   restore; for the very first run you can also run `ocs-sr` interactively from entry 0.
2. Boot the target in **UEFI** mode with network first in the boot order. Test with Secure Boot disabled.
3. Watch the server: `journalctl -fu dnsmasq` and `sudo tail -f /var/log/apache2/access.log`.
4. When the restore finishes, `post.sh` runs and the host reboots from `nvme0n1`.
5. On the new host verify the hostname, the IP (`192.168.5.20`), SSH access, and `cat /etc/machine-id`.
6. On the server, `sudo pxe-ctl status` should show the host as `installed`. The done marker is
   `/var/lib/tftpboot/done/<mac>`.
7. When it works, set `AUTO_INSTALL=1`, re-run `start`, and PXE boot again (use `pxe-ctl reinstall <mac>`
   first to remove the marker of a host that should be restored again).

## 7. Troubleshooting

| Symptom | Check |
| --- | --- |
| Clonezilla asks interactive questions | The arguments did not reach it. Run `cat /proc/cmdline` in the live shell and compare it with the entry in `grub.cfg` (the `\"` pairs must arrive as `"`). |
| `mount -t nfs` fails | `showmount -e 192.168.5.1` from the target; check `exportfs -v`, the firewall ports and that `nfs-server` is running. |
| `ocs-sr` cannot find the image | `ls /home/partimag` in the live shell; `IMAGE_NAME` must be a folder in `IMAGE_DIR`; run `sudo pxe-ctl nfs` on the server. |
| Restore is slow | Test a sequential `dd` read of an image file; check link speed with `ethtool <nic>`. |
| `ocs-sr` reports the disk is too small | The target disk is smaller than the source's used partitions. Use a larger disk. |
| Target boots to no OS | Check the UEFI boot entries and boot order. `-g auto` should rebuild the bootloader. |
| `post.sh` fails or does not run | Run the `curl` above; in the live shell check `/tmp/post.sh`; look at the Apache log. The host has no marker, so it is restored again. |
| Host restores again after finishing | The callback failed. Check the `pxe-done` request, or run `sudo pxe-ctl done <mac>` after verifying the clone. |
| `ocs_postrun` entries seem ignored | Check the Clonezilla Live version's documentation for `ocs_postrun`; if unsupported, move the three commands into `ocs_live_run` with a small wrapper script served over HTTP. |

## 8. Expected Time

On gigabit, with 220 GB of used data compressing to roughly 100-150 GB, expect about 20-40 minutes for one
target. Several targets restoring at once share the server's bandwidth. For many identical targets, use
Clonezilla Server Edition multicast instead.

## References

- Clonezilla: https://clonezilla.org
- Clonezilla Live PXE and unattended boot parameters (`ocs_live_run`, `ocs_prerun`, `ocs_postrun`): see the
  documentation shipped with your Clonezilla Live version.
- This project's README: `README.md`
