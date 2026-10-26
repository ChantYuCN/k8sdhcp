# Installing a Custom Intel Xe Kernel on Ubuntu (Two-Machine Workflow)

Build a custom Intel **Xe** kernel (with **i915 disabled** and **Intel PMT
telemetry** enabled) as a `.deb` on **Machine A** (isolated build environment),
then install it onto **Machine B's disk** via a USB-based Ubuntu install.

- **Repo:** `intel-innersource/drivers.gpu.linux.xe.kernel` (branch `xe-internal`)
- **Distro:** Ubuntu 24.04 (`noble`, amd64).
- **Build isolation:** Docker container (default) or a throwaway VM.
- **Install method:** official Ubuntu installer USB → install to disk →
  `dpkg -i` the custom kernel. (Two-step; see "Install strategy".)

---

## Overview

```
┌─────────────────────────────┐                  ┌──────────────────────────────┐
│   MACHINE A (build, no USB) │   transfer the   │   MACHINE B (target)         │
│  Docker container:          │   single .iso    │  boot installer ISO via       │
│  1. compile xe kernel .deb  │   (scp / BMC     │   USB / BMC virtual media/PXE │
│     (i915 off, PMT on)      │    virtual media)│   ↓ autoinstall (default)     │
│  2. build ONE installer ISO │  ──────────────▶ │   ↓ installs Ubuntu to DISK   │
│     (kernel + autoinstall   │                  │   ↓ installs -xe kernel       │
│      baked in via xorriso)  │                  │   ↓ reboot -> -xe kernel      │
└─────────────────────────────┘                  └──────────────────────────────┘
```

| Job | Privileges needed | Where |
|-----|-------------------|-------|
| Kernel `.deb` compile | None special | Plain Docker container (Machine A) |
| Build installer ISO | none (container) | Machine A — `xorriso` replay, single file |
| Boot media | varies | Machine B — USB / BMC virtual media / PXE |
| Install to disk + add kernel | n/a (automated) | Machine B (clean installer env) |

---

## Part 0 — Prerequisites (Machine A)

- Docker (or Podman — swap `docker` for `podman` everywhere).
- Intel innersource GitHub/SSH access configured.
- No base ISO download needed — `live-build` bootstraps a minimal rootfs from the
  Ubuntu archive directly.
- ~40 GB free disk for source + build artifacts (minimal image is small, but the
  kernel source tree is large).

Clone the kernel tree on the host (mounted into the container later):

```bash
mkdir -p ~/xe-linux && cd ~/xe-linux
git clone https://github.com/intel-innersource/drivers.gpu.linux.xe.kernel.git xe-kernel
# huge tree — add --depth 1 if you only need a buildable checkout
```

---

## Part 1 — Isolated container: build the kernel `.deb`

### 1.1 Build image definition


`~/xe-build/Dockerfile`:

```bash
mkdir ~/xe-build
touch ~/xe-build/Dockerfile
```

```dockerfile
FROM ubuntu:24.04

# Match the base distro you'll remaster, so kernel ABI/userspace line up
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      git build-essential fakeroot libncurses-dev bison flex libssl-dev \
      libelf-dev libdw-dev bc rsync kmod cpio dwarves debhelper-compat zstd \
      python3 lsb-release ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Build as a non-root user so output files aren't owned by root on the host.
# ubuntu:24.04 ships a default 'ubuntu' user/group at UID/GID 1000 — remove it
# first so our builder can claim the host's UID/GID without colliding.
ARG UID=1000
ARG GID=1000
RUN userdel -r ubuntu 2>/dev/null || true; \
    groupdel ubuntu 2>/dev/null || true; \
    groupadd -g $GID builder && \
    useradd -m -u $UID -g $GID builder
USER builder
WORKDIR /src
```

### 1.2 Build the image

```bash
cd ~/xe-build
docker build \
  --build-arg http_proxy=http://proxy-dmz.intel.com:911 \
  --build-arg https_proxy=http://proxy-dmz.intel.com:912 \
  --build-arg UID=$(id -u) --build-arg GID=$(id -g) \
  -t xe-kernel-build .
```

### 1.3 Compile (nothing installs on the host)

