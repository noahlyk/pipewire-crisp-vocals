//! Which build the running service is, for `crisp-vocals --version`.
//!
//! A package upgrade replaces `/usr/bin/crisp-vocals`, but the service keeps
//! running the old code until it is restarted; `--version` says so when the
//! installed binary differs from the processes still in memory.
//!
//! The service runs one executable as two processes, told apart by their
//! subcommand ([`Role`]): `crisp-vocals dsp` and `crisp-vocals links`.
//! There is no IPC channel between the CLI and them (the config file is the
//! only interface), so each leaves a small version file:
//!
//!   - at startup each writes `$XDG_RUNTIME_DIR/crisp-vocals/<role>.version`
//!     (`dsp.version`, `links.version`): three lines, the version string,
//!     the PID and the process start time (field 22 of `/proc/<pid>/stat`),
//!     written to a temp file and renamed into place. `links --dry-run` is
//!     not the service and writes none;
//!   - on a clean exit the file is removed (`VersionFile`'s `Drop`). A
//!     process that is killed leaves its file behind, which is why a record
//!     only counts while its PID is alive, runs the `crisp-vocals`
//!     executable with that role's subcommand (`/proc/<pid>/cmdline`) and
//!     has the recorded start time (so a reused PID never matches);
//!   - `--version` compares each recorded version string with its own, as
//!     exact strings. With no usable record for a role it falls back to
//!     looking for a process of that role whose `/proc/<pid>/exe` ends in
//!     ` (deleted)`: one whose binary was replaced on disk, e.g. a build from
//!     before these files existed. That fallback also recognises the
//!     pre-subcommand layout (a bare `crisp-vocals` was the DSP, a bare
//!     `crisp-links` the wiring daemon), so the first upgrade to the single
//!     binary is reported too.
//!
//! Everything on the `--version` side is best effort: any error reads as
//! "nothing to report", so the version line is always printed.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The version the binary reports: `0.3.0` on a release tag,
/// `0.3.0+4.gabc1234[.dirty]` on a checkout (see `build.rs`).
pub const VERSION: &str = env!("PIPEWIRE_CRISP_VOCALS_VERSION");

/// The one installed executable.
pub const BIN: &str = "crisp-vocals";

/// The wiring daemon's executable before it became `crisp-vocals links`;
/// only looked for in processes still running an old build.
const LEGACY_LINKS_BIN: &str = "crisp-links";

const PROC: &str = "/proc";
/// What the kernel appends to `/proc/<pid>/exe` once the file is unlinked.
const DELETED_SUFFIX: &str = " (deleted)";

/// The two long-running processes of `pipewire-crisp-vocals.service`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `crisp-vocals dsp`: the realtime JACK client.
    Dsp,
    /// `crisp-vocals links`: the PipeWire wiring daemon.
    Links,
}

impl Role {
    pub const ALL: [Self; 2] = [Self::Dsp, Self::Links];

    /// The subcommand, also the version file's stem and the name
    /// `--version` reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dsp => "dsp",
            Self::Links => "links",
        }
    }
}

/// `$XDG_RUNTIME_DIR/crisp-vocals`, falling back to `/run/user/<uid>` when
/// the variable is unset. `None` when neither exists.
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| {
        let uid = fs::metadata("/proc/self").ok()?.uid();
        let dir = PathBuf::from(format!("/run/user/{uid}"));
        dir.is_dir().then_some(dir)
    })?;
    Some(base.join("crisp-vocals"))
}

fn version_file_path(dir: &Path, role: Role) -> PathBuf {
    dir.join(format!("{}.version", role.name()))
}

/// One process's version file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    version: String,
    pid: u32,
    /// `None` when the process could not read its own start time; the PID
    /// and role checks still apply.
    start_time: Option<u64>,
}

impl Record {
    fn encode(&self) -> String {
        let start = self.start_time.map_or_else(String::new, |t| t.to_string());
        format!("{}\n{}\n{start}\n", self.version, self.pid)
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let version = lines.next()?.trim();
        if version.is_empty() {
            return None;
        }
        let pid = lines.next()?.trim().parse().ok()?;
        let start_time = lines.next().and_then(|l| l.trim().parse().ok());
        Some(Self { version: version.to_string(), pid, start_time })
    }
}

