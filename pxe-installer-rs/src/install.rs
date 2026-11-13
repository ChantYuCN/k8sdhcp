//! The `install` command installs dnsmasq + Apache + NFS + the Clonezilla tools and creates the directories,
//! then tells the admin which files to copy where; the `start` command writes the PXE configuration and
//! starts the services. Targets boot Clonezilla Live over PXE and restore a Clonezilla image (served over
//! NFS) onto their disk.
//! It does NOT download or copy the Ubuntu netboot files, Clonezilla Live or the image - the admin does.
use std::fs;
use std::path::Path;

use crate::config::{Config, RootMode};
use crate::ctl;
use crate::render::{self, CGI_PATH, CGI_SEED_PATH, CTL_WRAPPER_PATH, SELF_PATH};
use crate::util::*;

/// dnsmasq/apache2/nfs-kernel-server are the PXE services; curl is used by the checks and unzip unpacks the
/// Clonezilla Live zip; clonezilla/partclone/pigz/zstd let this host capture the golden image (`ocs-sr`).
const PACKAGES: [&str; 9] =
    ["dnsmasq", "apache2", "curl", "nfs-kernel-server", "clonezilla", "partclone", "pigz", "zstd", "unzip"];

/// Where the admin puts Clonezilla Live: kernel + initrd always under the TFTP root; the live filesystem
/// depends on the deploy way (ROOT_MODE): under the web root (tmpfs: the target fetches it over HTTP into
/// RAM) or inside the extracted Clonezilla Live folder `live_root` (nfs: the target mounts it).
fn cz_tftp_dir(c: &Config) -> String {
    format!("{}/clonezilla", c.tftp_root)
}
fn cz_web_dir(c: &Config) -> String {
    format!("{}/clonezilla", c.web_root)
}
fn squashfs_path(c: &Config) -> String {
    match c.root_mode {
        RootMode::Tmpfs => format!("{}/filesystem.squashfs", cz_web_dir(c)),
        RootMode::Nfs => format!("{}/live/filesystem.squashfs", c.live_root),
    }
}

/// Print a note for every file the chosen deploy way needs that is not there yet (the admin copies them
/// by hand, so these are notes, not errors). The image itself is reported by `setup_nfs`.
pub fn report_missing(c: &Config) {
    for (path, what) in [
        (format!("{}/bootx64.efi", c.tftp_root), "UEFI boot files from the netboot tarball"),
        (format!("{}/vmlinuz", cz_tftp_dir(c)), "Clonezilla Live vmlinuz"),
        (format!("{}/initrd.img", cz_tftp_dir(c)), "Clonezilla Live initrd.img"),
        (
            squashfs_path(c),
            match c.root_mode {
                RootMode::Tmpfs => "Clonezilla Live filesystem.squashfs",
                RootMode::Nfs => "extract the whole Clonezilla Live zip into LIVE_ROOT",
            },
        ),
    ] {
        if !Path::new(&path).exists() {
            println!("note: {path} not present yet ({what})");
        }
    }
}

/// `validate`: `start` needs a complete, valid config; `install` only needs the paths.
fn prepare(c: &Config, validate: bool) -> Result<Ctx, String> {
    if validate {
        let errs = c.validate();
        if !errs.is_empty() {
            return Err(format!("invalid configuration:\n  - {}", errs.join("\n  - ")));
        }
    }
    let ctx = Ctx { dry: c.dry_run };
    if !ctx.dry && output("id", &["-u"])? != "0" {
        return Err("must run as root (sudo)".into());
    }
    if ctx.dry {
        println!("[dry-run] nothing will be changed; showing what would be done");
    }
    Ok(ctx)
}

