//! `DbusFixture` — private `dbus-daemon` test harness.
//!
//! Phase 5 of the M3 milestone (M3-plan § "Phase 5", lines 643-725)
//! ships an end-to-end-via-bus test suite. The harness spawns a
//! private `dbus-daemon` against a temp-dir socket, then spawns the
//! `zwhisperd` binary against that bus, and exposes typed proxies the
//! tests can drive.
//!
//! ## Skip discipline (M0/M1 pattern)
//!
//! Hosts without `dbus-daemon` on `PATH` or without a session config
//! file (`/etc/dbus-1/session.conf` on Arch / Fedora; the
//! `/usr/share/dbus-1/session.conf` fallback for Debian-likes) get a
//! clean `[SKIP]` line — no panic, no flaky red on CI.
//!
//! ## C10 socket-readiness poll
//!
//! `dbus-daemon` returns from `--print-address` before its listening
//! socket exists on disk under load. The fixture polls
//! `std::fs::metadata(socket_path)` for up to 2 s in 20 ms slices and
//! fails the fixture (not the test) with a diagnostic if the socket
//! never appears (M3-plan correction C10).
//!
//! ## Why no global `set_var(DBUS_SESSION_BUS_ADDRESS)`
//!
//! Env vars are process-global and not test-isolated. Two parallel
//! tests would race on the variable. The fixture therefore exposes
//! `address()` and tests pass it explicitly to
//! `zbus::connection::Builder::address(...)`. The `zwhisperd` child
//! gets the variable through `Command::env`, which is per-child and
//! safe.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use zwhisper_ipc::{
    BUS_NAME, History1Proxy, Jobs1Proxy, OBJECT_PATH, Profiles1Proxy, Recorder1Proxy,
};

/// Reasons the fixture cannot run on this host. Tests map this to a
/// `[SKIP] {reason}` `eprintln!` and return early — same discipline
/// as the M0 PipeWire-skip + M1 whisper-cli-skip patterns.
#[derive(Debug)]
pub enum FixtureSkip {
    /// `dbus-daemon` not on PATH.
    NoDbusDaemon,
    /// Neither the Arch/Fedora nor the Debian-likes config file
    /// exists. The fixture probes both before skipping.
    NoDbusConfig,
    /// Anything else: socket never appears, daemon binary fails to
    /// claim the bus name, etc. Always carries a diagnostic so the
    /// `[SKIP]` line is actionable.
    Other(String),
}

impl std::fmt::Display for FixtureSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDbusDaemon => {
                write!(f, "dbus-daemon not on PATH; install dbus to run this test")
            }
            Self::NoDbusConfig => write!(
                f,
                "no dbus session.conf at /etc/dbus-1/session.conf or /usr/share/dbus-1/session.conf"
            ),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

/// Probes both well-known locations of `session.conf`. Arch/Fedora
/// keep it under `/etc/dbus-1/`; Debian-likes keep it under
/// `/usr/share/dbus-1/`. Returns the first one found.
fn locate_session_conf() -> Option<PathBuf> {
    for candidate in ["/etc/dbus-1/session.conf", "/usr/share/dbus-1/session.conf"] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Private `dbus-daemon` + `zwhisperd` fixture.
///
/// ## Lifecycle
///
/// 1. `try_new` — spawns `dbus-daemon` against a tempdir socket and
///    polls until the socket exists.
/// 2. `spawn_zwhisperd` — starts the daemon binary against this bus
///    and waits for it to claim the well-known name.
/// 3. tests use `proxy_recorder` / `proxy_profiles` (or build their
///    own connection from `address()`) to drive the daemon.
/// 4. `Drop` — kills the zwhisperd child first, then the dbus-daemon,
///    then cleans up the tempdir.
pub struct DbusFixture {
    daemon_proc: Child,
    daemon_addr: String,
    daemon_handle: Option<Child>,
    tmp: tempfile::TempDir,
}

impl std::fmt::Debug for DbusFixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbusFixture")
            .field("daemon_proc_id", &self.daemon_proc.id())
            .field("daemon_addr", &self.daemon_addr)
            .field("daemon_handle_running", &self.daemon_handle.is_some())
            .field("tmp", &self.tmp.path())
            .finish()
    }
}

