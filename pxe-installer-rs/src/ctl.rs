//! Day-2 commands: installed-host markers, MAC->IP bindings, status, and the Apache CGI callback.
use std::fs;
use std::path::Path;

use crate::config::{is_hostname, is_mac, normalize_mac, Config};
use crate::render;
use crate::util::{mkdir, now_iso, probe, run, run_soft, write_file, Ctx};

const LEASES: &str = "/var/lib/misc/dnsmasq.leases";

fn need_mac(arg: Option<&String>) -> Result<String, String> {
    let mac = normalize_mac(arg.ok_or("MAC address required")?);
    if is_mac(&mac) {
        Ok(mac)
    } else {
        Err(format!("invalid MAC: {mac}"))
    }
}

fn is_ip(s: &str) -> bool {
    s.parse::<std::net::Ipv4Addr>().is_ok()
}

/// Make the TFTP tree root-owned and read-only (no 777), except the marker dir the CGI (www-data) writes.
pub fn fix_perms(c: &Config, ctx: Ctx) -> Result<(), String> {
    let done = c.done_dir();
    mkdir(ctx, &done)?;
    let root = c.tftp_root.as_str();
    run(ctx, "find", &[root, "-path", &done, "-prune", "-o", "-exec", "chown", "root:root", "{}", "+"])?;
    run(ctx, "find", &[root, "-path", &done, "-prune", "-o", "-type", "d", "-exec", "chmod", "755", "{}", "+"])?;
    run(ctx, "find", &[root, "-path", &done, "-prune", "-o", "-type", "f", "-exec", "chmod", "644", "{}", "+"])?;
    run(ctx, "chown", &["www-data:www-data", &done])
}

/// Write our grub.cfg (it replaces the one shipped in the netboot tarball) and fix permissions.
pub fn write_grub(c: &Config, ctx: Ctx) -> Result<(), String> {
    write_file(ctx, &format!("{}/grub/grub.cfg", c.tftp_root), &render::grub_cfg(c), 0o644, false)?;
    fix_perms(c, ctx)
}

/// Create the "installed" marker for a MAC (grub.cfg skips PXE install for it).
pub fn mark_done(c: &Config, mac: &str) -> Result<(), String> {
    let dir = c.done_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
    let path = format!("{dir}/{mac}");
    fs::write(&path, format!("{}\n", now_iso())).map_err(|e| format!("write {path}: {e}"))
}

/// Prefix of every binding line in the hosts file (it is loaded with `conf-file=`).
const HOST_PREFIX: &str = "dhcp-host=";

