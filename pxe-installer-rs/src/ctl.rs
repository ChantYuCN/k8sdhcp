//! Day-2 commands: installed-host markers, MAC->IP bindings, status, and the Apache CGI callback.
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use crate::config::{is_disk_name, is_hostname, is_image_name, is_mac, normalize_mac, Binding, CastMode, Config, RootMode};
use crate::render;
use crate::util::{fmt_elapsed, mkdir, now_epoch, now_iso, output, probe, run, run_soft, run_tty, step, warn, write_file, Ctx};


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

/// Deploy times. The restore entry calls `pxe-seed/<mac>/start` when Clonezilla comes up (-> a file in
/// `done/start/<mac>`: epoch seconds + ISO time) and post.sh calls `pxe-done` at the end; that call appends
/// one line per finished deploy to `done/deploy-times.csv` (history, survives `reinstall`):
/// `mac,started,finished,elapsed_seconds`. Neither name is a MAC, so the marker logic ignores them.
const DEPLOY_LOG: &str = "deploy-times.csv";

fn start_dir(c: &Config) -> String {
    format!("{}/start", c.done_dir())
}

/// A deploy of `mac` starts now (a restarted deploy simply overwrites the earlier start).
pub fn mark_start(c: &Config, mac: &str) -> Result<(), String> {
    let dir = start_dir(c);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
    let path = format!("{dir}/{mac}");
    fs::write(&path, format!("{}\n{}\n", now_epoch(), now_iso())).map_err(|e| format!("write {path}: {e}"))
}

/// (epoch, ISO time) of a recorded start.
fn read_start(c: &Config, mac: &str) -> Option<(u64, String)> {
    let text = fs::read_to_string(format!("{}/{mac}", start_dir(c))).ok()?;
    let mut lines = text.lines();
    let epoch = lines.next()?.trim().parse().ok()?;
    Some((epoch, lines.next().unwrap_or("").trim().to_string()))
}

/// The deploy of `mac` finished now: append start/finish/elapsed to the history and drop the start file.
/// Without a recorded start (the call failed, or an older install) the line has empty start and elapsed.
pub fn record_deploy(c: &Config, mac: &str) -> Result<(), String> {
    let (started, secs) = match read_start(c, mac) {
        Some((epoch, iso)) => (iso, now_epoch().saturating_sub(epoch).to_string()),
        None => (String::new(), String::new()),
    };
    fs::create_dir_all(c.done_dir()).map_err(|e| format!("mkdir {}: {e}", c.done_dir()))?;
    let path = format!("{}/{DEPLOY_LOG}", c.done_dir());
    let mut line = String::new();
    if !Path::new(&path).exists() {
        line.push_str("mac,started,finished,elapsed_seconds\n");
    }
    line.push_str(&format!("{mac},{started},{},{secs}\n", now_iso()));
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("open {path}: {e}"))?;
    f.write_all(line.as_bytes()).map_err(|e| format!("write {path}: {e}"))?;
    let _ = fs::remove_file(format!("{}/{mac}", start_dir(c)));
    Ok(())
}

/// Finished deploys, oldest first: (mac, started, finished, elapsed seconds if known).
fn deploy_history(c: &Config) -> Vec<(String, String, String, Option<u64>)> {
    let text = fs::read_to_string(format!("{}/{DEPLOY_LOG}", c.done_dir())).unwrap_or_default();
    text.lines()
        .skip(1) // header
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            (f.len() == 4 && is_mac(f[0])).then(|| (f[0].to_string(), f[1].to_string(), f[2].to_string(), f[3].parse().ok()))
        })
        .collect()
}

/// `pxe-ctl times`: every finished deploy with its elapsed time, then the ones that started but have not
/// called done yet (still running, or failed before the end).
fn show_times(c: &Config) {
    let history = deploy_history(c);
    let mut running: Vec<(String, u64, String)> = Vec::new();
    if let Ok(rd) = fs::read_dir(start_dir(c)) {
        for e in rd.flatten() {
            let mac = e.file_name().to_string_lossy().to_string();
            if let (true, Some((epoch, iso))) = (is_mac(&mac), read_start(c, &mac)) {
                running.push((mac, epoch, iso));
            }
        }
    }
    running.sort();
    if history.is_empty() && running.is_empty() {
        println!("no deploy recorded yet (a deploy is recorded from the Clonezilla start to the done callback)");
        return;
    }
    println!("{:<18} {:<26} {:<26} {}", "MAC", "STARTED", "FINISHED", "ELAPSED");
    for (mac, start, end, secs) in &history {
        let elapsed = secs.map(fmt_elapsed).unwrap_or_else(|| "-".into());
        println!("{:<18} {:<26} {:<26} {}", mac, start, end, elapsed);
    }
    let now = now_epoch();
    for (mac, epoch, iso) in &running {
        println!("{:<18} {:<26} {:<26} {} so far", mac, iso, "(not finished)", fmt_elapsed(now.saturating_sub(*epoch)));
    }
}

