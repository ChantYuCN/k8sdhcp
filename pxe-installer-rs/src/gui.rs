//! GTK 4 front-end. Collects settings, validates them with the same code as the CLI, then runs
//! `pxe-installer install --config <temp file>` as root (directly, `sudo -n`, or pkexec) and streams its output.
//! Build: cargo build --release --features gui
use gtk4 as gtk;

use gtk::glib;
use gtk::prelude::*;
use pxe_installer::config::{Config, RootMode};
use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Number of "==> " step markers a full install prints.

enum Msg {
    Line(String),
    Done(i32),
}

// ------------------------------------------------------------------ process helpers

/// The CLI binary: $PXE_INSTALLER_BIN, next to this executable, or the installed copy.
/// Command that runs `installer` as root, asking for a password only when it has to:
/// already root -> run directly; passwordless sudo -> `sudo -n`; otherwise the pkexec prompt.
fn privileged(installer: &std::path::Path) -> Command {
    static MODE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    let quiet = |prog: &str, args: &[&str]| {
        Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let mode = *MODE.get_or_init(|| {
        let is_root = Command::new("id").arg("-u").output().map(|o| o.stdout.starts_with(b"0")).unwrap_or(false);
        if is_root {
            "direct"
        } else if quiet("sudo", &["-n", "true"]) {
            "sudo"
        } else {
            "pkexec"
        }
    });
    match mode {
        "direct" => Command::new(installer),
        "sudo" => {
            let mut c = Command::new("sudo");
            c.arg("-n").arg(installer);
            c
        }
        _ => {
            let mut c = Command::new("pkexec");
            c.arg(installer);
            c
        }
    }
}

fn find_installer() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("PXE_INSTALLER_BIN") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            candidates.push(dir.join("pxe-installer"));
        }
    }
    candidates.push(PathBuf::from("/usr/local/sbin/pxe-installer"));
    candidates.into_iter().find(|p| p.is_file())
}

fn forward<R: std::io::Read + Send + 'static>(r: R, tx: Sender<Msg>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(r).lines().map_while(Result::ok) {
            let _ = tx.send(Msg::Line(format!("{line}\n")));
        }
    })
}

/// Run `cmd` on a worker thread; stdout+stderr lines and the exit code arrive on the receiver.
/// `cleanup` (e.g. a temp file holding a password) is deleted when the process ends.
fn spawn_stream(mut cmd: Command, cleanup: Option<PathBuf>) -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        // stdin = /dev/null: a stray prompt must get EOF, never wait on the GUI's (invisible) terminal
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let code = match cmd.spawn() {
            Ok(mut child) => {
                let mut readers = Vec::new();
                if let Some(o) = child.stdout.take() {
                    readers.push(forward(o, tx.clone()));
                }
                if let Some(e) = child.stderr.take() {
                    readers.push(forward(e, tx.clone()));
                }
                let status = child.wait();
                for r in readers {
                    let _ = r.join();
                }
                status.ok().and_then(|s| s.code()).unwrap_or(-1)
            }
            Err(e) => {
                let _ = tx.send(Msg::Line(format!("cannot start process: {e}\n")));
                -1
            }
        };
        if let Some(p) = cleanup {
            let _ = std::fs::remove_file(p);
        }
        let _ = tx.send(Msg::Done(code));
    });
    rx
}

/// Deliver worker messages on the GTK main thread.
fn pump(rx: Receiver<Msg>, on_line: impl Fn(&str) + 'static, on_done: impl Fn(i32) + 'static) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Line(l) => on_line(&l),
                Msg::Done(c) => {
                    on_done(c);
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

fn write_temp_conf(c: &Config) -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = std::env::temp_dir().join(format!("pxe-gui-{}-{nanos}.conf", std::process::id()));
    // 0600 and create_new: keeps the temp config private and refuses to follow a pre-created file
    let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    f.write_all(c.to_conf().as_bytes())?;
    Ok(path)
}

// ------------------------------------------------------------------------ widgets

