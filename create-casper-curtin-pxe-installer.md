# Build a Generic Casper OS Installer and Deploy It with Curtin over PXE

## Purpose and Scope

Capture the existing Ubuntu OS on **source disk `/dev/sda`**, build a bootable
generic `cri-installer.iso` containing that OS, and use PXE to install it
automatically onto multiple approved servers concurrently. Each destination
must have **exactly one unused, sufficiently large NVMe candidate**.

```text
SOURCE / BUILD MACHINE
  /dev/sda -> identify OS root and optional /boot partitions
           -> offline read-only capture -> payload/os.squashfs
           -> package payload + Casper live environment + Curtin into ISO

PXE SERVER
  Serve UEFI boot files + live kernel/initrd over TFTP, installer ISO over HTTP

DESTINATION HOST
  UEFI PXE -> GRUB -> Casper downloads installer ISO -> guarded Curtin launcher
           -> partition approved NVMe -> extract captured OS -> configure GRUB
           -> notify PXE server -> reboot into the installed OS
```

The source disk is not converted or overwritten in place. The installer is a
new ISO file built on separate storage. `/dev/sda` is the source-machine disk;
it does not specify which disk may be erased on the destination host.

Casper boots the live environment. Curtin partitions and installs. The Rust
project supplies DHCP/TFTP/HTTP and the completion callback; its existing Ubuntu
Server autoinstall seed is not the Curtin configuration used here.

This is a reference procedure, not a hardware-certified installer. Run commands
on Linux, not Windows PowerShell. Test on a disposable UEFI VM before using a
real server. No commands in this document have been executed against your disks.

### Identify the Source OS

Your updated source is `/dev/sda`. Its root partition has not yet been confirmed:
do not reuse the earlier `/dev/nvme0n1p2` assumption or assume `/dev/sda1` is root
solely because it is the largest ext4 partition.

| Item | Role in this procedure |
| --- | --- |
| `/dev/sda` | Source disk containing the OS to capture |
| `SOURCE_ROOT` | Confirmed root partition on that disk, for example `/dev/sda1` only after inspection |
| `SOURCE_BOOT` | Optional separate `/boot` partition identified from the source OS's fstab |
| Source EFI partition | Not copied; Curtin creates and populates a new destination EFI partition |
| Destination NVMe | Discovered on each host; no preconfigured disk ID or serial required |

Capture the root filesystem and its kernel/modules, including a separate `/boot`
if present. Do not copy unrelated data partitions or the source disk's UUIDs and
EFI NVRAM entries. This is a filesystem deployment, not a sector-by-sector clone
of every partition on `/dev/sda`.

### Assumptions and Safety Rules

- Source and destination are x86-64 Ubuntu systems; this example builds a
  Ubuntu 24.04 LTS (`noble`) live environment. Check the source `/etc/os-release`.
  Validate Curtin compatibility for another release rather than assuming it.
- The source root is a plain ext4 partition. `/boot` can be inside root or on
  a separate ext3/ext4 partition captured explicitly below. Separate `/home`,
  `/var`, encrypted root, RAID, and LVM need an adapted capture; do not proceed
  while required OS data would be omitted.
- The destination boots in **UEFI mode with Secure Boot disabled** for this
  unsigned GRUB ISO example. Signed Secure Boot deployment is a separate task.
- Use an isolated provisioning VLAN, trusted server, and explicit MAC allowlist.
  HTTP and the existing completion endpoint are not an authenticated deployment
  protocol. Checksums detect corruption, not a malicious replacement ISO.
- The launcher discovers whole NVMe namespaces and requires exactly one that
  passes capacity and usage checks. It excludes disks with partitions,
  signatures, mounts, swap, or active holders. Zero or multiple candidates
  stop installation; there is no first-disk or largest-disk fallback.
- No detectable signature does **not** prove that a disk has no valuable data.
  Enroll only hosts whose qualifying disk is explicitly approved for erasure.
  Disconnect storage that must never be written during provisioning.
- Root size is calculated from the selected disk's capacity, reserving space
  for a 1 GiB EFI partition and alignment. The disk need not be as large as
  source `/dev/sda`, but it must meet the payload's minimum root size.
- The ISO is downloaded into RAM for this HTTP Casper method. RAM must cover
  the entire ISO plus the live OS and Curtin's working memory. Large payloads
  may need a separately served image or NFS design instead.
- The build host and installer need working Ubuntu package repositories, or
  a local mirror. This is not a fully offline installation recipe: Curtin may
  need target bootloader packages even when the kernel is already captured.

## 1. Prepare the Linux Build Workspace

Boot the source machine from a separate Ubuntu live USB, or attach `/dev/sda` to
another Linux machine after shutting down the source OS. Disable automounting.
Do not capture the running source root, and do not use its disk as build storage.

The following commands use a root Bash shell and a fresh workspace on a separate
Linux filesystem that supports ownership, ACLs, and extended attributes. Have
space for the uncompressed payload, live root, compressed images, and final ISO.

```bash
sudo -i
set -euo pipefail
apt-get update
apt-get install -y debootstrap rsync squashfs-tools xorriso \
  grub-pc-bin grub-efi-amd64-bin mtools dosfstools gdisk \
  initramfs-tools-core qemu-system-x86 qemu-utils ovmf

export WORK=/srv/cri-build
export SOURCE_DISK=/dev/sda
export SOURCE=/mnt/cri-source
export PAYLOAD="$WORK/payload-root"
export LIVE="$WORK/live-root"
export ISO="$WORK/iso"
test ! -e "$WORK"
mkdir -p "$SOURCE" "$PAYLOAD" "$LIVE" "$ISO/casper" "$ISO/payload" "$ISO/boot/grub"
```