/// What the admin has to copy by hand and where it goes (this installer downloads and copies none of it).
/// Shown at the end of `install` and `start`. Both deploy ways are listed so `pxe-ctl mode` can switch.
fn manual_steps(c: &Config) -> String {
    let tftp = &c.tftp_root;
    let cz_tftp = cz_tftp_dir(c);
    let cz_web = cz_web_dir(c);
    let live_root = &c.live_root;
    let (img, dir) = (&c.image_name, &c.image_dir);
    format!(
        "COPY THESE FILES BY HAND (this installer does not download or copy them):\n\
         \x20 1. Ubuntu netboot tarball (e.g. ubuntu-26.04.1-netboot-amd64.tar.gz from releases.ubuntu.com):\n\
         \x20      tar -xzf ubuntu-*-netboot-amd64.tar.gz\n\
         \x20      sudo cp -r amd64/* {tftp}/\n\
         \x20      (the CONTENT of its amd64/ folder: bootx64.efi, grubx64.efi, grub/ ...)\n\
         \x20 2. Clonezilla Live amd64 zip (e.g. clonezilla-live-3.3.3-37-amd64.zip from clonezilla.org):\n\
         \x20      unzip clonezilla-live-*-amd64.zip -d czlive\n\
         \x20      sudo cp czlive/live/vmlinuz czlive/live/initrd.img {cz_tftp}/\n\
         \x20      then, for the deploy way you use (`pxe-ctl mode tmpfs|nfs` switches between them):\n\
         \x20      tmpfs: sudo cp czlive/live/filesystem.squashfs {cz_web}/\n\
         \x20      nfs:   sudo cp -a czlive/. {live_root}/   (the WHOLE zip content, incl. live/filesystem.squashfs)\n\
         \x20 3. sudo pxe-ctl grub     (once `start` has run, and again after every copy into {tftp}/: the netboot\n\
         \x20    tarball ships its own grub.cfg; this re-applies ours and fixes the file permissions)\n\
         \x20 4. The Clonezilla image \"{img}\" -> {dir}/{img}/\n\
         \x20      sudo pxe-ctl clone <disk>   (captures a disk of this server) or copy an existing image there,\n\
         \x20      then: sudo pxe-ctl nfs"
    )
}

/// `install`: packages and directories only. Nothing is downloaded or configured and the services are
/// left stopped; it ends by telling the admin which files to copy where, and `start` does the configuration.
pub fn install(c: &Config) -> Result<(), String> {
    let ctx = prepare(c, false)?;

    step("Installing packages");
    run(ctx, "apt-get", &["update", "-y"])?;
    let mut apt = vec!["install", "-y"];
    apt.extend(PACKAGES);
    run(ctx, "apt-get", &apt)?;

    step("Creating directories");
    for dir in [c.tftp_root.clone(), cz_tftp_dir(c), cz_web_dir(c), c.image_dir.clone(), c.live_root.clone()] {
        mkdir(ctx, &dir)?;
        set_mode_soft(ctx, &dir, 0o755);
    }

    step("Checking installation");
    if !ctx.dry {
        check_packages()?;
        for svc in ["dnsmasq", "apache2"] {
            println!("{svc}: {}", if ctl::service_active(svc) { "running" } else { "not running" });
        }
    }

    step("Stopping services");
    run(ctx, "systemctl", &["stop", "dnsmasq", "apache2", "nfs-server"])?;

    if !ctx.dry {
        report_missing(c);
    }
    println!(
        "\nPXE packages are installed and the services are stopped.\n\n{}\n\n\
         Then set the network/DHCP/target options and click \"Start PXE server\" (or: pxe-installer start).",
        manual_steps(c)
    );
    Ok(())
}

fn check_packages() -> Result<(), String> {
    let missing: Vec<&str> = PACKAGES.iter().copied().filter(|p| !probe("dpkg", &["-s", p])).collect();
    if missing.is_empty() {
        println!("packages installed: {}", PACKAGES.join(", "));
        Ok(())
    } else {
        Err(format!("not installed: {}. Run the Install step first.", missing.join(", ")))
    }
}

