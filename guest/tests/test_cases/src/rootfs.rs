// Copyright The libkrun Authors.
// Copyright 2026 The cladefs Authors: changed for cladefs's guest tests.
// SPDX-License-Identifier: Apache-2.0

//! Podman-based rootfs provisioning for tests that need a full Linux rootfs.
//!
//! `prepare_rootfs` builds a podman image from a Containerfile string, creates a container,
//! and pipes `podman export` directly into `tar -x` to populate the destination directory.
//! The image tag is derived from the hash of the Containerfile content (`cladefs-test-<hash>`),
//! so podman's layer cache makes rebuilds fast when the Containerfile hasn't changed.
//! cladefs: or Docker's, where podman doesn't run, as on macOS ([`engine`]).
//!
//! cladefs: and the files its build takes from the host ([`Download`]), each fetched once into the
//! host's cache, checked by its sha256, and given to the build in a context directory of its own
//! (TASK-297); and the files it takes from the case itself ([`ContextFile`], TASK-665).

use anyhow::{Context, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// cladefs: a root filesystem's image: its Containerfile, and the files its build takes from the
/// host and from the case, which it copies in by their names (`COPY NAME ...`).
#[derive(Clone, Copy)]
pub struct Image {
    pub containerfile: &'static str,
    pub downloads: &'static [Download],
    pub files: &'static [ContextFile],
}

impl Image {
    /// The image `containerfile` builds, taking nothing from the host.
    pub const fn new(containerfile: &'static str) -> Self {
        Self { containerfile, downloads: &[], files: &[] }
    }
}

/// cladefs: a file of the case's own an image's build takes, such as a project's manifest and
/// lock file, written into its context by its name (TASK-665).
#[derive(Clone, Copy)]
pub struct ContextFile {
    pub name: &'static str,
    pub contents: &'static str,
}

/// cladefs: a file an image's build takes from the host, fetched into the host's cache at most once
/// ([`cache_dir`]), and checked by its sha256 as it is fetched and each time it is used: from the
/// first of its sources that serves it, each tried again before the next. A build that fetched
/// it from one host, each time, failed whenever that host was slow or down, skipping every case
/// on its image (lua.org, TASK-297).
#[derive(Clone, Copy)]
pub struct Download {
    /// Its name in the cache and in the build's context.
    pub name: &'static str,
    pub sha256: &'static str,
    /// Its URLs, in the order they are tried.
    pub sources: &'static [&'static str],
}

/// cladefs: the host's cache of downloads, CLADEFS_DOWNLOADS, or `cladefs/downloads` in the user's cache
/// directory (XDG_CACHE_HOME, or ~/.cache), as scripts/test-e2e.sh keeps Docker's.
fn cache_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLADEFS_DOWNLOADS") {
        return Ok(dir.into());
    }
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(cache) => PathBuf::from(cache),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".cache"),
    };
    Ok(cache.join("cladefs").join("downloads"))
}

/// The sha256 of `bytes`, in hexadecimal, as sha256sum(1) says it.
fn sha256(bytes: &[u8]) -> anyhow::Result<String> {
    let output = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(bytes)?;
            child.wait_with_output()
        })
        .context("sha256sum")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("sha256sum failed: {stderr}");
    }
    let hash = String::from_utf8(output.stdout).context("sha256sum output not utf-8")?;
    // sha256sum outputs "<hash>  -\n"; the hash always starts at position 0.
    Ok(hash.get(..64).context("sha256sum output too short")?.to_owned())
}

/// The sha256 of the file at `path`.
fn file_sha256(path: &Path) -> anyhow::Result<String> {
    sha256(&fs::read(path).with_context(|| format!("{}", path.display()))?)
}

impl Download {
    /// The file in `cache`, fetched there if it isn't, or isn't as its sha256 says.
    fn fetched(&self, cache: &Path) -> anyhow::Result<PathBuf> {
        let path = cache.join(self.name);
        if path.exists() && file_sha256(&path)? == self.sha256 {
            return Ok(path);
        }
        fs::create_dir_all(cache).with_context(|| format!("{}", cache.display()))?;
        // A name of this process's, renamed into place once whole and checked: a fetch elsewhere,
        // another runner's on this host, may be writing the file too.
        let partial = cache.join(format!("{}.{}.partial", self.name, std::process::id()));
        let mut failures = Vec::new();
        for source in self.sources {
            match self.fetch(source, &partial) {
                Ok(()) => {
                    fs::rename(&partial, &path).with_context(|| format!("{}", path.display()))?;
                    return Ok(path);
                }
                Err(err) => failures.push(format!("{source}: {err:#}")),
            }
        }
        let _ = fs::remove_file(&partial);
        bail!("{}: no source served it: {}", self.name, failures.join("; "))
    }

