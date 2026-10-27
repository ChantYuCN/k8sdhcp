//! GTK 4 front-end. Collects settings, validates them with the same code as the CLI, then runs
//! `pkexec pxe-installer install --config <temp file>` and streams its output.
//! Build: cargo build --release --features gui
use gtk4 as gtk;

use gtk::glib;
use gtk::prelude::*;
use pxe_installer::config::Config;
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
const TOTAL_STEPS: f64 = 8.0;

enum Msg {
    Line(String),
    Done(i32),
}

// ------------------------------------------------------------------ process helpers

/// The CLI binary: $PXE_INSTALLER_BIN, next to this executable, or the installed copy.
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
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
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
    // 0600 and create_new: the file holds the target password
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

fn secret(text: &str) -> gtk::Entry {
    gtk::Entry::builder().hexpand(true).text(text).visibility(false).build()
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
    t_user: gtk::Entry,
    t_pw: gtk::Entry,
    t_pw2: gtk::Entry,
    iso: gtk::Entry,
}

impl Form {
    fn collect(&self) -> Result<Config, String> {
        if text_of(&self.t_pw) != text_of(&self.t_pw2) {
            return Err("The two passwords do not match".into());
        }
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
        c.target_user = text_of(&self.t_user).trim().to_string();
        c.target_password = text_of(&self.t_pw);
        c.iso_name = text_of(&self.iso).trim().to_string();

        let errs = c.validate();
        if !errs.is_empty() {
            return Err(format!("Please fix:\n  - {}", errs.join("\n  - ")));
        }
        Ok(c)
    }
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
    let static_ip = gtk::CheckButton::with_label("Configure this static IP on the interface (netplan) - not over SSH on that NIC");
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
    let t_user = entry(&d.target_user);
    let t_pw = secret("");
    let t_pw2 = secret("");
    let iso = entry(&d.iso_name);
    grid_row(&tgt, 0, "Default hostname (no binding)", &t_host);
    grid_row(&tgt, 1, "Target username", &t_user);
    grid_row(&tgt, 2, "Target password", &t_pw);
    grid_row(&tgt, 3, "Repeat password", &t_pw2);
    iso.set_tooltip_text(Some("Name of the ISO you copy into /var/www/html/iso/ yourself"));
    grid_row(&tgt, 4, "Live-server ISO file name", &iso);
    let warn = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .use_markup(true)
        .label("<b>Warning:</b> hosts that PXE boot are <b>wiped</b> and reinstalled unless already installed (or marked done).")
        .build();
    tgt.attach(&warn, 0, 5, 2, 1);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.append(&framed("Network / DHCP", &net));
    body.append(&framed("Target OS (autoinstall)", &tgt));
    let form_sw = gtk::ScrolledWindow::builder().child(&body).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();

    let install_btn = gtk::Button::with_label("Install PXE server");
    install_btn.add_css_class("suggested-action");
    let stop_btn = gtk::Button::with_label("Stop PXE server");
    stop_btn.add_css_class("destructive-action");
    stop_btn.set_tooltip_text(Some("Stops dnsmasq (DHCP/TFTP) and Apache so no host can PXE boot"));
    let progress = gtk::ProgressBar::builder().hexpand(true).valign(gtk::Align::Center).build();
    let step_label = gtk::Label::builder().label("Ready.").xalign(0.0).build();
    let (log, log_sw) = log_view();
    log_sw.set_vexpand(false);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&install_btn);
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
        t_host, t_user, t_pw, t_pw2, iso,
    };

    {
        let (log, step_label) = (log.clone(), step_label.clone());
        stop_btn.connect_clicked(move |_| {
            log.buffer().set_text("");
            step_label.set_text("Stopping PXE server (dnsmasq + Apache)...");
            run_ctl(vec!["stop".to_string()], &log);
        });
    }

    install_btn.connect_clicked(move |btn| {
        let cfg = match form.collect() {
            Ok(c) => c,
            Err(e) => {
                log.buffer().set_text("");
                append(&log, &format!("{e}\n"));
                step_label.set_text("Fix the problems listed below.");
                return;
            }
        };
        let Some(installer) = find_installer() else {
            log.buffer().set_text("");
            append(&log, "pxe-installer binary not found next to this app. Build it or set PXE_INSTALLER_BIN.\n");
            return;
        };
        let conf_path = match write_temp_conf(&cfg) {
            Ok(p) => p,
            Err(e) => {
                append(&log, &format!("cannot write temp config: {e}\n"));
                return;
            }
        };

        log.buffer().set_text("");
        btn.set_sensitive(false);
        progress.set_fraction(0.0);
        step_label.set_text("Waiting for administrator authentication...");

        let mut cmd = Command::new("pkexec");
        cmd.arg(&installer).arg("install").arg("--config").arg(&conf_path);
        let rx = spawn_stream(cmd, Some(conf_path));

        let steps = Rc::new(Cell::new(0u32));
        let (log2, label2, prog2) = (log.clone(), step_label.clone(), progress.clone());
        let (label3, prog3, btn3) = (step_label.clone(), progress.clone(), btn.clone());
        let steps2 = steps.clone();
        pump(
            rx,
            move |line| {
                append(&log2, line);
                if let Some(rest) = line.strip_prefix("==> ") {
                    steps2.set(steps2.get() + 1);
                    label2.set_text(rest.trim());
                }
                prog2.set_fraction((steps2.get() as f64 / TOTAL_STEPS).min(0.99));
            },
            move |code| {
                btn3.set_sensitive(true);
                if code == 0 {
                    prog3.set_fraction(1.0);
                    label3.set_text("PXE server installed. Boot targets in UEFI mode, PXE first.");
                } else {
                    label3.set_text(&format!("Installation failed (exit code {code}). See the log."));
                }
            },
        );
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
    let mut cmd = Command::new("pkexec");
    cmd.arg(&installer).arg("ctl").args(&args);
    let rx = spawn_stream(cmd, None);
    let l1 = log.clone();
    let l2 = log.clone();
    pump(rx, move |line| append(&l1, line), move |_| append(&l2, "\n"));
}

fn manage_tab() -> gtk::Widget {
    let (log, log_sw) = log_view();

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for (label, args) in [
        ("Host status", vec!["status"]),
        ("Installed hosts", vec!["list"]),
        ("MAC bindings", vec!["binds"]),
    ] {
        let b = gtk::Button::with_label(label);
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

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    margins(&outer, 10);
    outer.append(&top);
    outer.append(&framed("Per-host actions", &inner));
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
