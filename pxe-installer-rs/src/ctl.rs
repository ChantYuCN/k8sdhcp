//! Day-2 commands: installed-host markers, MAC->IP bindings, status, and the Apache CGI callback.
use std::fs;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use crate::config::{is_disk_name, is_hostname, is_image_name, is_mac, normalize_mac, Binding, Config, RootMode};
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

/// Prefix of every binding line in the hosts file (dnsmasq loads it as a config file: `dhcp-host=mac,ip[,name]`).
const HOST_PREFIX: &str = "dhcp-host=";

/// dnsmasq's lease file: `<expiry> <mac> <ip> <hostname> <client-id>` per lease.
const LEASES: &str = "/var/lib/misc/dnsmasq.leases";

/// Bindings as "mac,ip[,name]" (prefix stripped; comments/blank lines skipped; unprefixed lines accepted).
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

/// The bindings as structs (a line without a valid "mac,ip" is skipped).
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

/// Take over the bindings of a hosts file somewhere else (an earlier build kept them elsewhere, with or without the
/// `dhcp-host=` prefix) and write them at HOSTS_FILE in the format dnsmasq loads.
pub fn import_bindings_from(c: &Config, old_file: &str) -> Result<(), String> {
    let mut old = c.clone();
    old.hosts_file = old_file.to_string();
    write_hosts(c, &read_hosts(&old))
}

/// Make dnsmasq load changed bindings. dhcp-host lines in a conf-file are only read at startup (SIGHUP does not
/// re-read them), so restart; leases are kept in dnsmasq's lease file and survive. Only when dnsmasq runs, so a
/// binding never starts a stopped PXE server.
fn reload_dnsmasq() {
    if service_active(render::DHCP_SERVICE) {
        run_soft(Ctx { dry: false }, "systemctl", &["restart", render::DHCP_SERVICE]);
    }
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

/// (mac, ip) of every lease in dnsmasq's lease file (`<expiry> <mac> <ip> <hostname> <client-id>`).
fn parse_leases(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.len() >= 4)
        .map(|f| (f[1].to_lowercase(), f[2].to_string()))
        .collect()
}