```bash
mkdir -p ~/xe-build/out

# --- Intel Xe GPU ---
# disable i915 so Xe owns all Intel GPUs
# --- Intel PMT (Platform Monitoring Technology) telemetry ---
# PCI discovery layer (PMT depends on it)
# PMT class driver -> /sys/class/intel_pmt/
# telemetry regions
# crashlog regions (optional)

docker run --rm -it \
  -v ~/xe-linux/xe-kernel:/home/builder/xe-kernel \
  -v ~/xe-build/out:/out \
  -v /boot:/boot:ro \
  xe-kernel-build \
  bash -c '
    cd /home/builder/xe-kernel
    cp /boot/config-$(uname -r) .config 2>/dev/null || make defconfig
    ./scripts/config --module  CONFIG_DRM_XE
    ./scripts/config --disable CONFIG_DRM_I915           
    ./scripts/config --module  CONFIG_INTEL_VSEC         
    ./scripts/config --module  CONFIG_INTEL_PMT_CLASS     
    ./scripts/config --module  CONFIG_INTEL_PMT_TELEMETRY
    ./scripts/config --module  CONFIG_INTEL_PMT_CRASHLOG  
    ./scripts/config --disable SYSTEM_TRUSTED_KEYS
    ./scripts/config --disable SYSTEM_REVOCATION_KEYS
    make olddefconfig
    make -j$(nproc) LOCALVERSION=-xe bindeb-pkg
    cp ../linux-*-xe_*.deb /out/    
  '
```

> **Why not `/src`?** `bindeb-pkg` writes the finished `.deb` files to the
> **parent** of the kernel source dir. With the tree at `/src`, the parent is `/`
> (container root), which the non-root `builder` user can't write to → "Permission
> denied". Mounting under `/home/builder/` puts the output in a builder-owned dir.

Result in `~/xe-build/out/`:

- `linux-image-<ver>-xe_*.deb` — kernel + modules + initramfs hooks
- `linux-headers-<ver>-xe_*.deb` — for out-of-tree modules later

> **Reproducible alternative:** instead of seeding from `/boot`, check a known
> `.config` into your source tree and copy that. Then drop the `-v /boot:/boot:ro`
> mount entirely.

> **Confirm Xe + PMT enabled and i915 disabled** before the long build:
> ```bash
> grep -E "CONFIG_DRM_XE\b|CONFIG_INTEL_VSEC|CONFIG_INTEL_PMT" .config
> grep -E "CONFIG_DRM_I915\b" .config      # expect: # CONFIG_DRM_I915 is not set
> ```
> Xe/PMT lines should show `=m` (or `=y`); i915 should be `is not set`. If
> `olddefconfig` reset a PMT line to `n`, a dependency (`CONFIG_INTEL_VSEC`,
> `CONFIG_PCI`) wasn't set — enable that first, then re-run.

---

## Install strategy — single installer ISO built on Machine A

Machine A has no USB, so we produce **one installer `.iso` file** there. It's the
official Ubuntu Server ISO with two things overlaid:

1. your kernel `.deb`, and
2. an **autoinstall** config that installs Ubuntu + the kernel hands-free.

The ISO is repacked with `xorriso`'s **`-boot_image any replay`**, which copies the
original UEFI+BIOS boot setup verbatim — no boot-structure guesswork (that was the
cause of every earlier repack failure). One validated script does the whole thing.

You then transfer `ubuntu-xe-installer.iso` to Machine B and boot it however that
box accepts media — USB written from any machine, **BMC/IPMI virtual media**, or
PXE. The kernel installs inside the clean installer environment, so the
`GZIP`/initramfs problem from a manual `dpkg` can't occur.

```
Machine A (no USB):
  official ubuntu-...-live-server.iso
        + linux-*-xe.deb                 ──►  ubuntu-xe-installer.iso  (single file)
        + autoinstall (user-data)              ↓ transfer to Machine B
        [xorriso replay = boot preserved]      ↓ boot (USB / BMC virtual media / PXE)
                                               ↓ installs Ubuntu + -xe kernel
                                               ↓ reboot → runs -xe
```

---