If the workspace already exists, choose a fresh path. Do not blindly delete it.
`/srv/cri-build` must be on persistent storage separate from the source disk;
change `WORK` if that path would be on the live USB's RAM overlay or `/dev/sda`.
Keep these variables in the build shell. Commands for other machines are
identified separately below.

## 2. Capture and Prepare the OS Payload

Reconfirm device identities after booting the live USB; enumeration can change.
Check disk model/serial as well as its name; update `SOURCE_DISK` if necessary.
The source must have been cleanly shut down. `noload` avoids ext4 journal replay;
if the filesystem is dirty, stop and arrange a clean shutdown or recovery first.

```bash
lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS,MODEL,SERIAL "$SOURCE_DISK"
fdisk -l "$SOURCE_DISK"
blkid
```

Set `SOURCE_ROOT` to the confirmed candidate partition. `/dev/sdaN` below is an
intentional placeholder and fails until replaced. For example, choose
`/dev/sda1` only if its contents are the Ubuntu OS you intend to deploy.

```bash
export SOURCE_ROOT=/dev/sdaN
test -b "$SOURCE_ROOT"
test "$(lsblk -dn -o TYPE "$SOURCE_ROOT")" = part
test "$(lsblk -dn -o PKNAME "$SOURCE_ROOT")" = "${SOURCE_DISK##*/}"
test "$(lsblk -dn -o FSTYPE "$SOURCE_ROOT")" = ext4
if findmnt --source "$SOURCE_ROOT"; then
  echo 'Source is already mounted; stop and unmount it before capture' >&2
  exit 1
fi
mount -o ro,noload "$SOURCE_ROOT" "$SOURCE"
test -f "$SOURCE/etc/os-release"
test -f "$SOURCE/etc/fstab"
test -d "$SOURCE/usr"
cat "$SOURCE/etc/os-release"
cat "$SOURCE/etc/fstab"
ls "$SOURCE/boot/"
```

Verify the Ubuntu release, applications, and fstab before copying. Map fstab
UUIDs to the `blkid` output. If this is not the correct root, unmount `SOURCE`
and inspect another partition; do not continue with the wrong filesystem.

If fstab defines a separate `/boot`, set `SOURCE_BOOT` to its actual partition.
Otherwise leave it empty. Do not select the `/boot/efi` partition here.

```bash
export SOURCE_BOOT=''
if [[ -n $SOURCE_BOOT ]]; then
  test -b "$SOURCE_BOOT"
  test "$SOURCE_BOOT" != "$SOURCE_ROOT"
  test "$(lsblk -dn -o PKNAME "$SOURCE_BOOT")" = "${SOURCE_DISK##*/}"
  [[ $(lsblk -dn -o FSTYPE "$SOURCE_BOOT") =~ ^ext[34]$ ]]
  if findmnt --source "$SOURCE_BOOT"; then
    echo 'Source boot partition is already mounted; stop before capture' >&2
    exit 1
  fi
  test -d "$SOURCE/boot"
  mount -o ro,noload "$SOURCE_BOOT" "$SOURCE/boot"
fi
```

Copy the OS into staging. The second copy is necessary for a separate `/boot`
because the root copy uses `rsync -x` and does not cross filesystem boundaries.
The destination will store these boot files in its root filesystem.

```bash
rsync -aHAXx --numeric-ids \
  --exclude='/dev/*' --exclude='/proc/*' --exclude='/sys/*' \
  --exclude='/run/*' --exclude='/tmp/*' --exclude='/mnt/*' \
  --exclude='/media/*' --exclude='/boot/efi/*' \
  --exclude='/lost+found' --exclude='/swapfile' --exclude='/swap.img' \
  "$SOURCE/" "$PAYLOAD/"
if [[ -n $SOURCE_BOOT ]]; then
  mkdir -p "$PAYLOAD/boot"
  rsync -aHAXx --numeric-ids --exclude='/efi/*' --exclude='/lost+found' \
    "$SOURCE/boot/" "$PAYLOAD/boot/"
  umount "$SOURCE/boot"
fi
umount "$SOURCE"
```

Only the staging copy is changed below. Audit it for user private keys, tokens,
Wi-Fi secrets, logs, proprietary data, and licenses before redistribution.
Users, passwords, applications, and home directories otherwise remain cloned.

```bash
mkdir -p "$PAYLOAD"/{dev,proc,sys,run,tmp,mnt,media,boot/efi}
chmod 1777 "$PAYLOAD/tmp"
rm -f "$PAYLOAD/etc/machine-id" "$PAYLOAD/var/lib/dbus/machine-id"
touch "$PAYLOAD/etc/machine-id"
mkdir -p "$PAYLOAD/var/lib/dbus"
ln -s /etc/machine-id "$PAYLOAD/var/lib/dbus/machine-id"
rm -f "$PAYLOAD"/etc/ssh/ssh_host_*
rm -f "$PAYLOAD"/etc/netplan/*.yaml "$PAYLOAD"/etc/netplan/*.yml
rm -f "$PAYLOAD"/etc/NetworkManager/system-connections/*
rm -f "$PAYLOAD/etc/udev/rules.d/70-persistent-net.rules"
truncate -s 0 "$PAYLOAD/etc/fstab"
truncate -s 0 "$PAYLOAD/etc/crypttab"
mkdir -p "$PAYLOAD/etc/initramfs-tools/conf.d" "$PAYLOAD/etc/cloud"
printf 'RESUME=none\n' > "$PAYLOAD/etc/initramfs-tools/conf.d/resume"
touch "$PAYLOAD/etc/cloud/cloud-init.disabled"
```

This example disables cloud-init in the installed OS and configures the selected
NIC with systemd-networkd later. Adapt this policy if your deployment needs
cloud-init or NetworkManager. Remove old DHCP leases and cloud-init instance
state from the staging copy after reviewing their locations for your release.

Before continuing, inspect the payload:

