// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's own: an OpenBao dev server, started for a test (TASK-411.16): [`DevServer`].

use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The environment variable naming the `bao` binary to run, if not the one on `PATH`.
pub const BAO_ENV: &str = "CLADEFS_BAO";

/// The environment variable that, set (to anything but empty or `0`), has a test without `bao`
/// fail rather than skip: CI's, which installs it (scripts/test-openbao.sh).
pub const REQUIRE_ENV: &str = "CLADEFS_REQUIRE_OPENBAO";

/// How long a dev server may take to start answering.
const READY_WITHIN: Duration = Duration::from_secs(30);

/// How often a starting server is asked whether it is ready.
const READY_POLL: Duration = Duration::from_millis(20);

/// `bao server -dev`, OpenBao (MPL-2.0) in memory, unsealed, on a free port of 127.0.0.1, its root
/// token random; with `-dev-tls`, over TLS, its CA in a file of its own ([`DevServer::ca`]). It
/// stores no token for the user (`-dev-no-store-token`), and is killed when dropped. Its CLI,
/// [`DevServer::bao`], does what a tenant or an operator does out of band: making, rotating,
/// trimming and deleting keys, writing policies, making tokens.
pub struct DevServer {
    child: Child,
    bao: PathBuf,
    port: u16,
    root: String,
    tls: bool,
    dir: tempfile::TempDir,
}

impl DevServer {
    /// A dev server over HTTP, started: none if there is no `bao` to run, which is said, unless
    /// [`REQUIRE_ENV`] is set, which fails.
    pub fn start() -> Option<Self> {
        Self::start_with(false)
    }

    /// A dev server over TLS, as [`DevServer::start`] starts one.
    pub fn start_tls() -> Option<Self> {
        Self::start_with(true)
    }

    fn start_with(tls: bool) -> Option<Self> {
        let Some(bao) = binary() else {
            let why = format!("no OpenBao: neither {BAO_ENV} nor PATH names a bao binary");
            let required = std::env::var(REQUIRE_ENV).is_ok_and(|v| !v.is_empty() && v != "0");
            assert!(!required, "{why}, and {REQUIRE_ENV} is set");
            eprintln!("skipped: {why} (brew install openbao, or scripts/test-openbao.sh)");
            return None;
        };
        // A port found free may be taken before the server binds it: another, then.
        let mut failures = Vec::new();
        for _ in 0..3 {
            match Self::spawn(&bao, tls) {
                Ok(server) => return Some(server),
                Err(why) => failures.push(why),
            }
        }
        panic!("bao server -dev did not start:\n{}", failures.join("\n"));
    }