/// Bindings as "mac,ip[,name]" (prefix stripped; comments/blank lines skipped; old unprefixed lines accepted).
fn read_hosts(c: &Config) -> Vec<String> {
    fs::read_to_string(&c.hosts_file)
        .map(|t| {
            t.lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.strip_prefix(HOST_PREFIX).unwrap_or(l).to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn write_hosts(c: &Config, lines: &[String]) -> Result<(), String> {
    let tmp = format!("{}.tmp", c.hosts_file);
    let mut body = String::from("# managed by pxe-installer (pxe-ctl bind / unbind) - dnsmasq dhcp-host entries\n");
    for l in lines {
        body.push_str(HOST_PREFIX);
        body.push_str(l);
        body.push('\n');
    }
    if let Some(parent) = Path::new(&c.hosts_file).parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(&tmp, body).map_err(|e| format!("write {tmp}: {e}"))?;
    fs::rename(&tmp, &c.hosts_file).map_err(|e| format!("rename {tmp}: {e}"))
}

fn reload_dnsmasq() {
    // dhcp-host lines in a conf-file are only read at startup (SIGHUP does not re-read them),
    // so restart. Leases are kept in dnsmasq's lease file and survive. No-op if not installed yet.
    run_soft(Ctx { dry: false }, "systemctl", &["restart", "dnsmasq"]);
}

/// Add or replace a binding (an existing line with the same MAC or IP is dropped first).
pub fn bind(c: &Config, mac: &str, ip: &str, name: &str, reload: bool) -> Result<(), String> {
    if !is_mac(mac) {
        return Err(format!("invalid MAC: {mac}"));
    }
    if !is_ip(ip) {
        return Err(format!("invalid IP: {ip}"));
    }
    if !name.is_empty() && !is_hostname(name) {
        return Err(format!("invalid hostname: {name} (use a-z, 0-9 and '-')"));
    }
    let mut lines: Vec<String> = read_hosts(c)
        .into_iter()
        .filter(|l| {
            let mut f = l.split(',');
            let m = f.next().unwrap_or("");
            let i = f.next().unwrap_or("");
            !(m.eq_ignore_ascii_case(mac) || i == ip)
        })
        .collect();
    lines.push(if name.is_empty() { format!("{mac},{ip}") } else { format!("{mac},{ip},{name}") });
    write_hosts(c, &lines)?;
    if reload {
        reload_dnsmasq();
    }
    println!("bound {mac} -> {ip}");
    Ok(())
}

fn unbind(c: &Config, key: &str) -> Result<(), String> {
    let key = normalize_mac(key);
    let lines: Vec<String> = read_hosts(c)
        .into_iter()
        .filter(|l| {
            let mut f = l.split(',');
            let m = f.next().unwrap_or("");
            let i = f.next().unwrap_or("");
            !(m == key || i == key)
        })
        .collect();
    write_hosts(c, &lines)?;
    reload_dnsmasq();
    println!("unbound {key}");
    Ok(())
}

fn list_done(c: &Config) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir(c.done_dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if is_mac(&name) {
                let when = fs::read_to_string(e.path()).unwrap_or_default().trim().to_string();
                v.push((name, when));
            }
        }
    }
    v.sort();
    v
}

fn status(c: &Config) {
    let installed = list_done(c);
    let hosts = read_hosts(c);
    // dnsmasq.leases: <expiry> <mac> <ip> <hostname> <client-id>
    let leases: Vec<Vec<String>> = fs::read_to_string(LEASES)
        .unwrap_or_default()
        .lines()
        .map(|l| l.split_whitespace().map(|s| s.to_string()).collect::<Vec<_>>())
        .filter(|f| f.len() >= 4)
        .collect();

    let mut macs: Vec<String> = installed.iter().map(|(m, _)| m.clone()).collect();
    for l in &hosts {
        macs.push(l.split(',').next().unwrap_or("").to_lowercase());
    }
    for f in &leases {
        macs.push(f[1].to_lowercase());
    }
    macs.retain(|m| is_mac(m));
    macs.sort();
    macs.dedup();

    println!("{:<18} {:<16} {:<16} {:<10} {}", "MAC", "BOUND IP", "LEASE IP", "STATE", "INSTALLED AT");
    for mac in &macs {
        let bound = hosts
            .iter()
            .find(|l| l.split(',').next().map(|m| m.eq_ignore_ascii_case(mac)).unwrap_or(false))
            .and_then(|l| l.split(',').nth(1))
            .unwrap_or("-");
        let lease = leases.iter().find(|f| f[1].eq_ignore_ascii_case(mac)).map(|f| f[2].as_str()).unwrap_or("-");
        let done = installed.iter().find(|(m, _)| m == mac);
        let state = if done.is_some() {
            "installed"
        } else if lease != "-" {
            "pending"
        } else {
            "waiting"
        };
        println!("{:<18} {:<16} {:<16} {:<10} {}", mac, bound, lease, state, done.map(|d| d.1.as_str()).unwrap_or(""));
    }
    println!("\nstate: installed = marker present (PXE skips it) | pending = has a DHCP lease, no done callback yet | waiting = bound, never seen");
}

/// `pxe-installer ctl <subcommand> ...`
pub fn ctl(args: &[String]) -> Result<(), String> {
    let c = Config::load_runtime();
    match args.first().map(|s| s.as_str()) {
        Some("list") => {
            for (mac, when) in list_done(&c) {
                println!("{mac}  {when}");
            }
            Ok(())
        }
        Some("status") => {
            status(&c);
            Ok(())
        }
        Some("reinstall") => {
            let mac = need_mac(args.get(1))?;
            let path = format!("{}/{mac}", c.done_dir());
            match fs::remove_file(&path) {
                Ok(()) => println!("removed {path}"),
                Err(e) => println!("no marker for {mac} ({e})"),
            }
            Ok(())
        }
        Some("done") => {
            let mac = need_mac(args.get(1))?;
            mark_done(&c, &mac)?;
            // the CGI runs as www-data; keep the marker removable/readable by root and tftp
            println!("marked {mac} as installed");
            Ok(())
        }
        Some("stop") => {
            // stop the PXE services (DHCP/TFTP + HTTP); they stay enabled and come back on reboot
            run(Ctx { dry: false }, "systemctl", &["stop", "dnsmasq", "apache2"])?;
            for svc in ["dnsmasq", "apache2"] {
                println!("{svc}: {}", if service_active(svc) { "STILL ACTIVE" } else { "stopped" });
            }
            println!("PXE server stopped. Start it again with the Install button or: sudo systemctl start dnsmasq apache2");
            Ok(())
        }
        Some("grub") => {
            // run after copying netboot files into the TFTP root (the tarball ships its own grub.cfg)
            write_grub(&c, Ctx { dry: false })?;
            println!("wrote {}/grub/grub.cfg and fixed permissions", c.tftp_root);
            Ok(())
        }
        Some("binds") => {
            for l in read_hosts(&c) {
                println!("{HOST_PREFIX}{l}");
            }
            Ok(())
        }
        Some("bind") => {
            let mac = need_mac(args.get(1))?;
            let ip = args.get(2).ok_or("IP required")?;
            let name = args.get(3).map(|s| s.as_str()).unwrap_or("");
            bind(&c, &mac, ip, name, true)
        }
        Some("unbind") => unbind(&c, args.get(1).ok_or("MAC or IP required")?),
        _ => Err("usage: pxe-installer ctl list | status | stop | grub | reinstall <mac> | done <mac> | binds | bind <mac> <ip> [name] | unbind <mac|ip>".into()),
    }
}

/// `pxe-installer cgi-done` - run by Apache as www-data via /usr/lib/cgi-bin/pxe-done.
pub fn cgi_done() -> Result<(), String> {
    let c = Config::load_runtime();
    let query = std::env::var("QUERY_STRING").unwrap_or_default();
    let mac = query.strip_prefix("mac=").map(normalize_mac).unwrap_or_default();
    if !is_mac(&mac) {
        print!("Status: 400 Bad Request\r\nContent-Type: text/plain\r\n\r\nbad mac\n");
        return Ok(());
    }
    match mark_done(&c, &mac) {
        Ok(()) => print!("Content-Type: text/plain\r\n\r\nok {mac}\n"),
        Err(e) => print!("Status: 500 Internal Server Error\r\nContent-Type: text/plain\r\n\r\n{e}\n"),
    }
    Ok(())
}

/// Replace the `hostname:` value in an autoinstall user-data (indentation kept).
pub fn set_hostname(user_data: &str, host: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in user_data.lines() {
        if line.trim_start().starts_with("hostname:") {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push(format!("{indent}hostname: {host}"));
        } else {
            out.push(line.to_string());
        }
    }
    out.join("\n") + "\n"
}

/// Hostname bound to this MAC (third field of its binding), if any and valid.
fn hostname_for(c: &Config, mac: &str) -> Option<String> {
    for l in read_hosts(c) {
        let mut f = l.split(',');
        if f.next().map(|m| m.eq_ignore_ascii_case(mac)).unwrap_or(false) {
            let _ip = f.next();
            let name = f.next().unwrap_or("").trim().to_string();
            if is_hostname(&name) {
                return Some(name);
            }
        }
    }
    None
}

fn cgi_error(status: &str, msg: &str) {
    print!("Status: {status}\r\nContent-Type: text/plain\r\n\r\n{msg}\n");
}

/// `pxe-installer cgi-seed` - Apache CGI at /cgi-bin/pxe-seed/<mac>/{user-data,meta-data}.
/// The installer fetches its autoinstall seed from here (grub.cfg passes its own MAC), so each
/// machine gets the hostname bound to its MAC; MACs without a binding get the default hostname.
pub fn cgi_seed() -> Result<(), String> {
    let c = Config::load_runtime();
    let path = std::env::var("PATH_INFO").unwrap_or_default();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() != 2 {
        cgi_error("404 Not Found", "expected /<mac>/user-data");
        return Ok(());
    }
    let mac = normalize_mac(parts[0]);
    if !is_mac(&mac) {
        cgi_error("400 Bad Request", "bad mac");
        return Ok(());
    }
    match parts[1] {
        "meta-data" => print!("Content-Type: text/plain\r\n\r\n"),
        "user-data" => {
            let base = match fs::read_to_string(format!("{}/autoinstall/user-data", c.web_root)) {
                Ok(t) => t,
                Err(e) => {
                    cgi_error("500 Internal Server Error", &format!("default user-data missing: {e}"));
                    return Ok(());
                }
            };
            let host = hostname_for(&c, &mac).unwrap_or_else(|| c.target_hostname.clone());
            print!("Content-Type: text/plain\r\n\r\n{}", set_hostname(&base, &host));
        }
        _ => cgi_error("404 Not Found", "unknown file"),
    }
    Ok(())
}

/// Used by `install` to check a service is up.
pub fn service_active(name: &str) -> bool {
    probe("systemctl", &["is-active", "--quiet", name])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_cfg(tag: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("pxe-test-{tag}-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let mut c = Config::default();
        c.hosts_file = dir.join("hosts").to_string_lossy().to_string();
        c.tftp_root = dir.to_string_lossy().to_string();
        c
    }

    #[test]
    fn bind_replaces_same_mac_or_ip() {
        let c = tmp_cfg("bind");
        bind(&c, "aa:bb:cc:dd:ee:01", "192.168.5.11", "n1", false).unwrap();
        bind(&c, "aa:bb:cc:dd:ee:02", "192.168.5.12", "", false).unwrap();
        bind(&c, "aa:bb:cc:dd:ee:01", "192.168.5.13", "n1", false).unwrap(); // same MAC
        bind(&c, "aa:bb:cc:dd:ee:03", "192.168.5.12", "", false).unwrap(); // same IP as :02
        let l = read_hosts(&c);
        assert_eq!(l, vec!["aa:bb:cc:dd:ee:01,192.168.5.13,n1", "aa:bb:cc:dd:ee:03,192.168.5.12"]);
    }

    #[test]
    fn bind_rejects_bad_input() {
        let c = tmp_cfg("bad");
        assert!(bind(&c, "nope", "192.168.5.11", "", false).is_err());
        assert!(bind(&c, "aa:bb:cc:dd:ee:01", "300.1.1.1", "", false).is_err());
    }

    #[test]
    fn hostname_replaced_per_mac() {
        let base = "autoinstall:\n  identity:\n    hostname: ubuntu-node\n    username: ubuntu\n";
        let out = set_hostname(base, "node07");
        assert!(out.contains("    hostname: node07\n"));
        assert!(out.contains("username: ubuntu"));
        assert!(!out.contains("ubuntu-node"));
    }

    #[test]
    fn hostname_lookup_from_binding() {
        let c = tmp_cfg("hn");
        bind(&c, "aa:bb:cc:dd:ee:01", "192.168.5.11", "node01", false).unwrap();
        bind(&c, "aa:bb:cc:dd:ee:02", "192.168.5.12", "", false).unwrap();
        assert_eq!(hostname_for(&c, "aa:bb:cc:dd:ee:01"), Some("node01".to_string()));
        assert_eq!(hostname_for(&c, "aa:bb:cc:dd:ee:02"), None);
        assert_eq!(hostname_for(&c, "aa:bb:cc:dd:ee:09"), None);
        assert!(bind(&c, "aa:bb:cc:dd:ee:03", "192.168.5.13", "Bad_Name", false).is_err());
    }

    #[test]
    fn marker_roundtrip() {
        let c = tmp_cfg("mark");
        mark_done(&c, "aa:bb:cc:dd:ee:01").unwrap();
        assert_eq!(list_done(&c).len(), 1);
    }
}
