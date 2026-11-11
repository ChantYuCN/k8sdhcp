//! pxe-installer: one static binary that installs and manages a UEFI PXE server
//! (dnsmasq for DHCP/DNS/TFTP, Apache, NFS) that deploys a Clonezilla image to targets, with per-host
//! install tracking.
use pxe_installer::{config, ctl, install};

use std::process::exit;

const USAGE: &str = "\
pxe-installer - UEFI PXE server that deploys a Clonezilla image (dnsmasq + Apache + NFS)

USAGE
  pxe-installer install [--config FILE] [--dry-run] [options]   install packages + create directories, services left stopped
  pxe-installer start   [--config FILE] [--dry-run] [options]   configure network/DHCP/PXE menu/NFS, start the services
  pxe-installer ctl list | status | stop | grub | nfs | reinstall <mac> | done <mac>
                    | binds | bind <mac> <ip> [name] | unbind <mac|ip>
  pxe-installer print-config            show the default config file
  pxe-installer cgi-done | cgi-seed     (internal) Apache callbacks

INSTALL/START OPTIONS  (flags override --config; same names as config keys)
  --iface NAME  --server-ip IP  --prefix N  --dhcp-start IP  --dhcp-end IP
  --lease 24h  --upstream-dns IP  --static-ip  --only-known-hosts
  --target-hostname H        (hostname of a restored host that has none bound to its MAC)
  --image-name NAME          (Clonezilla image restored onto the targets; default cri-golden)
  --image-dir DIR            (image repository, NFS-exported read-only; default /home/partimag)
  --target-disk DISK         (disk of the target that is ERASED and restored; default nvme0n1)
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