/// `start`: verify the packages, write all configuration, then (re)start dnsmasq + Apache.
pub fn start(c: &Config) -> Result<(), String> {
    let ctx = prepare(c, true)?;

    if c.only_known_hosts && c.bindings.is_empty() {
        warn("ONLY_KNOWN_HOSTS is on but no bindings were given: no host will get an address until you bind MACs (Manage tab / pxe-ctl bind)");
    }

    step("Checking dnsmasq and Apache are installed");
    if !ctx.dry {
        check_packages()?;
    }

    steps(c, ctx)?;

    println!(
        "\nPXE services are running ({} / {}-{}). Deploy way: {}.\n\n{}\n\n\
         Then boot the target in UEFI mode with \"PXE IPv4\" as first boot device. It restores the image\n\
         onto /dev/{} (ERASES THE DISK) and reboots.\n\
         Duplicate-install guard: restored hosts are recorded in {}/<mac>\n\
         \x20 sudo pxe-ctl status | list | reinstall <mac> | done <mac>\n\
         \x20 sudo pxe-ctl binds | bind <mac> <ip> [name] | unbind <mac|ip>\n\
         Watch requests with: journalctl -fu dnsmasq",
        c.server_ip,
        c.dhcp_start,
        c.dhcp_end,
        match c.root_mode {
            RootMode::Tmpfs => "tmpfs (the live system is downloaded into the target's RAM)",
            RootMode::Nfs => "nfs (the target mounts the live system from the server)",
        },
        manual_steps(c),
        c.target_disk,
        c.done_dir()
    );
    Ok(())
}