```bash
ls "$PAYLOAD/boot/"
ls "$PAYLOAD/lib/modules/"
grep -R -nE 'BOOT=nfs|boot=casper|root=UUID=|resume=UUID=' \
  "$PAYLOAD/etc/initramfs-tools" "$PAYLOAD/etc/default/grub" \
  "$PAYLOAD/etc/default/grub.d" 2>/dev/null || true
```

Remove live-only boot settings and hard-coded source UUIDs from the **payload**.
Keep a working installed kernel and its matching modules: the Curtin example
uses `kernel.install: false`. Do not install Casper or the auto-install service
into this payload. If previous experiments added either, remove them in a
properly prepared payload chroot before compression. Verify the installed OS
has `systemd-networkd`, `systemd-resolved`, `netplan.io`, and `openssh-server`;
the late commands below require them.

Build the payload and a conservative minimum root-size estimate. Review the
estimate for application growth; it is not an exact filesystem sizing proof.

```bash
payload_bytes=$(du -sx --apparent-size --block-size=1 "$PAYLOAD" | cut -f1)
min_root_gib=$(( (payload_bytes * 5 / 4 + 1073741823) / 1073741824 + 4 ))
printf '%s\n' "$min_root_gib" > "$ISO/payload/min-root-gib"
mksquashfs "$PAYLOAD" "$ISO/payload/os.squashfs" -noappend -comp xz
(
  cd "$ISO/payload"
  sha256sum os.squashfs min-root-gib > SHA256SUMS
)
```

## 3. Build a Separate Casper Live Environment

This small root is only the installer. Do not put `os.squashfs` inside it; that
would unnecessarily nest the payload in the live filesystem.

```bash
debootstrap --arch=amd64 --components=main,universe noble "$LIVE" \
  http://archive.ubuntu.com/ubuntu
cat > "$LIVE/etc/apt/sources.list" <<'EOF'
deb http://archive.ubuntu.com/ubuntu noble main restricted universe multiverse
deb http://archive.ubuntu.com/ubuntu noble-updates main restricted universe multiverse
deb http://security.ubuntu.com/ubuntu noble-security main restricted universe multiverse
EOF

mount --rbind /dev "$LIVE/dev"
mount --make-rslave "$LIVE/dev"
mount -t proc proc "$LIVE/proc"
mount --rbind /sys "$LIVE/sys"
mount --make-rslave "$LIVE/sys"
rm -f "$LIVE/etc/resolv.conf"
cp -L /etc/resolv.conf "$LIVE/etc/resolv.conf"
printf '#!/bin/sh\nexit 101\n' > "$LIVE/usr/sbin/policy-rc.d"
chmod 755 "$LIVE/usr/sbin/policy-rc.d"

chroot "$LIVE" /bin/bash
```

Run this block **inside the live chroot**:

```bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y linux-generic casper curtin systemd-sysv systemd-resolved \
  netplan.io sudo curl ca-certificates rsync squashfs-tools \
  grub-efi-amd64-bin grub-efi-amd64 efibootmgr dosfstools gdisk \
  iproute2 util-linux kmod jq
command -v curtin
curtin version
mkdir -p /etc/systemd/network /etc/cri
cat > /etc/systemd/network/20-installer.network <<'EOF'
[Match]
Type=ether

[Network]
DHCP=ipv4
EOF
systemctl enable systemd-networkd systemd-resolved
rm -f /etc/resolv.conf
ln -s /run/systemd/resolve/stub-resolv.conf /etc/resolv.conf
```

Do not set `BOOT=nfs`. Casper will be selected by `boot=casper` on the kernel
command line. Continue with sections 4 and 5 while still in this chroot.

## 4. Use Runtime Discovery Instead of a Destination Profile

Do not create `/etc/cri/target.conf`. The ISO contains no PXE server address,
target MAC, hostname, disk ID, or disk serial. Every host runs the same launcher
and generates its own configuration in its local live environment.

| Value | Runtime source |
| --- | --- |
| PXE HTTP origin | The `url=` or `iso-url=` kernel argument supplied by network GRUB |
| Boot MAC | `cri.pxe_mac=${net_default_mac}`, expanded automatically by GRUB |
| Provisioning NIC | IPv4 route to the HTTP server, required to match the boot MAC |
| Destination | Exactly one unused NVMe namespace with enough capacity |
| Root size | Whole GiB of destination capacity minus 2 GiB, checked against payload minimum |
| Hostname | `cri-` followed by the full MAC without colons |
| Completion | Separate `/done/<mac>` marker on the PXE server |

This example supports a direct **HTTP URL with a literal IPv4 address and
optional port**, with the ISO and callback on the same server. It rejects
credentials, query strings, fragments, DNS names, IPv6, and duplicate URL/MAC
arguments. Adapt and test discovery before using proxies or another topology.
No downloaded configuration is executed or sourced.

The PXE server still needs its actual interface, IP/subnet, and enrolled host
MACs. Those are server-side settings, not ISO contents. The DHCP allowlist and
the explicit `cri.install=1` boot flag authorize this destructive workflow;
MACs and kernel flags are not authentication. Use a trusted provisioning VLAN.
Do not put passwords or tokens on the kernel command line.

## 5. Add the Guarded Curtin Launcher

Still **inside the live build chroot**, create the following script. It generates
Curtin YAML from validated runtime values; the source is the clean payload, not
the running live root. It leaves only the capacity/alignment reserve unallocated.

`--check` performs preflight checks only. `--install` is destructive and also
requires the kernel argument `cri.install=1`. Diagnostics still require the
PXE URL and GRUB-supplied MAC so they exercise the same discovery path.