/// (mac, ip) of every lease of the DHCP server.
fn read_leases() -> Vec<(String, String)> {
    parse_leases(&fs::read_to_string(LEASES).unwrap_or_default())
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

/// `ocs-sr` saves images under this folder (Clonezilla's default `ocsroot`), so IMAGE_DIR has to be it.
const OCS_ROOT: &str = "/home/partimag";
/// Only one capture at a time (the GUI button could be clicked twice). /run is a tmpfs: gone after a reboot.
const CLONE_LOCK: &str = "/run/pxe-installer-clone.lock";

/// Arguments of `ctl clone`: (disk name without /dev/, --yes, --force, --image NAME).
fn parse_clone_args(args: &[String]) -> Result<(String, bool, bool, Option<String>), String> {
    let mut disk: Option<String> = None;
    let mut image: Option<String> = None;
    let (mut yes, mut force) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--yes" => yes = true,
            "--force" => force = true,
            "--image" => {
                let name = it.next().ok_or("--image needs an image name")?;
                if !is_image_name(name) {
                    return Err(format!("'{name}' is not a valid image name (letters, digits, '.', '_' and '-')"));
                }
                image = Some(name.clone());
            }
            d if !d.starts_with('-') && disk.is_none() => disk = Some(d.trim_start_matches("/dev/").to_string()),
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    let disk = disk.ok_or("disk name required, e.g.: pxe-ctl clone nvme0n2 [--image NAME] [--yes] [--force]")?;
    if !is_disk_name(&disk) {
        return Err(format!("'{disk}' is not a disk name (use e.g. sda or nvme0n2, lowercase letters and digits)"));
    }
    Ok((disk, yes, force, image))
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

/// `ctl clone <disk> [--image NAME] [--yes] [--force]` - capture <disk> of THIS host into the Clonezilla image
/// repository as NAME (default IMAGE_NAME; `ocs-sr savedisk`, partclone + parallel zstd) and re-export the repository over NFS, so the
/// targets can restore it. The disk is only read, but it must be idle: not mounted, no swap, not in use.
/// Default = interactive, on the terminal it is run from: `fsck -f` of every filesystem first (the user
/// fixes problems right there), then `ocs-sr` with its own prompts. `--yes` runs without any prompt
/// (read-only fsck that aborts on a problem, `ocs-sr -batch`); `--force` replaces an existing image of the
/// same name (interactive: it is asked, non-interactive without --force it is an error).
fn clone_image(mut c: Config, args: &[String]) -> Result<(), String> {
    let (disk, yes, force, image_name) = parse_clone_args(args)?;
    if let Some(name) = image_name {
        c.image_name = name;
    }
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
                "image {image} already exists: delete it first (sudo rm -rf {image}) or use --force"
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

// ------------------------------------------------------------------ multicast restore
//
// One stream for all targets instead of one copy per target. The targets boot the "Multicast restore" menu entry and
// run Clonezilla's own client script (`ocs-live-get-img <server>`): it downloads `ocs-client-run.conf` and
// `img-wo-fs.tar.xz` (the image folder without the data files) from this server's web root, then runs
// `ocs-sr ... multicast_restoredisk`, which receives each partition with `udp-receiver | partclone`. This server only
// has to publish those two files and send each partition with `udp-sender`, in the order of the image's `parts` file,
// on UDP port 2232, 2234, ... (+2 per partition), exactly as Clonezilla's own `ocs-live-feed-img` does. That script
// is NOT used here: it stops dnsmasq and rewrites the PXE configuration.

/// Clonezilla's multicast defaults (/etc/drbl/drbl-ocs.conf): the all-hosts group, TTL 1 (stays in the LAN), the first
/// UDP port, and the seconds the later partitions wait for the clients (PART_PREPARE_TIME).
const MCAST_ADDR: &str = "224.0.0.1";
const MCAST_PORT: u32 = 2232;
const MCAST_PART_WAIT: u32 = 30;
/// Only one session at a time; it holds the PID of `pxe-ctl multicast` (a stale file of a dead PID is ignored).
const MCAST_LOCK: &str = "/run/pxe-installer-multicast.lock";
const CLIENT_CONF: &str = "ocs-client-run.conf";
const CLIENT_PSEUDO: &str = "img-wo-fs.tar.xz";
/// Restore options the targets run with (the same as the unicast restore, `-batch` comes from `--batch`).
const MCAST_OCS_OPT: &str = "-g auto -e1 auto -e2 -r -j2 -k0 -p true -scr";

/// Arguments of `ctl multicast`.
#[derive(Debug, PartialEq)]
struct McastArgs {
    clients: Option<u32>,
    wait: u32,
    image: Option<String>,
    stop: bool,
}

/// `--clients N` (targets to wait for), `--wait SECS` (how long the first partition waits, default 600),
/// `--image NAME` (default IMAGE_NAME), `--stop` (clean up a session that was interrupted).
fn parse_multicast_args(args: &[String]) -> Result<McastArgs, String> {
    let mut a = McastArgs { clients: None, wait: 600, image: None, stop: false };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--stop" => a.stop = true,
            "--clients" => {
                let n: u32 = it.next().ok_or("--clients needs a number")?.parse().map_err(|_| "--clients must be a number")?;
                if n == 0 {
                    return Err("--clients must be at least 1".into());
                }
                a.clients = Some(n);
            }
            "--wait" => {
                a.wait = it.next().ok_or("--wait needs a number of seconds")?.parse().map_err(|_| "--wait must be a number of seconds")?;
            }
            "--image" => {
                let name = it.next().ok_or("--image needs an image name")?;
                if !is_image_name(name) {
                    return Err(format!("'{name}' is not a valid image name"));
                }
                a.image = Some(name.clone());
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
    }
    Ok(a)
}

/// Image data files (partclone / ntfsclone / dd). The clients get only EMPTY placeholders of them in the metadata
/// tarball; their content travels over multicast.
fn is_image_data_file(name: &str) -> bool {
    name.contains("-ptcl-img.") || name.contains(".ntfs-img.") || name.contains(".dd-img.")
}

/// The partclone image files of every partition, in the order of the image's `parts` file; the files of one
/// partition sorted (a split image is `.aa`, `.ab`, ...). Anything this cannot send is an error.
fn multicast_parts(image_dir: &Path) -> Result<Vec<(String, Vec<std::path::PathBuf>)>, String> {
    if image_dir.join("lvm_logv.list").exists() {
        return Err("this image has LVM volumes: multicast restore of LVM images is not supported here".into());
    }
    let parts_file = image_dir.join("parts");
    let parts = fs::read_to_string(&parts_file).map_err(|e| format!("cannot read {}: {e}", parts_file.display()))?;
    let names: Vec<String> = fs::read_dir(image_dir)
        .map_err(|e| format!("cannot read {}: {e}", image_dir.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    let mut out = Vec::new();
    for part in parts.split_whitespace() {
        if !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            return Err(format!("unexpected partition name '{part}' in {}", parts_file.display()));
        }
        let prefix = format!("{part}.");
        let mut files: Vec<&String> = names.iter().filter(|n| n.starts_with(&prefix) && n.contains("-ptcl-img.")).collect();
        files.sort();
        if files.is_empty() {
            return Err(format!(
                "no partclone image file for partition {part} in {} (only partclone images, *-ptcl-img.*, are supported)",
                image_dir.display()
            ));
        }
        out.push((part.to_string(), files.into_iter().map(|f| image_dir.join(f)).collect()));
    }
    if out.is_empty() {
        return Err(format!("{} lists no partitions", parts_file.display()));
    }
    Ok(out)
}

/// `udp-sender` options for partition number `n` (0 = the first one; each partition is on its own port).
fn udp_sender_args(iface: &str, clients: u32, first_wait: u32, n: u32) -> Vec<String> {
    let max_wait = if n == 0 { first_wait } else { MCAST_PART_WAIT };
    vec![
        "--min-clients".to_string(),
        clients.to_string(),
        "--max-wait".into(),
        max_wait.to_string(),
        "--interface".into(),
        iface.to_string(),
        "--nokbd".into(),
        "--mcast-all-addr".into(),
        MCAST_ADDR.into(),
        "--portbase".into(),
        (MCAST_PORT + 2 * n).to_string(),
        "--ttl".into(),
        "1".into(),
    ]
}

/// What the targets download as `ocs-client-run.conf` (the file is sourced by a root shell on the target: the
/// image and disk names are validated before they get here).
fn client_run_conf(image: &str, disk: &str) -> String {
    format!(
        "pseudo_img=\"{CLIENT_PSEUDO}\"\nimg_prefix=\"{image}\"\n\
         ocs_sr_opt=\"--ocsroot /tmp/ -l en_US.UTF-8 --batch {MCAST_OCS_OPT} --mcast-port {MCAST_PORT} multicast_restoredisk\"\n\
         ocs_restore_dev=\"{disk}\"\n"
    )
}

/// `img-wo-fs.tar.xz`: the image folder with every data file replaced by an empty placeholder (what the targets
/// need to know about the image: partition table, parts list, file system list ...).
fn pack_pseudo_image(image_dir: &Path, out: &Path) -> Result<(), String> {
    let tmp = std::path::PathBuf::from(format!("/var/tmp/pxe-installer-pseudo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| format!("mkdir {}: {e}", tmp.display()))?;
    let result = (|| -> Result<(), String> {
        for e in fs::read_dir(image_dir).map_err(|e| format!("cannot read {}: {e}", image_dir.display()))?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() {
                continue;
            }
            if is_image_data_file(&name) {
                fs::write(tmp.join(&name), b"").map_err(|e| format!("write placeholder {name}: {e}"))?;
            } else {
                fs::copy(e.path(), tmp.join(&name)).map_err(|e| format!("copy {name}: {e}"))?;
            }
        }
        let status = std::process::Command::new("tar")
            .arg("-cJf")
            .arg(out)
            .arg("-C")
            .arg(&tmp)
            .arg(".")
            .status()
            .map_err(|e| format!("cannot run tar: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("tar failed ({status}); is xz-utils installed?"))
        }
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

/// Remove what a session published and put the normal PXE menu back. Used at the end of a session and by `--stop`.
fn end_multicast(c: &Config) {
    for f in [CLIENT_CONF, CLIENT_PSEUDO] {
        let _ = fs::remove_file(Path::new(&c.web_root).join(f));
    }
    let _ = fs::remove_file(MCAST_LOCK);
    if let Err(e) = write_grub(c, Ctx { dry: false }) {
        warn(&format!("could not write the normal PXE menu back: {e} (run: sudo pxe-ctl grub)"));
    }
}

/// Cleans up when the session ends (also on an error).
struct McastSession(Config);
impl Drop for McastSession {
    fn drop(&mut self) {
        end_multicast(&self.0);
    }
}

/// `ctl multicast --clients N [--wait SECS] [--image NAME]` | `--stop` - send IMAGE_NAME to N targets at once.
///
/// It publishes the two client files in the web root, makes the multicast entry the default of the PXE menu (so every
/// target that PXE boots joins by itself), then sends each partition with `udp-sender` as soon as N targets have
/// connected (the first partition waits at most SECS seconds for them). Run it in a terminal and leave it open: it
/// shows the progress, and at the end it removes the files and writes the normal PXE menu back. If it was killed,
/// `--stop` does the clean-up. The targets ERASE their disk TARGET_DISK. Hosts that already have a done marker
/// skip PXE (`pxe-ctl reinstall <mac>` removes it).
fn multicast(mut c: Config, args: &[String]) -> Result<(), String> {
    let a = parse_multicast_args(args)?;
    if a.stop {
        end_multicast(&c);
        println!("multicast session cleaned up: client files removed, normal PXE menu written");
        return Ok(());
    }
    if let Some(name) = a.image {
        c.image_name = name;
    }
    let clients = a.clients.ok_or("--clients N is required: how many targets to wait for, e.g.: pxe-ctl multicast --clients 2")?;
    if !is_image_name(&c.image_name) || !is_disk_name(&c.target_disk) {
        return Err("IMAGE_NAME or TARGET_DISK is not valid".into());
    }
    let image_dir = Path::new(&c.image_dir).join(&c.image_name);
    if !image_dir.is_dir() {
        return Err(format!("image {} not found", image_dir.display()));
    }
    let parts = multicast_parts(&image_dir)?;
    if !probe("sh", &["-c", "command -v udp-sender >/dev/null"]) {
        return Err("udp-sender not found: install it with `sudo apt install udpcast`".into());
    }
    if let Ok(pid) = fs::read_to_string(MCAST_LOCK) {
        let pid = pid.trim();
        if !pid.is_empty() && Path::new(&format!("/proc/{pid}")).exists() {
            return Err(format!("another multicast session is running (pid {pid})"));
        }
    }
    fs::write(MCAST_LOCK, format!("{}\n", std::process::id())).map_err(|e| format!("write {MCAST_LOCK}: {e}"))?;
    let _session = McastSession(c.clone()); // from here on, whatever happens, the clean-up runs at the end

    fs::create_dir_all(&c.web_root).map_err(|e| format!("mkdir {}: {e}", c.web_root))?;
    pack_pseudo_image(&image_dir, &Path::new(&c.web_root).join(CLIENT_PSEUDO))?;
    let conf = Path::new(&c.web_root).join(CLIENT_CONF);
    fs::write(&conf, client_run_conf(&c.image_name, &c.target_disk)).map_err(|e| format!("write {}: {e}", conf.display()))?;
    let ctx = Ctx { dry: false };
    write_file(ctx, &format!("{}/grub/grub.cfg", c.tftp_root), &render::grub_cfg_multicast(&c), 0o644, false)?;
    fix_perms(&c, ctx)?;

    println!(
        "Multicast restore of image {} onto /dev/{} of every target that joins (the disk is ERASED).\n\
         The PXE menu now starts the multicast entry by itself: PXE boot the targets in UEFI mode. A target that was\n\
         already restored has a done marker and skips PXE: sudo pxe-ctl reinstall <mac>.\n\
         Waiting for {clients} target(s) (the first partition waits at most {} s); {} partition(s) to send on {}.\n\
         Leave this window open; Ctrl+C stops the session (then run: sudo pxe-ctl multicast --stop).",
        c.image_name,
        c.target_disk,
        a.wait,
        parts.len(),
        c.iface
    );
    for (n, (part, files)) in parts.iter().enumerate() {
        let n = n as u32;
        println!("\n=== partition {part} ({}/{}) on UDP port {} ===", n + 1, parts.len(), MCAST_PORT + 2 * n);
        let mut cat = std::process::Command::new("cat")
            .args(files.iter())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run cat: {e}"))?;
        let feed = cat.stdout.take().ok_or("no pipe from cat")?;
        let sent = std::process::Command::new("udp-sender")
            .args(udp_sender_args(&c.iface, clients, a.wait, n))
            .stdin(std::process::Stdio::from(feed))
            .status()
            .map_err(|e| format!("cannot run udp-sender: {e}"));
        let _ = cat.wait();
        let sent = sent?;
        if !sent.success() {
            return Err(format!("udp-sender failed for {part} ({sent}); the session is cancelled"));
        }
    }
    println!(
        "\nAll {} partition(s) were sent. The targets now finish by themselves (bootloader, resize, hostname/IP,\n\
         done marker) and reboot. Check with: sudo pxe-ctl status | times",
        parts.len()
    );
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
            let services = [render::DHCP_SERVICE, "apache2", "nfs-server"];
            run(Ctx { dry: false }, "systemctl", &["stop", services[0], services[1], services[2]])?;
            for svc in services {
                println!("{svc}: {}", if service_active(svc) { "STILL ACTIVE" } else { "stopped" });
            }
            println!(
                "PXE server stopped. Start it again with the Start button or: sudo systemctl start {} apache2 nfs-server",
                render::DHCP_SERVICE
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
        Some("clone") => clone_image(c, &args[1..]),
        Some("multicast") => multicast(c, &args[1..]),
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
        _ => Err("usage: pxe-installer ctl list | status | times | stop | grub | import | nfs | mode [tmpfs|nfs] | clone <disk> [--image NAME] [--yes] [--force] | multicast --clients N [--wait SECS] [--image NAME] | multicast --stop | reinstall <mac> | done <mac> | binds | bind <mac> <ip> [name] | unbind <mac|ip>".into()),
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
    fn bindings_load_with_or_without_prefix_and_are_written_for_dnsmasq() {
        let c = tmp_cfg("resv");
        // a hosts file with prefixed and unprefixed lines (an earlier build wrote them without), plus a junk line
        fs::write(
            &c.hosts_file,
            "# managed\ndhcp-host=AA:BB:CC:DD:EE:01,192.168.5.11,n1\naa:bb:cc:dd:ee:02,192.168.5.12\nnonsense\n",
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
        // every line is written with the dhcp-host= prefix, because dnsmasq loads the file as a config file
        let written = fs::read_to_string(&c.hosts_file).unwrap();
        let hosts: Vec<&str> = written.lines().filter(|l| !l.starts_with('#')).collect();
        assert!(hosts.iter().all(|l| l.starts_with("dhcp-host=")), "{written}");
        assert!(hosts.contains(&"dhcp-host=aa:bb:cc:dd:ee:03,192.168.5.13,n3"));
    }

    #[test]
    fn bindings_of_an_earlier_build_are_imported() {
        let c = tmp_cfg("import");
        let old = format!("{}.old", c.hosts_file);
        fs::write(&old, "# managed\n7c:c2:55:ef:05:9e,192.168.77.88,d3xtester18\n").unwrap();
        import_bindings_from(&c, &old).unwrap();
        let r = read_bindings(&c);
        assert_eq!((r.len(), r[0].ip.as_str(), r[0].hostname.as_str()), (1, "192.168.77.88", "d3xtester18"));
        assert!(fs::read_to_string(&c.hosts_file).unwrap().contains("dhcp-host=7c:c2:55:ef:05:9e,192.168.77.88,d3xtester18"));
    }

    #[test]
    fn dnsmasq_leases_parse() {
        let text = "1763000000 AA:BB:CC:DD:EE:01 192.168.5.100 node1 01:aa:bb:cc:dd:ee:01\n\
                    1763000001 aa:bb:cc:dd:ee:02 192.168.5.101 * *\n\
                    short line\n";
        assert_eq!(
            parse_leases(text),
            vec![
                ("aa:bb:cc:dd:ee:01".to_string(), "192.168.5.100".to_string()),
                ("aa:bb:cc:dd:ee:02".to_string(), "192.168.5.101".to_string()),
            ]
        );
        assert!(parse_leases("").is_empty());
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn multicast_args() {
        assert_eq!(
            parse_multicast_args(&strs(&["--clients", "100"])).unwrap(),
            McastArgs { clients: Some(100), wait: 600, image: None, stop: false }
        );
        assert_eq!(
            parse_multicast_args(&strs(&["--clients", "2", "--wait", "90", "--image", "golden-2"])).unwrap(),
            McastArgs { clients: Some(2), wait: 90, image: Some("golden-2".to_string()), stop: false }
        );
        assert!(parse_multicast_args(&strs(&["--stop"])).unwrap().stop);
        for bad in [
            vec!["--clients"],
            vec!["--clients", "0"],
            vec!["--clients", "many"],
            vec!["--wait", "-1"],
            vec!["--image", "../x"],
            vec!["--bogus"],
        ] {
            assert!(parse_multicast_args(&strs(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn multicast_sender_options_follow_clonezilla() {
        // first partition: waits for the clients, base port; later ones: short wait, port + 2 each
        let first = udp_sender_args("enp0s31f6", 100, 600, 0).join(" ");
        assert_eq!(
            first,
            "--min-clients 100 --max-wait 600 --interface enp0s31f6 --nokbd --mcast-all-addr 224.0.0.1 --portbase 2232 --ttl 1"
        );
        let third = udp_sender_args("enp0s31f6", 100, 600, 2).join(" ");
        assert!(third.contains("--max-wait 30 ") && third.contains("--portbase 2236 "), "{third}");
    }

    #[test]
    fn multicast_client_conf() {
        let c = client_run_conf("cri-golden", "nvme0n1");
        assert_eq!(
            c,
            "pseudo_img=\"img-wo-fs.tar.xz\"\nimg_prefix=\"cri-golden\"\n\
             ocs_sr_opt=\"--ocsroot /tmp/ -l en_US.UTF-8 --batch -g auto -e1 auto -e2 -r -j2 -k0 -p true -scr --mcast-port 2232 multicast_restoredisk\"\n\
             ocs_restore_dev=\"nvme0n1\"\n"
        );
    }

    #[test]
    fn multicast_parts_in_image_order() {
        let dir = std::env::temp_dir().join(format!("pxe-mc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("parts"), "sda1 sda2 sda10\n").unwrap();
        for f in [
            "sda1.ext4-ptcl-img.zst",
            "sda2.vfat-ptcl-img.zst.ab", // split image: the parts are sent in order
            "sda2.vfat-ptcl-img.zst.aa",
            "sda10.ntfs-ptcl-img.zst", // sda1 must not match "sda10"
            "sda1-pt.sf",
            "disk",
        ] {
            fs::write(dir.join(f), "x").unwrap();
        }
        let p = multicast_parts(&dir).unwrap();
        let names: Vec<(String, Vec<String>)> = p
            .iter()
            .map(|(n, f)| (n.clone(), f.iter().map(|x| x.file_name().unwrap().to_string_lossy().to_string()).collect()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("sda1".to_string(), strs(&["sda1.ext4-ptcl-img.zst"])),
                ("sda2".to_string(), strs(&["sda2.vfat-ptcl-img.zst.aa", "sda2.vfat-ptcl-img.zst.ab"])),
                ("sda10".to_string(), strs(&["sda10.ntfs-ptcl-img.zst"])),
            ]
        );
        // what is sent over multicast is a data file; the small files travel in the metadata tarball
        assert!(is_image_data_file("sda1.ext4-ptcl-img.zst") && is_image_data_file("sda2.vfat-ptcl-img.zst.aa"));
        assert!(!is_image_data_file("sda1-pt.sf") && !is_image_data_file("parts") && !is_image_data_file("disk"));

        // a partition without a partclone file, and an LVM image, are refused (never sent half)
        fs::write(dir.join("parts"), "sda1 sda3\n").unwrap();
        assert!(multicast_parts(&dir).unwrap_err().contains("sda3"));
        fs::write(dir.join("parts"), "sda1\n").unwrap();
        fs::write(dir.join("lvm_logv.list"), "x").unwrap();
        assert!(multicast_parts(&dir).unwrap_err().contains("LVM"));
        let _ = fs::remove_dir_all(&dir);
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
        assert_eq!(parse_clone_args(&a(&["sda"])).unwrap(), ("sda".to_string(), false, false, None));
        assert_eq!(
            parse_clone_args(&a(&["/dev/nvme0n2", "--yes", "--force"])).unwrap(),
            ("nvme0n2".to_string(), true, true, None)
        );
        // the image name comes from the GUI's "Image name" field
        assert_eq!(
            parse_clone_args(&a(&["sda", "--image", "golden-2", "--force"])).unwrap(),
            ("sda".to_string(), false, true, Some("golden-2".to_string()))
        );
        assert!(parse_clone_args(&a(&["sda", "--image"])).is_err()); // name missing
        assert!(parse_clone_args(&a(&["sda", "--image", "../x"])).is_err()); // not a plain folder name
        assert!(parse_clone_args(&a(&["sda", "--image", "a b"])).is_err());
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