impl DbusFixture {
    /// Build the fixture. Returns `Err(FixtureSkip)` when the host
    /// cannot run the suite (no `dbus-daemon`, no config file, socket
    /// never appears). The caller maps each skip variant to a
    /// `[SKIP] <reason>` print and returns early.
    pub fn try_new() -> Result<Self, FixtureSkip> {
        // 1. dbus-daemon must be on PATH.
        if which::which("dbus-daemon").is_err() {
            return Err(FixtureSkip::NoDbusDaemon);
        }

        // 2. session.conf must exist at one of the well-known
        // locations. The fixture refuses to invent a default config
        // (M3-plan correction C10).
        let conf = locate_session_conf().ok_or(FixtureSkip::NoDbusConfig)?;

        // 3. Socket lives under a per-fixture tempdir so two tests
        // running in parallel can each have their own bus.
        let tmp = tempfile::tempdir()
            .map_err(|e| FixtureSkip::Other(format!("failed to create tempdir: {e}")))?;
        let socket_path = tmp.path().join("bus.sock");
        let address = format!("unix:path={}", socket_path.display());

        // 4. Spawn dbus-daemon. `--nofork` keeps the child attached
        // so `Drop` can kill it cleanly. `--print-address` is not
        // strictly needed because we know the address up-front, but
        // having stdout helps if a future debug session needs it.
        let proc = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", conf.display()))
            .arg(format!("--address={address}"))
            .arg("--nofork")
            .arg("--print-address")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| FixtureSkip::Other(format!("failed to spawn dbus-daemon: {e}")))?;

        // 5. Wait for the socket to appear on disk. Up to 2 s in
        // 20 ms slices = 100 attempts. Hosts under load see
        // `dbus-daemon` take 50+ ms before the socket exists; this
        // poll is the documented C10 mitigation.
        if let Err(e) = wait_for_socket(&socket_path, 100, Duration::from_millis(20)) {
            // Kill the daemon proc before returning; otherwise we
            // leak a child.
            let _ = kill_child(proc);
            return Err(FixtureSkip::Other(format!(
                "dbus-daemon socket never appeared at {}: {e}",
                socket_path.display(),
            )));
        }

