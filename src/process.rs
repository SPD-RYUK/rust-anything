//! Every outside program runs with stdin closed, in its own process group, under a time limit.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const KEEP: usize = 1 << 20;

pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// `RUST_ANYTHING_COMMAND_TIMEOUT` seconds when set, else `default`.
pub fn limit(default: Duration) -> Duration {
    std::env::var("RUST_ANYTHING_COMMAND_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .and_then(seconds)
        .unwrap_or(default)
}

/// A duration from a number of seconds; `None` when the value cannot be one (not positive, not
/// finite, or too large for `Duration` — a plain conversion panics on `1e20`).
pub fn seconds(v: f64) -> Option<Duration> {
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(v).ok()
}

pub const TESTS: Duration = Duration::from_secs(30 * 60);
pub const BUILD: Duration = Duration::from_secs(30 * 60);
pub const QUICK: Duration = Duration::from_secs(60);

/// Runs `cmd` and captures its output (the last MiB of each stream).
pub fn output(cmd: &mut Command, default: Duration) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = spawn(cmd)?;
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let status = wait(&mut child, cmd, limit(default))?;
    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Runs `cmd` with the output streams it was given.
pub fn status(cmd: &mut Command, default: Duration) -> Result<ExitStatus> {
    let mut child = spawn(cmd)?;
    wait(&mut child, cmd, limit(default))
}

/// Runs `cmd` with `input` on its stdin and the output streams it was given (an agent reading
/// its prompt).
pub fn status_with_input(cmd: &mut Command, input: &[u8], default: Duration) -> Result<ExitStatus> {
    let mut child = spawn_with(cmd, Stdio::piped())?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        use std::io::Write as _;
        let _ = stdin.write_all(&input);
    });
    let status = wait(&mut child, cmd, limit(default));
    let _ = writer.join();
    status
}

fn spawn(cmd: &mut Command) -> Result<Child> {
    spawn_with(cmd, Stdio::null())
}

/// A failed launch of an outside command (`Command::spawn` / `Command::output`), kept as a
/// structured error so callers like `gate` can distinguish missing tools and launch failures from
/// compile errors without parsing message text.
#[derive(Debug)]
pub(crate) struct SpawnError {
    pub(crate) program: String,
    pub(crate) missing: bool,
    message: String,
    source: std::io::Error,
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        (!self.missing).then_some(&self.source)
    }
}

fn effective_env(cmd: &Command, key: &str) -> Option<std::ffi::OsString> {
    let mut overridden = None;
    for (k, v) in cmd.get_envs() {
        let matched = if cfg!(windows) {
            k.to_string_lossy().eq_ignore_ascii_case(key)
        } else {
            k == OsStr::new(key)
        };
        if matched {
            overridden = Some(v);
        }
    }
    match overridden {
        Some(Some(val)) => Some(val.to_os_string()),
        Some(None) => None,
        None => std::env::var_os(key),
    }
}

/// Matches `std::process::Command`'s executable check: exact file path, plus `.exe` on Windows
/// when no extension is given (Rust does not expand `PATHEXT` for `Command::spawn`).
#[cfg(windows)]
fn file_exists_as_executable(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }
    if path.extension().is_none() {
        let mut with_exe = path.as_os_str().to_os_string();
        with_exe.push(".exe");
        return Path::new(&with_exe).is_file();
    }
    false
}

/// Matches `std::process::Command`'s executable check on non-Windows platforms.
#[cfg(not(windows))]
fn file_exists_as_executable(path: &Path) -> bool {
    path.is_file()
}

/// Whether `cmd`'s executable exists at its explicit path or on `cmd`'s effective `PATH`.
/// Used to distinguish a missing executable from `ENOENT` caused by a missing shebang or ELF
/// interpreter on Linux.
fn program_exists(cmd: &Command) -> bool {
    find_program(cmd).is_some()
}