    fn spawn(bao: &Path, tls: bool) -> Result<Self, String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.local_addr())
            .map_err(|err| err.to_string())?
            .port();
        let root = format!("root-{}", hex(crate::Secret::random().expose()));
        let log = std::fs::File::create(dir.path().join("bao.log")).map_err(|e| e.to_string())?;
        let mut command = Command::new(bao);
        command
            .args(["server", "-dev", "-dev-no-store-token"])
            .arg(format!("-dev-root-token-id={root}"))
            .arg(format!("-dev-listen-address=127.0.0.1:{port}"))
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|err| err.to_string())?)
            .stderr(log);
        if tls {
            command.arg("-dev-tls").arg(format!("-dev-tls-cert-dir={}", dir.path().display()));
        }
        scrubbed(&mut command);
        let child = command.spawn().map_err(|err| format!("{}: {err}", bao.display()))?;
        let mut server = Self { child, bao: bao.to_owned(), port, root, tls, dir };
        server.ready()?;
        Ok(server)
    }

    /// Waits until the server answers, unsealed, or fails, saying its log.
    fn ready(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + READY_WITHIN;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("it exited, {status}:\n{}", self.log()));
            }
            if self.ca_ready() && self.try_bao(&self.root, &["status"]).is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!("not ready within {READY_WITHIN:?}:\n{}", self.log()));
            }
            std::thread::sleep(READY_POLL);
        }
    }

    /// Whether the CA its TLS needs is written, if it needs one.
    fn ca_ready(&self) -> bool {
        !self.tls || self.ca().exists()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("bao.log")).unwrap_or_default()
    }

    /// Its port, on 127.0.0.1.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Its root token.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// The file of the CA its TLS certificate is signed by, if over TLS.
    pub fn ca(&self) -> PathBuf {
        self.dir.path().join("vault-ca.pem")
    }

    /// A directory of its own, for files a test needs: tokens', overrides'.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// The URL of an [`crate::kms::openbao::OpenBao`] of its Transit engine at `mount`, `query`
    /// its query (`key=...`), and its CA's, over TLS.
    pub fn url(&self, mount: &str, query: &str) -> String {
        match self.tls {
            false => format!("openbao://127.0.0.1:{}/{mount}?{query}", self.port),
            true => format!(
                "openbaos://127.0.0.1:{}/{mount}?{query}&ca={}",
                self.port,
                self.ca().display()
            ),
        }
    }

    /// Runs `bao ARGS` against it as root: its output, trimmed. It fails, saying why, if `bao`
    /// does.
    pub fn bao(&self, args: &[&str]) -> String {
        self.try_bao(&self.root, args).unwrap_or_else(|why| panic!("bao {args:?}: {why}"))
    }

    /// Runs `bao ARGS` against it with `token`: its output, trimmed, or what it said failing.
    pub fn try_bao(&self, token: &str, args: &[&str]) -> Result<String, String> {
        let mut command = Command::new(&self.bao);
        command.args(args).stdin(Stdio::null());
        scrubbed(&mut command);
        let scheme = if self.tls { "https" } else { "http" };
        command
            .env("BAO_ADDR", format!("{scheme}://127.0.0.1:{}", self.port))
            .env("BAO_TOKEN", token);
        if self.tls {
            command.env("BAO_CACERT", self.ca());
        }
        let output = command.output().map_err(|err| err.to_string())?;
        match output.status.success() {
            true => Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned()),
            false => Err(format!(
                "{}: {}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )),
        }
    }

    /// Enables a Transit engine at `mount`.
    pub fn transit(&self, mount: &str) {
        self.bao(&["secrets", "enable", &format!("-path={mount}"), "transit"]);
    }

    /// Writes the policy `name`, `hcl`.
    pub fn policy(&self, name: &str, hcl: &str) {
        let path = self.dir.path().join(format!("{name}.hcl"));
        std::fs::write(&path, hcl).unwrap();
        self.bao(&["policy", "write", name, path.to_str().unwrap()]);
    }

    /// A new token of the policy `name` (and `default`'s).
    pub fn token(&self, policy: &str) -> String {
        self.bao(&["token", "create", &format!("-policy={policy}"), "-field=token"])
    }

    /// What cladefs's token needs, of the keys matching `keys` (a name or a glob) of the Transit
    /// engine at `mount`, as [`crate::kms::openbao::OpenBao`] says: and no `create`.
    pub fn cladefs_policy(mount: &str, keys: &str) -> String {
        format!(
            "path \"{mount}/encrypt/{keys}\" {{ capabilities = [\"update\"] }}\n\
             path \"{mount}/decrypt/{keys}\" {{ capabilities = [\"update\"] }}\n\
             path \"{mount}/keys/{keys}\" {{ capabilities = [\"read\"] }}\n"
        )
    }
}

impl Drop for DevServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `bao` binary to run: [`BAO_ENV`]'s, else the first on `PATH`.
fn binary() -> Option<PathBuf> {
    if let Some(bao) = std::env::var_os(BAO_ENV).filter(|bao| !bao.is_empty()) {
        return Some(bao.into());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join("bao")).find(|bao| bao.is_file())
}

/// Leaves out of `command`'s environment what would point the CLI or the server elsewhere.
fn scrubbed(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("BAO_") || name.starts_with("VAULT_") {
            command.env_remove(&*name);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
