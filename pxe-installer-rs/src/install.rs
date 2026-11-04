//! The `install` command installs dnsmasq + Apache and creates the directories; the `start` command
//! writes their PXE configuration and starts them.
//! It does NOT download or copy the netboot image or the ISO - the admin places those manually.
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::ctl;
use crate::render::{self, CGI_PATH, CGI_SEED_PATH, CTL_WRAPPER_PATH, SELF_PATH};
use crate::util::*;

const PACKAGES: [&str; 5] = ["dnsmasq", "apache2", "curl", "openssl", "nfs-kernel-server"];

/// `validate`: `start` needs a complete config (incl. the target password); `install` only needs the paths.
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

/// `install`: packages + directories only. Nothing is configured and the services are left stopped;
/// `start` does the configuration.
pub fn install(c: &Config) -> Result<(), String> {
    let ctx = prepare(c, false)?;

    step("Installing packages");
    run(ctx, "apt-get", &["update", "-y"])?;
    let mut apt = vec!["install", "-y"];
    apt.extend(PACKAGES);
    run(ctx, "apt-get", &apt)?;

    step("Creating directories");
    for dir in [format!("{}/iso", c.web_root), format!("{}/autoinstall", c.web_root), c.tftp_root.clone(), c.nfs_root.clone()] {
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

    println!(
        "\nPXE packages are installed and the services are stopped.\n\
         Next: set the network/DHCP/target options and click \"Start PXE server\" (or: pxe-installer start)."
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
        "\nPXE services are running ({} / {}-{}).\n\n\
         NEXT STEPS (manual - this installer does not download or copy these files):\n\
         \x20 1. Netboot files (bootx64.efi, grubx64.efi, casper/vmlinuz, casper/initrd, ...)\n\
         \x20      -> copy into {}/\n\
         \x20    then run:  sudo pxe-ctl grub     (re-applies our grub.cfg and file permissions)\n\
         \x20 2. Live-server ISO  -> {}/iso/{}\n\
         \x20    (loop-mounted on {} and exported over NFS: the live system boots from there, not from RAM;\n\
         \x20     after copying or replacing the ISO run:  sudo pxe-ctl nfs)\n\n\
         Then boot the target host in UEFI mode with \"PXE IPv4\" as first boot device.\n\
         Duplicate-install guard: installed hosts are recorded in {}/<mac>\n\
         \x20 sudo pxe-ctl status | list | reinstall <mac> | done <mac>\n\
         \x20 sudo pxe-ctl binds | bind <mac> <ip> [name] | unbind <mac|ip>\n\
         Watch requests with: journalctl -fu dnsmasq",
        c.server_ip,
        c.dhcp_start,
        c.dhcp_end,
        c.tftp_root,
        c.web_root,
        c.iso_name,
        c.nfs_root,
        c.done_dir()
    );
    Ok(())
}

fn steps(c: &Config, ctx: Ctx) -> Result<(), String> {
    let iso_dir = format!("{}/iso", c.web_root);
    let ai_dir = format!("{}/autoinstall", c.web_root);

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

    // ---- 7. Apache: web dirs, autoinstall files, "done" callback ----
    step("Configuring Apache");
    mkdir(ctx, &iso_dir)?;
    mkdir(ctx, &ai_dir)?;
    set_mode_soft(ctx, &iso_dir, 0o755);
    set_mode_soft(ctx, &ai_dir, 0o755);

    let pw_hash = if ctx.dry {
        "$6$dryrun$hash".to_string()
    } else {
        output_with_stdin("openssl", &["passwd", "-6", "-stdin"], &c.target_password)?
    };
    write_file(ctx, &format!("{ai_dir}/meta-data"), "", 0o644, false)?;
    write_file(ctx, &format!("{ai_dir}/user-data"), &render::user_data(c, &pw_hash), 0o644, false)?;
    write_file(ctx, CGI_PATH, &render::cgi_wrapper(), 0o755, false)?;
    write_file(ctx, CGI_SEED_PATH, &render::cgi_seed_wrapper(), 0o755, false)?;
    write_file(ctx, "/etc/apache2/conf-available/pxe-done.conf", &render::apache_conf(c), 0o644, false)?;
    run(ctx, "a2enmod", &["cgi"])?;
    run(ctx, "a2enconf", &["serve-cgi-bin", "pxe-done"])?;

    // ---- 8. NFS export of the loop-mounted ISO (the live system boots from it instead of RAM) ----
    step("Configuring NFS export of the ISO");
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
        // Apache was only just restarted, so give it a moment before calling it unreachable
        let url = format!("{base}/autoinstall/user-data");
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
            warn(&format!("autoinstall user-data not reachable at {url}: {last}"));
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
        // not errors: the admin copies these by hand after the install
        if !Path::new(&format!("{}/bootx64.efi", c.tftp_root)).exists() {
            println!("note: {}/bootx64.efi not present yet (copy the netboot files)", c.tftp_root);
        }
        if !Path::new(&format!("{iso_dir}/{}", c.iso_name)).exists() {
            println!("note: {iso_dir}/{} not present yet (copy the ISO)", c.iso_name);
        }
    }
    Ok(())
}