/// Where `cmd`'s executable is: its explicit path, or the first match on its effective `PATH`.
fn find_program(cmd: &Command) -> Option<PathBuf> {
    let program = cmd.get_program();
    let prog_str = program.to_string_lossy();
    let base = cmd
        .get_current_dir()
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok());
    if prog_str.contains('/') || prog_str.contains('\\') {
        let path = Path::new(program);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base?.join(path)
        };
        return file_exists_as_executable(&path).then_some(path);
    }
    let paths = effective_env(cmd, "PATH")?;
    // On Unix, `Command::spawn` changes into `current_dir` before searching `PATH`; on Windows,
    // `resolve_exe` resolves relative `PATH` entries against the parent's working directory.
    let path_base = if cfg!(windows) {
        std::env::current_dir().ok()
    } else {
        base
    };
    std::env::split_paths(&paths).find_map(|dir| {
        let resolved = if dir.is_absolute() {
            dir
        } else if let Some(cwd) = &path_base {
            cwd.join(dir)
        } else {
            dir
        };
        let path = resolved.join(program);
        file_exists_as_executable(&path).then_some(path)
    })
}

/// Turns a failed `Command::spawn` / `Command::output` into an actionable error.
/// When the program is confirmed absent (`ErrorKind::NotFound` and not found on `PATH`), names it
/// and includes `doctor`'s install command when known instead of the raw OS error.
pub(crate) fn spawn_error(cmd: &Command, what: &str, err: std::io::Error) -> anyhow::Error {
    let program = cmd.get_program().to_string_lossy().into_owned();
    if err.kind() == std::io::ErrorKind::NotFound && cmd.get_current_dir().is_none_or(Path::is_dir)
    {
        if !program_exists(cmd) {
            let message = if program.contains('/') || program.contains('\\') {
                format!("`{program}` not found; run `rust-anything doctor`")
            } else {
                match crate::doctor::missing_command(&program) {
                    Some(install) if install.contains('\n') => format!(
                        "`{program}` not found on PATH; run `rust-anything doctor` or install it:\n{install}"
                    ),
                    Some(install) => format!(
                        "`{program}` not found on PATH; install it with `{install}`, or run `rust-anything doctor`"
                    ),
                    None => format!("`{program}` not found on PATH; run `rust-anything doctor`"),
                }
            };
            return anyhow::Error::new(SpawnError {
                program,
                missing: true,
                message,
                source: err,
            });
        }
        return anyhow::Error::new(SpawnError {
            program,
            missing: false,
            message: format!(
                "running {what} (executable exists, but its interpreter or loader may be missing)"
            ),
            source: err,
        });
    }
    anyhow::Error::new(SpawnError {
        program,
        missing: false,
        message: format!("running {what}"),
        source: err,
    })
}

fn spawn_with(cmd: &mut Command, stdin: Stdio) -> Result<Child> {
    cmd.stdin(stdin);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd.spawn()
        .map_err(|e| spawn_error(cmd, &format!("`{}`", shown(cmd)), e))
}

fn drain(stream: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let Some(mut stream) = stream else {
            return kept;
        };
        let mut buf = [0u8; 8192];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&buf[..n]);
            if kept.len() > 2 * KEEP {
                kept.drain(..kept.len() - KEEP);
            }
        }
        if kept.len() > KEEP {
            kept.drain(..kept.len() - KEEP);
        }
        kept
    })
}

fn wait(child: &mut Child, cmd: &Command, limit: Duration) -> Result<ExitStatus> {
    let start = Instant::now();
    let mut pause = Duration::from_millis(5);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() > limit {
            kill(child);
            bail!(
                "`{}` was still running after {}s and was stopped (set RUST_ANYTHING_COMMAND_TIMEOUT to allow longer; a test runner in watch mode never ends, so run it once, e.g. `vitest run`)",
                shown(cmd),
                limit.as_secs()
            );
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(100));
    }
}

fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn shown(cmd: &Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where `record --sandbox` and `app record --sandbox` run the original: no network, its
/// repository read-only, and CPU and memory limits. bubblewrap on Linux (the whole file system
/// read-only apart from `scratch` and `writable`), sandbox-exec on macOS (writes refused in the
/// repository only, and no memory limit, which macOS does not enforce).
pub struct Sandbox<'a> {
    /// The original's repository: read, never written.
    pub read_only: &'a Path,
    /// A folder of the run's own it may write to, also its `TMPDIR` on Linux.
    pub scratch: &'a Path,
    /// More folders it may write to (the recording's work folder).
    pub writable: &'a [&'a Path],
    /// Keep loopback open: a server rust-anything sends requests to, or local effect sinks.
    pub loopback: bool,
}

/// The command `doctor` and a missing sandbox tool give to install it.
pub(crate) const SANDBOX_INSTALL: &str = if cfg!(target_os = "macos") {
    "sandbox-exec ships with macOS at /usr/bin/sandbox-exec"
} else {
    "sudo apt install bubblewrap  (or `sudo dnf install bubblewrap`, `sudo pacman -S bubblewrap`)"
};

/// `RUST_ANYTHING_SANDBOX_CPU` seconds of CPU time per process, 600 by default.
fn sandbox_cpu() -> u64 {
    std::env::var("RUST_ANYTHING_SANDBOX_CPU")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(600)
}

/// `RUST_ANYTHING_SANDBOX_MEMORY_MB` of data memory per process, 4096 by default (Linux).
fn sandbox_memory_mb() -> u64 {
    std::env::var("RUST_ANYTHING_SANDBOX_MEMORY_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(4096)
}

pub(crate) fn sandbox_tool() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let tool = PathBuf::from("/usr/bin/sandbox-exec");
        if !tool.is_file() {
            bail!("`--sandbox` needs sandbox-exec, which is missing: {SANDBOX_INSTALL}");
        }
        return Ok(tool);
    }
    if !cfg!(target_os = "linux") {
        bail!(
            "`--sandbox` runs on Linux (bubblewrap) and macOS (sandbox-exec), not on this system; \
             record without it"
        );
    }
    find_program(&Command::new("bwrap")).with_context(|| {
        format!(
            "`--sandbox` needs bubblewrap (`bwrap`), which is not on PATH; install it with \
             `{SANDBOX_INSTALL}`, or run `rust-anything doctor`"
        )
    })
}