/// Process start time in clock ticks since boot: field 22 of
/// `/proc/<pid>/stat`. Field 2 (the command name) may itself contain spaces
/// and parentheses, so fields are counted from the last `)`.
fn start_time(proc_root: &Path, pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    stat.rsplit_once(')')?.1.split_ascii_whitespace().nth(19)?.parse().ok()
}

/// File name of a process's executable and whether that file has since been
/// deleted or replaced on disk. `None` when the process does not exist (or
/// is not ours to inspect).
fn exe_of(proc_root: &Path, pid: u32) -> Option<(String, bool)> {
    let target = fs::read_link(proc_root.join(pid.to_string()).join("exe")).ok()?;
    let target = target.to_string_lossy();
    let deleted = target.ends_with(DELETED_SUFFIX);
    let path = target.strip_suffix(DELETED_SUFFIX).unwrap_or(&target);
    Some((Path::new(path).file_name()?.to_string_lossy().into_owned(), deleted))
}

/// A process's arguments after `argv[0]`, from `/proc/<pid>/cmdline`.
fn args_of(proc_root: &Path, pid: u32) -> Option<Vec<String>> {
    let raw = fs::read(proc_root.join(pid.to_string()).join("cmdline")).ok()?;
    let mut args = raw.split(|&b| b == 0).map(|a| String::from_utf8_lossy(a).into_owned());
    args.next()?; // argv[0]
    let mut args: Vec<String> = args.collect();
    // cmdline ends in a NUL, which leaves one empty string behind.
    if args.last().is_some_and(String::is_empty) {
        args.pop();
    }
    Some(args)
}

/// Which service process `exe` + `args` is, if any: `crisp-vocals dsp`,
/// `crisp-vocals links` (not `--dry-run`, which is a diagnostic), and the
/// pre-subcommand layout, a bare `crisp-vocals` (DSP) or `crisp-links`.
fn role_from(exe: &str, args: &[String]) -> Option<Role> {
    let first = args.first().map(String::as_str);
    match (exe, first) {
        (BIN, None | Some("dsp")) => Some(Role::Dsp),
        (BIN, Some("links")) if !args.iter().any(|a| a == "--dry-run") => Some(Role::Links),
        (LEGACY_LINKS_BIN, None) => Some(Role::Links),
        _ => None,
    }
}

/// The role of process `pid` and whether its executable was replaced on disk.
fn role_of(proc_root: &Path, pid: u32) -> Option<(Role, bool)> {
    let (exe, deleted) = exe_of(proc_root, pid)?;
    let role = role_from(&exe, &args_of(proc_root, pid)?)?;
    Some((role, deleted))
}

/// A process's version file; removed again when dropped (clean exit).
#[derive(Debug)]
pub struct VersionFile {
    path: PathBuf,
    pid: u32,
}