        Ok(Self {
            daemon_proc: proc,
            daemon_addr: address,
            daemon_handle: None,
            tmp,
        })
    }

    /// The session bus address (`unix:path=…`). Tests pass this to
    /// `zbus::connection::Builder::address(...)` so they do not
    /// stomp on the real session bus.
    pub fn address(&self) -> &str {
        &self.daemon_addr
    }

    /// The `XDG_STATE_HOME` the spawned daemon writes under. Tests
    /// read `<state_home>/zwhisper/last-session.json` to verify the
    /// C2 ordering invariant (M4-plan § "Stress-test corrections").
    pub fn state_home(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    /// Spawn the `zwhisperd` binary against this fixture's bus and
    /// wait for it to claim `cz.zajca.Zwhisper1`. Returns
    /// `Err(io::Error)` for spawn failure, or wraps a timeout
    /// diagnostic in `io::Error::other` when the bus name never
    /// appears within 5 s.
    pub async fn spawn_zwhisperd(&mut self) -> std::io::Result<()> {
        let exe = env!("CARGO_BIN_EXE_zwhisperd");
        // The daemon writes to `$XDG_STATE_HOME/zwhisper/zwhisperd.log`
        // by default. Point it at a fixture-private state dir so
        // parallel tests do not stomp on each other's logs.
        let state_dir = self.tmp.path().join("state");
        std::fs::create_dir_all(&state_dir)?;
        let data_dir = self.tmp.path().join("data");
        std::fs::create_dir_all(&data_dir)?;
        let config_dir = self.tmp.path().join("config");
        std::fs::create_dir_all(&config_dir)?;

        // Drain daemon stderr to a file inside the tempdir so a
        // hang or panic surfaces in test output instead of being
        // silently buffered. Tests can read this file when
        // diagnosing failures. When `ZWHISPERD_TEST_STDERR_DIR` is
        // set, we also mirror the file under that directory using
        // a per-fixture suffix — handy because the tempdir cleans
        // itself up on Drop.
        let stderr_path = self.tmp.path().join("zwhisperd.stderr");
        let stderr_file = std::fs::File::create(&stderr_path)?;
        if let Some(dir) = std::env::var_os("ZWHISPERD_TEST_STDERR_DIR") {
            let mirror_dir = PathBuf::from(dir);
            std::fs::create_dir_all(&mirror_dir)?;
            let suffix = self
                .tmp
                .path()
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("anon");
            let mirror = mirror_dir.join(format!("zwhisperd-{suffix}.stderr"));
            // Best-effort: a copy will land here when the daemon
            // actually writes anything; for streaming we set up a
            // hardlink from the original.
            let _ = std::fs::hard_link(&stderr_path, &mirror);
        }
        let child = Command::new(exe)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.daemon_addr)
            .env("XDG_STATE_HOME", &state_dir)
            .env("XDG_DATA_HOME", &data_dir)
            .env("XDG_CONFIG_HOME", &config_dir)
            // Daemon logs at info+ by default — verbose enough to
            // diagnose hangs from the per-test temp state dir,
            // quiet enough not to drown the test output.
            .env(
                "RUST_LOG",
                std::env::var_os("ZWHISPERD_TEST_RUST_LOG")
                    .unwrap_or_else(|| "info".into()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .spawn()?;
        self.daemon_handle = Some(child);

        // Poll for bus name ownership. zbus::fdo::DBusProxy is the
        // canonical NameHasOwner client.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::other(format!(
                    "zwhisperd never claimed {BUS_NAME} within 5 s on bus {}",
                    self.daemon_addr,
                )));
            }
            match self.is_name_owned().await {
                Ok(true) => return Ok(()),
                Ok(false) | Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    async fn is_name_owned(&self) -> zbus::Result<bool> {
        let conn = zbus::connection::Builder::address(self.daemon_addr.as_str())?
            .build()
            .await?;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let owned = dbus
            .name_has_owner(zbus::names::BusName::try_from(BUS_NAME)?)
            .await?;
        Ok(owned)
    }

    /// Build a fresh connection to the fixture's bus. Each call
    /// returns a new connection; cheap because zbus reuses the
    /// underlying socket descriptor.
    pub async fn connection(&self) -> zbus::Result<zbus::Connection> {
        zbus::connection::Builder::address(self.daemon_addr.as_str())?
            .build()
            .await
    }

    /// Build a `Recorder1` proxy bound to a fresh connection. The
    /// connection lives on the returned proxy; dropping the proxy
    /// closes the connection.
    pub async fn proxy_recorder(&self) -> zbus::Result<Recorder1Proxy<'static>> {
        let conn = self.connection().await?;
        Recorder1Proxy::builder(&conn)
            .destination(BUS_NAME)?
            .path(OBJECT_PATH)?
            .build()
            .await
    }

    /// Build a `Profiles1` proxy bound to a fresh connection.
    pub async fn proxy_profiles(&self) -> zbus::Result<Profiles1Proxy<'static>> {
        let conn = self.connection().await?;
        Profiles1Proxy::builder(&conn)
            .destination(BUS_NAME)?
            .path(OBJECT_PATH)?
            .build()
            .await
    }

    /// Build a `Jobs1` proxy bound to a fresh connection
    /// (RFC-daemon-role Feature 1).
    pub async fn proxy_jobs(&self) -> zbus::Result<Jobs1Proxy<'static>> {
        let conn = self.connection().await?;
        Jobs1Proxy::builder(&conn)
            .destination(BUS_NAME)?
            .path(OBJECT_PATH)?
            .build()
            .await
    }

    /// Build a `History1` proxy bound to a fresh connection
    /// (RFC-daemon-role Feature 2).
    pub async fn proxy_history(&self) -> zbus::Result<History1Proxy<'static>> {
        let conn = self.connection().await?;
        History1Proxy::builder(&conn)
            .destination(BUS_NAME)?
            .path(OBJECT_PATH)?
            .build()
            .await
    }
}

impl Drop for DbusFixture {
    fn drop(&mut self) {
        // Kill zwhisperd first so the daemon releases the bus name
        // before dbus-daemon goes away.
        if let Some(child) = self.daemon_handle.take() {
            let _ = kill_child(child);
        }
        // Then dbus-daemon. We can only `try_wait` on a Child that
        // is still in scope; replacing the field with a sentinel
        // `Command::new("true")` child is messier than just calling
        // `.kill()` directly.
        let _ = self.daemon_proc.kill();
        let _ = self.daemon_proc.wait();
        // tempfile::TempDir cleans up the socket dir.
    }
}

fn kill_child(mut child: Child) -> std::io::Result<()> {
    child.kill()?;
    child.wait()?;
    Ok(())
}

fn wait_for_socket(path: &Path, attempts: u32, interval: Duration) -> Result<(), String> {
    for _ in 0..attempts {
        if std::fs::metadata(path).is_ok() {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
    Err(format!(
        "socket {} did not appear after {} × {:?}",
        path.display(),
        attempts,
        interval,
    ))
}
