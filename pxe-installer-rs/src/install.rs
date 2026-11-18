//! The `install` command installs Kea (DHCP) + tftpd-hpa (TFTP) + Apache + NFS + the Clonezilla tools, creates the directories and
//! unpacks the Ubuntu netboot tarball (NETBOOT_DIR) and the Clonezilla Live zip (LIVESYSTEM_DIR) into place;
//! the `start` command writes the PXE configuration and starts the services. Targets boot Clonezilla Live over
//! PXE and restore a Clonezilla image (served over NFS) onto their disk.
//! It does NOT download anything, and the Clonezilla image itself is the admin's (`pxe-ctl clone` or a copy).
use std::fs;
use std::path::Path;
use std::time::Duration;

use crate::config::{resolve_dir, CastMode, Config, RootMode};
use crate::ctl;
use crate::render::{self, CGI_PATH, CGI_SEED_PATH, CTL_WRAPPER_PATH, SELF_PATH};
use crate::util::*;

/// kea-dhcp4-server (DHCP), tftpd-hpa (TFTP), apache2 and nfs-kernel-server are the PXE services; curl is used
/// by the checks and unzip unpacks the Clonezilla Live zip; clonezilla/partclone/pigz/zstd let this host
/// capture the golden image (`ocs-sr`).
const PACKAGES: [&str; 10] = [
    "kea-dhcp4-server",
    "tftpd-hpa",
    "apache2",
    "curl",
    "nfs-kernel-server",
    "clonezilla",
    "partclone",
    "pigz",
    "zstd",
    "unzip",
];

/// Multicast mode (DRBL + Clonezilla SE) needs these. They are installed one by one AFTER the required ones and a
/// failure is only a warning, so a repository without them (or a DRBL postinst problem) never breaks the unicast
/// install. `clonezilla` itself may already pull `drbl` in.
const OPTIONAL_PACKAGES: [&str; 2] = ["drbl", "udpcast"];

/// DHCP, TFTP and HTTP: the services a target needs to PXE boot (NFS is separate: it only serves images).
const PXE_SERVICES: [&str; 3] = [render::DHCP_SERVICE, render::TFTP_SERVICE, "apache2"];

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
        "FILES THE INSTALLER COULD NOT IMPORT (it unpacks them from the folders NETBOOT_DIR / LIVESYSTEM_DIR,\n\
         \x20 `sudo pxe-ctl import` repeats that; if a note above says a file is missing, copy it by hand):\n\
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

/// First file in `dir` named `<prefix>*<suffix>` (the highest name = the newest version), if there is one.
fn find_archive(dir: &str, prefix: &str, suffix: &str) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(prefix) && n.ends_with(suffix))
        .collect();
    names.sort();
    names.pop().map(|n| format!("{dir}/{n}"))
}

