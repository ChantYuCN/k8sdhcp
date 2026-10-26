//! pxe-installer: one static binary that installs and manages a UEFI PXE server
//! (dnsmasq for DHCP/DNS/TFTP + Apache for ISO/autoinstall) with per-host install tracking.
use pxe_installer::{config, ctl, install};

use std::process::exit;

const USAGE: &str = "\
pxe-installer - UEFI PXE server installer (dnsmasq + Apache)

USAGE
  pxe-installer install [--config FILE] [--dry-run] [options]
  pxe-installer ctl list | status | reinstall <mac> | done <mac>
                    | binds | bind <mac> <ip> [name] | unbind <mac|ip>
  pxe-installer print-config            show the default config file
  pxe-installer cgi-done                (internal) Apache callback

INSTALL OPTIONS  (flags override --config; same names as config keys)
  --iface NAME  --server-ip IP  --prefix N  --dhcp-start IP  --dhcp-end IP
  --lease 24h  --upstream-dns IP  --static-ip  --only-known-hosts
  --target-hostname H  --target-user U  --release-url URL  --netboot-tar F  --iso-name F
  --host mac,ip[,hostname]   (repeatable)
  --dry-run                  print every action and generated file, change nothing

The target password is read from the config file (TARGET_PASSWORD=...) or the
PXE_TARGET_PASSWORD environment variable - never pass it on the command line.
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
TARGET_USER=ubuntu
TARGET_PASSWORD=ChangeMe-please

RELEASE_URL=https://old-releases.ubuntu.com/releases/plucky
NETBOOT_TAR=ubuntu-25.04-netboot-amd64.tar.gz
ISO_NAME=ubuntu-25.04-beta-live-server-amd64.iso

# fixed MAC -> IP bindings (repeat HOST=)
# HOST=aa:bb:cc:dd:ee:01,192.168.5.11,node01
";

fn run(args: &[String]) -> Result<(), String> {
    match args.get(1).map(|s| s.as_str()) {
        Some("install") => {
            let cfg = config::Config::from_args(&args[2..])?;
            install::install(&cfg)
        }
        Some("ctl") => ctl::ctl(&args[2..]),
        Some("cgi-done") => ctl::cgi_done(),
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
