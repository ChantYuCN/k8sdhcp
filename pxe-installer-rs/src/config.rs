//! Settings, parsing (config file + CLI flags) and validation.
use std::fs;
use std::net::Ipv4Addr;

pub const RUNTIME_CONF: &str = "/etc/pxe-installer.conf";

#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub mac: String,
    pub ip: String,
    pub hostname: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub iface: String,
    pub server_ip: String,
    pub prefix: u8,
    pub dhcp_start: String,
    pub dhcp_end: String,
    pub lease: String,
    pub upstream_dns: String,
    pub set_static_ip: bool,
    pub only_known_hosts: bool,
    pub target_hostname: String,
    pub target_user: String,
    pub target_password: String,
    /// File name of the live-server ISO the admin places in <web_root>/iso/ (used in grub.cfg)
    pub iso_name: String,
    pub tftp_root: String,
    pub web_root: String,
    pub hosts_file: String,
    pub bindings: Vec<Binding>,
    pub dry_run: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            iface: "eth0".into(),
            server_ip: "192.168.5.1".into(),
            prefix: 24,
            dhcp_start: "192.168.5.100".into(),
            dhcp_end: "192.168.5.200".into(),
            lease: "24h".into(),
            upstream_dns: "8.8.8.8".into(),
            set_static_ip: false,
            only_known_hosts: false,
            target_hostname: "ubuntu-node".into(),
            target_user: "ubuntu".into(),
            target_password: String::new(),
            iso_name: "ubuntu-25.04-beta-live-server-amd64.iso".into(),
            tftp_root: "/var/lib/tftpboot".into(),
            web_root: "/var/www/html".into(),
            hosts_file: "/etc/dnsmasq.d/pxe-hosts.conf".into(),
            bindings: Vec::new(),
            dry_run: false,
        }
    }
}

pub fn normalize_mac(mac: &str) -> String {
    mac.trim().to_lowercase().replace('-', ":")
}

pub fn is_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts.iter().all(|p| {
            p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        })
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(format!("'{other}' is not a boolean (use 1/0)")),
    }
}

pub fn is_hostname(h: &str) -> bool {
    let b = h.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn is_username(u: &str) -> bool {
    let b = u.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn is_lease(l: &str) -> bool {
    if l == "infinite" {
        return true;
    }
    let digits = l.trim_end_matches(|c| c == 's' || c == 'm' || c == 'h' || c == 'd');
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) && l.len() - digits.len() <= 1
}

fn parse_ip(label: &str, v: &str, errs: &mut Vec<String>) -> Option<Ipv4Addr> {
    match v.trim().parse::<Ipv4Addr>() {
        Ok(ip) => Some(ip),
        Err(_) => {
            errs.push(format!("{label}: '{v}' is not a valid IPv4 address"));
            None
        }
    }
}

pub fn netmask_u32(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix as u32)
    }
}

impl Config {
    /// Apply one KEY=VALUE setting (keys as in the config file, case-insensitive).
    pub fn set(&mut self, key: &str, val: &str) -> Result<(), String> {
        let v = val.trim();
        match key.trim().to_uppercase().as_str() {
            "IFACE" => self.iface = v.into(),
            "SERVER_IP" => self.server_ip = v.into(),
            "PREFIX" => {
                self.prefix = v.parse().map_err(|_| format!("PREFIX: '{v}' is not a number"))?
            }
            "NETMASK" => {} // derived from PREFIX; accepted for compatibility
            "DHCP_START" => self.dhcp_start = v.into(),
            "DHCP_END" => self.dhcp_end = v.into(),
            "LEASE" => self.lease = v.into(),
            "UPSTREAM_DNS" => self.upstream_dns = v.into(),
            "SET_STATIC_IP" => self.set_static_ip = parse_bool(v)?,
            "ONLY_KNOWN_HOSTS" => self.only_known_hosts = parse_bool(v)?,
            "TARGET_HOSTNAME" => self.target_hostname = v.into(),
            "TARGET_USER" => self.target_user = v.into(),
            // password: keep spaces, only strip the line ending
            "TARGET_PASSWORD" => {
                self.target_password = val.trim_end_matches(|c| c == '\r' || c == '\n').into()
            }
            "ISO_NAME" => self.iso_name = v.into(),
            // settings from older versions that downloaded files: accepted and ignored
            "RELEASE_URL" | "NETBOOT_TAR" | "ISO_FILE" => {}
            "TFTP_ROOT" => self.tftp_root = v.trim_end_matches('/').into(),
            "WEB_ROOT" => self.web_root = v.trim_end_matches('/').into(),
            "HOSTS_FILE" => self.hosts_file = v.into(),
            "HOST" => {
                let mut it = v.splitn(3, ',');
                let mac = normalize_mac(it.next().unwrap_or(""));
                let ip = it.next().unwrap_or("").trim().to_string();
                let hostname = it.next().unwrap_or("").trim().to_string();
                self.bindings.push(Binding { mac, ip, hostname });
            }
            other => return Err(format!("unknown setting '{other}'")),
        }
        Ok(())
    }