/// Binding lines written by the dnsmasq versions of this tool carry this prefix; it is still accepted (and
/// dropped) when the hosts file is read, so old bindings survive the move to Kea.
const LEGACY_HOST_PREFIX: &str = "dhcp-host=";

/// Bindings as "mac,ip[,name]" (comments/blank lines skipped, legacy prefix stripped). The hosts file is our
/// own store; Kea only ever sees the reservations generated from it (`apply_dhcp`).
fn read_hosts(c: &Config) -> Vec<String> {
    fs::read_to_string(&c.hosts_file)
        .map(|t| {
            t.lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.strip_prefix(LEGACY_HOST_PREFIX).unwrap_or(l).to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The bindings as DHCP reservations (a line without a valid "mac,ip" is skipped).
pub fn read_bindings(c: &Config) -> Vec<Binding> {
    read_hosts(c)
        .iter()
        .filter_map(|l| {
            let mut f = l.split(',');
            let mac = normalize_mac(f.next()?);
            let ip = f.next()?.trim().to_string();
            let hostname = f.next().unwrap_or("").trim().to_string();
            (is_mac(&mac) && is_ip(&ip)).then_some(Binding { mac, ip, hostname })
        })
        .collect()
}

fn write_hosts(c: &Config, lines: &[String]) -> Result<(), String> {
    let tmp = format!("{}.tmp", c.hosts_file);
    let mut body = String::from("# managed by pxe-installer (pxe-ctl bind / unbind): mac,ip[,hostname] per line\n");
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    if let Some(parent) = Path::new(&c.hosts_file).parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(&tmp, body).map_err(|e| format!("write {tmp}: {e}"))?;
    fs::rename(&tmp, &c.hosts_file).map_err(|e| format!("rename {tmp}: {e}"))
}

/// Regenerate the Kea config from the current bindings and make Kea load it. The new file is checked with
/// `kea-dhcp4 -t` first and only then put in place, so a mistake never replaces a working config. A Kea
/// config that was not written by `start` (no marker) is left alone: `start` will write ours.
pub fn apply_dhcp(c: &Config) -> Result<(), String> {
    let ctx = Ctx { dry: false };
    if c.cast_mode == CastMode::Multicast {
        println!("note: multicast mode: DHCP is served by DRBL, so the binding only sets the hostname the clone gets");
        return Ok(());
    }
    let current = fs::read_to_string(render::KEA_CONF_PATH).unwrap_or_default();
    if !current.starts_with(render::GENERATED_MARK) {
        println!("note: the DHCP server is not configured yet (run Start); the binding is saved and applied then");
        return Ok(());
    }
    let new = format!("{}.new", render::KEA_CONF_PATH);
    write_file(ctx, &new, &render::kea_conf(c, &read_bindings(c)), 0o644, false)?;
    if let Err(e) = run(ctx, "kea-dhcp4", &["-t", &new]) {
        let _ = fs::remove_file(&new);
        return Err(format!("the new DHCP config was rejected and NOT applied (the bindings file is saved): {e}"));
    }
    fs::rename(&new, render::KEA_CONF_PATH).map_err(|e| format!("rename {new}: {e}"))?;
    // SIGHUP re-reads the config without dropping leases; falls back to a restart if the unit cannot reload.
    // Only when Kea runs: with the PXE server stopped, a binding must not start it again.
    if service_active(render::DHCP_SERVICE) {
        run_soft(ctx, "systemctl", &["reload-or-restart", render::DHCP_SERVICE]);
    }
    Ok(())
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
    println!("bound {mac} -> {ip}");
    if reload {
        apply_dhcp(c)?;
    }
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
    println!("unbound {key}");
    apply_dhcp(c)
}

/// Active leases of Kea's memfile (CSV: address,hwaddr,client_id,valid_lifetime,expire,subnet_id,...,state).
/// The file is a journal: a later line for the same address replaces the earlier one, and valid_lifetime 0
/// (released) or a non-zero state (declined / expired-reclaimed) means the lease is gone.
fn parse_leases(text: &str, now: u64) -> Vec<(String, String)> {
    let mut latest: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() >= 10 {
            latest.insert(f[0], f);
        }
    }
    latest
        .into_iter()
        .filter(|(_, f)| f[3] != "0" && f[9] == "0" && f[4].parse::<u64>().map(|e| e > now).unwrap_or(false))
        .map(|(ip, f)| (f[1].to_lowercase(), ip.to_string()))
        .collect()
}

/// (mac, ip) of every active lease of the DHCP server.
fn read_leases() -> Vec<(String, String)> {
    parse_leases(&fs::read_to_string(render::KEA_LEASES_PATH).unwrap_or_default(), now_epoch())
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
    let leases = read_leases();

    let mut macs: Vec<String> = installed.iter().map(|(m, _)| m.clone()).collect();
    for l in &hosts {
        macs.push(l.split(',').next().unwrap_or("").to_lowercase());
    }
    for (mac, _) in &leases {
        macs.push(mac.clone());
    }
    macs.retain(|m| is_mac(m));
    macs.sort();
    macs.dedup();

    let history = deploy_history(c);
    println!("cast mode: {} (switch while the PXE server is stopped: pxe-ctl cast unicast|multicast)\n", c.cast_mode.as_str());
    println!("{:<18} {:<16} {:<16} {:<10} {:<26} {}", "MAC", "BOUND IP", "LEASE IP", "STATE", "INSTALLED AT", "DEPLOY TIME");
    for mac in &macs {
        let bound = hosts
            .iter()
            .find(|l| l.split(',').next().map(|m| m.eq_ignore_ascii_case(mac)).unwrap_or(false))
            .and_then(|l| l.split(',').nth(1))
            .unwrap_or("-");
        let lease = leases.iter().find(|(m, _)| m.eq_ignore_ascii_case(mac)).map(|(_, ip)| ip.as_str()).unwrap_or("-");
        let done = installed.iter().find(|(m, _)| m == mac);
        let state = if done.is_some() {
            "installed"
        } else if lease != "-" {
            "pending"
        } else {
            "waiting"
        };
        // elapsed time of the latest finished deploy of this MAC
        let took = history
            .iter()
            .rev()
            .find(|h| &h.0 == mac)
            .and_then(|h| h.3)
            .map(fmt_elapsed)
            .unwrap_or_default();
        println!("{:<18} {:<16} {:<16} {:<10} {:<26} {}", mac, bound, lease, state, done.map(|d| d.1.as_str()).unwrap_or(""), took);
    }
    println!("\nstate: installed = marker present (PXE skips it) | pending = has a DHCP lease, no done callback yet | waiting = bound, never seen");
}

/// `ctl mode [tmpfs|nfs]` - show or switch the PXE deploy way. The switch is applied at once: the runtime
/// config is updated, grub.cfg is rewritten and the NFS exports are adjusted (the live root is only
/// exported in nfs mode). Hosts that are already booted into Clonezilla are not affected; the next PXE
/// boot uses the new way. No service restart is needed.
fn switch_mode(mut c: Config, wanted: Option<&str>) -> Result<(), String> {
    let Some(wanted) = wanted else {
        println!("PXE deploy way: {}", c.root_mode.as_str());
        return Ok(());
    };
    c.set("ROOT_MODE", wanted)?;
    let ctx = Ctx { dry: false };
    write_file(ctx, crate::config::RUNTIME_CONF, &c.runtime_conf(), 0o644, false)?;
    write_grub(&c, ctx)?;
    crate::install::setup_nfs(&c, ctx)?;
    println!(
        "PXE deploy way is now: {} ({})",
        c.root_mode.as_str(),
        match c.root_mode {
            RootMode::Tmpfs => "the live system is downloaded over HTTP into the target's RAM",
            RootMode::Nfs => "the target mounts the live system from the server over NFS",
        }
    );
    // the new way needs other files: unpack them from the Clonezilla Live zip when it is available
    if let Err(e) = crate::install::import_live_if_missing(&c, ctx) {
        warn(&e);
    }
    crate::install::report_missing(&c);
    Ok(())
}

/// DHCP server of a DRBL setup (multicast mode). It is not started by this tool, but it must be stopped too
/// before the cast mode can change.
const DRBL_DHCP_SERVICE: &str = "isc-dhcp-server";

/// The services of either stack that are running right now (empty = the PXE server is stopped).
pub fn running_pxe_services() -> Vec<&'static str> {
    [render::DHCP_SERVICE, render::TFTP_SERVICE, "apache2", "nfs-server", DRBL_DHCP_SERVICE]
        .into_iter()
        .filter(|s| service_active(s))
        .collect()
}

/// `ctl cast [unicast|multicast]` - show or switch how the image reaches the targets. Only allowed while the PXE
/// server is stopped (`pxe-ctl stop`), so no target is being served by the old way when the mode changes.
/// unicast = this tool serves the image over NFS (Kea + tftpd-hpa + Apache + NFS), one copy per target.
/// multicast = one stream for all targets, served by DRBL + Clonezilla SE (see drbl-clonezilla-se-multicast-guide.md);
/// `start` of this tool is refused in that mode, so Kea cannot fight DRBL's DHCP for the network.
fn switch_cast(mut c: Config, wanted: Option<&str>) -> Result<(), String> {
    let Some(wanted) = wanted else {
        println!("Cast mode: {}", c.cast_mode.as_str());
        return Ok(());
    };
    let new = CastMode::parse(wanted)?;
    if new == c.cast_mode {
        println!("Cast mode is already {}", new.as_str());
        return Ok(());
    }
    let running = running_pxe_services();
    if !running.is_empty() {
        return Err(format!(
            "the PXE server is still running ({}): stop it first (pxe-ctl stop / the Stop button), then switch the cast mode",
            running.join(", ")
        ));
    }
    c.cast_mode = new;
    let ctx = Ctx { dry: false };
    write_file(ctx, crate::config::RUNTIME_CONF, &c.runtime_conf(), 0o644, false)?;
    match new {
        CastMode::Multicast => {
            // Kea must not come back at boot and answer DHCP next to DRBL's server
            run_soft(ctx, "systemctl", &["disable", render::DHCP_SERVICE]);
            println!(
                "Cast mode is now: multicast (one stream for all targets, DRBL + Clonezilla SE).\n\
                 Start no longer starts Kea / TFTP: it sets up the callbacks and the DRBL pre/post-run scripts\n\
                 (hostname, machine-id, SSH keys, done marker, deploy time); DRBL serves DHCP/TFTP and the session,\n\
                 see drbl-clonezilla-se-multicast-guide.md (drblpush -i, then drbl-ocs ... multicast_restoredisk).\n\
                 Switch back with: sudo pxe-ctl cast unicast (after stopping DRBL's DHCP: pxe-ctl stop)."
            );
        }
        CastMode::Unicast => println!(
            "Cast mode is now: unicast (every target restores its own copy over NFS).\n\
             Run Start to serve it (DRBL's DHCP and TFTP must not run at the same time)."
        ),
    }
    Ok(())
}

/// `ctl auto [on|off]` - show or switch AUTO_INSTALL. on = a PXE-booted host starts Clonezilla Live and restores
/// the image onto TARGET_DISK by itself, no menu choice and no question (it ERASES the disk); off = the menu
/// starts interactive Clonezilla and the restore entry has to be selected by hand. Applied at once to the next
/// PXE boot (grub.cfg is rewritten); hosts that are already booted into Clonezilla are not affected.
fn switch_auto(mut c: Config, wanted: Option<&str>) -> Result<(), String> {
    let describe = |on: bool| {
        if on {
            "ON: PXE-booted hosts restore the image onto /dev/{disk} by themselves (the disk is ERASED, nothing is asked)"
        } else {
            "OFF: the PXE menu starts interactive Clonezilla; the restore entry must be selected by hand"
        }
    };
    if let Some(w) = wanted {
        let on = match w {
            "on" | "1" | "yes" | "true" => true,
            "off" | "0" | "no" | "false" => false,
            other => return Err(format!("'{other}' is not on or off")),
        };
        c.auto_install = on;
        let ctx = Ctx { dry: false };
        write_file(ctx, crate::config::RUNTIME_CONF, &c.runtime_conf(), 0o644, false)?;
        write_grub(&c, ctx)?;
    }
    println!("Automatic restore is {}", describe(c.auto_install).replace("{disk}", &c.target_disk));
    Ok(())
}

/// `ocs-sr` saves images under this folder (Clonezilla's default `ocsroot`), so IMAGE_DIR has to be it.
const OCS_ROOT: &str = "/home/partimag";
/// Only one capture at a time (the GUI button could be clicked twice). /run is a tmpfs: gone after a reboot.
const CLONE_LOCK: &str = "/run/pxe-installer-clone.lock";

/// Arguments of `ctl clone`: (disk name without /dev/, --yes, --force).
fn parse_clone_args(args: &[String]) -> Result<(String, bool, bool), String> {
    let mut disk: Option<String> = None;
    let (mut yes, mut force) = (false, false);
    for a in args {
        match a.as_str() {
            "--yes" => yes = true,
            "--force" => force = true,
            d if !d.starts_with('-') && disk.is_none() => disk = Some(d.trim_start_matches("/dev/").to_string()),
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    let disk = disk.ok_or("disk name required, e.g.: pxe-ctl clone nvme0n2 [--yes] [--force]")?;
    if !is_disk_name(&disk) {
        return Err(format!("'{disk}' is not a disk name (use e.g. sda or nvme0n2, lowercase letters and digits)"));
    }
    Ok((disk, yes, force))
}

/// Removes the clone lock when the capture ends (or fails).
struct CloneLock;
impl Drop for CloneLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(CLONE_LOCK);
    }
}

/// Read one answer from the terminal: repeats `msg` until the first letter typed is one of `choices`.
fn ask(msg: &str, choices: &str) -> Result<char, String> {
    let stdin = std::io::stdin();
    loop {
        print!("{msg}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).map_err(|e| format!("read answer: {e}"))? == 0 {
            return Err("no answer (the terminal was closed)".into());
        }
        if let Some(ch) = line.trim().chars().next().map(|c| c.to_ascii_lowercase()) {
            if choices.contains(ch) {
                return Ok(ch);
            }
        }
    }
}

/// Filesystems fsck can look at on `dev` (the disk and its partitions): (device path, fs type).
fn checkable_filesystems(dev: &str) -> Result<Vec<(String, String)>, String> {
    // not checkable as a filesystem: swap, and containers (LVM / LUKS / RAID members)
    const SKIP: [&str; 6] = ["swap", "LVM2_member", "crypto_LUKS", "linux_raid_member", "zfs_member", "ddf_raid_member"];
    let out = output("lsblk", &["-nrpo", "PATH,TYPE,FSTYPE", dev]).map_err(|e| format!("cannot list partitions of {dev}: {e}"))?;
    let mut found = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // a partition without a filesystem has only two fields
        if f.len() == 3 && matches!(f[1], "part" | "disk") && !SKIP.contains(&f[2]) {
            found.push((f[0].to_string(), f[2].to_string()));
        }
    }
    Ok(found)
}

