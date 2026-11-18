//! pxe-installer: one static binary that installs and manages a UEFI PXE server
//! (dnsmasq for DHCP/TFTP, Apache, NFS) that deploys a Clonezilla image to targets, with per-host
//! install tracking.
use pxe_installer::{config, ctl, install};

use std::process::exit;

const USAGE: &str = "\
pxe-installer - UEFI PXE server that deploys a Clonezilla image (dnsmasq + Apache + NFS)

USAGE
  pxe-installer install [--config FILE] [--dry-run] [options]   install packages (incl. clonezilla, partclone, pigz, zstd)
                                                                 + create directories, services left stopped; then it lists
                                                                 the netboot / Clonezilla Live files YOU copy by hand and where
  pxe-installer start   [--config FILE] [--dry-run] [options]   configure network/DHCP/PXE menu/NFS, start the services
  pxe-installer ctl list | status | times | stop | grub | import | auto [on|off] | nfs | mode [tmpfs|nfs] | clone <disk> [--yes] [--force]
                    | reinstall <mac> | done <mac>
                    | binds | bind <mac> <ip> [name] | unbind <mac|ip>
                    (clone = interactive fsck of each partition, then ocs-sr, on this terminal;
                     --yes = no prompts, --force = replace an existing image)
  pxe-installer print-config            show the default config file
  pxe-installer cgi-done | cgi-seed     (internal) Apache callbacks

INSTALL/START OPTIONS  (flags override --config; same names as config keys)
  --iface NAME  --server-ip IP  --prefix N  --dhcp-start IP  --dhcp-end IP
  --lease 24h  --upstream-dns IP  --static-ip  --only-known-hosts
  --target-hostname H        (hostname of a restored host that has none bound to its MAC)
  --image-name NAME          (Clonezilla image restored onto the targets; default cri-golden)
  --image-dir DIR            (image repository, NFS-exported read-only; default /home/partimag)
  --target-disk DISK         (disk of the target that is ERASED and restored; default nvme0n1)
  --root-mode tmpfs|nfs      (deploy way: tmpfs = Clonezilla Live downloaded into the target's RAM over HTTP,
                              nfs = the target mounts Clonezilla Live from the server; default tmpfs;
                              switch later with `pxe-ctl mode tmpfs|nfs`)
  --live-root DIR            (nfs way: extracted Clonezilla Live zip, exported read-only; default /srv/nfs/clonezilla)
  --netboot-dir DIR          (holds ubuntu-*-netboot-amd64.tar.gz, unpacked into the TFTP root by install;
                              default `netboot`: relative = next to this program, else the current folder, else /boot)
  --livesystem-dir DIR       (holds clonezilla-live-*-amd64.zip, unpacked by install; default `livesystem`, same lookup)
  --host mac,ip[,hostname]   (repeatable)
  --dry-run                  print every action and generated file, change nothing
";

const SAMPLE_CONFIG: &str = "\
# pxe-installer config: KEY=VALUE, '#' comments
IFACE=eth0
SERVER_IP=192.168.5.1
PREFIX=24
DHCP_START=192.168.5.100
DHCP_END=192.168.5.200
LEASE=24h
UPSTREAM_DNS=8.8.8.8
SET_STATIC_IP=0
ONLY_KNOWN_HOSTS=0

TARGET_HOSTNAME=ubuntu-node

# Clonezilla image (a folder inside IMAGE_DIR, made with ocs-sr savedisk) restored onto TARGET_DISK of
# every target that PXE boots. TARGET_DISK is ERASED. The installer does not create or copy the image.
IMAGE_NAME=cri-golden
IMAGE_DIR=/home/partimag
TARGET_DISK=nvme0n1

# How the target gets Clonezilla Live (the PXE deploy way); switch later with: sudo pxe-ctl mode tmpfs|nfs
#   tmpfs = live/filesystem.squashfs is downloaded over HTTP into the target's RAM (needs ~4 GB RAM)
#   nfs   = the target mounts the extracted Clonezilla Live zip from LIVE_ROOT over NFS (diskless root)
ROOT_MODE=tmpfs
LIVE_ROOT=/srv/nfs/clonezilla

# Nothing is downloaded. Put the two files into these folders; the last step of `install` unpacks them:
#   NETBOOT_DIR     ubuntu-*-netboot-amd64.tar.gz : its amd64/ folder (bootx64.efi, grubx64.efi, grub/ ...) -> the TFTP root
#   LIVESYSTEM_DIR  clonezilla-live-*-amd64.zip   : live/vmlinuz + live/initrd.img -> <tftp-root>/clonezilla/,
#                   live/filesystem.squashfs -> <web-root>/clonezilla/ (tmpfs way), the whole zip -> LIVE_ROOT (nfs way)
# then it writes grub.cfg (= sudo pxe-ctl grub). Repeat later with: sudo pxe-ctl import
# A relative name is looked up next to the pxe-installer program, then in the current folder, then under /boot;
# an absolute path (e.g. /boot/netboot) is used as given.
NETBOOT_DIR=netboot
LIVESYSTEM_DIR=livesystem

# 1 = the restore entry is the default: a PXE-booted host is ERASED and restored without any question
# 0 = the PXE menu starts interactive Clonezilla; pick the restore entry by hand (safer for first tests)
AUTO_INSTALL=1

# fixed MAC -> IP bindings (repeat HOST=)
# HOST=aa:bb:cc:dd:ee:01,192.168.5.11,node01
";

fn run(args: &[String]) -> Result<(), String> {
    match args.get(1).map(|s| s.as_str()) {
        Some("install") => {
            let cfg = config::Config::from_args(&args[2..])?;
            install::install(&cfg)
        }
        Some("start") => {
            let cfg = config::Config::from_args(&args[2..])?;
            install::start(&cfg)
        }
        Some("ctl") => ctl::ctl(&args[2..]),
        Some("cgi-done") => ctl::cgi_done(),
        Some("cgi-seed") => ctl::cgi_seed(),
        Some("print-config") => {
            print!("{SAMPLE_CONFIG}");
            Ok(())
        }
        Some("-h") | Some("--help") | Some("help") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some("-V") | Some("--version") => {
            println!("pxe-installer {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        exit(1);
    }
}