fn append(view: &gtk::TextView, text: &str) {
    let buf = view.buffer();
    let mut end = buf.end_iter();
    buf.insert(&mut end, text);
    let mark = buf.create_mark(None, &buf.end_iter(), false);
    view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
    buf.delete_mark(&mark);
}

fn log_view() -> (gtk::TextView, gtk::ScrolledWindow) {
    let tv = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    let sw = gtk::ScrolledWindow::builder().child(&tv).vexpand(true).min_content_height(150).build();
    (tv, sw)
}

fn entry(text: &str) -> gtk::Entry {
    gtk::Entry::builder().hexpand(true).text(text).build()
}

fn margins(w: &impl IsA<gtk::Widget>, n: i32) {
    w.set_margin_top(n);
    w.set_margin_bottom(n);
    w.set_margin_start(n);
    w.set_margin_end(n);
}

fn framed(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Frame {
    margins(child, 8);
    let f = gtk::Frame::new(Some(title));
    f.set_child(Some(child));
    f
}

fn grid_row(grid: &gtk::Grid, row: i32, label: &str, w: &impl IsA<gtk::Widget>) {
    let l = gtk::Label::builder().label(label).xalign(0.0).build();
    grid.attach(&l, 0, row, 1, 1);
    grid.attach(w, 1, row, 1, 1);
}

fn text_of(e: &gtk::Entry) -> String {
    e.text().to_string()
}

fn interfaces() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/sys/class/net")
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    v.retain(|n| n != "lo");
    v.sort();
    v
}

// -------------------------------------------------------------------- install tab

#[derive(Clone)]
struct Form {
    iface: gtk::Entry,
    server_ip: gtk::Entry,
    prefix: gtk::SpinButton,
    dhcp_start: gtk::Entry,
    dhcp_end: gtk::Entry,
    lease: gtk::Entry,
    dns: gtk::Entry,
    static_ip: gtk::CheckButton,
    only_known: gtk::CheckButton,
    t_host: gtk::Entry,
    image: gtk::Entry,
    disk: gtk::Entry,
    /// deploy way: this radio button selects NFS, its partner selects tmpfs
    root_nfs: gtk::CheckButton,
}

impl Form {
    fn collect(&self) -> Result<Config, String> {
        let mut c = Config::default();
        c.iface = text_of(&self.iface).trim().to_string();
        c.server_ip = text_of(&self.server_ip).trim().to_string();
        c.prefix = self.prefix.value() as u8;
        c.dhcp_start = text_of(&self.dhcp_start).trim().to_string();
        c.dhcp_end = text_of(&self.dhcp_end).trim().to_string();
        c.lease = text_of(&self.lease).trim().to_string();
        c.upstream_dns = text_of(&self.dns).trim().to_string();
        c.set_static_ip = self.static_ip.is_active();
        c.only_known_hosts = self.only_known.is_active();
        c.target_hostname = text_of(&self.t_host).trim().to_string();
        c.image_name = text_of(&self.image).trim().to_string();
        c.target_disk = text_of(&self.disk).trim().trim_start_matches("/dev/").to_string();
        c.root_mode = if self.root_nfs.is_active() { RootMode::Nfs } else { RootMode::Tmpfs };

        let errs = c.validate();
        if !errs.is_empty() {
            return Err(format!("Please fix:\n  - {}", errs.join("\n  - ")));
        }
        Ok(c)
    }
}

/// Widgets updated while an installer subcommand runs.
struct RunUi {
    log: gtk::TextView,
    step_label: gtk::Label,
    progress: gtk::ProgressBar,
    /// disabled while a run is in progress
    buttons: Vec<gtk::Button>,
}