/// Scratch folder that is removed again when it goes out of scope.
struct TempDir {
    path: String,
    dry: bool,
}
impl TempDir {
    fn new(ctx: Ctx, path: String) -> Result<Self, String> {
        mkdir(ctx, &path)?;
        Ok(TempDir { path, dry: ctx.dry })
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.dry {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// `sudo tar -xzf ubuntu-*-netboot-amd64.tar.gz` + `sudo cp -r amd64/* <tftp root>/`, with the tarball taken
/// from NETBOOT_DIR and unpacked in a scratch folder. Returns false (with a note) when there is no tarball.
fn import_netboot(c: &Config, ctx: Ctx) -> Result<bool, String> {
    let dir = resolve_dir(&c.netboot_dir);
    let Some(tar) = find_archive(&dir, "ubuntu-", "-netboot-amd64.tar.gz") else {
        println!("note: no ubuntu-*-netboot-amd64.tar.gz in {dir} (NETBOOT_DIR): copy the netboot files by hand");
        return Ok(false);
    };
    println!("netboot tarball: {tar}");
    let tmp = TempDir::new(ctx, format!("/var/tmp/pxe-installer-netboot-{}", std::process::id()))?;
    run(ctx, "tar", &["-xzf", &tar, "-C", &tmp.path])?;
    let src = format!("{}/amd64", tmp.path);
    if !ctx.dry && !Path::new(&src).is_dir() {
        return Err(format!("{tar} has no amd64/ folder: is it the Ubuntu amd64 netboot tarball?"));
    }
    mkdir(ctx, &c.tftp_root)?;
    run(ctx, "cp", &["-r", &format!("{src}/."), &c.tftp_root])?;
    Ok(true)
}

/// `unzip clonezilla-live-*-amd64.zip` + copy of what the deploy way needs, with the zip taken from
/// LIVESYSTEM_DIR: vmlinuz + initrd.img -> <tftp root>/clonezilla/ always; then, depending on ROOT_MODE, only
/// live/filesystem.squashfs -> <web root>/clonezilla/ (tmpfs) or the whole zip -> LIVE_ROOT (nfs).
/// Members are extracted straight to their destination (no scratch copy of the ~0.5 GB squashfs).
/// Returns false (with a note) when there is no zip.
fn import_live(c: &Config, ctx: Ctx) -> Result<bool, String> {
    let dir = resolve_dir(&c.livesystem_dir);
    let Some(zip) = find_archive(&dir, "clonezilla-live-", "-amd64.zip") else {
        println!("note: no clonezilla-live-*-amd64.zip in {dir} (LIVESYSTEM_DIR): copy Clonezilla Live by hand");
        return Ok(false);
    };
    println!("Clonezilla Live zip: {zip}");
    let tftp = cz_tftp_dir(c);
    mkdir(ctx, &tftp)?;
    // -o: overwrite without asking, -q: quiet, -j: no folder names (live/vmlinuz -> <tftp>/vmlinuz)
    run(ctx, "unzip", &["-oqj", &zip, "live/vmlinuz", "live/initrd.img", "-d", &tftp])?;
    match c.root_mode {
        RootMode::Tmpfs => {
            let web = cz_web_dir(c);
            mkdir(ctx, &web)?;
            run(ctx, "unzip", &["-oqj", &zip, "live/filesystem.squashfs", "-d", &web])?;
        }
        RootMode::Nfs => {
            mkdir(ctx, &c.live_root)?;
            run(ctx, "unzip", &["-oq", &zip, "-d", &c.live_root])?;
        }
    }
    Ok(true)
}

/// Used by `start` and `pxe-ctl mode`: the live filesystem of the chosen deploy way is not there yet (the way
/// was chosen or switched after `install`) -> unpack it from the Clonezilla Live zip, if that is available.
pub fn import_live_if_missing(c: &Config, ctx: Ctx) -> Result<(), String> {
    if ctx.dry || Path::new(&squashfs_path(c)).exists() {
        return Ok(());
    }
    step("Unpacking Clonezilla Live for the chosen deploy way");
    if import_live(c, ctx)? {
        ctl::fix_perms(c, ctx)?;
    }
    Ok(())
}

/// Both imports, then what `sudo pxe-ctl grub` does: the netboot tarball ships its own grub.cfg that has just
/// replaced ours, so ours is written again, and the owner/mode of everything under the TFTP root is fixed.
/// Used by `install` (end of the process) and `pxe-ctl import`. Missing archives are notes, not errors.
pub fn import_boot_files(c: &Config, ctx: Ctx) -> Result<(), String> {
    let netboot = import_netboot(c, ctx)?;
    let live = import_live(c, ctx)?;
    if netboot || live {
        step("Writing grub.cfg and fixing permissions");
        ctl::write_grub(c, ctx)?;
    }
    Ok(())
}

/// `install`: packages and directories, then the netboot / Clonezilla Live files are unpacked from
/// NETBOOT_DIR / LIVESYSTEM_DIR into place. Nothing is downloaded; the services are left stopped and `start`
/// does the configuration. What is still missing is listed at the end.
pub fn install(c: &Config) -> Result<(), String> {
    let ctx = prepare(c, false)?;

    step("Installing packages");
    install_packages(ctx)?;

    step("Creating directories");
    for dir in [c.tftp_root.clone(), cz_tftp_dir(c), cz_web_dir(c), c.image_dir.clone(), c.live_root.clone()] {
        mkdir(ctx, &dir)?;
        set_mode_soft(ctx, &dir, 0o755);
    }

    step("Checking installation");
    if !ctx.dry {
        check_packages()?;
        for svc in PXE_SERVICES {
            println!("{svc}: {}", if ctl::service_active(svc) { "running" } else { "not running" });
        }
    }

    step("Stopping services");
    let mut stop = vec!["stop"];
    stop.extend(PXE_SERVICES);
    stop.push("nfs-server");
    run(ctx, "systemctl", &stop)?;

    // last: everything it needs is installed and the services are stopped
    step("Importing the netboot files and Clonezilla Live");
    import_boot_files(c, ctx)?;

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

/// apt-get options that keep it from ever waiting for a person or for the network forever:
/// - -y/-q and the two --force-conf* options answer every dpkg/conffile question;
/// - DPkg::Lock::Timeout waits (bounded) for unattended-upgrades or another apt instead of failing at once;
/// - Acquire::* bound a dead mirror (retry 3 times, 30 s per connection).
const APT_OPTS: [&str; 14] = [
    "-y",
    "-q",
    "-o",
    "DPkg::Lock::Timeout=600",
    "-o",
    "Acquire::Retries=3",
    "-o",
    "Acquire::http::Timeout=30",
    "-o",
    "Acquire::https::Timeout=30",
    "-o",
    "Dpkg::Options::=--force-confdef",
    "-o",
    "Dpkg::Options::=--force-confold",
];
const DPKG_FIX: [&str; 4] = ["--configure", "-a", "--force-confdef", "--force-confold"];
const APT_UPDATE_TIMEOUT: Duration = Duration::from_secs(600);
const APT_INSTALL_TIMEOUT: Duration = Duration::from_secs(1800);
const POLICY_RC_D: &str = "/usr/sbin/policy-rc.d";

/// While alive, packages being installed do not start their services (policy-rc.d answers 101 = "denied").
/// Kea would start with the packaged example config and tftpd-hpa with its default folder, and either can
/// collide with a dnsmasq an older version of this tool left running on UDP 67/69; a failing service start in
/// a postinst script would leave the package half-configured. `start` starts everything once configured.
/// An already existing policy-rc.d (e.g. in a container) is left alone.
struct NoAutoStart(bool);
impl NoAutoStart {
    fn new(ctx: Ctx) -> Self {
        if ctx.dry || Path::new(POLICY_RC_D).exists() {
            return Self(false);
        }
        Self(write_file(ctx, POLICY_RC_D, "#!/bin/sh\nexit 101\n", 0o755, false).is_ok())
    }
}
impl Drop for NoAutoStart {
    fn drop(&mut self) {
        if self.0 {
            let _ = fs::remove_file(POLICY_RC_D);
        }
    }
}

fn apt_get(ctx: Ctx, timeout: Duration, tail: &[&str]) -> Result<(), String> {
    let mut args: Vec<&str> = APT_OPTS.to_vec();
    args.extend(tail);
    run_timeout(ctx, "apt-get", &args, timeout)
}

/// Install whatever of PACKAGES is missing, fully unattended: no prompt can block it, waiting for the apt
/// lock and for the network is bounded, and an interrupted earlier dpkg run is repaired first.
fn install_packages(ctx: Ctx) -> Result<(), String> {
    let missing = missing_packages();
    let missing_optional = missing_of(&OPTIONAL_PACKAGES);
    if missing.is_empty() && missing_optional.is_empty() {
        println!(
            "all packages already installed: {}, {}; skipping apt",
            PACKAGES.join(", "),
            OPTIONAL_PACKAGES.join(", ")
        );
        return Ok(());
    }
    let wanted: Vec<&str> = missing.iter().chain(missing_optional.iter()).copied().collect();
    println!("installing: {} (waits up to 10 min if another apt/dpkg, e.g. unattended-upgrades, is running)", wanted.join(" "));
    let _no_autostart = NoAutoStart::new(ctx);

    // finish a previous, interrupted dpkg run, otherwise every install fails on it
    if let Err(e) = run_timeout(ctx, "dpkg", &DPKG_FIX, APT_INSTALL_TIMEOUT) {
        warn(&e);
    }

    // a failing `update` (one unreachable repo) is not fatal: the install below may still work from the cache
    let mut updated = false;
    for attempt in 1..=3 {
        match apt_get(ctx, APT_UPDATE_TIMEOUT, &["update"]) {
            Ok(()) => {
                updated = true;
                break;
            }
            Err(e) => {
                warn(&format!("apt-get update, attempt {attempt}/3: {e}"));
                if attempt < 3 {
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }
    if !updated {
        warn("apt-get update did not succeed; trying to install from the existing package lists");
    }

    if !missing.is_empty() {
        let mut tail = vec!["install"];
        tail.extend(missing.iter().copied());
        if let Err(first) = apt_get(ctx, APT_INSTALL_TIMEOUT, &tail) {
            warn(&format!("{first}; repairing and retrying once"));
            run_soft_timeout(ctx, "dpkg", &DPKG_FIX);
            let _ = apt_get(ctx, APT_INSTALL_TIMEOUT, &["-f", "install"]);
            apt_get(ctx, APT_INSTALL_TIMEOUT, &tail)?;
        }
    }

    // multicast packages, one by one: a failure is a warning, never an error of the install
    for pkg in missing_optional {
        if let Err(e) = apt_get(ctx, APT_INSTALL_TIMEOUT, &["install", pkg]) {
            warn(&format!(
                "{pkg} could not be installed ({e}); only the multicast mode needs it - see the multicast guide for DRBL's own repository"
            ));
            // do not leave a half-configured package behind: it would block the next apt run
            run_soft_timeout(ctx, "dpkg", &DPKG_FIX);
            let _ = apt_get(ctx, APT_INSTALL_TIMEOUT, &["-f", "install"]);
        }
    }
    Ok(())
}

fn run_soft_timeout(ctx: Ctx, prog: &str, args: &[&str]) {
    if let Err(e) = run_timeout(ctx, prog, args, APT_INSTALL_TIMEOUT) {
        warn(&e);
    }
}

/// `dpkg -s` also succeeds for removed-but-configured packages, so ask for the exact state.
fn pkg_installed(p: &str) -> bool {
    output("dpkg-query", &["-W", "-f=${Status}", p]).is_ok_and(|s| s == "install ok installed")
}

fn missing_of(list: &[&'static str]) -> Vec<&'static str> {
    list.iter().copied().filter(|p| !pkg_installed(p)).collect()
}

fn missing_packages() -> Vec<&'static str> {
    missing_of(&PACKAGES)
}

fn check_packages() -> Result<(), String> {
    let missing = missing_packages();
    if missing.is_empty() {
        println!("packages installed: {}", PACKAGES.join(", "));
        let optional = missing_of(&OPTIONAL_PACKAGES);
        if optional.is_empty() {
            println!("multicast packages installed: {}", OPTIONAL_PACKAGES.join(", "));
        } else {
            println!("multicast packages NOT installed: {} (only the multicast mode needs them)", optional.join(", "));
        }
        Ok(())
    } else {
        Err(format!("not installed: {}. Run the Install step first.", missing.join(", ")))
    }
}

/// `start`: verify the packages, write all configuration, then (re)start Kea + tftpd-hpa + Apache + NFS.
pub fn start(c: &Config) -> Result<(), String> {
    // The cast mode saved by `pxe-ctl cast` decides (it can only change while the PXE server is stopped). A GUI
    // form or config file carries a mode of its own, which must not override it.
    let mut with_saved_mode = c.clone();
    with_saved_mode.cast_mode = Config::load_runtime().cast_mode;
    let c = &with_saved_mode;
    if c.cast_mode == CastMode::Multicast {
        // DHCP/TFTP belong to DRBL + Clonezilla SE: starting Kea here would fight it for the network
        return start_multicast(c);
    }
    let ctx = prepare(c, true)?;

    if c.only_known_hosts && c.bindings.is_empty() {
        warn("ONLY_KNOWN_HOSTS is on but no bindings were given: no host will get an address until you bind MACs (Manage tab / pxe-ctl bind)");
    }

    step("Checking the PXE packages are installed");
    if !ctx.dry {
        check_packages()?;
    }

    // a failure here only means the live system has to be copied by hand (report_missing says so at the end)
    if let Err(e) = import_live_if_missing(c, ctx) {
        warn(&e);
    }

    steps(c, ctx)?;

    println!(
        "\nPXE services are running ({} / {}-{}). Deploy way: {}.\n\n{}\n\n\
         Then boot the target in UEFI mode with \"PXE IPv4\" as first boot device. It restores the image\n\
         onto /dev/{} (ERASES THE DISK) and reboots.\n\
         Duplicate-install guard: restored hosts are recorded in {}/<mac>\n\
         \x20 sudo pxe-ctl status | list | reinstall <mac> | done <mac>\n\
         \x20 sudo pxe-ctl binds | bind <mac> <ip> [name] | unbind <mac|ip>\n\
         Watch DHCP / TFTP requests with: journalctl -fu kea-dhcp4-server -u tftpd-hpa",
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

    // ---- 3. DHCP (Kea) + TFTP (tftpd-hpa). No DNS server is needed: port 53 stays with systemd-resolved ----
    if ctl::service_active("isc-dhcp-server") {
        warn("isc-dhcp-server (DRBL's DHCP) is running: it answers DHCP next to Kea. Stop it (pxe-ctl stop) first, or use the multicast cast mode");
    }
    retire_dnsmasq(ctx);

    step("Writing the DHCP (Kea) and TFTP (tftpd-hpa) config");
    import_old_bindings(c, ctx);
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
    keep_original_kea_conf(ctx);
    // the bindings (hosts file) are Kea's "reservations"
    let reservations = if ctx.dry { c.bindings.clone() } else { ctl::read_bindings(c) };
    write_file(ctx, render::KEA_CONF_PATH, &render::kea_conf(c, &reservations), 0o644, false)?;
    run(ctx, "kea-dhcp4", &["-t", render::KEA_CONF_PATH])?;
    write_file(ctx, render::TFTPD_DEFAULT_PATH, &render::tftpd_defaults(c), 0o644, false)?;

    // ---- 5. TFTP root skeleton: grub.cfg with the duplicate-install guard + marker dir ----
    step("Writing grub.cfg (duplicate-install guard)");
    mkdir(ctx, &c.tftp_root)?;
    ctl::write_grub(c, ctx)?;

    // ---- 6. helper commands (this binary copies itself so cgi/ctl keep working) ----
    install_helpers(c, ctx)?;

    // ---- 7. Apache: Clonezilla web dir, "done" callback, per-MAC post-restore script ----
    configure_apache(c, ctx)?;

    // ---- 8. NFS export of the Clonezilla image repository ----
    step("Configuring NFS export of the Clonezilla images");
    setup_nfs(c, ctx)?;

    // ---- 9. enable + start services ----
    step("Starting services");
    for action in ["enable", "restart"] {
        let mut args = vec![action];
        args.extend(PXE_SERVICES);
        args.push("nfs-server");
        run(ctx, "systemctl", &args)?;
    }

    if ctx.dry || probe("sh", &["-c", "command -v ufw >/dev/null && ufw status | grep -q 'Status: active'"]) {
        for (port, proto) in [
            ("67", "udp"),
            ("69", "udp"),
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
        for svc in PXE_SERVICES {
            println!("{svc}: {}", if ctl::service_active(svc) { "active" } else { "NOT ACTIVE" });
        }
        // TFTP: fetch the PXE menu the way GRUB does (the service was only just restarted: allow a retry)
        let tftp_url = format!("tftp://{}/grub/grub.cfg", c.server_ip);
        let tftp_ok = (0..3).any(|i| {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            std::process::Command::new("curl")
                .args(["-sS", "-f", "--max-time", "5", "-o", "/dev/null", &tftp_url])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        });
        if !tftp_ok {
            warn(&format!("TFTP does not answer at {tftp_url} (is {} running and {} up on {}?)", render::TFTP_SERVICE, c.server_ip, c.iface));
        }
        check_callbacks(c);
        // not errors: the admin copies these by hand after the install (the image is noted by setup_nfs)
        report_missing(c);
    }
    Ok(())
}

/// Copy this binary to /usr/local/sbin (the CGIs and `pxe-ctl` run it from there), write the `pxe-ctl`
/// wrapper and the runtime config the helper commands read.
fn install_helpers(c: &Config, ctx: Ctx) -> Result<(), String> {
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
    write_file(ctx, crate::config::RUNTIME_CONF, &c.runtime_conf(), 0o644, false)
}

/// Apache: the Clonezilla web dir, the "done" callback and the per-MAC post-restore script (CGIs). Both the
/// unicast boot menu and the multicast hooks (see install_drbl_hooks) call them.
fn configure_apache(c: &Config, ctx: Ctx) -> Result<(), String> {
    step("Configuring Apache");
    let cz_web = cz_web_dir(c);
    mkdir(ctx, &cz_web)?;
    set_mode_soft(ctx, &cz_web, 0o755);
    write_file(ctx, CGI_PATH, &render::cgi_wrapper(), 0o755, false)?;
    write_file(ctx, CGI_SEED_PATH, &render::cgi_seed_wrapper(), 0o755, false)?;
    write_file(ctx, "/etc/apache2/conf-available/pxe-done.conf", &render::apache_conf(c), 0o644, false)?;
    run(ctx, "a2enmod", &["cgi"])?;
    run(ctx, "a2enconf", &["serve-cgi-bin", "pxe-done"])
}

/// The callbacks must answer: the per-MAC post.sh (generated by the pxe-seed CGI) and pxe-done. Only warnings.
fn check_callbacks(c: &Config) {
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
}

/// Multicast mode: put the two scripts into the folders Clonezilla SE runs on every client before / after the
/// restore (`drbl-ocs ... -o0 -o1`). They make a multicast deploy do what a unicast one does: deploy start
/// time, per-MAC hostname, new machine-id and SSH host keys, done marker, deploy time.
fn install_drbl_hooks(c: &Config, ctx: Ctx) -> Result<(), String> {
    step("Installing the DRBL pre/post-run scripts (multicast)");
    let pre = format!("{}/{}", render::DRBL_PRERUN_DIR, render::PRERUN_HOOK_NAME);
    let post = format!("{}/{}", render::DRBL_POSTRUN_DIR, render::POSTRUN_HOOK_NAME);
    write_file(ctx, &pre, &render::prerun_hook(c), 0o755, false)?;
    write_file(ctx, &post, &render::postrun_hook(c), 0o755, false)?;
    if !ctx.dry && !probe("sh", &["-c", "command -v drblpush >/dev/null"]) {
        warn("DRBL is not installed yet (apt install drbl): the scripts are in place for when it is");
    }
    Ok(())
}

/// `start` in cast mode multicast. DHCP, TFTP and the multicast session itself belong to DRBL + Clonezilla SE
/// (this tool does not run them), so this does only what a multicast deploy needs from this tool: the callbacks
/// (Apache + the pxe-seed / pxe-done CGIs, the done marker folder) and the DRBL pre/post-run scripts that call
/// them. Kea is switched off so it cannot answer DHCP next to DRBL.
fn start_multicast(c: &Config) -> Result<(), String> {
    let ctx = prepare(c, true)?;

    step("Checking the packages are installed");
    if !ctx.dry {
        check_packages()?;
    }

    // DRBL serves DHCP/TFTP: this tool's Kea must not answer too (tftpd-hpa is DRBL's TFTP as well: left alone)
    step("Switching this tool's DHCP (Kea) off: DRBL serves DHCP in multicast mode");
    run_soft(ctx, "systemctl", &["disable", "--now", render::DHCP_SERVICE]);

    install_helpers(c, ctx)?;
    configure_apache(c, ctx)?;

    // the CGIs (www-data) write the start/done records here
    step("Preparing the done-marker folder");
    mkdir(ctx, &c.done_dir())?;
    run(ctx, "chown", &["www-data:www-data", &c.done_dir()])?;
    if !ctx.dry && !Path::new(&c.hosts_file).exists() {
        write_file(ctx, &c.hosts_file, "", 0o644, false)?;
    }

    install_drbl_hooks(c, ctx)?;

    step("Starting Apache");
    run(ctx, "systemctl", &["enable", "apache2"])?;
    run(ctx, "systemctl", &["restart", "apache2"])?;
    if ctx.dry || probe("sh", &["-c", "command -v ufw >/dev/null && ufw status | grep -q 'Status: active'"]) {
        run_soft(ctx, "ufw", &["allow", "in", "on", c.iface.as_str(), "to", "any", "port", "80", "proto", "tcp"]);
    }

    step("Checks");
    if !ctx.dry {
        println!("apache2: {}", if ctl::service_active("apache2") { "active" } else { "NOT ACTIVE" });
        check_callbacks(c);
    }

    println!(
        "\nMulticast mode: the callbacks ({}/cgi-bin/pxe-seed, pxe-done) and the DRBL pre/post-run scripts are in place.\n\
         DHCP, TFTP and the multicast session are DRBL + Clonezilla SE (see drbl-clonezilla-se-multicast-guide.md):\n\
         \x20 sudo drblpush -i            (once; Clonezilla box mode, UEFI, the provisioning NIC {})\n\
         \x20 sudo drbl-ocs -b -g auto -e1 auto -e2 -x -r -j2 -sc0 -p reboot -o0 -o1 \\\n\
         \x20   --clients-to-wait <N> --max-time-to-wait 600 -l en_US.UTF-8 \\\n\
         \x20   startdisk multicast_restoredisk {} {}\n\
         The disk name must be {} on every client: the post-restore script looks for the system there.\n\
         Hostnames come from the MAC bindings (sudo pxe-ctl bind <mac> <ip> <hostname>); a MAC without one gets {}.\n\
         Check a deploy on this server: sudo pxe-ctl times | status | list",
        c.server_ip, c.iface, c.image_name, c.target_disk, c.target_disk, c.target_hostname
    );
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

/// Where the dnsmasq versions of this tool kept their bindings and PXE settings.
const OLD_HOSTS_FILES: [&str; 2] = ["/etc/dnsmasq.d/pxe-hosts.conf", "/etc/kea/pxe-hosts.conf"];
const OLD_DNSMASQ_PXE_CONF: &str = "/etc/dnsmasq.d/pxe.conf";

/// Older versions of this tool served DHCP and TFTP with dnsmasq. It would fight Kea and tftpd-hpa for UDP
/// 67/69, so stop and disable it. Only when our old config is there: a dnsmasq the admin runs for other
/// purposes is left alone. The package and its files stay.
fn retire_dnsmasq(ctx: Ctx) {
    if ctx.dry {
        println!("[dry-run] stop + disable dnsmasq if an older version of this tool left it serving DHCP/TFTP");
        return;
    }
    if Path::new(OLD_DNSMASQ_PXE_CONF).exists() && probe("systemctl", &["cat", "dnsmasq.service"]) {
        step("Retiring dnsmasq (DHCP/TFTP are served by Kea and tftpd-hpa now)");
        run_soft(ctx, "systemctl", &["disable", "--now", "dnsmasq"]);
    }
}

/// First run on a server that was set up with dnsmasq: take over its MAC -> IP bindings (the line format
/// is still understood, see ctl::read_hosts).
fn import_old_bindings(c: &Config, ctx: Ctx) {
    if ctx.dry || Path::new(&c.hosts_file).exists() {
        return;
    }
    let Some(old) = OLD_HOSTS_FILES.iter().find(|p| Path::new(p).exists()) else { return };
    let copied = fs::read_to_string(old).map_err(|e| e.to_string()).and_then(|text| {
        if let Some(parent) = Path::new(&c.hosts_file).parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(&c.hosts_file, text).map_err(|e| e.to_string())
    });
    match copied {
        Ok(()) => println!("imported the MAC bindings of an earlier version from {old}"),
        Err(e) => warn(&format!("could not import {old}: {e}")),
    }
}

/// The Kea package ships its own /etc/kea/kea-dhcp4.conf; keep a copy of it once before ours replaces it.
fn keep_original_kea_conf(ctx: Ctx) {
    if ctx.dry {
        return;
    }
    let keep = format!("{}.pxe-orig", render::KEA_CONF_PATH);
    if let Ok(text) = fs::read_to_string(render::KEA_CONF_PATH) {
        if !text.starts_with(render::GENERATED_MARK) && !Path::new(&keep).exists() {
            let _ = fs::write(&keep, text);
        }
    }
}

fn set_mode_soft(ctx: Ctx, path: &str, mode: u32) {
    if ctx.dry {
        println!("[dry-run] chmod {mode:o} {path}");
    } else if let Err(e) = set_mode(path, mode) {
        warn(&e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_matching_archive_is_found() {
        let dir = std::env::temp_dir().join(format!("pxe-archives-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for n in [
            "ubuntu-24.04.4-netboot-amd64.tar.gz",
            "ubuntu-26.04.1-netboot-amd64.tar.gz",
            "ubuntu-26.04.1-live-server-amd64.iso", // not a netboot tarball
            "notes.txt",
        ] {
            fs::write(dir.join(n), "").unwrap();
        }
        let d = dir.to_string_lossy().to_string();
        assert_eq!(
            find_archive(&d, "ubuntu-", "-netboot-amd64.tar.gz"),
            Some(format!("{d}/ubuntu-26.04.1-netboot-amd64.tar.gz"))
        );
        assert_eq!(find_archive(&d, "clonezilla-live-", "-amd64.zip"), None);
        assert_eq!(find_archive("/no/such/dir", "ubuntu-", ".gz"), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
