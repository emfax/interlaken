//! A LazyFS mount for power-loss tests (ADR-0009 phase 7).
//!
//! [LazyFS](https://github.com/dsrhaslab/lazyfs) is a FUSE filesystem that
//! keeps written data in its own page cache until the application syncs it.
//! Its `lazyfs::clear-cache` command drops everything unsynced, which is
//! what a power cut does to the kernel page cache. File creation, renames
//! and deletes go straight to the backing directory, so LazyFS loses file
//! *contents* but not directory entries.
//!
//! `LAZYFS_ROOT` names a LazyFS checkout built with its own `build.sh`
//! scripts (`scripts/build-lazyfs.sh` does this without root). The mount
//! runs as the current user without `allow_other`, so `/etc/fuse.conf`
//! needs no change; it needs `/dev/fuse` and `fusermount3`.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The environment variable naming the LazyFS checkout.
pub const LAZYFS_ROOT: &str = "LAZYFS_ROOT";

/// A running LazyFS mount, unmounted on drop.
pub struct LazyFs {
    /// The FUSE process.
    process: Child,
    /// Where the filesystem is mounted.
    mount: PathBuf,
    /// The FIFO LazyFS reads commands from.
    commands: PathBuf,
    /// The FIFO LazyFS reports finished commands on.
    completed: PathBuf,
    /// Holds the backing directory, mount point, FIFOs, config and log.
    _work: tempfile::TempDir,
}

impl LazyFs {
    /// Mount LazyFS from the checkout in `LAZYFS_ROOT`, or return `None` if it is unset.
    ///
    /// # Panics
    ///
    /// Panics if the checkout has no built `lazyfs` binary or the mount does
    /// not appear within 30 seconds.
    pub fn from_env() -> Option<Self> {
        let root = std::env::var_os(LAZYFS_ROOT)?;
        Some(Self::mount(Path::new(&root)))
    }

    /// Mount LazyFS from the checkout at `root` over a fresh temporary directory.
    ///
    /// # Panics
    ///
    /// Panics if the checkout has no built `lazyfs` binary or the mount does
    /// not appear within 30 seconds.
    pub fn mount(root: &Path) -> Self {
        let binary = root.join("lazyfs/build/lazyfs");
        assert!(
            binary.is_file(),
            "{} not found; build LazyFS with scripts/build-lazyfs.sh",
            binary.display()
        );
        let work = tempfile::Builder::new()
            .prefix("lazyfs-")
            .tempdir()
            .unwrap();
        let backing = work.path().join("backing");
        let mount = work.path().join("mnt");
        std::fs::create_dir(&backing).unwrap();
        std::fs::create_dir(&mount).unwrap();
        let commands = work.path().join("faults.fifo");
        let completed = work.path().join("completed.fifo");
        let config = work.path().join("lazyfs.toml");
        let cache = std::env::var("LAZYFS_CACHE").unwrap_or_else(|_| "512mb".into());
        std::fs::write(
            &config,
            format!(
                "[faults]\nfifo_path=\"{}\"\nfifo_path_completed=\"{}\"\n\
                 [cache]\napply_eviction=false\n\
                 [cache.simple]\ncustom_size=\"{cache}\"\nblocks_per_page=1\n\
                 [filesystem]\nlog_all_operations=false\nlogfile=\"{}\"\n",
                commands.display(),
                completed.display(),
                work.path().join("lazyfs.log").display(),
            ),
        )
        .unwrap();
        let process = Command::new(&binary)
            .arg(&mount)
            .arg("--config-path")
            .arg(&config)
            .args(["-o", "modules=subdir", "-o"])
            .arg(format!("subdir={}", backing.display()))
            .arg("-f")
            .env("LD_LIBRARY_PATH", root.join("libs/libpcache/build"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(work.path().join("lazyfs.stderr")).unwrap())
            .spawn()
            .unwrap();
        let lazyfs = Self {
            process,
            mount,
            commands,
            completed,
            _work: work,
        };
        lazyfs.await_mounted();
        lazyfs
    }

    /// Return the mounted directory.
    pub fn path(&self) -> &Path {
        &self.mount
    }

    /// Drop every write not yet synced, as a power cut would, and wait until LazyFS has.
    ///
    /// # Panics
    ///
    /// Panics if the command FIFOs can't be opened or LazyFS reports
    /// something other than completion.
    pub fn clear_cache(&self) {
        let mut fifo = OpenOptions::new().write(true).open(&self.commands).unwrap();
        fifo.write_all(b"lazyfs::clear-cache\n").unwrap();
        drop(fifo);
        // LazyFS opens the completion FIFO for writing once the cache is clear.
        let done = OpenOptions::new().read(true).open(&self.completed).unwrap();
        let mut line = String::new();
        BufReader::new(done).read_line(&mut line).unwrap();
        assert_eq!(
            line.trim(),
            "finished::clear-cache",
            "unexpected LazyFS reply"
        );
    }

    /// Wait until `/proc/mounts` lists the mount point.
    ///
    /// # Panics
    ///
    /// Panics if the LazyFS process exits or 30 seconds pass first.
    fn await_mounted(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let target = self.mount.to_string_lossy().into_owned();
        loop {
            let mounts = std::fs::read_to_string("/proc/mounts").unwrap();
            if mounts
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(target.as_str()))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "LazyFS did not mount {target} within 30 s"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for LazyFs {
    /// Unmount the filesystem and reap the FUSE process.
    fn drop(&mut self) {
        let unmounted = Command::new("fusermount3")
            .arg("-u")
            .arg(&self.mount)
            .status()
            .is_ok_and(|status| status.success());
        if !unmounted {
            let _ = Command::new("fusermount3")
                .arg("-uz")
                .arg(&self.mount)
                .status();
            let _ = self.process.kill();
        }
        let _ = self.process.wait();
    }
}