fn steps(c: &Config, ctx: Ctx) -> Result<(), String> {
    // ---- 1. server network (optional) ----
    if c.set_static_ip {
        step(&format!("Configuring static IP {}/{} on {}", c.server_ip, c.prefix, c.iface));
        write_file(ctx, "/etc/netplan/60-pxe.yaml", &render::netplan(c), 0o600, false)?;
        run(ctx, "netplan", &["apply"])?;
    } else if !ctx.dry {
        let have = output("ip", &["-4", "addr", "show", "dev", &c.iface]).unwrap_or_default();
        if !have.contains(&format!("inet {}/", c.server_ip)) {
            warn(&format!(
                "{} does not have {}. Set it manually or use --static-ip.",
                c.iface, c.server_ip
            ));
        }
    }

    // ---- 3. free port 53 (keep systemd-resolved, only drop its stub listener) ----
    if ctx.dry || ctl::service_active("systemd-resolved") {
        step("Disabling systemd-resolved stub listener");
        write_file(
            ctx,
            "/etc/systemd/resolved.conf.d/nostub.conf",
            "[Resolve]\nDNSStubListener=no\n",
            0o644,
            false,
        )?;
        run(ctx, "ln", &["-sf", "/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"])?;
        run(ctx, "systemctl", &["restart", "systemd-resolved"])?;
    }

    // ---- 4. dnsmasq ----
    step("Writing dnsmasq config");
    ensure_global_dnsmasq(ctx)?;
    mkdir(ctx, "/etc/dnsmasq.d")?;
    write_file(ctx, render::DNSMASQ_PXE_PATH, &render::dnsmasq_conf(c), 0o644, false)?;
    if !ctx.dry && !Path::new(&c.hosts_file).exists() {
        write_file(ctx, &c.hosts_file, "", 0o644, false)?;
    }
    for b in &c.bindings {
        if ctx.dry {
            println!("[dry-run] bind {} -> {} {}", b.mac, b.ip, b.hostname);
        } else {
            ctl::bind(c, &b.mac, &b.ip, &b.hostname, false)?;
        }
    }
    run(ctx, "dnsmasq", &["--test"])?;

    // ---- 5. TFTP root skeleton: grub.cfg with the duplicate-install guard + marker dir ----
    step("Writing grub.cfg (duplicate-install guard)");
    mkdir(ctx, &c.tftp_root)?;
    ctl::write_grub(c, ctx)?;

    // ---- 6. helper commands (this binary copies itself so cgi/ctl keep working) ----
    step("Installing helper commands");
    if ctx.dry {
        println!("[dry-run] copy this binary to {SELF_PATH}");
    } else {
        let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        if me.as_path() != Path::new(SELF_PATH) {
            fs::copy(&me, SELF_PATH).map_err(|e| format!("copy to {SELF_PATH}: {e}"))?;
        }
        set_mode(SELF_PATH, 0o755)?;
    }
    write_file(ctx, CTL_WRAPPER_PATH, &render::ctl_wrapper(), 0o755, false)?;
    write_file(ctx, crate::config::RUNTIME_CONF, &c.runtime_conf(), 0o644, false)?;

    // ---- 7. Apache: Clonezilla web dir, "done" callback, per-MAC post-restore script ----
    step("Configuring Apache");
    let cz_web = cz_web_dir(c);
    mkdir(ctx, &cz_web)?;
    set_mode_soft(ctx, &cz_web, 0o755);
    write_file(ctx, CGI_PATH, &render::cgi_wrapper(), 0o755, false)?;
    write_file(ctx, CGI_SEED_PATH, &render::cgi_seed_wrapper(), 0o755, false)?;
    write_file(ctx, "/etc/apache2/conf-available/pxe-done.conf", &render::apache_conf(c), 0o644, false)?;
    run(ctx, "a2enmod", &["cgi"])?;
    run(ctx, "a2enconf", &["serve-cgi-bin", "pxe-done"])?;

    // ---- 8. NFS export of the Clonezilla image repository ----
    step("Configuring NFS export of the Clonezilla images");
    setup_nfs(c, ctx)?;

    // ---- 9. enable + start services ----
    step("Starting services");
    run(ctx, "systemctl", &["enable", "apache2", "dnsmasq", "nfs-server"])?;
    run(ctx, "systemctl", &["restart", "apache2", "dnsmasq", "nfs-server"])?;

    if ctx.dry || probe("sh", &["-c", "command -v ufw >/dev/null && ufw status | grep -q 'Status: active'"]) {
        for (port, proto) in [
            ("67", "udp"),
            ("69", "udp"),
            ("53", "any"),
            ("80", "tcp"),
            ("111", "any"),
            ("2049", "any"),
            (NFS_MOUNTD_PORT, "any"),
        ] {
            let mut a = vec!["allow", "in", "on", c.iface.as_str(), "to", "any", "port", port];
            if proto != "any" {
                a.push("proto");
                a.push(proto);
            }
            run_soft(ctx, "ufw", &a);
        }
    }

    // ---- 10. checks ----
    step("Checks");
    if !ctx.dry {
        for svc in ["dnsmasq", "apache2"] {
            println!("{svc}: {}", if ctl::service_active(svc) { "active" } else { "NOT ACTIVE" });
        }
        let base = format!("http://{}", c.server_ip);
        // Apache was only just restarted, so give it a moment before calling it unreachable.
        // The post-restore script is generated by the pxe-seed CGI, so this also proves the CGI works.
        let url = format!("{base}/cgi-bin/pxe-seed/aa:bb:cc:dd:ee:ff/post.sh");
        let mut last = String::new();
        let ok = (0..5).any(|i| {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            match std::process::Command::new("curl").args(["-sS", "-f", "--max-time", "5", "-o", "/dev/null", &url]).output() {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    last = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    false
                }
                Err(e) => {
                    last = e.to_string();
                    false
                }
            }
        });
        if !ok {
            warn(&format!("post-restore script not reachable at {url}: {last}"));
            let have = output("ip", &["-4", "addr", "show", "dev", &c.iface]).unwrap_or_default();
            if !have.contains(&format!("inet {}/", c.server_ip)) {
                warn(&format!(
                    "cause: {} does not have {} (tick the static-IP option or set it manually)",
                    c.iface, c.server_ip
                ));
            }
        }
        let cgi = output("curl", &["-s", &format!("{base}/cgi-bin/pxe-done?mac=bad")]).unwrap_or_default();
        if !cgi.contains("bad mac") {
            warn("pxe-done CGI not working");
        }
        // not errors: the admin copies these by hand after the install (the image is noted by setup_nfs)
        report_missing(c);
    }
    Ok(())
}

const NFS_MOUNTD_PORT: &str = "20048";
const FSTAB: &str = "/etc/fstab";
const FSTAB_MARK: &str = "# pxe-installer";

