//! The `install` command: turns a plain Ubuntu host into a UEFI PXE server.
use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::ctl;
use crate::render::{self, CGI_PATH, CTL_WRAPPER_PATH, SELF_PATH};
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

    let workdir = if ctx.dry { "/tmp/pxe-installer-XXXXXX".to_string() } else { output("mktemp", &["-d"])? };
    let result = steps(c, ctx, &workdir);
    if !ctx.dry {
        let _ = fs::remove_dir_all(&workdir);
    }
    result?;

    println!(
        "\nPXE server ready.\n  DHCP/TFTP : {} ({}-{})\n  HTTP ISO  : http://{}/iso/{}\n\
         Boot the target host in UEFI mode with \"PXE IPv4\" as first boot device.\n\n\
         Duplicate-install guard: installed hosts are recorded in {}/<mac>\n\
         \x20 sudo pxe-ctl status | list | reinstall <mac> | done <mac>\n\
         \x20 sudo pxe-ctl binds | bind <mac> <ip> [name] | unbind <mac|ip>\n\
         Watch requests with: journalctl -fu dnsmasq",
        c.server_ip,
        c.dhcp_start,
        c.dhcp_end,
        c.server_ip,
        c.iso_name,
        c.done_dir()
    );
    Ok(())
}

fn steps(c: &Config, ctx: Ctx, workdir: &str) -> Result<(), String> {
    let iso_dir = format!("{}/iso", c.web_root);
    let ai_dir = format!("{}/autoinstall", c.web_root);
    let iso_path = format!("{iso_dir}/{}", c.iso_name);

    // ---- 1. packages ----
    step("Installing packages");
    run(ctx, "apt-get", &["update", "-y"])?;
    run(ctx, "apt-get", &["install", "-y", "dnsmasq", "apache2", "wget", "curl", "openssl"])?;

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

    // ---- 5. netboot files + grub.cfg with the duplicate-install guard ----
    step("Fetching netboot image");
    mkdir(ctx, &c.tftp_root)?;
    let tar_path = format!("{workdir}/{}", c.netboot_tar);
    run(ctx, "wget", &["-nv", "-O", &tar_path, &format!("{}/{}", c.release_url, c.netboot_tar)])?;
    run(ctx, "tar", &["-xzf", &tar_path, "-C", workdir])?;
    run(ctx, "cp", &["-r", &format!("{workdir}/amd64/."), &format!("{}/", c.tftp_root)])?;

    step("Writing grub.cfg (duplicate-install guard)");
    write_file(ctx, &format!("{}/grub/grub.cfg", c.tftp_root), &render::grub_cfg(c), 0o644, false)?;
    mkdir(ctx, &c.done_dir())?;
    // root-owned and read-only (no 777), except the marker dir which the CGI (www-data) writes
    run(ctx, "chown", &["-R", "root:root", &c.tftp_root])?;
    run(ctx, "chmod", &["-R", "u=rwX,go=rX", &c.tftp_root])?;
    run(ctx, "chown", &["www-data:www-data", &c.done_dir()])?;

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

    // ---- 7. Apache: ISO, autoinstall files, "done" callback ----
    step("Configuring Apache");
    mkdir(ctx, &iso_dir)?;
    mkdir(ctx, &ai_dir)?;
    if ctx.dry || !Path::new(&iso_path).exists() {
        run(ctx, "wget", &["-nv", "-c", "-O", &iso_path, &format!("{}/{}", c.release_url, c.iso_name)])?;
    }
    set_mode_soft(ctx, &iso_dir, 0o755);
    set_mode_soft(ctx, &ai_dir, 0o755);
    set_mode_soft(ctx, &iso_path, 0o644);

    let pw_hash = if ctx.dry {
        "$6$dryrun$hash".to_string()
    } else {
        output_with_stdin("openssl", &["passwd", "-6", "-stdin"], &c.target_password)?
    };
    write_file(ctx, &format!("{ai_dir}/meta-data"), "", 0o644, false)?;
    write_file(ctx, &format!("{ai_dir}/user-data"), &render::user_data(c, &pw_hash), 0o644, false)?;
    write_file(ctx, CGI_PATH, &render::cgi_wrapper(), 0o755, false)?;
    write_file(ctx, "/etc/apache2/conf-available/pxe-done.conf", &render::apache_conf(c), 0o644, false)?;
    run(ctx, "a2enmod", &["cgi"])?;
    run(ctx, "a2enconf", &["serve-cgi-bin", "pxe-done"])?;

    // ---- 8. services ----
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
        if !probe("curl", &["-sfI", &format!("{base}/iso/{}", c.iso_name)]) {
            warn("ISO not reachable over HTTP");
        }
        if !probe("curl", &["-sf", &format!("{base}/autoinstall/user-data")]) {
            warn("autoinstall user-data not reachable");
        }
        let cgi = output("curl", &["-s", &format!("{base}/cgi-bin/pxe-done?mac=bad")]).unwrap_or_default();
        if !cgi.contains("bad mac") {
            warn("pxe-done CGI not working");
        }
        if !Path::new(&format!("{}/bootx64.efi", c.tftp_root)).exists() {
            warn(&format!("{}/bootx64.efi missing", c.tftp_root));
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