/// Runs `pxe-installer <sub> --config <form>` as root (see `privileged`), streaming output into the log and progress bar.
fn run_installer(sub: &str, total_steps: f64, done_msg: &'static str, cfg: Result<Config, String>, ui: &Rc<RunUi>) {
    let cfg = match cfg {
        Ok(c) => c,
        Err(e) => {
            ui.log.buffer().set_text("");
            append(&ui.log, &format!("{e}\n"));
            ui.step_label.set_text("Fix the problems listed below.");
            return;
        }
    };
    let Some(installer) = find_installer() else {
        ui.log.buffer().set_text("");
        append(&ui.log, "pxe-installer binary not found next to this app. Build it or set PXE_INSTALLER_BIN.\n");
        return;
    };
    let conf_path = match write_temp_conf(&cfg) {
        Ok(p) => p,
        Err(e) => {
            append(&ui.log, &format!("cannot write temp config: {e}\n"));
            return;
        }
    };

    ui.log.buffer().set_text("");
    for b in &ui.buttons {
        b.set_sensitive(false);
    }
    ui.progress.set_fraction(0.0);
    ui.step_label.set_text("Waiting for administrator authentication...");

    let mut cmd = privileged(&installer);
    cmd.arg(sub).arg("--config").arg(&conf_path);
    let rx = spawn_stream(cmd, Some(conf_path));

    let steps = Rc::new(Cell::new(0u32));
    let (ui1, ui2) = (ui.clone(), ui.clone());
    let sub = sub.to_string();
    pump(
        rx,
        move |line| {
            append(&ui1.log, line);
            if let Some(rest) = line.strip_prefix("==> ") {
                steps.set(steps.get() + 1);
                ui1.step_label.set_text(rest.trim());
            }
            ui1.progress.set_fraction((steps.get() as f64 / total_steps).min(0.99));
        },
        move |code| {
            for b in &ui2.buttons {
                b.set_sensitive(true);
            }
            if code == 0 {
                ui2.progress.set_fraction(1.0);
                ui2.step_label.set_text(done_msg);
            } else {
                ui2.step_label.set_text(&format!("{sub} failed (exit code {code}). See the log."));
            }
        },
    );
}