impl Drop for VersionFile {
    fn drop(&mut self) {
        // Only remove our own record: a second instance of the same role
        // may have replaced the file since.
        let ours =
            fs::read_to_string(&self.path).ok().and_then(|t| Record::parse(&t)).is_some_and(|r| r.pid == self.pid);
        if ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_version_file_in(dir: &Path, proc_root: &Path, role: Role, version: &str) -> io::Result<VersionFile> {
    fs::create_dir_all(dir)?;
    let pid = std::process::id();
    let record = Record { version: version.to_string(), pid, start_time: start_time(proc_root, pid) };
    let path = version_file_path(dir, role);
    // Temp file + rename, so a concurrent `--version` never reads half a file.
    let tmp = dir.join(format!(".{}.version.{pid}", role.name()));
    if let Err(e) = fs::write(&tmp, record.encode()).and_then(|()| fs::rename(&tmp, &path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(VersionFile { path, pid })
}

/// Write `<dir>/<role>.version` for the current process.
///
/// # Errors
///
/// When the directory cannot be created or the file cannot be written.
pub fn write_version_file(dir: &Path, role: Role, version: &str) -> io::Result<VersionFile> {
    write_version_file_in(dir, Path::new(PROC), role, version)
}

/// Process startup: record this process's version for `--version` to find.
///
/// Keep the returned guard alive for as long as the process runs. Failing
/// to write the file is logged and otherwise ignored; it must never stop the
/// audio chain from starting.
#[must_use]
pub fn announce(role: Role) -> Option<VersionFile> {
    let tag = format!("{BIN} {}", role.name());
    let Some(dir) = runtime_dir() else {
        eprintln!("[{tag}] no XDG_RUNTIME_DIR; `--version` cannot tell which build is running");
        return None;
    };
    match write_version_file(&dir, role, VERSION) {
        Ok(file) => Some(file),
        Err(e) => {
            eprintln!("[{tag}] could not write {}: {e}", version_file_path(&dir, role).display());
            None
        }
    }
}

/// The version recorded by the running `role` process, if its record is
/// present and still describes a live process of that role.
fn running_version(dir: &Path, proc_root: &Path, role: Role) -> Option<String> {
    let record = Record::parse(&fs::read_to_string(version_file_path(dir, role)).ok()?)?;
    let (found, _deleted) = role_of(proc_root, record.pid)?;
    if found != role {
        return None;
    }
    if record.start_time.is_some() && record.start_time != start_time(proc_root, record.pid) {
        return None;
    }
    Some(record.version)
}

/// Is some other process running `role` from an executable that has been
/// deleted or replaced on disk?
fn deleted_exe_running(proc_root: &Path, role: Role) -> bool {
    let own_pid = std::process::id();
    let Ok(entries) = fs::read_dir(proc_root) else {
        return false;
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != own_pid)
        .any(|pid| role_of(proc_root, pid).is_some_and(|(found, deleted)| deleted && found == role))
}

/// Why a running process counts as out of date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stale {
    /// It reports this version, which differs from ours.
    Version(String),
    /// It runs from a replaced executable and left no usable record.
    OlderBuild,
}

/// Compare the running processes with `own_version`: the out-of-date ones,
/// in [`Role::ALL`] order. Empty when nothing is running or all match.
fn stale_daemons_in(dir: Option<&Path>, proc_root: &Path, own_version: &str) -> Vec<(Role, Stale)> {
    Role::ALL
        .into_iter()
        .filter_map(|role| match dir.and_then(|dir| running_version(dir, proc_root, role)) {
            Some(version) if version != own_version => Some((role, Stale::Version(version))),
            Some(_) => None,
            None => deleted_exe_running(proc_root, role).then_some((role, Stale::OlderBuild)),
        })
        .collect()
}

/// The running service's processes that differ from this binary.
#[must_use]
pub fn stale_daemons() -> Vec<(Role, Stale)> {
    stale_daemons_in(runtime_dir().as_deref(), Path::new(PROC), VERSION)
}

/// The `--version` line: `crisp-vocals <version>`, plus a note naming the
/// out-of-date processes, grouped by what they run:
/// `crisp-vocals 0.4.0 (dsp, links out-of-date, running 0.3.0)` or
/// `(dsp out-of-date, running 0.3.0; links out-of-date, running an older build)`.
#[must_use]
pub fn format_version(version: &str, stale: &[(Role, Stale)]) -> String {
    let mut groups: Vec<(Vec<&str>, &Stale)> = Vec::new();
    for (role, why) in stale {
        match groups.iter_mut().find(|(_, w)| *w == why) {
            Some((roles, _)) => roles.push(role.name()),
            None => groups.push((vec![role.name()], why)),
        }
    }
    let line = format!("{BIN} {version}");
    if groups.is_empty() {
        return line;
    }
    let notes: Vec<String> = groups
        .iter()
        .map(|(roles, why)| {
            let running = match why {
                Stale::Version(v) => v.as_str(),
                Stale::OlderBuild => "an older build",
            };
            format!("{} out-of-date, running {running}", roles.join(", "))
        })
        .collect();
    format!("{line} ({})", notes.join("; "))
}

/// Print the `--version` line. Never fails: a closed stdout is ignored
/// rather than panicking.
pub fn print_version() {
    let text = format_version(VERSION, &stale_daemons());
    let _ = writeln!(io::stdout(), "{text}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch directory that removes itself.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("crisp-config-test-{}-{n}-{tag}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Add a process to a fake `/proc`: an `exe` link (which, like the real
    /// one for a replaced binary, may point at a path that does not exist),
    /// a NUL-separated `cmdline` and a `stat` line carrying `start_time`.
    fn fake_process(proc_root: &Path, pid: u32, exe: &str, args: &[&str], start_time: u64) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        let _ = fs::remove_file(dir.join("exe"));
        symlink(exe, dir.join("exe")).unwrap();
        let argv0 = Path::new(exe.strip_suffix(DELETED_SUFFIX).unwrap_or(exe)).file_name().unwrap().to_str().unwrap();
        let cmdline: String = std::iter::once(argv0).chain(args.iter().copied()).map(|a| format!("{a}\0")).collect();
        fs::write(dir.join("cmdline"), cmdline).unwrap();
        // The command name deliberately contains ") " to exercise the parser.
        let stat = format!("{pid} (odd) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 {start_time} 20 21\n");
        fs::write(dir.join("stat"), stat).unwrap();
    }

    fn write_record(dir: &Path, role: Role, record: &Record) {
        fs::create_dir_all(dir).unwrap();
        fs::write(version_file_path(dir, role), record.encode()).unwrap();
    }

    fn record(version: &str, pid: u32, start_time: Option<u64>) -> Record {
        Record { version: version.to_string(), pid, start_time }
    }

    fn v(s: &str) -> Stale {
        Stale::Version(s.to_string())
    }

    #[test]
    fn version_alone_when_nothing_is_stale() {
        assert_eq!(format_version("0.3.0+4.gabc1234", &[]), "crisp-vocals 0.3.0+4.gabc1234");
    }

    #[test]
    fn one_line_names_the_stale_processes_grouped_by_what_they_run() {
        assert_eq!(
            format_version("0.4.0", &[(Role::Dsp, v("0.3.0")), (Role::Links, v("0.3.0"))]),
            "crisp-vocals 0.4.0 (dsp, links out-of-date, running 0.3.0)"
        );
        assert_eq!(
            format_version("0.4.0", &[(Role::Links, v("0.3.0+1.gdef5678"))]),
            "crisp-vocals 0.4.0 (links out-of-date, running 0.3.0+1.gdef5678)"
        );
        assert_eq!(
            format_version("0.4.0", &[(Role::Dsp, v("0.3.0")), (Role::Links, Stale::OlderBuild)]),
            "crisp-vocals 0.4.0 (dsp out-of-date, running 0.3.0; links out-of-date, running an older build)"
        );
        assert_eq!(
            format_version("0.4.0", &[(Role::Dsp, Stale::OlderBuild), (Role::Links, Stale::OlderBuild)]),
            "crisp-vocals 0.4.0 (dsp, links out-of-date, running an older build)"
        );
    }

    #[test]
    fn roles_come_from_the_subcommand() {
        let args = |a: &[&str]| a.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(role_from("crisp-vocals", &args(&["dsp"])), Some(Role::Dsp));
        assert_eq!(role_from("crisp-vocals", &args(&["links"])), Some(Role::Links));
        assert_eq!(role_from("crisp-vocals", &args(&["links", "--dry-run"])), None);
        assert_eq!(role_from("crisp-vocals", &args(&["mic", "--list"])), None);
        assert_eq!(role_from("crisp-vocals", &args(&["--version"])), None);
        // Pre-subcommand builds: a bare `crisp-vocals` was the DSP, `crisp-links` the wiring.
        assert_eq!(role_from("crisp-vocals", &args(&[])), Some(Role::Dsp));
        assert_eq!(role_from("crisp-links", &args(&[])), Some(Role::Links));
        assert_eq!(role_from("crisp-links", &args(&["mic", "--list"])), None);
        assert_eq!(role_from("bash", &args(&["dsp"])), None);
    }

    #[test]
    fn record_round_trips() {
        for r in [record("0.3.0+4.gabc1234.dirty", 1313, Some(987_654)), record("0.3.0", 7, None)] {
            assert_eq!(Record::parse(&r.encode()), Some(r));
        }
    }

    #[test]
    fn malformed_records_are_rejected() {
        for text in ["", "\n", "0.3.0", "0.3.0\n", "0.3.0\nnot-a-pid\n", "\n1313\n5\n"] {
            assert_eq!(Record::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn written_file_is_read_back_and_removed_on_drop() {
        let tmp = TempDir::new("write");
        let (run, proc_root) = (tmp.path().join("run/crisp-vocals"), tmp.path().join("proc"));
        let pid = std::process::id();
        fake_process(&proc_root, pid, "/usr/bin/crisp-vocals", &["dsp"], 4242);

        let file = write_version_file_in(&run, &proc_root, Role::Dsp, "0.3.0+4.gabc1234").unwrap();
        let path = version_file_path(&run, Role::Dsp);
        assert!(path.ends_with("dsp.version"));
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("0.3.0+4.gabc1234\n{pid}\n4242\n"));
        assert_eq!(running_version(&run, &proc_root, Role::Dsp).as_deref(), Some("0.3.0+4.gabc1234"));
        // The record is the DSP's, not the wiring daemon's.
        assert_eq!(running_version(&run, &proc_root, Role::Links), None);
        // Only the version file itself is left in the directory (no temp file).
        assert_eq!(fs::read_dir(&run).unwrap().count(), 1);

        drop(file);
        assert!(!path.exists());
        assert_eq!(running_version(&run, &proc_root, Role::Dsp), None);
    }

    #[test]
    fn drop_leaves_a_file_written_by_another_instance() {
        let tmp = TempDir::new("foreign");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        let file = write_version_file_in(&run, &proc_root, Role::Links, "0.3.0").unwrap();
        let other = record("0.3.1", std::process::id() + 1, None);
        write_record(&run, Role::Links, &other);

        drop(file);
        let left = fs::read_to_string(version_file_path(&run, Role::Links)).unwrap();
        assert_eq!(Record::parse(&left), Some(other));
    }

    #[test]
    fn record_of_a_dead_reused_or_other_role_pid_is_ignored() {
        let tmp = TempDir::new("stale-record");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fs::create_dir_all(&proc_root).unwrap();

        // No such process.
        write_record(&run, Role::Dsp, &record("0.2.9", 500, Some(10)));
        assert_eq!(running_version(&run, &proc_root, Role::Dsp), None);

        // PID now belongs to a different program.
        fake_process(&proc_root, 500, "/usr/bin/bash", &[], 10);
        assert_eq!(running_version(&run, &proc_root, Role::Dsp), None);

        // PID belongs to the same executable in the other role.
        fake_process(&proc_root, 500, "/usr/bin/crisp-vocals", &["links"], 10);
        assert_eq!(running_version(&run, &proc_root, Role::Dsp), None);

        // PID belongs to a later DSP than the one that wrote the record.
        fake_process(&proc_root, 501, "/usr/bin/crisp-vocals", &["dsp"], 99);
        write_record(&run, Role::Dsp, &record("0.2.9", 501, Some(10)));
        assert_eq!(running_version(&run, &proc_root, Role::Dsp), None);

        // Same process: accepted, also when its binary was replaced on disk.
        write_record(&run, Role::Dsp, &record("0.2.9", 501, Some(99)));
        assert_eq!(running_version(&run, &proc_root, Role::Dsp).as_deref(), Some("0.2.9"));
        fake_process(&proc_root, 502, "/usr/bin/crisp-vocals (deleted)", &["dsp"], 7);
        write_record(&run, Role::Dsp, &record("0.2.9", 502, Some(7)));
        assert_eq!(running_version(&run, &proc_root, Role::Dsp).as_deref(), Some("0.2.9"));
    }

    #[test]
    fn nothing_running_reports_nothing() {
        let tmp = TempDir::new("idle");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fs::create_dir_all(&proc_root).unwrap();
        fake_process(&proc_root, 40, "/usr/bin/bash", &[], 1);
        assert_eq!(stale_daemons_in(Some(&run), &proc_root, "0.3.0"), []);
        assert_eq!(stale_daemons_in(None, &proc_root, "0.3.0"), []);
        // A missing /proc is not an error either.
        assert_eq!(stale_daemons_in(Some(&run), &tmp.path().join("absent"), "0.3.0"), []);
    }

    #[test]
    fn matching_processes_report_nothing_and_differing_ones_are_named() {
        let tmp = TempDir::new("compare");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fake_process(&proc_root, 1313, "/usr/bin/crisp-vocals", &["dsp"], 5);
        fake_process(&proc_root, 1314, "/usr/bin/crisp-vocals", &["links"], 6);
        write_record(&run, Role::Dsp, &record("0.3.0+4.gabc1234", 1313, Some(5)));
        write_record(&run, Role::Links, &record("0.3.0+4.gabc1234", 1314, Some(6)));
        assert_eq!(stale_daemons_in(Some(&run), &proc_root, "0.3.0+4.gabc1234"), []);

        // Exact string comparison: any difference counts, including `.dirty`.
        let old = v("0.3.0+4.gabc1234");
        assert_eq!(
            stale_daemons_in(Some(&run), &proc_root, "0.3.0+4.gabc1234.dirty"),
            [(Role::Dsp, old.clone()), (Role::Links, old)]
        );

        write_record(&run, Role::Links, &record("0.2.9+1.gdef5678", 1314, Some(6)));
        assert_eq!(stale_daemons_in(Some(&run), &proc_root, "0.3.0+4.gabc1234"), [(Role::Links, v("0.2.9+1.gdef5678"))]);
    }

    #[test]
    fn replaced_executable_without_a_record_is_an_older_build() {
        let tmp = TempDir::new("deleted");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fake_process(&proc_root, 1313, "/usr/bin/crisp-vocals (deleted)", &["dsp"], 5);
        fake_process(&proc_root, 1314, "/usr/bin/crisp-vocals", &["links"], 6);
        fs::create_dir_all(proc_root.join("self")).unwrap(); // non-numeric entries are skipped
        assert_eq!(stale_daemons_in(Some(&run), &proc_root, "0.3.0"), [(Role::Dsp, Stale::OlderBuild)]);
        assert_eq!(stale_daemons_in(None, &proc_root, "0.3.0"), [(Role::Dsp, Stale::OlderBuild)]);

        // A leftover record from a dead PID does not hide it.
        write_record(&run, Role::Dsp, &record("0.2.0", 9999, Some(1)));
        assert_eq!(stale_daemons_in(Some(&run), &proc_root, "0.3.0"), [(Role::Dsp, Stale::OlderBuild)]);

        // An unrelated program, or a dry run, with a deleted executable is not the service.
        let other = TempDir::new("deleted-other");
        fake_process(other.path(), 77, "/usr/bin/firefox (deleted)", &[], 5);
        fake_process(other.path(), 78, "/usr/bin/crisp-vocals (deleted)", &["links", "--dry-run"], 5);
        assert_eq!(stale_daemons_in(None, other.path(), "0.3.0"), []);
    }

    #[test]
    fn the_pre_subcommand_layout_is_recognised_after_the_upgrade() {
        // The 0.3.0 service: a bare `crisp-vocals` and `crisp-links`, both
        // binaries replaced or removed by the upgrade, no version files.
        let tmp = TempDir::new("legacy");
        let proc_root = tmp.path().join("proc");
        fake_process(&proc_root, 1313, "/usr/bin/crisp-vocals (deleted)", &[], 5);
        fake_process(&proc_root, 1314, "/usr/bin/crisp-links (deleted)", &[], 6);
        let stale = stale_daemons_in(Some(&tmp.path().join("run")), &proc_root, "1.0.0");
        assert_eq!(stale, [(Role::Dsp, Stale::OlderBuild), (Role::Links, Stale::OlderBuild)]);
        assert_eq!(format_version("1.0.0", &stale), "crisp-vocals 1.0.0 (dsp, links out-of-date, running an older build)");
    }

    #[test]
    fn a_known_version_wins_over_an_unidentified_older_build() {
        let tmp = TempDir::new("both");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fake_process(&proc_root, 1313, "/usr/bin/crisp-vocals (deleted)", &["dsp"], 5);
        fake_process(&proc_root, 1314, "/usr/bin/crisp-vocals (deleted)", &["links"], 6);
        write_record(&run, Role::Links, &record("0.2.9", 1314, Some(6)));
        assert_eq!(
            stale_daemons_in(Some(&run), &proc_root, "0.3.0"),
            [(Role::Dsp, Stale::OlderBuild), (Role::Links, v("0.2.9"))]
        );
    }

    #[test]
    fn reads_the_real_proc_for_this_process() {
        let pid = std::process::id();
        assert!(start_time(Path::new(PROC), pid).is_some());
        let (_exe, deleted) = exe_of(Path::new(PROC), pid).expect("own exe link");
        assert!(!deleted);
        assert!(args_of(Path::new(PROC), pid).is_some());
    }
}