## Part 2 — Build the installer ISO (Machine A, in a container)

### 2.1 Start the build container and get the official ISO

```bash
mkdir -p ~/xe-build/iso
docker run --rm -it \
  -v ~/xe-build/out:/debs:ro \
  -v ~/xe-build/iso:/iso \
  ubuntu:24.04 bash
```

Inside the container:

```bash
cat > /etc/apt/apt.conf.d/95proxy <<'EOF'
Acquire::http::Proxy  "http://proxy-dmz.intel.com:911";
Acquire::https::Proxy "http://proxy-dmz.intel.com:912";
EOF
apt-get update && apt-get install -y xorriso whois curl vim

cd /iso
https_proxy=http://proxy-dmz.intel.com:912 \
  curl -LO https://releases.ubuntu.com/24.04/ubuntu-24.04.2-live-server-amd64.iso
```

### 2.2 Save the build script

Create `/iso/build-installer-iso.sh` with this content:

```bash
#!/usr/bin/env bash
# Bake a custom kernel + autoinstall into the official Ubuntu Server ISO,
# producing ONE bootable installer ISO. xorriso "replay" preserves the original
# UEFI+BIOS boot exactly — no boot-structure guesswork.
set -euo pipefail

SRC="${1:?usage: build-installer-iso.sh <official-ubuntu-server.iso> <kernel.deb> [out.iso]}"
DEB="${2:?need path to linux-image-*-xe_*.deb}"
OUT="${3:-/iso/ubuntu-xe-installer.iso}"
PASSWORD="${PASSWORD:-helloworld}"
USERNAME="${USERNAME:-swe}"                                  # must match identity below
PMT_SRC="${PMT_SRC:-/iso/frameworks.gpu.telemetry.pmt-telemetry}"  # dir to bake in

# 0) validate inputs up front — this prevents the empty-/wrong-$SRC bug
[ -s "$SRC" ] || { echo "ERROR: source ISO '$SRC' missing or empty"; exit 1; }
[ -s "$DEB" ] || { echo "ERROR: kernel .deb '$DEB' missing or empty"; exit 1; }
echo "Source ISO : $SRC ($(du -h "$SRC" | cut -f1))"
echo "Kernel deb : $DEB ($(du -h "$DEB" | cut -f1))"

HASH="$(mkpasswd -m sha-512 "$PASSWORD")"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/server" "$WORK/extras"

# 1) kernel onto the ISO
cp "$DEB" "$WORK/extras/"

# 1b) PMT telemetry tooling onto the ISO -> goes to /home/$USERNAME/pmt-telemetry
if [ -d "$PMT_SRC" ]; then
  cp -r "$PMT_SRC" "$WORK/extras/pmt-telemetry"
  echo "PMT source : $PMT_SRC ($(du -sh "$PMT_SRC" | cut -f1))"
else
  echo "ERROR: PMT source dir '$PMT_SRC' not found"; exit 1
fi

# 1c) OFFLINE package bundle (air-gap): download the listed packages AND every
#     package in their TRANSITIVE dependency tree as .debs, so Machine B installs
#     them with NO network. `apt-cache depends --recurse` expands the full closure
#     (e.g. python3-lxml -> libxml2, libxslt1.1, ...); the loop downloads each one.
#     (Runs here on Machine A, which has the proxy.)
APT_PKGS="${APT_PKGS:-python3 python3-pip python3-venv python3-lxml}"
mkdir -p "$WORK/extras/debs"
apt-get update
# resolve the complete dependency closure (names at column 0 = real packages;
# indented / <virtual> lines are skipped by the awk filter)
DEPS="$(apt-cache depends --recurse --no-recommends --no-suggests \
        --no-conflicts --no-breaks --no-replaces --no-enhances $APT_PKGS \
        | awk '/^[a-zA-Z0-9]/{print $1}' | sort -u)"
echo "Resolved $(echo "$DEPS" | wc -w) packages in the dependency closure"
( cd "$WORK/extras/debs"
  for p in $DEPS; do apt-get download "$p" 2>/dev/null || true; done )
echo "Offline debs: $(ls "$WORK/extras/debs"/*.deb 2>/dev/null | wc -l) downloaded for: $APT_PKGS"
[ -n "$(ls "$WORK/extras/debs"/*.deb 2>/dev/null)" ] || { echo "ERROR: no debs downloaded"; exit 1; }

# 2) autoinstall config (NoCloud)
: > "$WORK/server/meta-data"
cat > "$WORK/server/user-data" <<EOF
#cloud-config
autoinstall:
  version: 1
  locale: en_US.UTF-8
  keyboard: {layout: us}
  storage:
    layout:
      name: lvm
      match:
        path: /dev/nvme0n1     # install on /dev/nvme0n1 specifically (WIPES this disk)
  identity:
    hostname: xe-host
    username: $USERNAME
    password: "$HASH"
  ssh: {install-server: true}
  late-commands:
    - cp /cdrom/extras/linux-image-*-xe_*.deb /target/tmp/
    - curtin in-target --target=/target -- apt-get install -y linux-firmware
    - curtin in-target --target=/target -- bash -c 'dpkg -i /tmp/linux-image-*-xe_*.deb || apt-get -y -f install'
    - curtin in-target --target=/target -- update-grub
    # don't block boot waiting for the network to come online
    - curtin in-target --target=/target -- systemctl mask systemd-networkd-wait-online.service
    # copy the offline .debs into the user's home (NOT installed — install later
    # by hand with: sudo dpkg -i ~/offline-debs/*.deb)
    - mkdir -p /target/home/$USERNAME/offline-debs
    - cp /cdrom/extras/debs/*.deb /target/home/$USERNAME/offline-debs/
    - curtin in-target --target=/target -- chown -R $USERNAME:$USERNAME /target/home/$USERNAME/offline-debs
    # drop the PMT telemetry tooling into the user's home, owned by the user
    - cp -r /cdrom/extras/pmt-telemetry /target/home/$USERNAME/pmt-telemetry
    - curtin in-target --target=/target -- chown -R $USERNAME:$USERNAME /target/home/$USERNAME/pmt-telemetry
EOF

# 3) prepend an auto-running install entry to the ISO's existing grub.cfg
#    (keeps the original entries/video setup; ours becomes default=0)
xorriso -osirrox on -indev "$SRC" -extract /boot/grub/grub.cfg "$WORK/grub.orig"
{
  cat <<'MENU'
set timeout=5
set default=0
menuentry "Install Ubuntu Server (XE kernel, autoinstall)" {
    set gfxpayload=keep
    linux  /casper/vmlinuz autoinstall ds=nocloud\;s=/cdrom/server/ ---
    initrd /casper/initrd
}
MENU
  cat "$WORK/grub.orig"
} > "$WORK/grub.cfg"

# 4) empty md5sum.txt so the modified ISO passes the disc integrity check
: > "$WORK/md5sum.txt"

# 5) repack, preserving the original boot setup exactly
xorriso -indev "$SRC" -outdev "$OUT" \
  -boot_image any replay \
  -map "$WORK/extras"     /extras \
  -map "$WORK/server"     /server \
  -map "$WORK/grub.cfg"   /boot/grub/grub.cfg \
  -map "$WORK/md5sum.txt" /md5sum.txt \
  -commit -eject all

echo
echo "Built: $OUT ($(du -h "$OUT" | cut -f1))"
xorriso -indev "$OUT" -report_el_torito plain | grep -i -E 'Volume id|Boot|El Torito' || true
```

