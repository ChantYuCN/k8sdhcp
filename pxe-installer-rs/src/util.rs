//! Small helpers: logging, running commands, writing files (all honour --dry-run).
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub struct Ctx {
    pub dry: bool,
}

/// Step marker. The GUI parses lines starting with "==> " for its progress bar.
pub fn step(msg: &str) {
    println!("\n==> {msg}");
}

pub fn warn(msg: &str) {
    eprintln!("WARNING: {msg}");
}

fn describe(prog: &str, args: &[&str]) -> String {
    let mut s = prog.to_string();
    for a in args {
        s.push(' ');
        s.push_str(a);
    }
    s
}

/// Run a command, inheriting stdout/stderr. Error if it cannot start or exits non-zero.
pub fn run(ctx: Ctx, prog: &str, args: &[&str]) -> Result<(), String> {
    if ctx.dry {
        println!("[dry-run] $ {}", describe(prog, args));
        return Ok(());
    }
    let status = unattended(Command::new(prog).args(args))
        .status()
        .map_err(|e| format!("cannot run '{}': {e}", describe(prog, args)))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("'{}' failed ({status})", describe(prog, args)))
    }
}

/// Make a command unable to wait for a human: stdin is /dev/null (a prompt gets EOF instead of blocking)
/// and the package tools are told to never ask (debconf, conffile, needrestart, apt-listchanges).
fn unattended(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null())
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("DEBIAN_PRIORITY", "critical")
        .env("NEEDRESTART_MODE", "a")
        .env("NEEDRESTART_SUSPEND", "1")
        .env("APT_LISTCHANGES_FRONTEND", "none")
        .env("UCF_FORCE_CONFFOLD", "1")
}

/// Run a command on the user's own terminal: stdin/stdout/stderr are inherited, so the user sees everything
/// it prints and can answer its prompts. No timeout (a person is in the loop). Returns the exit code
/// (-1 when it was killed by a signal).
pub fn run_tty(prog: &str, args: &[&str]) -> Result<i32, String> {
    Command::new(prog)
        .args(args)
        .status()
        .map(|s| s.code().unwrap_or(-1))
        .map_err(|e| format!("cannot run '{}': {e}", describe(prog, args)))
}

/// Like `run`, but the command is killed (and an error returned) if it is still running after `timeout`,
/// so a wedged apt/dpkg can never hang the installer forever.
pub fn run_timeout(ctx: Ctx, prog: &str, args: &[&str], timeout: Duration) -> Result<(), String> {
    if ctx.dry {
        println!("[dry-run] $ {}", describe(prog, args));
        return Ok(());
    }
    let mut child = unattended(Command::new(prog).args(args))
        .spawn()
        .map_err(|e| format!("cannot run '{}': {e}", describe(prog, args)))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("'{}' failed ({status})", describe(prog, args))),
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("'{}' timed out after {}s and was stopped", describe(prog, args), timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(250)),
            Err(e) => return Err(format!("wait for '{prog}': {e}")),
        }
    }
}

/// Like `run`, but a failure is only a warning.
pub fn run_soft(ctx: Ctx, prog: &str, args: &[&str]) {
    if let Err(e) = run(ctx, prog, args) {
        warn(&e);
    }
}

/// Read-only probe: true if the command ran and exited 0. Output is discarded.
pub fn probe(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command and capture stdout (trimmed).
pub fn output(prog: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run '{}': {e}", describe(prog, args)))?;
    if !out.status.success() {
        return Err(format!("'{}' failed ({})", describe(prog, args), out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run a command feeding `input` on stdin (keeps secrets out of the process list).
pub fn output_with_stdin(prog: &str, args: &[&str], input: &str) -> Result<String, String> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run '{}': {e}", describe(prog, args)))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .map_err(|e| format!("write to '{prog}': {e}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("wait for '{prog}': {e}"))?;
    if !out.status.success() {
        return Err(format!("'{prog}' failed ({})", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(unix)]
pub fn set_mode(path: &str, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {path}: {e}"))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &str, _mode: u32) -> Result<(), String> {
    Ok(())
}

pub fn mkdir(ctx: Ctx, path: &str) -> Result<(), String> {
    if ctx.dry {
        println!("[dry-run] mkdir -p {path}");
        return Ok(());
    }
    fs::create_dir_all(path).map_err(|e| format!("mkdir {path}: {e}"))
}

/// Write a file (creating parent dirs) and set its mode. `secret` hides content in dry-run output.
pub fn write_file(ctx: Ctx, path: &str, content: &str, mode: u32, secret: bool) -> Result<(), String> {
    if ctx.dry {
        println!("[dry-run] write {path} (mode {mode:o}, {} bytes)", content.len());
        if !secret {
            for line in content.lines() {
                println!("    | {line}");
            }
        }
        return Ok(());
    }
    if let Some(parent) = std::path::Path::new(path).parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(path, content).map_err(|e| format!("write {path}: {e}"))?;
    set_mode(path, mode)
}

/// Replace every "@KEY@" in a template.
pub fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut s = template.to_string();
    for (k, v) in vars {
        s = s.replace(&format!("@{k}@"), v);
    }
    s
}

/// Seconds since the unix epoch (for measuring elapsed times).
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current time as ISO-8601 (via date(1)); falls back to unix seconds.
pub fn now_iso() -> String {
    output("date", &["-Is"]).unwrap_or_else(|_| format!("unix:{}", now_epoch()))
}

/// Seconds as "1h 02m 05s" / "12m 05s" / "45s".
pub fn fmt_elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s:02}s"),
        _ => format!("{h}h {m:02}m {s:02}s"),
    }
}