    /// Fetches `source` to `to`, tried again on any failure, a moment between, and checks it.
    fn fetch(&self, source: &str, to: &Path) -> anyhow::Result<()> {
        let output = Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args(["--retry", "2", "--retry-all-errors", "--retry-delay", "1"])
            .args(["--connect-timeout", "15", "--max-time", "120", "--output"])
            .arg(to)
            .arg(source)
            .stdin(Stdio::null())
            .output()
            .context("curl")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("curl: {}: {}", output.status, stderr.trim());
        }
        let sha256 = file_sha256(to)?;
        if sha256 != self.sha256 {
            bail!("its sha256 is {sha256}, not {}", self.sha256);
        }
        Ok(())
    }
}

/// The tag of `image`: of its Containerfile, its downloads' names and hashes, and its own files'
/// names and hashes.
fn image_tag(image: &Image) -> anyhow::Result<String> {
    let mut described = image.containerfile.to_owned();
    for download in image.downloads {
        described += &format!("\n{} {}", download.name, download.sha256);
    }
    for file in image.files {
        described += &format!("\n{} {}", file.name, sha256(file.contents.as_bytes())?);
    }
    Ok(format!("cladefs-test-{}", &sha256(described.as_bytes())?[..16]))
}

/// cladefs: the container engine images are built with: CLADEFS_CONTAINER_ENGINE if set, else
/// podman or docker, whichever answers, podman's first, as the test hosts build with it; one
/// installed but not running, as a podman whose machine is stopped on macOS, passed over
/// (scripts/lib/containers.sh, TASK-149.20).
fn engine() -> anyhow::Result<String> {
    if let Some(engine) = std::env::var("CLADEFS_CONTAINER_ENGINE").ok().filter(|e| !e.is_empty()) {
        return Ok(engine);
    }
    let answers = |engine: &str| {
        let mut info = Command::new(engine);
        info.arg("info").stdout(Stdio::null()).stderr(Stdio::null());
        info.status().is_ok_and(|status| status.success())
    };
    let found = ["podman", "docker"].into_iter().find(|engine| answers(engine));
    Ok(found
        .context("neither podman nor docker runs here: start one, or set CLADEFS_CONTAINER_ENGINE")?
        .to_owned())
}

/// cladefs: fails unless `dir` is on a case-sensitive filesystem, as a Linux root's names need: on
/// macOS's case-insensitive APFS, Fedora's `terminfo/E/Eterm` and `terminfo/e/eterm` are one file,
/// the root extracted there not the image's (scripts/test-e2e.sh's results are on a case-sensitive
/// volume there).
#[cfg(target_os = "macos")]
fn case_sensitive(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: Reads a property of the path's filesystem, the path a C string that outlives it.
    let sensitive = unsafe { nix::libc::pathconf(path.as_ptr(), nix::libc::_PC_CASE_SENSITIVE) };
    anyhow::ensure!(
        sensitive >= 0,
        "whether {} is case-sensitive: {}",
        dir.display(),
        std::io::Error::last_os_error()
    );
    anyhow::ensure!(
        sensitive == 1,
        "{} is on a case-insensitive filesystem, where a Linux root's names collide: extract it \
         on a case-sensitive one",
        dir.display()
    );
    Ok(())
}