### 2.3 Run it

First make sure the PMT directory is present under the mounted `/iso` (the host's
`~/xe-build/iso/`), i.e. `/iso/frameworks.gpu.telemetry.pmt-telemetry`. Then:

```bash
chmod +x /iso/build-installer-iso.sh
mv /iso/ubuntu-xe-installer.iso /iso/ubuntu-xe-installer.iso.bck
PASSWORD='helloworld' USERNAME='swe' \
PMT_SRC=/iso/frameworks.gpu.telemetry.pmt-telemetry \
/iso/build-installer-iso.sh \
  /iso/ubuntu-24.04.3-live-server-amd64.iso \
  /debs/linux-image-*-xe_*.deb \
  /iso/ubuntu-xe-installer.iso
```

`USERNAME` must match the account the installer creates — the script uses it for
both `identity.username` and the `/home/$USERNAME/pmt-telemetry` destination.

> **Air-gapped packages:** step 1c downloads `python3 python3-pip python3-venv
> python3-lxml` **plus their full dependency closure** (incl. `libxml2`,
> `libxslt1.1` for lxml) as `.debs` into the ISO (override the set with
> `APT_PKGS='python3 python3-lxml foo bar'`). The autoinstall **does not install
> them** — it just copies them to `/home/$USERNAME/offline-debs/` on Machine B.
> Install them later, offline, by hand:
> ```bash
> sudo dpkg -i ~/offline-debs/*.deb || sudo dpkg --configure -a
> ```