/// Run `fsck -f` on every filesystem of `dev` BEFORE the capture, so a damaged filesystem does not end up in
/// the golden image. Interactive: fsck runs on the terminal (its own prompts are answered by the user) and on
/// a problem the user chooses: run it again, open a shell to fix it by hand, continue anyway, or abort.
/// Not interactive: read-only `fsck -f -n`; any problem aborts the clone.
fn check_filesystems(dev: &str, interactive: bool) -> Result<(), String> {
    let list = checkable_filesystems(dev)?;
    if list.is_empty() {
        println!("no checkable filesystem found on {dev}");
        return Ok(());
    }
    step(&format!("Checking {} filesystem(s) of {dev} with fsck", list.len()));
    for (path, fs_type) in &list {
        // `fsck.<type>` is looked up by fsck itself; check first so a missing tool is a note, not a "problem"
        if !probe("sh", &["-c", "command -v \"fsck.$1\" >/dev/null", "sh", fs_type]) {
            println!("skip {path}: fsck.{fs_type} is not installed");
            continue;
        }
        loop {
            println!("\n--- fsck -f {path} ({fs_type}) ---");
            let args: Vec<&str> = if interactive { vec!["-f", path] } else { vec!["-f", "-n", path] };
            let code = run_tty("fsck", &args)?;
            // fsck exit code bits: 1 = errors corrected, 2 = corrected + reboot advised; anything else is a problem
            if code & !3 == 0 {
                println!("{path}: OK");
                break;
            }
            if !interactive {
                return Err(format!(
                    "fsck found a problem on {path} (exit code {code}): run `sudo pxe-ctl clone <disk>` in a terminal to fix it interactively"
                ));
            }
            let msg = format!(
                "\nfsck reported a problem on {path} (exit code {code}). What now?\n  \
                 [r] run fsck again   [s] open a shell to fix it by hand   [c] continue anyway   [a] abort\n> "
            );
            match ask(&msg, "rsca")? {
                'r' => {}
                's' => {
                    println!("Shell on {path}'s host: fix it, then type `exit` (fsck runs again afterwards).");
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                    let _ = run_tty(&shell, &[]);
                }
                'c' => {
                    warn(&format!("continuing although fsck reported a problem on {path}"));
                    break;
                }
                _ => return Err("aborted by the user: nothing was captured".into()),
            }
        }
    }
    Ok(())
}

