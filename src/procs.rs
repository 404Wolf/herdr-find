//! Process inspection: which Herdr pane a process runs in, what it has open.

use std::fs;
use std::path::{Path, PathBuf};

/// Read-only view of the running processes.
///
/// Agent sources use this to map an agent process to its Herdr pane (via the
/// `HERDR_PANE_ID` in its environment) and to its transcript.
pub trait ProcessTable: Send + Sync {
    /// Whether this platform supports process inspection at all.
    fn supported(&self) -> bool;
    /// All visible process ids.
    fn pids(&self) -> Vec<u32>;
    /// One variable from the process's initial environment.
    fn env_var(&self, pid: u32, name: &str) -> Option<String>;
    /// Paths of the regular files the process has open.
    fn open_files(&self, pid: u32) -> Vec<PathBuf>;
    /// Opaque start time, used to detect a reused pid (procfs `starttime`).
    fn start_time(&self, pid: u32) -> Option<String>;
    /// Parent process id.
    fn parent(&self, pid: u32) -> Option<u32>;
    /// Command line (`argv`); empty when unknown.
    fn cmdline(&self, pid: u32) -> Vec<String>;

    /// The Herdr pane a process runs in.
    fn pane_of(&self, pid: u32) -> Option<String> {
        self.env_var(pid, "HERDR_PANE_ID").filter(|p| !p.is_empty())
    }
}

/// A procfs-backed process table (`/proc` on Linux, or any directory laid out
/// the same way, for tests).
#[derive(Debug, Clone)]
pub struct ProcFs {
    root: PathBuf,
    supported: bool,
}

