//! Day-2 commands: installed-host markers, MAC->IP bindings, status, and the Apache CGI callback.
use std::fs;
use std::path::Path;

use crate::config::{is_mac, normalize_mac, Config};
use crate::util::{now_iso, probe, run_soft, Ctx};

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

/// Create the "installed" marker for a MAC (grub.cfg skips PXE install for it).
pub fn mark_done(c: &Config, mac: &str) -> Result<(), String> {
    let dir = c.done_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
    let path = format!("{dir}/{mac}");
    fs::write(&path, format!("{}\n", now_iso())).map_err(|e| format!("write {path}: {e}"))
}

fn read_hosts(c: &Config) -> Vec<String> {
    fs::read_to_string(&c.hosts_file)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

fn write_hosts(c: &Config, lines: &[String]) -> Result<(), String> {
    let tmp = format!("{}.tmp", c.hosts_file);
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    if let Some(parent) = Path::new(&c.hosts_file).parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(&tmp, body).map_err(|e| format!("write {tmp}: {e}"))?;
    fs::rename(&tmp, &c.hosts_file).map_err(|e| format!("rename {tmp}: {e}"))
}

fn reload_dnsmasq() {
    // SIGHUP makes dnsmasq re-read dhcp-hostsfile; harmless if the service isn't running yet.
    run_soft(Ctx { dry: false }, "systemctl", &["kill", "-s", "HUP", "dnsmasq"]);
}

/// Add or replace a binding (an existing line with the same MAC or IP is dropped first).
pub fn bind(c: &Config, mac: &str, ip: &str, name: &str, reload: bool) -> Result<(), String> {
    if !is_mac(mac) {
        return Err(format!("invalid MAC: {mac}"));
    }
    if !is_ip(ip) {
        return Err(format!("invalid IP: {ip}"));
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
        Some("binds") => {
            for l in read_hosts(&c) {
                println!("{l}");
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
        _ => Err("usage: pxe-installer ctl list | status | reinstall <mac> | done <mac> | binds | bind <mac> <ip> [name] | unbind <mac|ip>".into()),
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
    fn marker_roundtrip() {
        let c = tmp_cfg("mark");
        mark_done(&c, "aa:bb:cc:dd:ee:01").unwrap();
        assert_eq!(list_done(&c).len(), 1);
    }
}