A correct result prints **`Volume id : Ubuntu-Server 24.04...`** (not `ISOIMAGE`),
a size of **~2–3 GB**, and **El Torito boot** entries. The ISO lands at
`~/xe-build/iso/ubuntu-xe-installer.iso` on the host.

> The `storage:` block installs onto **`/dev/nvme0n1`** and **erases that disk**
> (`match: {path: /dev/nvme0n1}`). Change the path for a different disk, match by
> `serial:` if device names may reorder, or delete the whole `storage:` block to
> partition by hand during install.

---

## Part 3 — Boot the ISO on Machine B

1. **Disable Secure Boot** in BIOS (unsigned kernel won't boot otherwise).
2. Present the ISO to Machine B by whichever method that box supports:
   - **USB:** write the ISO with `dd`/Rufus/balenaEtcher from any machine that has
     a USB port, then boot it; or
   - **BMC/IPMI virtual media** (iDRAC/iLO/Redfish): mount `ubuntu-xe-installer.iso`
     as a virtual CD and boot it — no physical USB needed; or
   - **PXE/netboot.**
3. At the GRUB menu, the **"XE kernel, autoinstall"** entry is the default — it
   runs unattended (no prompts) using the embedded config.
4. The installer wipes/partitions the disk, installs Ubuntu, runs the
   `late-commands` to install firmware + your `-xe` kernel, then reboots into it.

> Kernel install (firmware + `dpkg` + initramfs + GRUB) runs inside the fresh
> target via `curtin in-target` — a clean environment, so no `GZIP`/initramfs
> breakage. The stock Ubuntu kernel stays in GRUB as a fallback.

> **Manual fallback:** pick the second GRUB entry ("manual, no autoinstall") to do
> a normal interactive install, then install the kernel afterward with
> `sudo env -u GZIP dpkg -i linux-image-*-xe_*.deb` (the `env -u GZIP` guards
> against the initramfs/gzip issue you hit).

### Verify after boot

```bash
uname -r                              # should end in -xe
lsmod | grep xe                       # xe module loaded?
lsmod | grep i915                     # should return NOTHING (i915 not built)
sudo dmesg | grep -i -E "xe|drm"      # driver probe messages
ls /sys/class/drm/                    # GPU bound? (card0 should be driven by xe)
readlink /sys/class/drm/card0/device/driver   # should point to .../drivers/xe

# --- Intel PMT telemetry ---
lsmod | grep -E "intel_vsec|pmt"      # intel_vsec, intel_pmt, pmt_telemetry loaded?
sudo dmesg | grep -i -E "vsec|pmt"    # PMT probe messages
ls /sys/class/intel_pmt/              # telemetry<N> entries appear if a PMT region was found
# read raw telemetry (binary):
sudo hexdump -C /sys/class/intel_pmt/telem1/telem | head
```

> If `/sys/class/intel_pmt/` is empty, the modules loaded but the platform
> exposed no PMT region — check that the CPU/PCH actually supports PMT and that
> `intel_vsec` probed the device in `dmesg`.

### (Optional) Decode telemetry in userspace

The kernel exposes **raw** telemetry. To decode it into named counters you need
Intel's userspace project + the per-platform XML metadata:

```bash
# on Machine B (or add to the ISO package list)
git clone https://github.com/intel/Intel-PMT.git
# follow its README to build the decode tool / use the XML maps that match your SoC
```

If you want the decode tool on Machine B, just `apt-get install -y git build-essential`
there and build it after install.

---

## Gotchas

- **Secure Boot must be off** on Machine B (or sign the kernel + enroll a MOK key).
- **Match the kernel `.config` to the ISO's Ubuntu** — seed `.config` from the
  same release (`noble`/24.04) you base the installer ISO on, so the `-xe` kernel's
  modules/userspace line up.
- **Firmware:** the autoinstall `late-commands` install `linux-firmware`, or the
  Xe GPU may fail to probe (missing GuC/HuC blobs). If `dmesg` complains, copy
  blobs into `/lib/firmware/i915/` and `/lib/firmware/xe/`.
- **Keep the stock kernel** as a fallback boot entry until `-xe` is confirmed
  working on Machine B's hardware (the install keeps it automatically).
- **`-dbg` package is huge and optional** — bake only `linux-image-*-xe` into the
  ISO; skip `linux-image-*-xe-dbg` unless you need debug symbols.
- **Validate `$SRC` before repacking** — if the rebuilt ISO shows volume id
  `ISOIMAGE`, ~100 MB size, or "No El Torito", the source ISO path was wrong/empty.
  The build script checks this up front so it fails loudly instead.

---

## Quick reference — one pass

```bash
# === Machine A: build the kernel .deb ===
git clone https://github.com/intel-innersource/drivers.gpu.linux.xe.kernel.git ~/xe-linux/xe-kernel
docker build --build-arg http_proxy=http://proxy-dmz.intel.com:911 \
  --build-arg https_proxy=http://proxy-dmz.intel.com:912 \
  --build-arg UID=$(id -u) --build-arg GID=$(id -g) -t xe-kernel-build ~/xe-build
docker run --rm -v ~/xe-linux/xe-kernel:/home/builder/xe-kernel -v ~/xe-build/out:/out \
  -v /boot:/boot:ro xe-kernel-build bash -c 'cd /home/builder/xe-kernel && ...Part 1.3...'

# === Machine A: build ONE installer ISO (kernel + autoinstall baked in) ===
docker run --rm -it -v ~/xe-build/out:/debs:ro -v ~/xe-build/iso:/iso ubuntu:24.04 bash
#   (inside) apt-get update && apt-get install -y xorriso whois curl
#   (inside) cd /iso && curl -LO https://releases.ubuntu.com/24.04/ubuntu-24.04.3-live-server-amd64.iso
#   (inside) save Part 2.2 script as /iso/build-installer-iso.sh, then:
#   (inside) PASSWORD='helloworld' /iso/build-installer-iso.sh \
#              /iso/ubuntu-24.04.3-live-server-amd64.iso /debs/linux-image-*-xe_*.deb \
#              /iso/ubuntu-xe-installer.iso
# -> result: ~/xe-build/iso/ubuntu-xe-installer.iso  (single file, on the host)

# === Machine B ===
# Disable Secure Boot. Present ubuntu-xe-installer.iso via USB (dd from any machine),
# BMC/IPMI virtual media, or PXE. Boot it -> default GRUB entry autoinstalls
# Ubuntu + -xe kernel -> reboot -> uname -r ends in -xe
```

---

## Appendix A — Optional: quick live-boot test (no disk install)

If you only want to **boot the custom kernel temporarily** to sanity-check Xe/PMT
on Machine B *before* committing to a disk install, you can build a live ISO with
live-build. Note: this runs from USB/RAM and does **not** touch the disk, and
Ubuntu's live-build fork is finicky (needs `--mode ubuntu`, explicit Ubuntu
mirrors, `--apt-http-proxy`, and `gnupg` installed). The installer-ISO path above
is more reliable and gives a permanent on-disk result, so this is only for a fast
smoke test.

## Appendix B — VM build isolation (alternative to Docker)

To isolate the Part 1 build in a full VM instead of a container:

```bash
multipass launch 24.04 --name xe-builder --disk 60G --memory 8G --cpus $(nproc)
multipass shell xe-builder
# install the Part 1.1 build deps, clone the tree, run the Part 1.3 compile
```

Heavier than Docker but fully separated from Machine A's host OS.