fn install_tab() -> gtk::Widget {
    let d = Config::default();
    let ifaces = interfaces();
    let first_if = ifaces.first().cloned().unwrap_or_else(|| d.iface.clone());

    let net = gtk::Grid::builder().column_spacing(8).row_spacing(6).build();
    let iface = entry(&first_if);
    iface.set_tooltip_text(Some(&format!("Detected: {}", ifaces.join(", "))));
    let server_ip = entry(&d.server_ip);
    let prefix = gtk::SpinButton::with_range(8.0, 30.0, 1.0);
    prefix.set_value(d.prefix as f64);
    let dhcp_start = entry(&d.dhcp_start);
    let dhcp_end = entry(&d.dhcp_end);
    let lease = entry(&d.lease);
    let dns = entry(&d.upstream_dns);
    let static_ip = gtk::CheckButton::with_label("Configure this static IP on the pxe installer interface (netplan) - DO NOT click this checkbox if your connection over SSH on that NIC");
    let only_known = gtk::CheckButton::with_label("Only serve hosts bound on the Manage tab (ignore unknown hosts)");
    grid_row(&net, 0, "Interface (facing targets)", &iface);
    grid_row(&net, 1, "Server IP", &server_ip);
    grid_row(&net, 2, "Prefix length", &prefix);
    grid_row(&net, 3, "DHCP range start", &dhcp_start);
    grid_row(&net, 4, "DHCP range end", &dhcp_end);
    grid_row(&net, 5, "Lease time", &lease);
    grid_row(&net, 6, "Upstream DNS", &dns);
    net.attach(&static_ip, 0, 7, 2, 1);
    net.attach(&only_known, 0, 8, 2, 1);

    let tgt = gtk::Grid::builder().column_spacing(8).row_spacing(6).build();
    let t_host = entry(&d.target_hostname);
    let image = entry(&d.image_name);
    let disk = entry(&d.target_disk);
    image.set_tooltip_text(Some("Folder name of the Clonezilla image inside /home/partimag (made with ocs-sr savedisk)"));
    disk.set_tooltip_text(Some("Disk of the target that is erased and restored, e.g. nvme0n1 or sda"));
    grid_row(&tgt, 0, "Default hostname (no binding)", &t_host);
    grid_row(&tgt, 1, "Clonezilla image name", &image);
    grid_row(&tgt, 2, "Target disk (/dev/...)", &disk);
    // PXE deploy way switch: two radio buttons (a group); tmpfs is the default
    let root_tmpfs = gtk::CheckButton::with_label(
        "Deploy way 1: tmpfs - Clonezilla Live is downloaded over HTTP into the target's RAM (needs about 4 GB RAM)",
    );
    let root_nfs = gtk::CheckButton::with_label(
        "Deploy way 2: NFS - the target mounts Clonezilla Live from the server over NFS (diskless root, no RAM copy)",
    );
    root_nfs.set_group(Some(&root_tmpfs));
    root_tmpfs.set_active(d.root_mode == RootMode::Tmpfs);
    root_nfs.set_active(d.root_mode == RootMode::Nfs);
    root_nfs.set_tooltip_text(Some("Needs the whole Clonezilla Live zip extracted into /srv/nfs/clonezilla (it must contain live/filesystem.squashfs)"));
    tgt.attach(&root_tmpfs, 0, 3, 2, 1);
    tgt.attach(&root_nfs, 0, 4, 2, 1);
    let warn = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .use_markup(true)
        .label("<b>Warning:</b> hosts that PXE boot have the target disk <b>wiped</b> and restored from the image unless already installed (or marked done).")
        .build();
    tgt.attach(&warn, 0, 5, 2, 1);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.append(&framed("Network / DHCP", &net));
    body.append(&framed("Target OS (Clonezilla image restore)", &tgt));
    let form_sw = gtk::ScrolledWindow::builder().child(&body).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

    let install_btn = gtk::Button::with_label("Install PXE server");
    install_btn.set_tooltip_text(Some("Installs dnsmasq, Apache, NFS and the Clonezilla tools and creates the directories; services are left stopped. Nothing is downloaded: the log then lists the netboot and Clonezilla Live files you copy by hand, and where"));
    let start_btn = gtk::Button::with_label("Start PXE server");
    start_btn.add_css_class("suggested-action");
    start_btn.set_tooltip_text(Some("Writes the network/DHCP/PXE menu/NFS configuration and starts dnsmasq + Apache + NFS"));
    let stop_btn = gtk::Button::with_label("Stop PXE server");
    stop_btn.add_css_class("destructive-action");
    stop_btn.set_tooltip_text(Some("Stops dnsmasq (DHCP/TFTP) and Apache so no host can PXE boot"));
    let progress = gtk::ProgressBar::builder().hexpand(true).valign(gtk::Align::Center).build();
    let step_label = gtk::Label::builder().label("Ready.").xalign(0.0).build();
    let (log, log_sw) = log_view();
    log_sw.set_vexpand(false);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&install_btn);
    row.append(&start_btn);
    row.append(&stop_btn);
    row.append(&progress);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    margins(&outer, 10);
    outer.append(&form_sw);
    outer.append(&row);
    outer.append(&step_label);
    outer.append(&log_sw);

    let form = Form {
        iface, server_ip, prefix, dhcp_start, dhcp_end, lease, dns, static_ip, only_known,
        t_host, image, disk, root_nfs,
    };

    let ui = Rc::new(RunUi { log, step_label, progress, buttons: vec![install_btn.clone(), start_btn.clone()] });

    {
        let ui = ui.clone();
        stop_btn.connect_clicked(move |_| {
            ui.log.buffer().set_text("");
            ui.step_label.set_text("Stopping PXE server (dnsmasq + Apache)...");
            run_ctl(vec!["stop".to_string()], &ui.log);
        });
    }

    {
        // Install needs none of the form values (and so none of its validation)
        let ui = ui.clone();
        install_btn.connect_clicked(move |_| {
            run_installer(
                "install",
                7.0,
                "Packages installed, netboot files and Clonezilla Live unpacked from the netboot and livesystem folders next to this program; services stopped. Anything the log lists as missing must be copied by hand. Then click \"Start PXE server\".",
                Ok(Config::default()),
                &ui,
            )
        });
    }
    start_btn.connect_clicked(move |_| {
        run_installer("start", 9.0, "PXE server running. Boot targets in UEFI mode, PXE first.", form.collect(), &ui)
    });

    outer.upcast()
}