    /// Load KEY=VALUE lines ('#' comments, blank lines ignored).
    pub fn load_file(&mut self, path: &str) -> Result<(), String> {
        let text = fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        for (n, line) in text.lines().enumerate() {
            let t = line.trim_start();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let (k, v) = t
                .split_once('=')
                .ok_or_else(|| format!("{path}:{}: expected KEY=VALUE", n + 1))?;
            self.set(k, v).map_err(|e| format!("{path}:{}: {e}", n + 1))?;
        }
        Ok(())
    }

    /// Parse `install` arguments: --config FILE first, then flags override it.
    pub fn from_args(args: &[String]) -> Result<Config, String> {
        let mut c = Config::default();
        if let Ok(pw) = std::env::var("PXE_TARGET_PASSWORD") {
            c.target_password = pw;
        }
        // pass 1: config files
        let mut i = 0;
        while i < args.len() {
            if args[i] == "--config" {
                let p = args.get(i + 1).ok_or("--config needs a file")?;
                c.load_file(p)?;
                i += 1;
            }
            i += 1;
        }
        // pass 2: flags
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            match a {
                "--config" => i += 1,
                "--dry-run" => c.dry_run = true,
                "--static-ip" => c.set_static_ip = true,
                "--only-known-hosts" => c.only_known_hosts = true,
                _ if a.starts_with("--") => {
                    let key = a[2..].replace('-', "_");
                    let val = args.get(i + 1).ok_or(format!("{a} needs a value"))?;
                    c.set(&key, val)?;
                    i += 1;
                }
                _ => return Err(format!("unexpected argument '{a}'")),
            }
            i += 1;
        }
        Ok(c)
    }

    pub fn netmask(&self) -> Ipv4Addr {
        Ipv4Addr::from(netmask_u32(self.prefix))
    }

    /// Network address, e.g. 192.168.5.0 for 192.168.5.1/24.
    pub fn network(&self) -> Option<Ipv4Addr> {
        let ip: Ipv4Addr = self.server_ip.parse().ok()?;
        Some(Ipv4Addr::from(u32::from(ip) & netmask_u32(self.prefix)))
    }

    pub fn done_dir(&self) -> String {
        format!("{}/done", self.tftp_root)
    }

    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.iface.is_empty() {
            errs.push("IFACE is required".to_string());
        }
        if !(8..=30).contains(&self.prefix) {
            errs.push("PREFIX must be between 8 and 30".to_string());
        }
        let server = parse_ip("SERVER_IP", &self.server_ip, &mut errs);
        let start = parse_ip("DHCP_START", &self.dhcp_start, &mut errs);
        let end = parse_ip("DHCP_END", &self.dhcp_end, &mut errs);
        parse_ip("UPSTREAM_DNS", &self.upstream_dns, &mut errs);

        let mask = netmask_u32(self.prefix);
        let in_net = |a: Ipv4Addr, s: Ipv4Addr| (u32::from(a) & mask) == (u32::from(s) & mask);
        if let (Some(s), true) = (server, (8..=30).contains(&self.prefix)) {
            for (label, a) in [("DHCP_START", start), ("DHCP_END", end)] {
                if let Some(a) = a {
                    if !in_net(a, s) {
                        errs.push(format!("{label} {a} is outside the server subnet"));
                    }
                }
            }
            if let (Some(a), Some(b)) = (start, end) {
                if u32::from(a) > u32::from(b) {
                    errs.push("DHCP_START must not be greater than DHCP_END".to_string());
                }
                if u32::from(a) <= u32::from(s) && u32::from(s) <= u32::from(b) {
                    errs.push("SERVER_IP must not be inside the DHCP range".to_string());
                }
            }
        }
        if !is_lease(&self.lease) {
            errs.push("LEASE must look like 24h, 30m, 3600 or infinite".to_string());
        }
        if !is_hostname(&self.target_hostname) {
            errs.push("TARGET_HOSTNAME may only contain a-z, 0-9 and '-'".to_string());
        }
        if !is_username(&self.target_user) {
            errs.push("TARGET_USER must be lowercase letters, digits, '_' or '-'".to_string());
        }
        if self.target_password.chars().count() < 8 {
            errs.push(
                "TARGET_PASSWORD must be at least 8 characters (config file or PXE_TARGET_PASSWORD)"
                    .to_string(),
            );
        }
        if !self.iso_name.ends_with(".iso") || self.iso_name.contains('/') {
            errs.push("ISO_NAME must be a plain .iso file name".to_string());
        }
        for p in [&self.tftp_root, &self.web_root, &self.hosts_file] {
            if !p.starts_with('/') {
                errs.push(format!("path '{p}' must be absolute"));
            }
        }

        let mut seen_mac: Vec<&str> = Vec::new();
        let mut seen_ip: Vec<&str> = Vec::new();
        for b in &self.bindings {
            if !is_mac(&b.mac) {
                errs.push(format!("HOST: '{}' is not a valid MAC address", b.mac));
            }
            if seen_mac.contains(&b.mac.as_str()) {
                errs.push(format!("HOST: MAC {} listed twice", b.mac));
            }
            seen_mac.push(&b.mac);
            if let Some(a) = parse_ip(&format!("HOST {}", b.mac), &b.ip, &mut errs) {
                if seen_ip.contains(&b.ip.as_str()) {
                    errs.push(format!("HOST: IP {} used twice", b.ip));
                }
                seen_ip.push(&b.ip);
                if let Some(s) = server {
                    if !in_net(a, s) {
                        errs.push(format!("HOST {}: {} is outside the server subnet", b.mac, a));
                    }
                    if a == s {
                        errs.push(format!("HOST {}: IP equals SERVER_IP", b.mac));
                    }
                }
            }
            if !b.hostname.is_empty() && !is_hostname(&b.hostname) {
                errs.push(format!("HOST {}: invalid hostname '{}'", b.mac, b.hostname));
            }
        }
        errs
    }

    /// KEY=VALUE text accepted by `load_file` (used by the GUI to hand settings to `install`).
    pub fn to_conf(&self) -> String {
        let flag = |v: bool| if v { "1" } else { "0" };
        let mut lines: Vec<String> = vec![
            format!("IFACE={}", self.iface),
            format!("SERVER_IP={}", self.server_ip),
            format!("PREFIX={}", self.prefix),
            format!("DHCP_START={}", self.dhcp_start),
            format!("DHCP_END={}", self.dhcp_end),
            format!("LEASE={}", self.lease),
            format!("UPSTREAM_DNS={}", self.upstream_dns),
            format!("SET_STATIC_IP={}", flag(self.set_static_ip)),
            format!("ONLY_KNOWN_HOSTS={}", flag(self.only_known_hosts)),
            format!("TARGET_HOSTNAME={}", self.target_hostname),
            format!("TARGET_USER={}", self.target_user),
            format!("TARGET_PASSWORD={}", self.target_password),
            format!("ISO_NAME={}", self.iso_name),
        ];
        for h in &self.bindings {
            lines.push(format!("HOST={},{},{}", h.mac, h.ip, h.hostname));
        }
        lines.join("
") + "
"
    }

    /// Non-secret settings the helper commands (ctl / cgi-done) need later.
    pub fn runtime_conf(&self) -> String {
        format!(
            "TFTP_ROOT={}\nWEB_ROOT={}\nHOSTS_FILE={}\nSERVER_IP={}\nPREFIX={}\nISO_NAME={}\nTARGET_HOSTNAME={}\n",
            self.tftp_root, self.web_root, self.hosts_file, self.server_ip, self.prefix, self.iso_name,
            self.target_hostname
        )
    }

    /// Defaults overlaid with /etc/pxe-installer.conf (if present). Used by ctl / cgi-done.
    pub fn load_runtime() -> Config {
        let mut c = Config::default();
        if fs::metadata(RUNTIME_CONF).is_ok() {
            let _ = c.load_file(RUNTIME_CONF);
        }
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Config {
        Config { target_password: "longenough1".into(), ..Config::default() }
    }

    #[test]
    fn defaults_valid() {
        assert!(good().validate().is_empty(), "{:?}", good().validate());
    }

    #[test]
    fn iso_name_checked() {
        let mut c = good();
        c.iso_name = "dir/x.iso".into();
        assert!(c.validate().iter().any(|e| e.contains("ISO_NAME")));
        assert!(good().set("RELEASE_URL", "http://x").is_ok()); // ignored, not an error
    }

    #[test]
    fn netmask() {
        assert_eq!(netmask_u32(24), 0xFFFF_FF00);
        assert_eq!(good().netmask().to_string(), "255.255.255.0");
        assert_eq!(good().network().unwrap().to_string(), "192.168.5.0");
    }

    #[test]
    fn range_checks() {
        let mut c = good();
        c.dhcp_end = "192.168.6.200".into();
        assert!(c.validate().iter().any(|e| e.contains("outside")));
        let mut c = good();
        c.server_ip = "192.168.5.150".into();
        assert!(c.validate().iter().any(|e| e.contains("inside the DHCP range")));
    }

    #[test]
    fn binding_checks() {
        let mut c = good();
        c.set("HOST", "AA-BB-CC-DD-EE-01,192.168.5.11,n1").unwrap();
        c.set("HOST", "aa:bb:cc:dd:ee:01,192.168.5.12").unwrap();
        assert!(c.validate().iter().any(|e| e.contains("twice")));
        let mut c = good();
        c.set("HOST", "zz,192.168.5.11").unwrap();
        assert!(c.validate().iter().any(|e| e.contains("MAC")));
    }

    #[test]
    fn args_parse() {
        let args: Vec<String> = [
            "--iface",
            "ens3",
            "--host",
            "aa:bb:cc:dd:ee:01,192.168.5.11,n1",
            "--static-ip",
            "--dry-run",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let c = Config::from_args(&args).unwrap();
        assert_eq!(c.iface, "ens3");
        assert!(c.set_static_ip && c.dry_run);
        assert_eq!(c.bindings.len(), 1);
    }

    #[test]
    fn conf_roundtrip() {
        let mut c = good();
        c.target_password = "pa ss'$word".into();
        c.set("HOST", "aa:bb:cc:dd:ee:01,192.168.5.11,n1").unwrap();
        let path = std::env::temp_dir().join(format!("pxe-rt-{}.conf", std::process::id()));
        std::fs::write(&path, c.to_conf()).unwrap();
        let mut d = Config::default();
        d.load_file(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(d.target_password, "pa ss'$word");
        assert_eq!(d.bindings, c.bindings);
        assert_eq!(d.prefix, 24);
    }

    #[test]
    fn lease_forms() {
        assert!(is_lease("24h") && is_lease("3600") && is_lease("infinite"));
        assert!(!is_lease("h") && !is_lease("24hh") && !is_lease("x"));
    }
}
