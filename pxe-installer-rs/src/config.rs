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

/// How the target gets its Clonezilla Live system (the PXE deploy way).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootMode {
    /// filesystem.squashfs is downloaded over HTTP into the target's RAM; the root is a tmpfs overlay
    Tmpfs,
    /// the target mounts the extracted Clonezilla Live folder from the server over NFS (diskless root)
    Nfs,
}

impl RootMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RootMode::Tmpfs => "tmpfs",
            RootMode::Nfs => "nfs",
        }
    }

    pub fn parse(v: &str) -> Result<RootMode, String> {
        match v.trim().to_lowercase().as_str() {
            "tmpfs" | "ram" | "http" => Ok(RootMode::Tmpfs),
            "nfs" => Ok(RootMode::Nfs),
            other => Err(format!("ROOT_MODE: '{other}' is not valid (use tmpfs or nfs)")),
        }
    }
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
    /// PXE menu restores the Clonezilla image by default (ERASES the target disk, no question asked);
    /// off = the default entry is interactive Clonezilla and the restore entry must be selected by hand
    pub auto_install: bool,
    /// Hostname given to a restored host that has no hostname bound to its MAC
    pub target_hostname: String,
    /// Name of the Clonezilla image (a directory inside `image_dir`) that is restored onto the targets
    pub image_name: String,
    /// Clonezilla image repository: exported read-only over NFS and mounted on /home/partimag by the target
    pub image_dir: String,
    /// Disk of the target the image is restored onto (name under /dev, e.g. nvme0n1)
    pub target_disk: String,
    /// How the target gets Clonezilla Live: `Tmpfs` (HTTP download into RAM) or `Nfs` (NFS root)
    pub root_mode: RootMode,
    /// NFS mode: the extracted Clonezilla Live zip (it contains live/filesystem.squashfs), exported read-only
    pub live_root: String,
    /// `install` downloads and places the netboot files and Clonezilla Live (off = the admin copies them)
    pub download: bool,
    /// Ubuntu netboot tarball (its amd64/ folder goes into the TFTP root)
    pub netboot_url: String,
    /// Clonezilla Live amd64 zip (kernel + initrd -> TFTP, filesystem.squashfs -> web root, whole zip -> live_root)
    pub clonezilla_url: String,
    /// Where the downloaded archives are kept (a re-run does not download them again)
    pub download_dir: String,
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
            auto_install: true,
            target_hostname: "ubuntu-node".into(),
            image_name: "cri-golden".into(),
            image_dir: "/home/partimag".into(),
            target_disk: "nvme0n1".into(),
            root_mode: RootMode::Tmpfs,
            live_root: "/srv/nfs/clonezilla".into(),
            download: true,
            netboot_url: "https://releases.ubuntu.com/26.04/ubuntu-26.04.1-netboot-amd64.tar.gz".into(),
            clonezilla_url: "https://sourceforge.net/projects/clonezilla/files/clonezilla_live_stable/3.3.3-37/clonezilla-live-3.3.3-37-amd64.zip/download?use_mirror=twds".into(),
            download_dir: "/var/cache/pxe-installer".into(),
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

/// Clonezilla image name: a plain directory name (it ends up in the kernel command line and a shell command).
pub fn is_image_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with('.')
        && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-')
}

/// An http(s) URL without whitespace, quotes or control characters (it is handed to curl as one argument).
pub fn is_url(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://"))
        && u.len() <= 1000
        && u.bytes().all(|c| c.is_ascii_graphic() && c != b'"' && c != b'\'' && c != b'\\')
}

/// File name a download is saved as: the last path segment, ignoring the query. SourceForge style links
/// end in `/download`, so the segment before it is used then.
pub fn url_filename(u: &str) -> Option<String> {
    let path = u.split(['?', '#']).next()?;
    let rest = path.split_once("://")?.1;
    let mut segs: Vec<&str> = rest.split('/').skip(1).filter(|s| !s.is_empty()).collect();
    if segs.last() == Some(&"download") {
        segs.pop();
    }
    let name = *segs.last()?;
    let ok = !name.starts_with('.') && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-+".contains(&c));
    ok.then(|| name.to_string())
}