impl Sandbox<'_> {
    /// `cmd` (its program, arguments, environment and working directory) run inside the sandbox,
    /// with the environment it gets unsandboxed: `env` puts back the variables the shell that
    /// sets the limits changes (`PWD`, `OLDPWD`, `SHLVL`, `_`). Call it once the environment is
    /// set, with `inherit_env` false when the program must get only what `cmd` sets; give the
    /// result its stdio afterwards.
    pub fn wrap(&self, cmd: &Command, inherit_env: bool) -> Result<Command> {
        let tool = sandbox_tool()?;
        if self.loopback && cfg!(target_os = "linux") {
            bail!(
                "`--sandbox` gives the original a network of its own on Linux (bubblewrap's \
                 --unshare-net), so no loopback is shared with rust-anything: it cannot reach a \
                 server's port (`--http`) or let the original reach local effect sinks \
                 (`--effects`). Record those without `--sandbox`; on macOS loopback stays open"
            );
        }
        let program = find_program(cmd).ok_or_else(|| {
            spawn_error(
                cmd,
                &format!("`{}`", shown(cmd)),
                std::io::Error::from(std::io::ErrorKind::NotFound),
            )
        })?;
        let real = |p: &Path| {
            use crate::walk::Canonical as _;
            p.canonical()
                .with_context(|| format!("resolving {}", p.display()))
        };
        let read_only = real(self.read_only)?;
        let scratch = real(self.scratch)?;
        let writable = self
            .writable
            .iter()
            .map(|p| real(p))
            .collect::<Result<Vec<_>>>()?;
        let mut out = Command::new(&tool);
        if cfg!(target_os = "macos") {
            let mut profile = String::from(
                "(version 1)(allow default)(deny network*)(deny file-write* (subpath (param \"REPO\")))",
            );
            if self.loopback {
                profile.push_str(
                    "(allow network-outbound (remote ip \"localhost:*\"))\
                     (allow network-inbound (local ip \"localhost:*\"))\
                     (allow network-bind (local ip \"localhost:*\"))",
                );
            }
            out.arg("-D").arg(format!("REPO={}", read_only.display()));
            for (i, dir) in std::iter::once(&scratch).chain(&writable).enumerate() {
                profile.push_str(&format!("(allow file-write* (subpath (param \"W{i}\")))"));
                out.arg("-D").arg(format!("W{i}={}", dir.display()));
            }
            out.arg("-p").arg(profile);
        } else {
            out.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
            for dir in std::iter::once(&scratch).chain(&writable) {
                out.arg("--bind").arg(dir).arg(dir);
            }
            out.args([
                "--unshare-net",
                "--unshare-pid",
                "--unshare-ipc",
                "--die-with-parent",
                "--new-session",
            ]);
            out.arg("--setenv").arg("TMPDIR").arg(&scratch);
            if let Some(dir) = cmd.get_current_dir() {
                out.arg("--chdir").arg(dir);
            }
            out.arg("--");
        }
        let mut script = format!("ulimit -t {}", sandbox_cpu());
        if cfg!(target_os = "linux") {
            script.push_str(&format!(" && ulimit -d {}", sandbox_memory_mb() * 1024));
        }
        script.push_str(" || exit 126; exec /usr/bin/env \"$@\"");
        out.args(["/bin/sh", "-c", &script, "sh"]);
        for key in ["PWD", "OLDPWD", "SHLVL", "_"] {
            let value = if inherit_env {
                effective_env(cmd, key)
            } else {
                cmd.get_envs()
                    .find(|(k, _)| *k == OsStr::new(key))
                    .and_then(|(_, v)| v.map(OsStr::to_os_string))
            };
            match value {
                Some(value) => {
                    let mut set = std::ffi::OsString::from(format!("{key}="));
                    set.push(value);
                    out.arg(set)
                }
                None => out.args(["-u", key]),
            };
        }
        out.arg(program).args(cmd.get_args());
        if !inherit_env {
            out.env_clear();
        }
        for (key, value) in cmd.get_envs() {
            match value {
                Some(value) => out.env(key, value),
                None => out.env_remove(key),
            };
        }
        if let Some(dir) = cmd.get_current_dir() {
            out.current_dir(dir);
        }
        Ok(out)
    }
}

