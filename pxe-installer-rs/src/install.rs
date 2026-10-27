//! The `install` command: installs and enables dnsmasq + Apache and writes their PXE configuration.
//! It does NOT download or copy the netboot image or the ISO - the admin places those manually.
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::ctl;
use crate::render::{self, CGI_PATH, CGI_SEED_PATH, CTL_WRAPPER_PATH, SELF_PATH};
use crate::util::*;

pub fn install(c: &Config) -> Result<(), String> {
    let errs = c.validate();
    if !errs.is_empty() {
        return Err(format!("invalid configuration:\n  - {}", errs.join("\n  - ")));
    }
    let ctx = Ctx { dry: c.dry_run };
    if !ctx.dry && output("id", &["-u"])? != "0" {
        return Err("must run as root (sudo)".into());
    }
    if ctx.dry {
        println!("[dry-run] nothing will be changed; showing what would be done");
    }

    if c.only_known_hosts && c.bindings.is_empty() {
        warn("ONLY_KNOWN_HOSTS is on but no bindings were given: no host will get an address until you bind MACs (Manage tab / pxe-ctl bind)");
    }

    steps(c, ctx)?;

    println!(
        "\nPXE services are installed and running ({} / {}-{}).\n\n\
         NEXT STEPS (manual - this installer does not download or copy these files):\n\
         \x20 1. Netboot files (bootx64.efi, grubx64.efi, casper/vmlinuz, casper/initrd, ...)\n\
         \x20      -> copy into {}/\n\
         \x20    then run:  sudo pxe-ctl grub     (re-applies our grub.cfg and file permissions)\n\
         \x20 2. Live-server ISO  -> {}/iso/{}\n\n\
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
        c.done_dir()
    );
    Ok(())
}

fn steps(c: &Config, ctx: Ctx) -> Result<(), String> {
    let iso_dir = format!("{}/iso", c.web_root);
    let ai_dir = format!("{}/autoinstall", c.web_root);

    // ---- 1. packages ----
    step("Installing packages");
    run(ctx, "apt-get", &["update", "-y"])?;
    run(ctx, "apt-get", &["install", "-y", "dnsmasq", "apache2", "curl", "openssl"])?;

    // ---- 2. server network (optional) ----
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
    if !ctx.dry && Path::new("/etc/dnsmasq.conf").exists() && !Path::new("/etc/dnsmasq.conf.bak").exists() {
        fs::rename("/etc/dnsmasq.conf", "/etc/dnsmasq.conf.bak").map_err(|e| format!("backup dnsmasq.conf: {e}"))?;
    }
    write_file(ctx, "/etc/dnsmasq.conf", &render::dnsmasq_conf(c), 0o644, false)?;
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

    // ---- 8. enable + start services ----
    step("Starting services");
    run(ctx, "systemctl", &["enable", "apache2", "dnsmasq"])?;
    run(ctx, "systemctl", &["restart", "apache2", "dnsmasq"])?;

    if ctx.dry || probe("sh", &["-c", "command -v ufw >/dev/null && ufw status | grep -q 'Status: active'"]) {
        for (port, proto) in [("67", "udp"), ("69", "udp"), ("53", "any"), ("80", "tcp")] {
            let mut a = vec!["allow", "in", "on", c.iface.as_str(), "to", "any", "port", port];
            if proto != "any" {
                a.push("proto");
                a.push(proto);
            }
            run_soft(ctx, "ufw", &a);
        }
    }

    // ---- 9. checks ----
    step("Checks");
    if !ctx.dry {
        for svc in ["dnsmasq", "apache2"] {
            println!("{svc}: {}", if ctl::service_active(svc) { "active" } else { "NOT ACTIVE" });
        }
        let base = format!("http://{}", c.server_ip);
        if !probe("curl", &["-sf", &format!("{base}/autoinstall/user-data")]) {
            warn("autoinstall user-data not reachable");
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

fn set_mode_soft(ctx: Ctx, path: &str, mode: u32) {
    if ctx.dry {
        println!("[dry-run] chmod {mode:o} {path}");
    } else if let Err(e) = set_mode(path, mode) {
        warn(&e);
    }
}