/// `ctl clone <disk> [--yes] [--force]` - capture <disk> of THIS host into the Clonezilla image repository as
/// IMAGE_NAME (`ocs-sr savedisk`, partclone + parallel zstd) and re-export the repository over NFS, so the
/// targets can restore it. The disk is only read, but it must be idle: not mounted, no swap, not in use.
/// Default = interactive, on the terminal it is run from: `fsck -f` of every filesystem first (the user
/// fixes problems right there), then `ocs-sr` with its own prompts. `--yes` runs without any prompt
/// (read-only fsck that aborts on a problem, `ocs-sr -batch`); `--force` replaces an existing image of the
/// same name (interactive: it is asked, non-interactive without --force it is an error).
fn clone_image(c: Config, args: &[String]) -> Result<(), String> {
    let (disk, yes, force) = parse_clone_args(args)?;
    if !is_image_name(&c.image_name) {
        return Err(format!("IMAGE_NAME '{}' is not valid", c.image_name));
    }
    if c.image_dir != OCS_ROOT {
        return Err(format!(
            "IMAGE_DIR is {} but ocs-sr saves images under {OCS_ROOT}: set IMAGE_DIR={OCS_ROOT} (or make {OCS_ROOT} a bind mount of it) and re-run start",
            c.image_dir
        ));
    }
    if !probe("sh", &["-c", "command -v ocs-sr >/dev/null"]) {
        return Err("ocs-sr not found: install the Clonezilla tools first (pxe-installer install)".into());
    }

    // the source must be a whole disk and idle (lsblk lists the mount points of its partitions/LVM children too)
    let dev = format!("/dev/{disk}");
    let kind = output("lsblk", &["-dno", "TYPE", &dev]).map_err(|_| format!("{dev} is not a block device"))?;
    if kind != "disk" {
        return Err(format!("{dev} is a '{kind}', not a whole disk (use e.g. sda, not sda1)"));
    }
    let used = output("lsblk", &["-nro", "MOUNTPOINTS", &dev])
        .or_else(|_| output("lsblk", &["-nro", "MOUNTPOINT", &dev]))
        .map_err(|e| format!("cannot check whether {dev} is in use: {e}"))?;
    let used: Vec<&str> = used.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if !used.is_empty() {
        return Err(format!(
            "{dev} is in use ({}): it must be unmounted and not hold swap; boot Clonezilla Live to image an OS disk that is running",
            used.join(", ")
        ));
    }

    // interactive (default): fsck and ocs-sr run on this terminal, the user sees everything and answers the
    // prompts; --yes: no prompts at all (read-only fsck that aborts on a problem, `ocs-sr -batch`)
    let interactive = !yes;
    if interactive && !std::io::stdin().is_terminal() {
        return Err("an interactive clone needs a terminal: run `sudo pxe-ctl clone <disk>` in one, or pass --yes to run without prompts".into());
    }

    // take the lock BEFORE touching the image: a second click must not delete what the first one is writing
    let _lock = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(CLONE_LOCK)
        .map(|_| CloneLock)
        .map_err(|_| format!("another clone is running (or a stale lock: remove {CLONE_LOCK} if none is)"))?;

    // decide about an existing image NOW, not after the (long) fsck; it is only deleted after fsck succeeded
    let image = format!("{}/{}", c.image_dir, c.image_name);
    let replace = Path::new(&image).exists();
    if replace && !force {
        if !interactive {
            return Err(format!(
                "image {image} already exists: delete it first (sudo rm -rf {image}) or use --force / the \"Overwrite\" box"
            ));
        }
        if ask(&format!("image {image} already exists. Replace it? [y/n] "), "yn")? != 'y' {
            return Err("aborted: the existing image was kept".into());
        }
    }

    check_filesystems(&dev, interactive)?;

    if replace {
        fs::remove_dir_all(&image).map_err(|e| format!("remove old image {image}: {e}"))?;
        println!("removed the old image {image}");
    }

    println!("\nCapturing {dev} as image {} (this takes a while, depending on the used data)", c.image_name);
    let confirm = if interactive { "-c" } else { "-batch" };
    let ocs: &[&str] = &["-q2", confirm, "-j2", "-z9p", "-sfsck", "-senc", "-p", "true", "savedisk", &c.image_name, &disk];
    if interactive {
        println!("ocs-sr runs on this terminal: read its messages and answer its prompts.\n$ ocs-sr {}\n", ocs.join(" "));
        let code = run_tty("ocs-sr", ocs)?;
        if code != 0 {
            return Err(format!("'ocs-sr savedisk' failed (exit code {code})"));
        }
    } else {
        run(Ctx { dry: false }, "ocs-sr", ocs)?;
    }

    // make the new image visible to the targets
    crate::install::setup_nfs(&c, Ctx { dry: false })?;
    println!("Image {image} captured and exported over NFS ({}).", c.image_dir);
    crate::install::report_missing(&c);
    Ok(())
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
        Some("times") => {
            show_times(&c);
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
            let services = [render::DHCP_SERVICE, render::TFTP_SERVICE, "apache2", "nfs-server"];
            run(Ctx { dry: false }, "systemctl", &["stop", services[0], services[1], services[2], services[3]])?;
            // multicast mode: DRBL's DHCP server counts as part of the PXE server too
            if service_active(DRBL_DHCP_SERVICE) {
                run_soft(Ctx { dry: false }, "systemctl", &["stop", DRBL_DHCP_SERVICE]);
            }
            for svc in services.into_iter().chain([DRBL_DHCP_SERVICE]) {
                if svc != DRBL_DHCP_SERVICE || service_active(svc) {
                    println!("{svc}: {}", if service_active(svc) { "STILL ACTIVE" } else { "stopped" });
                }
            }
            println!(
                "PXE server stopped. Start it again with the Start button or: sudo systemctl start {} {} apache2 nfs-server",
                render::DHCP_SERVICE,
                render::TFTP_SERVICE
            );
            Ok(())
        }
        Some("grub") => {
            // run after copying netboot files into the TFTP root (the tarball ships its own grub.cfg)
            write_grub(&c, Ctx { dry: false })?;
            println!("wrote {}/grub/grub.cfg and fixed permissions", c.tftp_root);
            Ok(())
        }
        Some("nfs") => {
            // run after adding or changing a Clonezilla image: (re-)export the image repository
            crate::install::setup_nfs(&c, Ctx { dry: false })?;
            println!("Clonezilla image repository {} exported read-only (image: {})", c.image_dir, c.image_name);
            crate::install::report_missing(&c);
            Ok(())
        }
        Some("import") => {
            // run after putting a new netboot tarball / Clonezilla Live zip into NETBOOT_DIR / LIVESYSTEM_DIR
            crate::install::import_boot_files(&c, Ctx { dry: false })?;
            crate::install::report_missing(&c);
            Ok(())
        }
        Some("mode") => switch_mode(c, args.get(1).map(|s| s.as_str())),
        Some("auto") => switch_auto(c, args.get(1).map(|s| s.as_str())),
        Some("cast") => switch_cast(c, args.get(1).map(|s| s.as_str())),
        Some("clone") => clone_image(c, &args[1..]),
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
        _ => Err("usage: pxe-installer ctl list | status | times | stop | grub | import | auto [on|off] | cast [unicast|multicast] | nfs | mode [tmpfs|nfs] | clone <disk> [--yes] [--force] | reinstall <mac> | done <mac> | binds | bind <mac> <ip> [name] | unbind <mac|ip>".into()),
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
        Ok(()) => {
            // the host is marked either way; a failing time record must not fail the callback
            if let Err(e) = record_deploy(&c, &mac) {
                eprintln!("pxe-done: deploy time not recorded: {e}");
            }
            print!("Content-Type: text/plain\r\n\r\nok {mac}\n")
        }
        Err(e) => print!("Status: 500 Internal Server Error\r\nContent-Type: text/plain\r\n\r\n{e}\n"),
    }
    Ok(())
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