/// Whether `--sandbox` can run here: what it runs with, or why not. Wraps `true` the way a
/// recording is wrapped, so a bubblewrap that is installed but not allowed to make namespaces
/// shows up here rather than mid-recording.
pub fn sandbox_ready() -> Result<String, String> {
    let tool = sandbox_tool().map_err(|e| format!("{e:#}"))?;
    let dir = crate::app::temp_dir("sandbox-check").map_err(|e| format!("{e:#}"))?;
    let sandbox = Sandbox {
        read_only: &dir,
        scratch: &dir,
        writable: &[],
        loopback: false,
    };
    let ran = sandbox
        .wrap(Command::new("true").current_dir(&dir), true)
        .and_then(|mut cmd| output(&mut cmd, QUICK));
    let _ = std::fs::remove_dir_all(&dir);
    match ran {
        Ok(out) if out.success() => Ok(tool.display().to_string()),
        Ok(out) => Err(format!(
            "{} is installed but could not start a sandbox: {}",
            tool.display(),
            out.stderr_text().trim()
        )),
        Err(e) => Err(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_value_that_cannot_be_a_duration_falls_back_instead_of_panicking() {
        // `Duration::from_secs_f64(1e20)` panics; an over-large value is ignored, as a bad one is.
        assert_eq!(seconds(1e20), None);
        assert_eq!(seconds(f64::INFINITY), None);
        assert_eq!(seconds(f64::NAN), None);
        assert_eq!(seconds(-1.0), None);
        assert_eq!(seconds(0.0), None);
        assert_eq!(seconds(2.5), Some(Duration::from_secs_f64(2.5)));
    }

    #[cfg(unix)]
    #[test]
    fn a_program_past_its_limit_is_stopped() {
        let start = Instant::now();
        let err = output(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(300),
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(err.contains("still running"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn stdin_is_closed() {
        let out = output(&mut Command::new("cat"), Duration::from_secs(5)).unwrap();
        assert!(out.success());
        assert!(out.stdout.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn output_is_kept_and_capped() {
        let out = output(
            Command::new("sh").args(["-c", "yes | head -c 3000000; echo err >&2"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(out.stdout.len(), KEEP);
        assert_eq!(out.stderr_text(), "err\n");
    }

    #[test]
    fn a_sandboxed_program_gets_the_environment_it_gets_unsandboxed() {
        if sandbox_ready().is_err() {
            return;
        }
        let dir = crate::app::temp_dir("sandbox-env").unwrap();
        let sandbox = Sandbox {
            read_only: &dir,
            scratch: &dir,
            writable: &[],
            loopback: false,
        };
        let env = |cmd: &mut Command| output(cmd, QUICK).unwrap().stdout_text();
        let mut plain = Command::new("env");
        plain.current_dir(&dir).env("FOO", "1").env_remove("HOME");
        let inherited = env(&mut sandbox.wrap(&plain, true).unwrap());
        let cleared = env(&mut sandbox.wrap(&plain, false).unwrap());
        let unsandboxed = env(&mut plain);
        let _ = std::fs::remove_dir_all(&dir);
        // The order of `env`'s lines is the sandbox tool's, and on Linux the sandbox sets
        // `TMPDIR` to its scratch folder on purpose. Compare the variables, and name the ones
        // that differ: a failure must not print the environment, which can hold tokens.
        let vars = |text: &str| -> std::collections::BTreeMap<String, String> {
            text.lines()
                .filter_map(|line| line.split_once('='))
                .filter(|(name, _)| *name != "TMPDIR")
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect()
        };
        let (inherited, unsandboxed, cleared) =
            (vars(&inherited), vars(&unsandboxed), vars(&cleared));
        let differing: Vec<&String> = inherited
            .keys()
            .chain(unsandboxed.keys())
            .filter(|name| inherited.get(*name) != unsandboxed.get(*name))
            .collect();
        assert!(differing.is_empty(), "the sandbox changed: {differing:?}");
        // Nothing is inherited when the environment is cleared: only what the command sets.
        assert_eq!(cleared.keys().collect::<Vec<_>>(), ["FOO"]);
        // The value the command set is the value it sees (the message does not print it).
        assert!(
            cleared.get("FOO").is_some_and(|value| value == "1"),
            "FOO was not kept as the command set it"
        );
    }

    #[test]
    fn missing_program_names_it_and_points_to_doctor() {
        let program = "rust-anything-nonexistent-program-498";
        let err = output(&mut Command::new(program), Duration::from_secs(5))
            .err()
            .expect("nonexistent program must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("`{program}` not found on PATH")),
            "{msg}"
        );
        assert!(msg.contains("rust-anything doctor"), "{msg}");
        assert!(!msg.contains("os error"), "{msg}");
    }

    #[test]
    fn missing_cargo_includes_doctor_install_command() {
        let mut cmd = Command::new("cargo");
        cmd.env_remove("PATH");
        let err = spawn_error(
            &cmd,
            "cargo build",
            std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        );
        let spawn = err.downcast_ref::<SpawnError>().unwrap();
        assert!(spawn.missing);
        assert_eq!(spawn.program, "cargo");
        let msg = format!("{err:#}");
        let install = crate::doctor::missing_command("cargo").unwrap();
        assert!(msg.contains("`cargo` not found on PATH"), "{msg}");
        assert!(msg.contains(&install), "{msg}");
        assert!(msg.contains("rust-anything doctor"), "{msg}");
    }

    #[test]
    fn existing_executable_with_not_found_keeps_launch_context() {
        let dir = std::env::temp_dir().join(format!("ra-proc-exists-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wrapper = dir.join("fake-cargo");
        std::fs::write(&wrapper, "#!/missing/interpreter\n").unwrap();

        let mut on_path = Command::new("fake-cargo");
        on_path.env("PATH", &dir);
        let err = spawn_error(
            &on_path,
            "cargo build",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no interpreter"),
        );
        let spawn = err.downcast_ref::<SpawnError>().unwrap();
        assert!(!spawn.missing);
        let msg = format!("{err:#}");
        assert!(msg.starts_with("running cargo build"), "{msg}");
        assert!(msg.contains("interpreter"), "{msg}");
        assert!(msg.contains("no interpreter"), "{msg}");
        assert!(!msg.contains("not found on PATH"), "{msg}");

        let explicit = Command::new(&wrapper);
        let explicit_err = spawn_error(
            &explicit,
            "fake-cargo",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no interpreter"),
        );
        assert!(!explicit_err.downcast_ref::<SpawnError>().unwrap().missing);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_path_entry_resolves_against_child_cwd() {
        let dir = std::env::temp_dir().join(format!("ra-proc-relpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let name = if cfg!(windows) {
            "fake-rel-tool.exe"
        } else {
            "fake-rel-tool"
        };
        std::fs::write(bin.join(name), "").unwrap();

        let mut cmd = Command::new("fake-rel-tool");
        cmd.current_dir(&dir).env("PATH", "bin");
        let err = spawn_error(
            &cmd,
            "fake-rel-tool",
            std::io::Error::new(std::io::ErrorKind::NotFound, "no interpreter"),
        );
        if cfg!(windows) {
            assert!(
                err.downcast_ref::<SpawnError>().unwrap().missing,
                "on Windows, relative PATH entries resolve against the parent cwd, not child cwd"
            );
        } else {
            assert!(
                !err.downcast_ref::<SpawnError>().unwrap().missing,
                "on Unix, relative PATH entries resolve against child cwd"
            );
        }

        #[cfg(windows)]
        {
            std::fs::write(bin.join("only-cmd.cmd"), "@echo off\r\n").unwrap();
            let mut cmd_only = Command::new("only-cmd");
            cmd_only.env("PATH", &bin);
            let err = spawn_error(
                &cmd_only,
                "only-cmd",
                std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
            );
            assert!(
                err.downcast_ref::<SpawnError>().unwrap().missing,
                "bare extensionless lookup on Windows should not match .cmd via PATHEXT"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_cwd_or_other_spawn_errors_keep_running_context() {
        let mut bad_cwd = Command::new("cargo");
        bad_cwd.env_remove("PATH");
        bad_cwd.current_dir(std::env::temp_dir().join("rust-anything-no-such-dir-498"));
        let err = spawn_error(
            &bad_cwd,
            "cargo build",
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing cwd"),
        );
        assert!(!err.downcast_ref::<SpawnError>().unwrap().missing);
        let msg = format!("{err:#}");
        assert!(msg.starts_with("running cargo build"), "{msg}");
        assert!(!msg.contains("not found on PATH"), "{msg}");

        let cmd = Command::new("cargo");
        let perm = spawn_error(
            &cmd,
            "cargo build",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        assert!(!perm.downcast_ref::<SpawnError>().unwrap().missing);
        let perm_msg = format!("{perm:#}");
        assert!(perm_msg.starts_with("running cargo build"), "{perm_msg}");
    }
}