/// Export over NFS, read-only, to the PXE subnet: the Clonezilla image repository (`image_dir`, mounted by
/// the target on /home/partimag) and, in the `nfs` deploy way, the extracted Clonezilla Live folder
/// (`live_root`, the diskless root of the target). The files themselves may not exist yet; `pxe-ctl nfs`
/// and `pxe-ctl mode` re-run this (and report what is missing) once they have been added.
pub fn setup_nfs(c: &Config, ctx: Ctx) -> Result<(), String> {
    let net = c.network().map(|n| format!("{n}/{}", c.prefix)).unwrap_or_default();
    let opts = "ro,no_subtree_check,no_root_squash,async";

    mkdir(ctx, &c.image_dir)?;
    let mut exports = format!("{} {net}({opts})\n", c.image_dir);
    if c.root_mode == RootMode::Nfs {
        mkdir(ctx, &c.live_root)?;
        exports.push_str(&format!("{} {net}({opts})\n", c.live_root));
    }
    drop_old_iso_mount(ctx)?;

    // pin mountd so the firewall rule is predictable (the Clonezilla initrd mounts with NFSv3)
    write_file(ctx, "/etc/nfs.conf.d/pxe-installer.conf", &format!("[mountd]\nport={NFS_MOUNTD_PORT}\n"), 0o644, false)?;
    write_file(ctx, "/etc/exports.d/pxe-installer.exports", &exports, 0o644, false)?;
    run(ctx, "exportfs", &["-ra"])?;

    if !ctx.dry && !Path::new(&format!("{}/{}", c.image_dir, c.image_name)).is_dir() {
        println!(
            "note: image {}/{} not present yet - create it with ocs-sr savedisk or copy it there",
            c.image_dir, c.image_name
        );
    }
    Ok(())
}

/// Older versions loop-mounted the live-server ISO through an fstab line of ours. Remove that line so the
/// stale mount does not come back after a reboot (the mount point itself is left alone).
fn drop_old_iso_mount(ctx: Ctx) -> Result<(), String> {
    if ctx.dry {
        return Ok(());
    }
    let Ok(old) = fs::read_to_string(FSTAB) else { return Ok(()) };
    if !old.lines().any(|l| l.ends_with(FSTAB_MARK)) {
        return Ok(());
    }
    let text: String = old.lines().filter(|l| !l.ends_with(FSTAB_MARK)).map(|l| format!("{l}\n")).collect();
    fs::write(FSTAB, text).map_err(|e| format!("write {FSTAB}: {e}"))?;
    run_soft(ctx, "systemctl", &["daemon-reload"]);
    println!("removed the old ISO loop-mount line from {FSTAB}");
    Ok(())
}

const DNSMASQ_GLOBAL: &str = "/etc/dnsmasq.conf";
const CONF_DIR_LINE: &str = "conf-dir=/etc/dnsmasq.d/,*.conf";

/// /etc/dnsmasq.conf stays the global config; the PXE settings live in /etc/dnsmasq.d/pxe.conf.
/// This only makes sure the global file loads /etc/dnsmasq.d, and undoes older versions of this
/// installer that replaced the whole file.
fn ensure_global_dnsmasq(ctx: Ctx) -> Result<(), String> {
    let bak = format!("{DNSMASQ_GLOBAL}.bak");
    let current = if ctx.dry { None } else { fs::read_to_string(DNSMASQ_GLOBAL).ok() };

    if current.as_deref().is_some_and(|t| t.starts_with("# generated by pxe-installer")) {
        // old layout: our generated file was the global one, the original is in .bak
        if Path::new(&bak).exists() {
            fs::rename(&bak, DNSMASQ_GLOBAL).map_err(|e| format!("restore {bak}: {e}"))?;
        } else {
            write_file(ctx, DNSMASQ_GLOBAL, &format!("{CONF_DIR_LINE}\n"), 0o644, false)?;
        }
    }

    let text = if ctx.dry { String::new() } else { fs::read_to_string(DNSMASQ_GLOBAL).unwrap_or_default() };
    let loads_dir = text.lines().any(|l| {
        let l = l.trim();
        l.starts_with("conf-dir=") && l.contains("/etc/dnsmasq.d")
    });
    if ctx.dry || !loads_dir {
        let mut body = text;
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(CONF_DIR_LINE);
        body.push('\n');
        write_file(ctx, DNSMASQ_GLOBAL, &body, 0o644, false)?;
    }
    Ok(())
}

fn set_mode_soft(ctx: Ctx, path: &str, mode: u32) {
    if ctx.dry {
        println!("[dry-run] chmod {mode:o} {path}");
    } else if let Err(e) = set_mode(path, mode) {
        warn(&e);
    }
}