impl ProcFs {
    /// The system process table. Only Linux has a procfs; elsewhere this
    /// reports itself unsupported and finds nothing.
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/proc"),
            supported: cfg!(target_os = "linux"),
        }
    }

    /// A procfs laid out under `root`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            supported: true,
        }
    }

    fn dir(&self, pid: u32) -> PathBuf {
        self.root.join(pid.to_string())
    }

    /// Fields of `/proc/<pid>/stat` after the `(comm)` field, which may itself
    /// contain spaces and parentheses. Index 0 is `state` (field 3).
    fn stat_fields(&self, pid: u32) -> Option<Vec<String>> {
        let stat = fs::read_to_string(self.dir(pid).join("stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        Some(rest.split_whitespace().map(String::from).collect())
    }
}

impl ProcessTable for ProcFs {
    fn supported(&self) -> bool {
        self.supported
    }

    fn pids(&self) -> Vec<u32> {
        if !self.supported {
            return Vec::new();
        }
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect()
    }

    fn env_var(&self, pid: u32, name: &str) -> Option<String> {
        if !self.supported {
            return None;
        }
        let env = fs::read(self.dir(pid).join("environ")).ok()?;
        let prefix = format!("{name}=");
        env.split(|b| *b == 0).find_map(|entry| {
            entry
                .strip_prefix(prefix.as_bytes())
                .map(|v| String::from_utf8_lossy(v).into_owned())
        })
    }

    fn open_files(&self, pid: u32) -> Vec<PathBuf> {
        if !self.supported {
            return Vec::new();
        }
        let Ok(fds) = fs::read_dir(self.dir(pid).join("fd")) else {
            return Vec::new();
        };
        fds.flatten()
            .filter_map(|fd| fs::read_link(fd.path()).ok())
            .filter(|p| p.is_absolute())
            .collect()
    }

    fn start_time(&self, pid: u32) -> Option<String> {
        // starttime is field 22; index 0 here is field 3.
        self.stat_fields(pid)?.get(22 - 3).cloned()
    }

    fn parent(&self, pid: u32) -> Option<u32> {
        // ppid is field 4.
        self.stat_fields(pid)?.get(4 - 3)?.parse().ok()
    }

    fn cmdline(&self, pid: u32) -> Vec<String> {
        if !self.supported {
            return Vec::new();
        }
        let Ok(raw) = fs::read(self.dir(pid).join("cmdline")) else {
            return Vec::new();
        };
        raw.split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }
}

/// Process ids to their children, for walking process trees.
#[derive(Debug, Default)]
pub struct Tree {
    children: std::collections::HashMap<u32, Vec<u32>>,
}

impl Tree {
    /// Snapshot the parent links of every visible process.
    pub fn new(procs: &dyn ProcessTable) -> Self {
        let mut children: std::collections::HashMap<u32, Vec<u32>> = Default::default();
        for pid in procs.pids() {
            if let Some(ppid) = procs.parent(pid) {
                children.entry(ppid).or_default().push(pid);
            }
        }
        children.values_mut().for_each(|c| c.sort_unstable());
        Self { children }
    }

    /// `root` and its descendants, breadth first (outer processes first).
    /// `descend` decides whether to look below a process.
    pub fn walk(&self, root: u32, mut descend: impl FnMut(u32) -> bool) -> Vec<u32> {
        let mut out = vec![root];
        let mut seen = std::collections::HashSet::from([root]);
        let mut i = 0;
        while i < out.len() {
            let pid = out[i];
            i += 1;
            if pid != root && !descend(pid) {
                continue;
            }
            for &child in self.children.get(&pid).into_iter().flatten() {
                if seen.insert(child) {
                    out.push(child);
                }
            }
        }
        out
    }
}

/// Whether `path` lies under `dir`, also comparing against `dir`'s canonical
/// form (procfs reports resolved paths when `dir` goes through a symlink).
pub fn is_under(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
        || dir
            .canonicalize()
            .is_ok_and(|canonical| path.starts_with(canonical))
}

#[cfg(test)]
pub(crate) mod fake {
    //! Build a fake procfs on disk for tests.
    use std::fs;
    use std::path::Path;

    /// Give `<root>/<pid>` a command line.
    pub fn cmdline(root: &Path, pid: u32, argv: &[&str]) {
        let raw: Vec<u8> = argv
            .iter()
            .flat_map(|a| format!("{a}\0").into_bytes())
            .collect();
        fs::write(root.join(pid.to_string()).join("cmdline"), raw).unwrap();
    }

    /// Create `<root>/<pid>` with an environ, a stat line and open files.
    pub fn process(root: &Path, pid: u32, ppid: u32, env: &[(&str, &str)], open: &[&Path]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(dir.join("fd")).unwrap();
        let environ: Vec<u8> = env
            .iter()
            .flat_map(|(k, v)| format!("{k}={v}\0").into_bytes())
            .collect();
        fs::write(dir.join("environ"), environ).unwrap();
        // Fields 3..=22; starttime (22) is 1000 + pid.
        let mut fields = vec!["S".to_string(), ppid.to_string()];
        fields.extend((5..22).map(|_| "0".to_string()));
        fields.push((1000 + pid).to_string());
        fs::write(
            dir.join("stat"),
            format!("{pid} (odd (name) x) {}\n", fields.join(" ")),
        )
        .unwrap();
        for (i, target) in open.iter().enumerate() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, dir.join("fd").join((i + 3).to_string())).unwrap();
            #[cfg(not(unix))]
            let _ = (i, target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_env_stat_and_fds() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("open.jsonl");
        fs::write(&file, "").unwrap();
        fake::process(
            tmp.path(),
            42,
            7,
            &[("A", "1"), ("HERDR_PANE_ID", "p_1")],
            &[&file],
        );
        let procs = ProcFs::at(tmp.path());
        assert_eq!(procs.pids(), vec![42]);
        assert_eq!(procs.pane_of(42).as_deref(), Some("p_1"));
        assert_eq!(procs.env_var(42, "A").as_deref(), Some("1"));
        assert_eq!(procs.env_var(42, "B"), None);
        assert_eq!(procs.parent(42), Some(7));
        assert_eq!(procs.start_time(42).as_deref(), Some("1042"));
        #[cfg(unix)]
        assert_eq!(procs.open_files(42), vec![file]);
        assert_eq!(procs.pane_of(43), None);
        assert!(procs.cmdline(42).is_empty());
        fake::cmdline(tmp.path(), 42, &["claude", "--resume", "x"]);
        assert_eq!(procs.cmdline(42), ["claude", "--resume", "x"]);
    }

    #[test]
    fn walks_process_trees_outer_first() {
        let tmp = tempfile::tempdir().unwrap();
        for (pid, ppid) in [(10, 1), (11, 10), (12, 10), (13, 11), (20, 1)] {
            fake::process(tmp.path(), pid, ppid, &[], &[]);
        }
        let tree = Tree::new(&ProcFs::at(tmp.path()));
        assert_eq!(tree.walk(10, |_| true), [10, 11, 12, 13]);
        assert_eq!(tree.walk(10, |p| p != 11), [10, 11, 12]);
        assert_eq!(tree.walk(99, |_| true), [99]);
    }
}