```bash
cat > /usr/local/sbin/cri-install <<'SCRIPT'
#!/bin/bash
set -euo pipefail
export LC_ALL=C

fail() { echo "CRI installer: $*" >&2; exit 1; }
[[ $EUID -eq 0 ]] || fail 'Run as root'
mode=${1:---check}
[[ $mode == --check || $mode == --install ]] || fail 'Use --check or --install'
exec 9>/run/cri-install.lock
flock -n 9 || fail 'Another installer is running'
umask 077

[[ -d /sys/firmware/efi ]] || fail 'UEFI boot required'
read -r -a boot_args < /proc/cmdline
iso_urls=()
boot_macs=()
install_flags=()
for argument in "${boot_args[@]}"; do
  case "$argument" in
    url=*|iso-url=*) iso_urls+=("${argument#*=}") ;;
    cri.pxe_mac=*) boot_macs+=("${argument#*=}") ;;
    cri.install=*) install_flags+=("${argument#*=}") ;;
  esac
done
[[ ${#iso_urls[@]} -eq 1 && ${#boot_macs[@]} -eq 1 ]] || fail 'Require exactly one ISO URL and cri.pxe_mac'
iso_url=${iso_urls[0]}
[[ $iso_url =~ ^http://([0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3})(:([0-9]{1,5}))?(/[A-Za-z0-9._~/-]+)$ ]] || fail 'Require direct HTTP IPv4 ISO URL'
PXE_SERVER=${BASH_REMATCH[1]}
port=${BASH_REMATCH[3]}
IFS=. read -r -a octets <<< "$PXE_SERVER"
for octet in "${octets[@]}"; do
  [[ $octet == 0 || $octet != 0* ]] || fail 'IPv4 octets must not have leading zeros'
  (( 10#$octet <= 255 )) || fail 'Invalid IPv4 octet'
done
PXE_ORIGIN="http://$PXE_SERVER"
if [[ -n $port ]]; then
  (( 10#$port >= 1 && 10#$port <= 65535 )) || fail 'Invalid HTTP port'
  PXE_ORIGIN+=":$port"
fi
TARGET_MAC=${boot_macs[0],,}
[[ $TARGET_MAC =~ ^([0-9a-f]{2}:){5}[0-9a-f]{2}$ ]] || fail 'Use a lowercase colon-separated MAC'
TARGET_HOSTNAME="cri-${TARGET_MAC//:/}"
iface=$(ip -4 -j route get "$PXE_SERVER" | jq -er 'if length == 1 then .[0].dev // empty else empty end') || fail 'No unique route to PXE server'
[[ $iface =~ ^[A-Za-z0-9_.:-]{1,15}$ && -f /sys/class/net/$iface/address ]] || fail 'Invalid provisioning NIC'
[[ $(cat "/sys/class/net/$iface/address") == "$TARGET_MAC" ]] || fail 'Route NIC does not match GRUB boot MAC'

media=/cdrom/payload
[[ -f $media/os.squashfs && -f $media/SHA256SUMS ]] || fail 'Payload not mounted under /cdrom/payload'
(cd "$media" && sha256sum --check SHA256SUMS) || fail 'Payload checksum failed'
min_root_gib=$(cat "$media/min-root-gib")
[[ $min_root_gib =~ ^[1-9][0-9]{0,5}$ ]] || fail 'Invalid minimum root size'

eligible_disk() {
  local candidate=$1 value holder swap_device swap_path swaps
  [[ -b $candidate ]] || return 1
  value=$(lsblk -dn -o TYPE "$candidate") || fail 'Cannot read disk type'
  [[ $value == disk ]] || return 1
  value=$(lsblk -dn -o RO "$candidate") || fail 'Cannot read disk write protection'
  [[ $value == 0 ]] || return 1
  value=$(lsblk -nrpo NAME "$candidate") || fail 'Cannot inspect disk children'
  [[ $value == "$candidate" ]] || return 1
  value=$(lsblk -nrpo MOUNTPOINTS "$candidate") || fail 'Cannot inspect mounts'
  [[ -z ${value//[[:space:]]/} ]] || return 1
  value=$(wipefs --no-act --noheadings --output TYPE "$candidate") || fail 'Cannot inspect signatures'
  [[ -z $value ]] || return 1
  for holder in /sys/class/block/"${candidate##*/}"/holders/*; do
    [[ ! -e $holder && ! -L $holder ]] || return 1
  done
  swaps=$(swapon --show=NAME --noheadings --raw) || fail 'Cannot inspect swap'
  while IFS= read -r swap_device; do
    [[ -n $swap_device ]] || continue
    swap_path=$(readlink -f "$swap_device") || fail 'Cannot resolve swap device'
    [[ $swap_path != "$candidate" ]] || return 1
  done <<< "$swaps"
  value=$(blockdev --getsize64 "$candidate") || fail 'Cannot read disk capacity'
  [[ $value =~ ^[1-9][0-9]*$ ]] || fail 'Invalid disk capacity'
  (( value / 1073741824 - 2 >= min_root_gib ))
}

discover_disks() {
  local candidate
  candidates=()
  for candidate in /dev/nvme*n*; do
    [[ $candidate =~ ^/dev/nvme[0-9]+n[0-9]+$ ]] || continue
    if eligible_disk "$candidate"; then
      candidates+=("$candidate")
    else
      echo "Excluded NVMe: $candidate (in use, non-empty, or too small)" >&2
    fi
  done
  [[ ${#candidates[@]} -eq 1 ]] || fail "Expected exactly one eligible NVMe; found ${#candidates[@]}"
}

udevadm settle --timeout=30 || fail 'Device discovery did not settle'
discover_disks
disk=${candidates[0]}
disk_identity=$(lsblk -bdn -o MAJ:MIN,SIZE,MODEL,SERIAL,WWN "$disk") || fail 'Cannot record disk identity'
disk_bytes=$(blockdev --getsize64 "$disk") || fail 'Cannot read disk capacity'
ROOT_GIB=$(( disk_bytes / 1073741824 - 2 ))
ready=$(curl --noproxy '*' --fail --silent --show-error --max-time 15 --interface "$iface" \
  "$PXE_ORIGIN/iso/cri-ready.txt") || fail 'PXE HTTP server unreachable on selected NIC'
[[ $ready == ready ]] || fail 'Unexpected readiness response'

printf 'Server: %s\nNIC: %s\nMAC: %s\nHostname: %s\nDestination: %s\nIdentity: %s\nRoot: %s GiB\n' \
  "$PXE_ORIGIN" "$iface" "$TARGET_MAC" "$TARGET_HOSTNAME" "$disk" "$disk_identity" "$ROOT_GIB"
[[ $mode == --install ]] || { echo 'Preflight passed; no disk writes performed'; exit 0; }
[[ ${#install_flags[@]} -eq 1 && ${install_flags[0]} == 1 ]] || fail 'Installation not armed exactly once at boot'
[[ ! -e /run/cri-install.attempted ]] || fail 'Already attempted this boot; inspect logs'
touch /run/cri-install.attempted

cat > /run/cri-curtin.yaml <<EOF
sources:
  - squashfs:///cdrom/payload/os.squashfs
install:
  target: /target
  log_file: /var/log/curtin/install.log
  extra_rsync_args: ['--acls', '--xattrs']
kernel:
  install: false
curthooks:
  mode: builtin
grub:
  update_nvram: true
  reorder_uefi: false
swap:
  size: 0
storage:
  version: 1
  config:
    - id: destination
      type: disk
      path: '$disk'
      ptable: gpt
      wipe: superblock-recursive
      grub_device: true
    - {id: esp, type: partition, device: destination, number: 1, size: 1G, flag: boot}
    - {id: root, type: partition, device: destination, number: 2, size: ${ROOT_GIB}G}
    - {id: esp-fs, type: format, volume: esp, fstype: fat32}
    - {id: root-fs, type: format, volume: root, fstype: ext4}
    - {id: root-mount, type: mount, device: root-fs, path: /}
    - {id: esp-mount, type: mount, device: esp-fs, path: /boot/efi}
late_commands:
  10-customize: ['/usr/local/sbin/cri-customize', '$TARGET_HOSTNAME', '$TARGET_MAC']
EOF

udevadm settle --timeout=30 || fail 'Device discovery did not settle'
discover_disks
[[ ${candidates[0]} == "$disk" ]] || fail 'Selected disk changed'
current_identity=$(lsblk -bdn -o MAJ:MIN,SIZE,MODEL,SERIAL,WWN "$disk") || fail 'Cannot recheck disk identity'
[[ $current_identity == "$disk_identity" ]] || fail 'Disk identity changed'
curtin -c /run/cri-curtin.yaml install
touch /run/cri-install.succeeded
sync
reply=$(curl --noproxy '*' --fail --silent --show-error --max-time 30 --retry 3 --interface "$iface" \
  "$PXE_ORIGIN/cgi-bin/pxe-done?mac=$TARGET_MAC") || fail 'Installed, but callback failed; do not reinstall'
[[ $reply == "ok $TARGET_MAC" ]] || fail 'Installed, but completion acknowledgment invalid; do not reinstall'
echo 'Installation completed and PXE server notified; rebooting'
systemctl reboot
SCRIPT
chmod 755 /usr/local/sbin/cri-install
```