const NFS_MOUNTD_PORT: &str = "20048";
const FSTAB: &str = "/etc/fstab";
const FSTAB_MARK: &str = "# pxe-installer";

/// Loop-mount the ISO on `nfs_root` (persistently via fstab) and export it read-only to the PXE subnet.
/// The ISO itself may not exist yet; `pxe-ctl nfs` re-runs this once it has been copied.
pub fn setup_nfs(c: &Config, ctx: Ctx) -> Result<(), String> {
    let iso = format!("{}/iso/{}", c.web_root, c.iso_name);
    let net = c.network().map(|n| format!("{n}/{}", c.prefix)).unwrap_or_default();

    mkdir(ctx, &c.nfs_root)?;

    // fstab: replace our previous line (the ISO name may have changed), keep everything else
    let line = format!("{iso} {} iso9660 loop,ro,nofail 0 0 {FSTAB_MARK}", c.nfs_root);
    if ctx.dry {
        println!("[dry-run] fstab: {line}");
    } else {
        let old = fs::read_to_string(FSTAB).unwrap_or_default();
        let mut text: String = old.lines().filter(|l| !l.ends_with(FSTAB_MARK)).map(|l| format!("{l}\n")).collect();
        text.push_str(&line);
        text.push('\n');
        if text != old {
            fs::write(FSTAB, text).map_err(|e| format!("write {FSTAB}: {e}"))?;
            run_soft(ctx, "systemctl", &["daemon-reload"]);
        }
    }

    // pin mountd so the firewall rule is predictable (the initramfs mounts with NFSv3)
    write_file(ctx, "/etc/nfs.conf.d/pxe-installer.conf", &format!("[mountd]\nport={NFS_MOUNTD_PORT}\n"), 0o644, false)?;
    write_file(
        ctx,
        "/etc/exports.d/pxe-installer.exports",
        &format!("{} {net}(ro,fsid=1,no_subtree_check,no_root_squash,async)\n", c.nfs_root),
        0o644,
        false,
    )?;

    if ctx.dry {
        println!("[dry-run] (re)mount {iso} on {}; exportfs -ra", c.nfs_root);
        return Ok(());
    }
    if !Path::new(&iso).exists() {
        println!("note: {iso} not present yet - copy the ISO, then run: sudo pxe-ctl nfs");
        return Ok(());
    }
    if probe("mountpoint", &["-q", &c.nfs_root]) {
        run(ctx, "umount", &[&c.nfs_root])?; // may hold an older ISO
    }
    run(ctx, "mount", &[&c.nfs_root])?;
    run(ctx, "exportfs", &["-ra"])
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