/// Disk name under /dev (nvme0n1, sda, vda ...).
pub fn is_disk_name(d: &str) -> bool {
    !d.is_empty() && d.len() <= 16 && d.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
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
            "AUTO_INSTALL" => self.auto_install = parse_bool(v)?,
            "TARGET_HOSTNAME" => self.target_hostname = v.into(),
            "IMAGE_NAME" => self.image_name = v.into(),
            "IMAGE_DIR" => self.image_dir = v.trim_end_matches('/').into(),
            "TARGET_DISK" => self.target_disk = v.trim_start_matches("/dev/").into(),
            "ROOT_MODE" => self.root_mode = RootMode::parse(v)?,
            "LIVE_ROOT" => self.live_root = v.trim_end_matches('/').into(),
            "DOWNLOAD" => self.download = parse_bool(v)?,
            "NETBOOT_URL" => self.netboot_url = v.into(),
            "CLONEZILLA_URL" => self.clonezilla_url = v.into(),
            "DOWNLOAD_DIR" => self.download_dir = v.trim_end_matches('/').into(),
            // settings of older (autoinstall / ISO based) versions: accepted and ignored
            "TARGET_USER" | "TARGET_PASSWORD" | "ISO_NAME" | "NFS_ROOT" => {}
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
                "--no-download" => c.download = false,
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
        if !is_image_name(&self.image_name) {
            errs.push("IMAGE_NAME may only contain letters, digits, '.', '_' and '-'".to_string());
        }
        if !is_disk_name(&self.target_disk) {
            errs.push("TARGET_DISK must be a disk name such as nvme0n1 or sda (no /dev/ prefix)".to_string());
        }
        // image_dir and live_root are put on the target's kernel command line
        for (label, p) in [("IMAGE_DIR", &self.image_dir), ("LIVE_ROOT", &self.live_root)] {
            if !p.bytes().all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)) {
                errs.push(format!("{label} may only contain letters, digits, '/', '.', '_' and '-'"));
            }
        }
        for p in [&self.tftp_root, &self.web_root, &self.image_dir, &self.live_root, &self.hosts_file] {
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

    /// Problems with the download settings; checked by `install` (only when DOWNLOAD is on).
    pub fn validate_download(&self) -> Vec<String> {
        let mut errs = Vec::new();
        for (label, u) in [("NETBOOT_URL", &self.netboot_url), ("CLONEZILLA_URL", &self.clonezilla_url)] {
            if !is_url(u) {
                errs.push(format!("{label} must be an http(s) URL without spaces or quotes"));
            } else if url_filename(u).is_none() {
                errs.push(format!("{label}: cannot derive a file name from '{u}'"));
            }
        }
        if !self.download_dir.starts_with('/') {
            errs.push(format!("path '{}' must be absolute", self.download_dir));
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
            format!("AUTO_INSTALL={}", flag(self.auto_install)),
            format!("TARGET_HOSTNAME={}", self.target_hostname),
            format!("IMAGE_NAME={}", self.image_name),
            format!("IMAGE_DIR={}", self.image_dir),
            format!("TARGET_DISK={}", self.target_disk),
            format!("ROOT_MODE={}", self.root_mode.as_str()),
            format!("LIVE_ROOT={}", self.live_root),
            format!("DOWNLOAD={}", flag(self.download)),
            format!("NETBOOT_URL={}", self.netboot_url),
            format!("CLONEZILLA_URL={}", self.clonezilla_url),
            format!("DOWNLOAD_DIR={}", self.download_dir),
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
            "TFTP_ROOT={}\nWEB_ROOT={}\nIMAGE_DIR={}\nIMAGE_NAME={}\nTARGET_DISK={}\nHOSTS_FILE={}\nSERVER_IP={}\nPREFIX={}\nAUTO_INSTALL={}\nTARGET_HOSTNAME={}\nROOT_MODE={}\nLIVE_ROOT={}\n",
            self.tftp_root, self.web_root, self.image_dir, self.image_name, self.target_disk, self.hosts_file,
            self.server_ip, self.prefix,
            u8::from(self.auto_install),
            self.target_hostname,
            self.root_mode.as_str(),
            self.live_root
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
        Config::default()
    }

    #[test]
    fn defaults_valid() {
        assert!(good().validate().is_empty(), "{:?}", good().validate());
    }

    #[test]
    fn image_and_disk_checked() {
        let mut c = good();
        c.image_name = "dir/img".into();
        assert!(c.validate().iter().any(|e| e.contains("IMAGE_NAME")));
        let mut c = good();
        c.image_name = "x\"; reboot".into(); // must never reach the kernel command line / a shell
        assert!(c.validate().iter().any(|e| e.contains("IMAGE_NAME")));
        let mut c = good();
        c.target_disk = "nvme0n1 sda".into();
        assert!(c.validate().iter().any(|e| e.contains("TARGET_DISK")));
        let mut c = good();
        c.image_dir = "relative/dir".into();
        assert!(c.validate().iter().any(|e| e.contains("absolute")));
        let mut c = good();
        c.image_dir = "/srv/my images".into(); // a space would split the kernel argument
        assert!(c.validate().iter().any(|e| e.contains("IMAGE_DIR")));
        // "/dev/" prefix is accepted and stripped; settings of older versions are ignored, not errors
        let mut c = good();
        c.set("TARGET_DISK", "/dev/sda").unwrap();
        assert_eq!(c.target_disk, "sda");
        for k in ["RELEASE_URL", "ISO_NAME", "NFS_ROOT", "TARGET_USER", "TARGET_PASSWORD"] {
            assert!(c.set(k, "x").is_ok(), "{k}");
        }
    }

    #[test]
    fn root_mode_parse_and_roundtrip() {
        assert_eq!(good().root_mode, RootMode::Tmpfs); // default: live system in RAM
        assert_eq!(RootMode::parse("NFS").unwrap(), RootMode::Nfs);
        assert_eq!(RootMode::parse("ram").unwrap(), RootMode::Tmpfs);
        assert!(RootMode::parse("squashfs").is_err());
        let mut c = good();
        assert!(c.set("ROOT_MODE", "bogus").is_err());
        c.set("ROOT_MODE", "nfs").unwrap();
        c.set("LIVE_ROOT", "/srv/cz/").unwrap();
        assert_eq!((c.root_mode, c.live_root.as_str()), (RootMode::Nfs, "/srv/cz"));
        // survives both the temp config the GUI writes and the runtime config the helpers read
        let path = std::env::temp_dir().join(format!("pxe-mode-{}.conf", std::process::id()));
        for text in [c.to_conf(), c.runtime_conf()] {
            std::fs::write(&path, text).unwrap();
            let mut d = Config::default();
            d.load_file(path.to_str().unwrap()).unwrap();
            assert_eq!((d.root_mode, d.live_root.as_str()), (RootMode::Nfs, "/srv/cz"));
        }
        let _ = std::fs::remove_file(&path);
        // live_root ends up on the kernel command line
        c.live_root = "/srv/my clonezilla".into();
        assert!(c.validate().iter().any(|e| e.contains("LIVE_ROOT")));
    }

    #[test]
    fn download_settings() {
        // the defaults are valid and map to sensible file names
        let c = good();
        assert!(c.download && c.validate_download().is_empty(), "{:?}", c.validate_download());
        assert_eq!(url_filename(&c.netboot_url).as_deref(), Some("ubuntu-26.04.1-netboot-amd64.tar.gz"));
        // SourceForge style link: ".../<file>.zip/download?use_mirror=twds" -> <file>.zip
        assert_eq!(url_filename(&c.clonezilla_url).as_deref(), Some("clonezilla-live-3.3.3-37-amd64.zip"));
        assert_eq!(url_filename("http://h/a/b.tar.gz?x=1#y").as_deref(), Some("b.tar.gz"));
        assert_eq!(url_filename("http://h/"), None);
        assert_eq!(url_filename("https://h/..%2f"), None);
        // bad URLs are refused
        for bad in ["ftp://h/a.zip", "https://h/a b.zip", "https://h/a\".zip", "file.zip", "https://h/'x'.zip"] {
            let mut c = good();
            c.netboot_url = bad.into();
            assert!(!c.validate_download().is_empty(), "{bad}");
        }
        // keys and flag
        let mut c = good();
        c.set("DOWNLOAD", "0").unwrap();
        assert!(!c.download);
        let args: Vec<String> = ["--no-download", "--netboot-url", "https://h/n.tar.gz"].iter().map(|s| s.to_string()).collect();
        let c = Config::from_args(&args).unwrap();
        assert!(!c.download);
        assert_eq!(c.netboot_url, "https://h/n.tar.gz");
        let mut c = good();
        c.set("DOWNLOAD_DIR", "/var/cache/x/").unwrap();
        assert_eq!(c.download_dir, "/var/cache/x");
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
        c.image_name = "golden-2".into();
        c.target_disk = "sda".into();
        c.set("HOST", "aa:bb:cc:dd:ee:01,192.168.5.11,n1").unwrap();
        let path = std::env::temp_dir().join(format!("pxe-rt-{}.conf", std::process::id()));
        std::fs::write(&path, c.to_conf()).unwrap();
        let mut d = Config::default();
        d.load_file(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!((d.image_name.as_str(), d.target_disk.as_str()), ("golden-2", "sda"));
        assert_eq!(d.bindings, c.bindings);
        assert_eq!(d.prefix, 24);
    }

    #[test]
    fn lease_forms() {
        assert!(is_lease("24h") && is_lease("3600") && is_lease("infinite"));
        assert!(!is_lease("h") && !is_lease("24hh") && !is_lease("x"));
    }
}