No operator-supplied serial is needed. The launcher records the discovered disk
identity and repeats selection and identity checks immediately before Curtin.
Do not hotplug storage during installation; these checks cannot make hotplug
race-free. Existing SATA/SAS disks are never candidates. Existing NVMe contents
are excluded, not erased. A partial install normally leaves signatures and is
not automatically erased and retried. Diagnose failures instead of bypassing
the guards. Hardware RAID, multipath, NVMe-oF, and SAN storage need a separate
reviewed selection policy; this recipe is for directly attached NVMe.

Add the target customization helper in the same **live chroot**. It is run by
Curtin's late stage, where `TARGET_MOUNT_POINT` is defined. Errors abort success.
It creates unique SSH host keys during installation and leaves machine-id empty
for regeneration at first boot. The source user/password are otherwise retained.

```bash
cat > /usr/local/sbin/cri-customize <<'SCRIPT'
#!/bin/bash
set -euo pipefail
hostname_value=$1
mac_value=$2
target=${TARGET_MOUNT_POINT:?Curtin target missing}
[[ $target == /target && -f $target/etc/os-release ]]
mkdir -p "$target/etc/netplan" "$target/etc/cloud"
rm -f "$target"/etc/netplan/*.yaml "$target"/etc/netplan/*.yml
cat > "$target/etc/netplan/01-cri.yaml" <<EOF
network:
  version: 2
  renderer: networkd
  ethernets:
    provisioned:
      match:
        macaddress: '$mac_value'
      dhcp4: true
EOF
chmod 600 "$target/etc/netplan/01-cri.yaml"
touch "$target/etc/cloud/cloud-init.disabled"
printf '%s\n' "$hostname_value" > "$target/etc/hostname"
cat > "$target/etc/hosts" <<EOF
127.0.0.1 localhost
127.0.1.1 $hostname_value
::1 localhost ip6-localhost ip6-loopback
EOF
rm -f "$target/etc/machine-id" "$target/var/lib/dbus/machine-id"
touch "$target/etc/machine-id"
mkdir -p "$target/var/lib/dbus"
ln -s /etc/machine-id "$target/var/lib/dbus/machine-id"
rm -f "$target"/etc/ssh/ssh_host_*
curtin in-target -- systemctl enable systemd-networkd systemd-resolved ssh
curtin in-target -- systemctl mask NetworkManager.service NetworkManager-wait-online.service
rm -f "$target/etc/resolv.conf"
ln -s /run/systemd/resolve/stub-resolv.conf "$target/etc/resolv.conf"
curtin in-target -- netplan generate
curtin in-target -- ssh-keygen -A
curtin in-target -- update-initramfs -u -k all
curtin in-target -- update-grub
test -s "$target/etc/fstab"
test -s "$target/boot/grub/grub.cfg"
find "$target/boot/efi/EFI" -type f -iname '*.efi' -print -quit | grep -q .
SCRIPT
chmod 755 /usr/local/sbin/cri-customize
```