/// IP bound to this MAC (`pxe-ctl bind <mac> <ip>`), if any. `read_bindings` only returns valid IPv4 addresses.
fn ip_for(c: &Config, mac: &str) -> Option<String> {
    read_bindings(c).into_iter().find(|b| b.mac == mac).map(|b| b.ip)
}

fn cgi_error(status: &str, msg: &str) {
    print!("Status: {status}\r\nContent-Type: text/plain\r\n\r\n{msg}\n");
}

/// `pxe-installer cgi-seed` - Apache CGI at /cgi-bin/pxe-seed/<mac>/post.sh.
/// After restoring the Clonezilla image, the target fetches this script (grub.cfg passes its own MAC) and
/// runs it: it gives the clone the hostname bound to its MAC (MACs without a binding get the default
/// hostname), regenerates its machine-id / SSH host keys and calls pxe-done.
pub fn cgi_seed() -> Result<(), String> {
    let c = Config::load_runtime();
    let path = std::env::var("PATH_INFO").unwrap_or_default();
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() != 2 {
        cgi_error("404 Not Found", "expected /<mac>/post.sh");
        return Ok(());
    }
    let mac = normalize_mac(parts[0]);
    if !is_mac(&mac) {
        cgi_error("400 Bad Request", "bad mac");
        return Ok(());
    }
    match parts[1] {
        "post.sh" => {
            // these values are substituted into a root shell script: refuse anything but plain names
            let host = hostname_for(&c, &mac).unwrap_or_else(|| c.target_hostname.clone());
            if !is_hostname(&host) || !is_disk_name(&c.target_disk) {
                cgi_error("500 Internal Server Error", "invalid hostname or target disk in /etc/pxe-installer.conf");
                return Ok(());
            }
            // the address bound to this MAC (pxe-ctl bind) becomes its static IP; none = keep the image's network config
            let ip = ip_for(&c, &mac).unwrap_or_default();
            print!("Content-Type: text/plain\r\n\r\n{}", render::post_script(&c, &host, &mac, &ip));
        }
        "start" => match mark_start(&c, &mac) {
            Ok(()) => print!("Content-Type: text/plain\r\n\r\nstart {mac}\n"),
            Err(e) => cgi_error("500 Internal Server Error", &e),
        },
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
    fn bindings_become_reservations_and_legacy_lines_still_load() {
        let c = tmp_cfg("resv");
        // a hosts file as the dnsmasq versions wrote it, plus a junk line
        fs::write(
            &c.hosts_file,
            "# managed\ndhcp-host=AA:BB:CC:DD:EE:01,192.168.5.11,n1\ndhcp-host=aa:bb:cc:dd:ee:02,192.168.5.12\nnonsense\n",
        )
        .unwrap();
        bind(&c, "aa:bb:cc:dd:ee:03", "192.168.5.13", "n3", false).unwrap();
        let r = read_bindings(&c);
        let got: Vec<(&str, &str, &str)> = r.iter().map(|b| (b.mac.as_str(), b.ip.as_str(), b.hostname.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("aa:bb:cc:dd:ee:01", "192.168.5.11", "n1"),
                ("aa:bb:cc:dd:ee:02", "192.168.5.12", ""),
                ("aa:bb:cc:dd:ee:03", "192.168.5.13", "n3"),
            ]
        );
        // new lines are written without the old dnsmasq prefix
        assert!(!fs::read_to_string(&c.hosts_file).unwrap().contains("dhcp-host="));
    }

    #[test]
    fn kea_leases_journal() {
        let csv = "address,hwaddr,client_id,valid_lifetime,expire,subnet_id,fqdn_fwd,fqdn_rev,hostname,state,user_context\n\
                   192.168.5.100,AA:BB:CC:DD:EE:01,,86400,2000,1,0,0,,0,\n\
                   192.168.5.101,aa:bb:cc:dd:ee:02,,86400,2000,1,0,0,,0,\n\
                   192.168.5.102,aa:bb:cc:dd:ee:03,,86400,500,1,0,0,,0,\n\
                   192.168.5.103,aa:bb:cc:dd:ee:04,,86400,2000,1,0,0,,1,\n\
                   192.168.5.101,aa:bb:cc:dd:ee:02,,0,2000,1,0,0,,0,\n\
                   192.168.5.100,aa:bb:cc:dd:ee:01,,86400,3000,1,0,0,,0,\n";
        // now = 1000: .100 (renewed, later line wins) is active; .101 was released (lifetime 0 in a later line);
        // .102 expired at 500; .103 is declined (state 1)
        assert_eq!(parse_leases(csv, 1000), vec![("aa:bb:cc:dd:ee:01".to_string(), "192.168.5.100".to_string())]);
        assert!(parse_leases("", 1000).is_empty());
        assert!(parse_leases("address,hwaddr\n", 1000).is_empty());
    }

    #[test]
    fn bind_rejects_bad_input() {
        let c = tmp_cfg("bad");
        assert!(bind(&c, "nope", "192.168.5.11", "", false).is_err());
        assert!(bind(&c, "aa:bb:cc:dd:ee:01", "300.1.1.1", "", false).is_err());
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
    fn deploy_time_from_start_to_done() {
        let c = tmp_cfg("times");
        let (a, b) = ("aa:bb:cc:dd:ee:01", "aa:bb:cc:dd:ee:02");
        // a: started 125 s ago, then done -> one history line with ~125 s, start file gone
        mark_start(&c, a).unwrap();
        fs::write(format!("{}/{a}", start_dir(&c)), format!("{}\n2026-01-01T00:00:00+00:00\n", now_epoch() - 125)).unwrap();
        record_deploy(&c, a).unwrap();
        // b: done without any recorded start -> line with empty start/elapsed, not an error
        record_deploy(&c, b).unwrap();
        // a second deploy of a is appended, not overwritten
        mark_start(&c, a).unwrap();
        record_deploy(&c, a).unwrap();

        let h = deploy_history(&c);
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].0, a);
        assert!(matches!(h[0].3, Some(s) if (125..130).contains(&s)));
        assert_eq!(h[0].1, "2026-01-01T00:00:00+00:00");
        assert_eq!((h[1].0.as_str(), h[1].1.as_str(), h[1].3), (b, "", None));
        assert!(h[2].3.is_some());
        assert!(read_start(&c, a).is_none());
        // the start files and the csv are not MAC markers
        assert!(list_done(&c).is_empty());
    }

    #[test]
    fn elapsed_format() {
        assert_eq!(fmt_elapsed(45), "45s");
        assert_eq!(fmt_elapsed(725), "12m 05s");
        assert_eq!(fmt_elapsed(3725), "1h 02m 05s");
    }

    #[test]
    fn mode_switch_rejects_unknown_way() {
        // validated before anything is written (the real switch needs root and the PXE server)
        let mut c = Config::default();
        assert!(c.set("ROOT_MODE", "bogus").is_err());
        assert_eq!(c.root_mode, RootMode::Tmpfs);
        assert!(switch_mode(c.clone(), Some("bogus")).is_err());
        assert!(switch_mode(c, None).is_ok()); // just prints the current way
    }

    #[test]
    fn clone_args() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<String>>();
        assert_eq!(parse_clone_args(&a(&["sda"])).unwrap(), ("sda".to_string(), false, false));
        assert_eq!(parse_clone_args(&a(&["/dev/nvme0n2", "--yes", "--force"])).unwrap(), ("nvme0n2".to_string(), true, true));
        assert_eq!(parse_clone_args(&a(&["--yes", "sdb"])).unwrap().0, "sdb");
        assert!(parse_clone_args(&a(&[])).is_err()); // no disk
        assert!(parse_clone_args(&a(&["--yes"])).is_err());
        assert!(parse_clone_args(&a(&["sda; reboot"])).is_err()); // not a disk name
        assert!(parse_clone_args(&a(&["sda", "sdb"])).is_err()); // two disks
        assert!(parse_clone_args(&a(&["sda", "--bogus"])).is_err());
        assert!(parse_clone_args(&a(&["Sda"])).is_err());
    }

    #[test]
    fn marker_roundtrip() {
        let c = tmp_cfg("mark");
        mark_done(&c, "aa:bb:cc:dd:ee:01").unwrap();
        assert_eq!(list_done(&c).len(), 1);
    }
}