// --------------------------------------------------------------------- manage tab

fn run_ctl(args: Vec<String>, log: &gtk::TextView) {
    let Some(installer) = find_installer() else {
        append(log, "pxe-installer binary not found.\n");
        return;
    };
    append(log, &format!("$ pxe-ctl {}\n", args.join(" ")));
    let mut cmd = privileged(&installer);
    cmd.arg("ctl").args(&args);
    let rx = spawn_stream(cmd, None);
    let l1 = log.clone();
    let l2 = log.clone();
    pump(rx, move |line| append(&l1, line), move |_| append(&l2, "\n"));
}

/// Terminal emulators and the flag that makes them run the rest of the command line (first found wins).
const TERMINALS: [(&str, &str); 7] = [
    ("x-terminal-emulator", "-e"),
    ("gnome-terminal", "--"),
    ("konsole", "-e"),
    ("xfce4-terminal", "-x"),
    ("mate-terminal", "-x"),
    ("alacritty", "-e"),
    ("xterm", "-e"),
];

fn have_command(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", "command -v \"$1\" >/dev/null", "sh", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `pxe-installer ctl <args>` in a terminal window the user can see and type into (fsck / ocs-sr prompts).
/// The window asks for the sudo password itself when needed and stays open until Enter is pressed, so the
/// output can still be read when the command has finished or failed.
fn run_ctl_terminal(args: Vec<String>, log: &gtk::TextView) {
    let Some(installer) = find_installer() else {
        append(log, "pxe-installer binary not found.\n");
        return;
    };
    let Some((term, flag)) = TERMINALS.iter().find(|(t, _)| have_command(t)) else {
        append(
            log,
            &format!(
                "No terminal emulator found. Open a terminal yourself and run:\n  sudo pxe-ctl {}\n",
                args.join(" ")
            ),
        );
        return;
    };
    // "$@" = installer + ctl + args, passed as separate words: nothing is re-parsed by a shell
    let script = "if [ \"$(id -u)\" = 0 ]; then \"$@\"; else sudo \"$@\"; fi; rc=$?; \
                  echo; echo \"Finished (exit code $rc). Press Enter to close this window.\"; read _; exit $rc";
    append(log, &format!("$ pxe-ctl {}   (in a terminal window: {term})\n", args.join(" ")));
    match Command::new(term)
        .arg(flag)
        .args(["sh", "-c", script, "sh"])
        .arg(&installer)
        .arg("ctl")
        .args(&args)
        .spawn()
    {
        Ok(mut child) => {
            // reap it when the window closes (no zombie); the result is in the terminal itself
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => append(log, &format!("cannot start {term}: {e}\n")),
    }
}

fn manage_tab() -> gtk::Widget {
    let (log, log_sw) = log_view();

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for (label, args) in [
        ("Host status", vec!["status"]),
        ("Installed hosts", vec!["list"]),
        ("Deploy times", vec!["times"]),
        ("MAC bindings", vec!["binds"]),
        ("Re-export images (NFS)", vec!["nfs"]),
        ("Deploy way: show", vec!["mode"]),
        ("Switch to 1: tmpfs", vec!["mode", "tmpfs"]),
        ("Switch to 2: NFS root", vec!["mode", "nfs"]),
    ] {
        let b = gtk::Button::with_label(label);
        if args[0] == "nfs" {
            b.set_tooltip_text(Some("After adding or changing a Clonezilla image: re-exports the image folder over NFS and checks the image exists"));
        }
        if args[0] == "mode" {
            b.set_tooltip_text(Some("PXE deploy way: tmpfs = Clonezilla Live downloaded into the target's RAM, nfs = mounted from the server. Applies at once to the next PXE boot."));
        }
        let log = log.clone();
        b.connect_clicked(move |_| run_ctl(args.iter().map(|s| s.to_string()).collect(), &log));
        top.append(&b);
    }

    let mac = entry("");
    mac.set_placeholder_text(Some("MAC  aa:bb:cc:dd:ee:ff"));
    let ip = entry("");
    ip.set_placeholder_text(Some("IP (for Bind)"));
    let name = entry("");
    name.set_placeholder_text(Some("hostname (optional)"));
    let fields = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    fields.append(&mac);
    fields.append(&ip);
    fields.append(&name);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for (label, key) in [
        ("Allow reinstall", "reinstall"),
        ("Mark as installed", "done"),
        ("Bind MAC to IP", "bind"),
        ("Unbind", "unbind"),
    ] {
        let b = gtk::Button::with_label(label);
        let (log, mac, ip, name) = (log.clone(), mac.clone(), ip.clone(), name.clone());
        b.connect_clicked(move |_| {
            let m = text_of(&mac).trim().to_string();
            if m.is_empty() {
                append(&log, "Enter a MAC address first.\n");
                return;
            }
            let mut args = vec![key.to_string(), m];
            if key == "bind" {
                let i = text_of(&ip).trim().to_string();
                if i.is_empty() {
                    append(&log, "Enter an IP address to bind.\n");
                    return;
                }
                args.push(i);
                let n = text_of(&name).trim().to_string();
                if !n.is_empty() {
                    args.push(n);
                }
            }
            run_ctl(args, &log);
        });
        actions.append(&b);
    }

    let inner = gtk::Box::new(gtk::Orientation::Vertical, 6);
    inner.append(&fields);
    inner.append(&actions);

    // Clone Image: capture a disk of THIS server into the Clonezilla image repository (ocs-sr savedisk),
    // then re-export the repository over NFS so the targets can restore it.
    let clone_disk = entry("");
    clone_disk.set_placeholder_text(Some("source disk of this server, e.g. sda or nvme0n2"));
    let clone_force = gtk::CheckButton::with_label("Overwrite the existing image");
    clone_force.set_tooltip_text(Some("Without this an image that already exists is never replaced"));
    let clone_btn = gtk::Button::with_label("Clone Image");
    clone_btn.add_css_class("suggested-action");
    clone_btn.set_tooltip_text(Some(
        "Opens a terminal window: first `fsck -f` of every partition of the disk (answer its questions or fix a problem right there), \
         then `ocs-sr savedisk` with its own prompts, then the image folder is re-exported over NFS. \
         The disk must be idle (not mounted, no swap). It is only read. This takes a long time for a big disk.",
    ));
    {
        let (log, clone_disk, clone_force) = (log.clone(), clone_disk.clone(), clone_force.clone());
        clone_btn.connect_clicked(move |_| {
            let d = text_of(&clone_disk).trim().trim_start_matches("/dev/").to_string();
            if d.is_empty() {
                append(&log, "Enter the source disk name first (e.g. sda).\n");
                return;
            }
            // no --yes: the CLI runs interactively in the terminal window (fsck prompts, ocs-sr prompts)
            let mut args = vec!["clone".to_string(), d];
            if clone_force.is_active() {
                args.push("--force".to_string());
            }
            run_ctl_terminal(args, &log);
        });
    }
    let clone_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    clone_row.append(&clone_disk);
    clone_row.append(&clone_force);
    clone_row.append(&clone_btn);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    margins(&outer, 10);
    outer.append(&top);
    outer.append(&framed("Per-host actions", &inner));
    outer.append(&framed("Clone image (capture a disk of this server for the targets)", &clone_row));
    outer.append(&log_sw);
    outer.upcast()
}

// ------------------------------------------------------------------------- main

fn build_ui(app: &gtk::Application) {
    let nb = gtk::Notebook::new();
    nb.append_page(&install_tab(), Some(&gtk::Label::new(Some("Install"))));
    nb.append_page(&manage_tab(), Some(&gtk::Label::new(Some("Manage"))));

    let win = gtk::ApplicationWindow::builder()
        .application(app)
        .title("PXE Server Installer")
        .default_width(820)
        .default_height(780)
        .child(&nb)
        .build();
    win.present();
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id("org.example.PxeInstaller").build();
    app.connect_activate(build_ui);
    app.run()
}