This intentionally replaces hostname/hosts and the network configuration.
Preserve additional site-specific host entries or network definitions if needed.
Curtin's mount actions create `/etc/fstab` using the new filesystem UUIDs.

Add the systemd service, still in the **live chroot**:

```bash
cat > /etc/systemd/system/cri-install.service <<'EOF'
[Unit]
Description=Install captured CRI OS with Curtin
Wants=network-online.target
After=network-online.target
ConditionKernelCommandLine=cri.install=1

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/cri-install --install
TimeoutStartSec=infinity
Restart=no
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
EOF
systemctl enable systemd-networkd-wait-online.service cri-install.service
bash -n /usr/local/sbin/cri-install
bash -n /usr/local/sbin/cri-customize
update-initramfs -u -k all
apt-get clean
rm -f /usr/sbin/policy-rc.d
exit
```

You are now back in the **build host** shell. If an earlier chroot step failed,
unmount its virtual filesystems before retrying or packaging; never compress
the chroot while `/dev`, `/proc`, or `/sys` remain mounted.

## 6. Assemble and Build the ISO

```bash
umount -R "$LIVE/dev"
umount "$LIVE/proc"
umount -R "$LIVE/sys"
findmnt -R "$LIVE" || true

kernel_file=$(find "$LIVE/boot" -maxdepth 1 -type f -name 'vmlinuz-*' | sort -V | tail -n 1)
test -n "$kernel_file"
kernel_version=${kernel_file##*/vmlinuz-}
test -d "$LIVE/lib/modules/$kernel_version"
test -f "$LIVE/boot/initrd.img-$kernel_version"
lsinitramfs "$LIVE/boot/initrd.img-$kernel_version" | grep 'scripts/casper'
cp "$kernel_file" "$ISO/casper/vmlinuz"
cp "$LIVE/boot/initrd.img-$kernel_version" "$ISO/casper/initrd"
mksquashfs "$LIVE" "$ISO/casper/filesystem.squashfs" -noappend -comp xz
du -sx --block-size=1 "$LIVE" | cut -f1 > "$ISO/casper/filesystem.size"
mkdir -p "$ISO/.disk"
printf 'CRI Casper Curtin installer\n' > "$ISO/.disk/info"

cat > "$ISO/boot/grub/grub.cfg" <<'EOF'
set default=0
set timeout=10

menuentry "CRI live diagnostics (no installation)" {
  linux /casper/vmlinuz boot=casper ignore_uuid noprompt ip=dhcp ---
    initrd /casper/initrd
}
EOF

grub-mkrescue -o "$WORK/cri-installer.iso" "$ISO" -- -volid CRI_INSTALLER
sha256sum "$WORK/cri-installer.iso"
```

The ISO's local USB/BMC menu is diagnostics-only: it has no PXE URL or boot MAC.
Use the network GRUB menu below for preflight and installation. This keeps the
ISO generic without embedding a server address for local-media installation.
For each rebuild, use a new output path or deliberately replace only generated
SquashFS outputs; do not append new content to an old filesystem image.

The boot kernel/initrd above are from the **live installer**, not the captured
OS. The payload retains its own installed kernel for booting the finished system.

`ignore_uuid` disables Casper's embedded media-UUID match for this hand-built
ISO; alternatively, generate and install matching Casper UUID metadata in both
the initrd and ISO. This is separate from the payload checksum verification.
`noprompt` prevents Casper from waiting for media removal on reboot, which would
otherwise interrupt unattended installation. Neither option belongs in the
installed OS's normal disk-boot arguments.

## 7. Configure the Rust PXE Server

On a dedicated **Ubuntu PXE server**, build the CLI with Rust available:

```bash
cargo test
cargo build --release --bin pxe-installer
umask 077
./target/release/pxe-installer print-config > pxe.conf
```

Edit `pxe.conf` with the actual interface, subnet, and each approved target MAC.
These addresses are examples for the **PXE server only**, not values embedded
in the ISO. Use distinct DHCP addresses and globally unique NIC MACs:

```ini
IFACE=enp2s0
SERVER_IP=192.168.5.1
PREFIX=24
DHCP_START=192.168.5.100
DHCP_END=192.168.5.200
SET_STATIC_IP=1
ONLY_KNOWN_HOSTS=1
HOST=aa:bb:cc:dd:ee:01,192.168.5.101,cri-node01
HOST=aa:bb:cc:dd:ee:02,192.168.5.102,cri-node02
TARGET_PASSWORD=REPLACE_WITH_A_STRONG_LOCAL_VALUE
ISO_NAME=cri-installer.iso
```

The `HOST` names are DHCP labels; the installed hostname is derived from the
MAC, for example `cri-aabbccddee01`. Add enrolled hosts on the server without
rebuilding the ISO. Never remove the allowlist merely to make deployment easier.

The existing CLI still requires a target password to generate its unrelated
Ubuntu Server seed. That password does **not** change the users/passwords inside
your captured OS. Do not expose that seed outside the provisioning network.

Run from a local console if changing the server NIC configuration:

```bash
sudo ./target/release/pxe-installer install --config pxe.conf
sudo ./target/release/pxe-installer start --config pxe.conf --dry-run
sudo ./target/release/pxe-installer start --config pxe.conf
```

