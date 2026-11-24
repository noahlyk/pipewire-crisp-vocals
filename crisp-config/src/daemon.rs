//! Which build the running service is, for both binaries' `--version`.
//!
//! A package upgrade replaces the files in `/usr/bin`, but the service keeps
//! running the old code until it is restarted; `--version` says so when the
//! installed binaries differ from the daemons still in memory.
//!
//! There is no IPC channel between the CLI and the daemons (the config file
//! is the only interface), so each daemon leaves a small version file:
//!
//!   - at startup `crisp-vocals` and `crisp-links` each write
//!     `$XDG_RUNTIME_DIR/crisp-vocals/<name>.version`: three lines, the
//!     version string, the PID and the process start time (field 22 of
//!     `/proc/<pid>/stat`), written to a temp file and renamed into place;
//!   - on a clean exit the file is removed (`VersionFile`'s `Drop`). A
//!     daemon that is killed leaves its file behind, which is why a record
//!     only counts while its PID is alive, is that same daemon's executable
//!     and has the recorded start time (so a reused PID never matches);
//!   - `--version` compares the recorded version string with its own, as
//!     exact strings. With no usable record it falls back to looking for a
//!     daemon whose `/proc/<pid>/exe` ends in ` (deleted)`: a process whose
//!     binary was replaced on disk, e.g. a build from before these files
//!     existed.
//!
//! Everything on the `--version` side is best effort: any error reads as
//! "nothing to report", so the first line is always printed.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The version every binary of this workspace reports: `0.3.0` on a release
/// tag, `0.3.0+4.gabc1234[.dirty]` on a checkout (see `build.rs`).
pub const VERSION: &str = env!("PIPEWIRE_CRISP_VOCALS_VERSION");

/// The long-running processes of `pipewire-crisp-vocals.service`, by
/// executable name.
pub const DAEMONS: [&str; 2] = ["crisp-vocals", "crisp-links"];

const PROC: &str = "/proc";
/// What the kernel appends to `/proc/<pid>/exe` once the file is unlinked.
const DELETED_SUFFIX: &str = " (deleted)";

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

fn version_file_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.version"))
}

/// One daemon's version file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    version: String,
    pid: u32,
    /// `None` when the daemon could not read its own start time; the PID
    /// and executable checks still apply.
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

/// A daemon's version file; removed again when dropped (clean exit).
#[derive(Debug)]
pub struct VersionFile {
    path: PathBuf,
    pid: u32,
}