/// As macOS's: Linux's filesystems are case-sensitive.
#[cfg(not(target_os = "macos"))]
fn case_sensitive(_dir: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Builds a podman image from `image`, creates a container, and extracts
/// its filesystem directly into `dest` (no intermediate tarball).
///
/// Returns an error if no container engine runs ([`engine`]) or the build/export fails.
pub fn prepare_rootfs(image: &Image, dest: &Path) -> anyhow::Result<()> {
    let engine = engine()?;
    let engine = engine.as_str();

    let tag = image_tag(image)?;
    let containerfile = image.containerfile;
    // cladefs: the build's context, its downloads alone, from the host's cache, rather than this
    // process's directory, whatever it held.
    let context = tempfile::Builder::new()
        .prefix("cladefs-rootfs-context")
        .tempdir()
        .context("the build's context")?;
    let cache = cache_dir()?;
    for download in image.downloads {
        let cached = download.fetched(&cache)?;
        fs::copy(&cached, context.path().join(download.name))
            .with_context(|| format!("{}", cached.display()))?;
    }
    for file in image.files {
        fs::write(context.path().join(file.name), file.contents)
            .with_context(|| format!("writing {} into the build's context", file.name))?;
    }

    // Build image (podman layer cache makes this fast when unchanged)
    let mut build = Command::new(engine)
        .args(["build", "-t", &tag, "-f", "-"])
        .arg(context.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {engine} build"))?;

    build
        .stdin
        .take()
        .unwrap()
        .write_all(containerfile.as_bytes())
        .with_context(|| format!("writing containerfile to {engine} stdin"))?;

    let output = build.wait_with_output().with_context(|| format!("waiting for {engine} build"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{engine} build failed: {stderr}");
    }

    // Create a container from the image
    let create_out = Command::new(engine)
        .args(["create", &tag])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("{engine} create"))?;

    if !create_out.status.success() {
        let stderr = String::from_utf8_lossy(&create_out.stderr);
        bail!("{engine} create failed: {stderr}");
    }

    let ctr_id =
        String::from_utf8(create_out.stdout).context("container id not utf-8")?.trim().to_string();

    // Pipe podman export directly into tar extract
    fs::create_dir_all(dest).context("creating rootfs destination directory")?;
    case_sensitive(dest)?;

    let mut export = Command::new(engine)
        .args(["export", &ctr_id])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{engine} export"))?;

    let export_stdout = export.stdout.take().unwrap();

    let tar_status = Command::new("tar")
        .args(["-x", "--no-same-owner", "-C"])
        .arg(dest)
        .stdin(export_stdout)
        .status()
        .with_context(|| format!("tar extract from {engine} export"))?;

    let export_out =
        export.wait_with_output().with_context(|| format!("waiting for {engine} export"))?;

    if !export_out.status.success() {
        bail!("{engine} export failed");
    }
    if !tar_status.success() {
        bail!("tar extraction failed");
    }
    // cladefs: on macOS, every file readable by its owner, this process's user, and every directory
    // searchable: what `mkfs.ext4 -d` copies into the root's image must be, a root's mode-0 files
    // (Fedora's /etc/shadow) among them, which on Linux the case reads as its user namespace's root
    // (the runner's `buildah unshare`), whom no mode stops (board 14336).
    if cfg!(target_os = "macos") {
        let status = Command::new("chmod").args(["-R", "u+rX"]).arg(dest).status();
        let status = status.with_context(|| format!("chmod {}", dest.display()))?;
        if !status.success() {
            bail!("making {} readable failed: {status}", dest.display());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A download of `bytes`' hash from `sources`, for a test's life (the process's).
    fn lua(bytes: &[u8], sources: &[&str]) -> Download {
        let sources: Vec<&'static str> =
            sources.iter().map(|url| &*url.to_string().leak()).collect();
        Download {
            name: "lua.tar.gz",
            sha256: sha256(bytes).unwrap().leak(),
            sources: sources.leak(),
        }
    }

    /// A file of `bytes` at `dir`/`name`, and its URL.
    fn served(dir: &Path, name: &str, bytes: &[u8]) -> String {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        format!("file://{}", path.display())
    }

    /// The URL of a port of this host's that no one listens on: one just let go.
    fn unreachable() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/lua.tar.gz", listener.local_addr().unwrap())
    }

    /// The names in `dir`.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = (fs::read_dir(dir).unwrap())
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// TASK-297: a source that can't be reached, as lua.org couldn't, is passed over for the next,
    /// and what that serves is cached, checked by its sha256.
    #[test]
    fn a_download_comes_from_the_first_source_that_serves_it() {
        let dir = tempfile::Builder::new().prefix("downloads").tempdir().unwrap();
        let mirror = served(dir.path(), "mirrored", b"lua");
        let cache = dir.path().join("cache");
        let fetched = lua(b"lua", &[&unreachable(), &mirror]).fetched(&cache).unwrap();
        assert_eq!(fetched, cache.join("lua.tar.gz"));
        assert_eq!(fs::read(&fetched).unwrap(), b"lua");
        assert_eq!(names(&cache), ["lua.tar.gz"]);
    }

    /// Once cached, a download is taken from the cache with every source unreachable, and fetched
    /// again only if the cache's copy isn't as its sha256 says.
    #[test]
    fn a_cached_download_needs_no_source() {
        let dir = tempfile::Builder::new().prefix("downloads").tempdir().unwrap();
        let cache = dir.path().join("cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("lua.tar.gz"), b"lua").unwrap();
        let unreachable = unreachable();
        let download = lua(b"lua", &[&unreachable]);
        assert_eq!(fs::read(download.fetched(&cache).unwrap()).unwrap(), b"lua");

        fs::write(cache.join("lua.tar.gz"), b"lu").unwrap();
        let err = format!("{:#}", download.fetched(&cache).unwrap_err());
        let refused =
            format!("lua.tar.gz: no source served it: {unreachable}: curl: exit status: 7: ");
        assert!(err.starts_with(&refused), "{err}");
        assert_eq!(names(&cache), ["lua.tar.gz"]);
    }

    /// A source serving other bytes than its sha256 says is passed over, and with no other, the
    /// download fails, saying what each source served.
    #[test]
    fn a_source_serving_other_bytes_is_passed_over() {
        let dir = tempfile::Builder::new().prefix("downloads").tempdir().unwrap();
        let wrong = served(dir.path(), "wrong", b"luna");
        let right = served(dir.path(), "right", b"lua");
        let cache = dir.path().join("cache");
        let err = lua(b"lua", &[&wrong]).fetched(&cache).unwrap_err();
        let (lua_hash, luna) = (sha256(b"lua").unwrap(), sha256(b"luna").unwrap());
        assert_eq!(
            format!("{err:#}"),
            format!(
                "lua.tar.gz: no source served it: {wrong}: its sha256 is {luna}, not {lua_hash}"
            )
        );
        assert_eq!(names(&cache), Vec::<String>::new());
        let fetched = lua(b"lua", &[&wrong, &right]).fetched(&cache).unwrap();
        assert_eq!(fs::read(fetched).unwrap(), b"lua");
    }
}