`install` installs packages and leaves services stopped. `start` writes configs
and starts them. Neither downloads boot files. The tool advertises itself as
the clients' gateway but does not set up IP forwarding/NAT. Provide a working
gateway/routing arrangement or a local Ubuntu mirror. Avoid competing DHCP.

Copy compatible Ubuntu **UEFI network boot** files into `/var/lib/tftpboot/`:
`bootx64.efi`, `grubx64.efi`, and the accompanying GRUB support files. Use a
release netboot bundle and its expected directory layout. Do not substitute
arbitrary EFI executables from an installed disk: their embedded GRUB prefix
may point to the wrong configuration. Verify the bundle's published checksum.

Transfer the three build outputs from the build host and place them as follows:

```text
/var/lib/tftpboot/bootx64.efi
/var/lib/tftpboot/grubx64.efi
/var/lib/tftpboot/grub/grub.cfg
/var/lib/tftpboot/casper/vmlinuz        <- iso/casper/vmlinuz
/var/lib/tftpboot/casper/initrd         <- iso/casper/initrd
/var/www/html/iso/cri-installer.iso     <- cri-installer.iso
```

Apply the project's permissions first, then install your custom GRUB menu:

```bash
sudo pxe-ctl grub
printf 'ready\n' | sudo tee /var/www/html/iso/cri-ready.txt >/dev/null
```

On the PXE server, replace `/var/lib/tftpboot/grub/grub.cfg` with the following.
Initially keep `default=0` (diagnostics). GRUB obtains the TFTP server address
from `net_default_server` and passes its boot MAC automatically. This assumes
TFTP, HTTP, and the completion CGI are on the same IPv4 server, as in this
project. For a non-default HTTP port, change only `cri_iso_url` in this
**server-side menu**, not the ISO. Separate HTTP/TFTP hosts need an adapted
callback and shared completion-marker arrangement; changing the URL alone
is not sufficient. No Ubuntu `autoinstall` or NoCloud seed arguments are needed:
the custom live system launches Curtin itself.

```grub
set timeout=10
set default=0
set cri_iso_url="http://${net_default_server}/iso/cri-installer.iso"
set cri_pxe_mac="${net_default_mac}"
export cri_iso_url
export cri_pxe_mac

if [ -f /done/${net_default_mac} ]; then
    echo "Installation already recorded; leaving PXE."
    exit
fi

menuentry "CRI live diagnostics (no installation)" {
  linux /casper/vmlinuz boot=casper ignore_uuid noprompt ip=dhcp url=${cri_iso_url} cri.pxe_mac=${cri_pxe_mac} ---
    initrd /casper/initrd
}
menuentry "Install CRI to approved empty NVMe (ERASES DISK)" {
  linux /casper/vmlinuz boot=casper ignore_uuid noprompt ip=dhcp url=${cri_iso_url} cri.pxe_mac=${cri_pxe_mac} cri.install=1 ---
    initrd /casper/initrd
}
```

**Important:** `pxe-installer start` and `pxe-ctl grub` regenerate the original
Ubuntu Server menu from `src/render.rs`. They overwrite this custom menu. Keep
an administrator-owned copy outside the TFTP tree and reapply it after those
commands. No Rust source change is required for this documented manual setup.

UEFI `exit` does not guarantee local boot on every firmware. Verify that the
installed Ubuntu boot entry exists and is ahead of PXE, or use one-time PXE boot
through the server BMC. Curtin's `reorder_uefi: false` avoids deliberately
restoring PXE to first place after GRUB is installed.

Check service health and readability without marking a real target done:

```bash
sudo dnsmasq --test
sudo systemctl status dnsmasq apache2 --no-pager
curl -fI http://192.168.5.1/iso/cri-installer.iso
curl -f http://192.168.5.1/iso/cri-ready.txt
curl -i 'http://192.168.5.1/cgi-bin/pxe-done?mac=bad'
sudo pxe-ctl status
```

The last HTTP request should reject the invalid MAC. Never test that endpoint
using the real MAC before installation: a successful request marks it installed.
Confirm UDP 67/69, TCP/UDP 53, and TCP 80 are permitted on the provisioning
network, including TFTP connection tracking where required.

## 8. Test Before Enabling Unattended Installation

### VM Test

Use disposable VMs with OVMF UEFI firmware, Secure Boot off, virtual NVMe disks,
and distinct fixed NIC MACs enrolled in the server allowlist. Give each enough
RAM for the ISO and installer. No VM-specific profile or serial is required.
Use an isolated bridged test network when testing DHCP/TFTP.

1. Boot the local ISO diagnostic entry first to verify Casper reaches a shell,
   then PXE-boot the network diagnostic entry for runtime discovery tests.
2. Check `/cdrom/payload`, `cat /proc/cmdline`, NIC addressing, and repository
   access. A readiness URL alone does not prove target APT repositories work.
3. Run `sudo /usr/local/sbin/cri-install --check`. This checks metadata and
   hashes but does not invoke Curtin or write to the destination disk.
4. Test zero candidates, two eligible NVMes, insufficient capacity, partitions,
  filesystem/RAID/LVM signatures, mounts, swap, and holders. Used disks must
  never qualify; zero or multiple eligible disks must stop. Test missing or
  duplicate URL/MAC arguments, malformed URLs, a route-NIC/MAC mismatch,
  missing/corrupt payload, and unreachable HTTP. Each invalid case must stop.
5. Select the PXE install entry manually on one fresh disposable disk. No ISO
  rebuild or embedded erase switch is needed; `cri.install=1` arms this boot.
6. Observe Curtin and check the completion marker on the PXE server.
7. Disconnect ISO/PXE and boot the virtual NVMe. Verify EFI boot, root UUID,
   hostname, DHCP, SSH access, unique host keys, and the actual CRI applications.
8. Re-enable PXE and verify the done guard and disk-signature checks prevent
   another installation. Failure recovery must not silently wipe again.