impl Drop for VersionFile {
    fn drop(&mut self) {
        // Only remove our own record: a second instance of the same daemon
        // may have replaced the file since.
        let ours =
            fs::read_to_string(&self.path).ok().and_then(|t| Record::parse(&t)).is_some_and(|r| r.pid == self.pid);
        if ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_version_file_in(dir: &Path, proc_root: &Path, name: &str, version: &str) -> io::Result<VersionFile> {
    fs::create_dir_all(dir)?;
    let pid = std::process::id();
    let record = Record { version: version.to_string(), pid, start_time: start_time(proc_root, pid) };
    let path = version_file_path(dir, name);
    // Temp file + rename, so a concurrent `--version` never reads half a file.
    let tmp = dir.join(format!(".{name}.version.{pid}"));
    if let Err(e) = fs::write(&tmp, record.encode()).and_then(|()| fs::rename(&tmp, &path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(VersionFile { path, pid })
}

/// Write `<dir>/<name>.version` for the current process.
///
/// # Errors
///
/// When the directory cannot be created or the file cannot be written.
pub fn write_version_file(dir: &Path, name: &str, version: &str) -> io::Result<VersionFile> {
    write_version_file_in(dir, Path::new(PROC), name, version)
}

/// Daemon startup: record this process's version for `--version` to find.
///
/// Keep the returned guard alive for as long as the daemon runs. Failing to
/// write the file is logged and otherwise ignored; it must never stop the
/// audio chain from starting.
#[must_use]
pub fn announce(name: &str) -> Option<VersionFile> {
    let Some(dir) = runtime_dir() else {
        eprintln!("[{name}] no XDG_RUNTIME_DIR; `--version` cannot tell which build is running");
        return None;
    };
    match write_version_file(&dir, name, VERSION) {
        Ok(file) => Some(file),
        Err(e) => {
            eprintln!("[{name}] could not write {}: {e}", version_file_path(&dir, name).display());
            None
        }
    }
}

/// The version recorded by the running daemon `name`, if its record is
/// present and still describes a live process of that name.
fn running_version(dir: &Path, proc_root: &Path, name: &str) -> Option<String> {
    let record = Record::parse(&fs::read_to_string(version_file_path(dir, name)).ok()?)?;
    let (exe, _deleted) = exe_of(proc_root, record.pid)?;
    if exe != name {
        return None;
    }
    if record.start_time.is_some() && record.start_time != start_time(proc_root, record.pid) {
        return None;
    }
    Some(record.version)
}

/// Is some other process running `name` from an executable that has been
/// deleted or replaced on disk?
fn deleted_exe_running(proc_root: &Path, name: &str) -> bool {
    let own_pid = std::process::id();
    let Ok(entries) = fs::read_dir(proc_root) else {
        return false;
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != own_pid)
        .any(|pid| exe_of(proc_root, pid).is_some_and(|(exe, deleted)| deleted && exe == name))
}

/// Why the running service counts as out of date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stale {
    /// A daemon reports this version, which differs from ours.
    Version(String),
    /// A daemon runs from a replaced executable and left no usable record.
    OlderBuild,
}

/// Compare the running daemons with `own_version`. `None` when nothing is
/// running or everything running matches.
fn stale_daemon_in(dir: Option<&Path>, proc_root: &Path, own_version: &str) -> Option<Stale> {
    let mut older_build = false;
    for name in DAEMONS {
        match dir.and_then(|dir| running_version(dir, proc_root, name)) {
            Some(version) if version != own_version => return Some(Stale::Version(version)),
            Some(_) => {}
            None => older_build |= deleted_exe_running(proc_root, name),
        }
    }
    older_build.then_some(Stale::OlderBuild)
}

/// The running service's state relative to this binary.
#[must_use]
pub fn stale_daemon() -> Option<Stale> {
    stale_daemon_in(runtime_dir().as_deref(), Path::new(PROC), VERSION)
}

/// The `--version` text: `<bin> <version>`, plus a second line when the
/// running service is out of date. No trailing newline.
#[must_use]
pub fn format_version(bin: &str, version: &str, stale: Option<&Stale>) -> String {
    let first = format!("{bin} {version}");
    match stale {
        None => first,
        Some(Stale::Version(running)) => format!("{first}\n(daemon out-of-date, running {running})"),
        Some(Stale::OlderBuild) => format!("{first}\n(daemon out-of-date: running an older build)"),
    }
}

/// Print the `--version` output for `bin`. Never fails: a closed stdout is
/// ignored rather than panicking.
pub fn print_version(bin: &str) {
    let text = format_version(bin, VERSION, stale_daemon().as_ref());
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
    /// one for a replaced binary, may point at a path that does not exist)
    /// and a `stat` line carrying `start_time`.
    fn fake_process(proc_root: &Path, pid: u32, exe: &str, start_time: u64) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        symlink(exe, dir.join("exe")).unwrap();
        // The command name deliberately contains ") " to exercise the parser.
        let stat = format!("{pid} (odd) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 {start_time} 20 21\n");
        fs::write(dir.join("stat"), stat).unwrap();
    }

    fn write_record(dir: &Path, name: &str, record: &Record) {
        fs::create_dir_all(dir).unwrap();
        fs::write(version_file_path(dir, name), record.encode()).unwrap();
    }

    fn record(version: &str, pid: u32, start_time: Option<u64>) -> Record {
        Record { version: version.to_string(), pid, start_time }
    }

    #[test]
    fn version_alone_when_nothing_is_stale() {
        assert_eq!(format_version("crisp-vocals", "0.3.0+4.gabc1234", None), "crisp-vocals 0.3.0+4.gabc1234");
    }

    #[test]
    fn second_line_names_the_running_version() {
        let stale = Stale::Version("0.2.9+1.gdef5678".to_string());
        assert_eq!(
            format_version("crisp-vocals", "0.3.0+4.gabc1234", Some(&stale)),
            "crisp-vocals 0.3.0+4.gabc1234\n(daemon out-of-date, running 0.2.9+1.gdef5678)"
        );
    }

    #[test]
    fn second_line_for_an_unidentified_older_build() {
        assert_eq!(
            format_version("crisp-links", "0.3.0", Some(&Stale::OlderBuild)),
            "crisp-links 0.3.0\n(daemon out-of-date: running an older build)"
        );
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
        fake_process(&proc_root, pid, "/usr/bin/crisp-vocals", 4242);

        let file = write_version_file_in(&run, &proc_root, "crisp-vocals", "0.3.0+4.gabc1234").unwrap();
        let path = version_file_path(&run, "crisp-vocals");
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("0.3.0+4.gabc1234\n{pid}\n4242\n"));
        assert_eq!(running_version(&run, &proc_root, "crisp-vocals").as_deref(), Some("0.3.0+4.gabc1234"));
        // Only the version file itself is left in the directory (no temp file).
        assert_eq!(fs::read_dir(&run).unwrap().count(), 1);

        drop(file);
        assert!(!path.exists());
        assert_eq!(running_version(&run, &proc_root, "crisp-vocals"), None);
    }

    #[test]
    fn drop_leaves_a_file_written_by_another_instance() {
        let tmp = TempDir::new("foreign");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        let file = write_version_file_in(&run, &proc_root, "crisp-links", "0.3.0").unwrap();
        let other = record("0.3.1", std::process::id() + 1, None);
        write_record(&run, "crisp-links", &other);

        drop(file);
        let left = fs::read_to_string(version_file_path(&run, "crisp-links")).unwrap();
        assert_eq!(Record::parse(&left), Some(other));
    }

    #[test]
    fn record_of_a_dead_or_reused_pid_is_ignored() {
        let tmp = TempDir::new("stale-record");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fs::create_dir_all(&proc_root).unwrap();

        // No such process.
        write_record(&run, "crisp-vocals", &record("0.2.9", 500, Some(10)));
        assert_eq!(running_version(&run, &proc_root, "crisp-vocals"), None);

        // PID now belongs to a different program.
        fake_process(&proc_root, 500, "/usr/bin/bash", 10);
        assert_eq!(running_version(&run, &proc_root, "crisp-vocals"), None);

        // PID belongs to a later crisp-vocals than the one that wrote the record.
        fake_process(&proc_root, 501, "/usr/bin/crisp-vocals", 99);
        write_record(&run, "crisp-vocals", &record("0.2.9", 501, Some(10)));
        assert_eq!(running_version(&run, &proc_root, "crisp-vocals"), None);

        // Same process: accepted, also when its binary was replaced on disk.
        write_record(&run, "crisp-vocals", &record("0.2.9", 501, Some(99)));
        assert_eq!(running_version(&run, &proc_root, "crisp-vocals").as_deref(), Some("0.2.9"));
        fake_process(&proc_root, 502, "/usr/bin/crisp-vocals (deleted)", 7);
        write_record(&run, "crisp-vocals", &record("0.2.9", 502, Some(7)));
        assert_eq!(running_version(&run, &proc_root, "crisp-vocals").as_deref(), Some("0.2.9"));
    }

    #[test]
    fn nothing_running_reports_nothing() {
        let tmp = TempDir::new("idle");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fs::create_dir_all(&proc_root).unwrap();
        fake_process(&proc_root, 40, "/usr/bin/bash", 1);
        assert_eq!(stale_daemon_in(Some(&run), &proc_root, "0.3.0"), None);
        assert_eq!(stale_daemon_in(None, &proc_root, "0.3.0"), None);
        // A missing /proc is not an error either.
        assert_eq!(stale_daemon_in(Some(&run), &tmp.path().join("absent"), "0.3.0"), None);
    }

    #[test]
    fn matching_daemons_report_nothing_and_a_differing_one_is_named() {
        let tmp = TempDir::new("compare");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fake_process(&proc_root, 1313, "/usr/bin/crisp-vocals", 5);
        fake_process(&proc_root, 1314, "/usr/bin/crisp-links", 6);
        write_record(&run, "crisp-vocals", &record("0.3.0+4.gabc1234", 1313, Some(5)));
        write_record(&run, "crisp-links", &record("0.3.0+4.gabc1234", 1314, Some(6)));
        assert_eq!(stale_daemon_in(Some(&run), &proc_root, "0.3.0+4.gabc1234"), None);

        // Exact string comparison: any difference counts, including `.dirty`.
        assert_eq!(
            stale_daemon_in(Some(&run), &proc_root, "0.3.0+4.gabc1234.dirty"),
            Some(Stale::Version("0.3.0+4.gabc1234".to_string()))
        );

        write_record(&run, "crisp-links", &record("0.2.9+1.gdef5678", 1314, Some(6)));
        assert_eq!(
            stale_daemon_in(Some(&run), &proc_root, "0.3.0+4.gabc1234"),
            Some(Stale::Version("0.2.9+1.gdef5678".to_string()))
        );
    }

    #[test]
    fn replaced_executable_without_a_record_is_an_older_build() {
        let tmp = TempDir::new("deleted");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fake_process(&proc_root, 1313, "/usr/bin/crisp-vocals (deleted)", 5);
        fake_process(&proc_root, 1314, "/usr/bin/crisp-links", 6);
        fs::create_dir_all(proc_root.join("self")).unwrap(); // non-numeric entries are skipped
        assert_eq!(stale_daemon_in(Some(&run), &proc_root, "0.3.0"), Some(Stale::OlderBuild));
        assert_eq!(stale_daemon_in(None, &proc_root, "0.3.0"), Some(Stale::OlderBuild));

        // A leftover record from a dead PID does not hide it.
        write_record(&run, "crisp-vocals", &record("0.2.0", 9999, Some(1)));
        assert_eq!(stale_daemon_in(Some(&run), &proc_root, "0.3.0"), Some(Stale::OlderBuild));

        // An unrelated program with a deleted executable is not our daemon.
        let other = TempDir::new("deleted-other");
        fake_process(other.path(), 77, "/usr/bin/firefox (deleted)", 5);
        assert_eq!(stale_daemon_in(None, other.path(), "0.3.0"), None);
    }

    #[test]
    fn a_known_version_wins_over_an_unidentified_older_build() {
        let tmp = TempDir::new("both");
        let (run, proc_root) = (tmp.path().join("run"), tmp.path().join("proc"));
        fake_process(&proc_root, 1313, "/usr/bin/crisp-vocals (deleted)", 5);
        fake_process(&proc_root, 1314, "/usr/bin/crisp-links (deleted)", 6);
        write_record(&run, "crisp-links", &record("0.2.9", 1314, Some(6)));
        assert_eq!(stale_daemon_in(Some(&run), &proc_root, "0.3.0"), Some(Stale::Version("0.2.9".to_string())));
    }

    #[test]
    fn reads_the_real_proc_for_this_process() {
        let pid = std::process::id();
        assert!(start_time(Path::new(PROC), pid).is_some());
        let (_exe, deleted) = exe_of(Path::new(PROC), pid).expect("own exe link");
        assert!(!deleted);
    }
}