9. Boot two enrolled VMs concurrently using the same ISO. Verify different
   generated hostnames, SSH host keys, machine IDs after first boot, and done
   markers. One completed client must not suppress another client's install.
10. Interrupt an installation and separately simulate callback failure. Confirm
  neither automatically wipes again and callback failure prevents reboot.
11. Change the PXE server IP and corresponding DHCP/server settings in the test
  network, reapply the custom menu, and use the unchanged ISO. Verify the new
  origin and matching NIC are discovered, including on multi-NIC hosts.

### Enable Automatic PXE Installation

Only after those tests pass:

1. Build and test the generic ISO once; approve each host and its eligible disk
  for erasure, then enroll its MAC on the PXE server.
2. Replace the served ISO and matching live kernel/initrd as one deployment
   while target machines are not booting. Verify their checksums after transfer.
3. Set `default=1` in the **PXE server's custom GRUB menu**, selecting the install
   entry after the timeout. Keep the MAC allowlist and done guard.
4. Boot approved servers using UEFI PXE IPv4. No interactive confirmation
  follows once the install entry is selected and runtime guards pass.
5. Confirm local disk boot after the installer reports success.

The ISO's own menu is independent of the network menu and remains diagnostics-only.

### Reuse and Concurrent Deployment

When migrating from the earlier per-host recipe, remove the old
`/etc/cri/target.conf` from the **live staging tree**, replace the launcher and
service, and install `jq` there. Rebuild the live SquashFS and ISO once using
section 6, and deploy the matching live kernel/initrd together. Reuse the
existing payload if the captured OS has not changed; do not recapture `/dev/sda`
just because target machines or the PXE server address changed.

After that one-time rebuild, a new host needs only server-side enrollment. A
new subnet requires updated server/DHCP/firewall settings, not another ISO.
Reapply the custom menu after `start` or `pxe-ctl grub`. Do not replace served
boot assets or the ISO while clients are downloading them.

Each host has independent `/run/cri-curtin.yaml`, installation state, and logs
in its live environment; only the read-only ISO is shared. Completion is keyed
by the boot MAC. Each client downloads the entire ISO, so provision sufficient
RAM per host and HTTP bandwidth/storage throughput for the batch. Stagger
boots when necessary; this recipe does not provide multicast distribution.

## 9. Logs, Recovery, and Troubleshooting

From the **live target**:

```bash
sudo journalctl -u cri-install.service -b --no-pager
sudo tail -n 100 /var/log/curtin/install.log
cat /proc/cmdline
lsblk -o NAME,SIZE,FSTYPE,MOUNTPOINTS,UUID
sudo efibootmgr -v
```

From the **PXE server**:

```bash
sudo journalctl -u dnsmasq -b --no-pager
sudo tail -n 100 /var/log/apache2/access.log /var/log/apache2/error.log
sudo pxe-ctl status
```

| Symptom | Check |
| --- | --- |
| Initramfs asks for `root=` | Check `/proc/cmdline` for `boot=casper` and `/scripts/casper` in initramfs. Do not point `root=` at the source disk. |
| Casper cannot find live media | Check the ISO URL, matching live kernel/initrd, Casper scripts, NIC support, RAM, and `/casper/filesystem.squashfs` inside the ISO. |
| `/cdrom/payload` missing | Inspect actual media mounts and ISO contents before adjusting the launcher and Curtin source together. |
| Installer did not start | Diagnostic entry intentionally lacks `cri.install=1`; inspect systemd status and the kernel arguments. |
| URL or NIC refused | Require one HTTP IPv4 ISO URL and one GRUB-supplied `cri.pxe_mac`; the route to that server must use the same physical NIC. Local ISO boot alone does not supply these. |
| Destination refused | Zero/multiple eligible NVMes, insufficient size, existing signatures, active usage, or an earlier partial installation. Do not bypass the guard without a separate erase review. |
| Curtin fails in curthooks | Check source Ubuntu compatibility, APT/DNS/routing, installed kernel/modules, bootloader packages, and UEFI variables. |
| Installed OS has no network | Check its generated Netplan configuration, selected MAC, networkd/resolved services, and destination gateway. |
| PXE starts again after success | Check UEFI boot order and `/var/lib/tftpboot/done/<mac>`; compare the logged runtime MAC with GRUB's PXE MAC. |

If Curtin succeeded but the callback failed, the service stops without rebooting.
Do **not** rerun the installation. Verify the success log and
`/run/cri-install.succeeded`, correct connectivity, and retry only the callback
or mark the host done from the server with `sudo pxe-ctl done <mac>`. Then boot
the installed disk. A done marker indicates installer completion, not a tested
first boot of your applications.

If installation failed or power was lost, preserve logs and inspect the disk.
Reinstallation requires both a deliberate marker removal (`pxe-ctl reinstall
<mac>`) and a separate, explicitly approved disk-erasure procedure. The launcher
will not clear existing partitions simply because the marker was removed.

## References

- [Existing live-image notes](create-bootable-iso-sbs-cri.md)
- [PXE project README](README.md) (the source's install/start split takes precedence)
- [Casper boot options](https://manpages.ubuntu.com/manpages/noble/man7/casper.7.html)
- [Curtin configuration and image sources](https://curtin.readthedocs.io/en/latest/topics/config.html)
- [Curtin storage configuration](https://curtin.readthedocs.io/en/latest/topics/storage.html)
- [Curtin stages and target environment](https://curtin.readthedocs.io/en/latest/topics/overview.html)

Before production, record the actual source release, live kernel, Casper/Curtin
package versions, payload hash, discovered disk identity and MAC, destination firmware, and VM
test results. Linux image building, UEFI boot, and disk installation must be
validated on your Linux test environment; documentation syntax checks alone
cannot establish that the deployment works on your hardware.
