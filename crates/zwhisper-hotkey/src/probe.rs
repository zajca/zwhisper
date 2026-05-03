//! Backend probe — detects whether the `GlobalShortcuts` portal is
//! reachable and reports the running compositor flavour.
//!
//! Implements Group D (D1: `detect_backend`, D2: `probe`) of the M6
//! plan. The CLI (`zwhisper hotkey probe`) reads the resulting
//! [`ProbeReport`] and translates it into the truth-table output
//! described in `docs/M6-plan.md` § `DoD` #10.
//!
//! Detection strategy:
//!
//! 1. Ask the session bus whether `org.freedesktop.portal.Desktop`
//!    has an owner. If not → [`BackendDetected::None`] (typical
//!    i3/X11 case where xdg-desktop-portal isn't running).
//! 2. Resolve the owner's PID and read `/proc/<pid>/cmdline` to
//!    figure out which portal binary is providing the bus name
//!    (`xdg-desktop-portal-kde` / `…-gnome` / `…-wlr`). NOTE: we
//!    cannot use `/proc/<pid>/comm` here because the kernel
//!    truncates `comm` to 15 bytes — `xdg-desktop-portal-kde`,
//!    `…-gnome` and `…-wlr` all collapse to the same prefix
//!    `xdg-desktop-por`, defeating substring discrimination.
//!    `cmdline` carries `argv[0]` (full executable path) verbatim.
//! 3. Introspect the portal's `GlobalShortcuts` interface and read
//!    its `version` property; absence ⇒ available=false.
//!
//! All bus / disk failures are downgraded to graceful states; this
//! module never panics.

#![cfg(feature = "portal")]

use async_trait::async_trait;
use std::convert::TryFrom;

/// Well-known D-Bus name of the xdg-desktop-portal frontend.
const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";

/// Object path exposing the `GlobalShortcuts` interface on the
/// portal frontend.
const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

/// Interface name used to fetch the `version` property.
const GLOBAL_SHORTCUTS_IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

/// Detected portal backend behind `org.freedesktop.portal.Desktop`.
///
/// The `Other` variant carries the raw `comm` value so the CLI can
/// surface it for troubleshooting (e.g. an unrecognised distro
/// portal flavour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendDetected {
    /// `xdg-desktop-portal-kde` (KDE Plasma).
    Kde,
    /// `xdg-desktop-portal-gnome` (GNOME Shell).
    Gnome,
    /// `xdg-desktop-portal-wlr` (wlroots-based: Sway, Hyprland, …).
    Wlr,
    /// Some other portal binary owns the well-known name. Carries
    /// the raw `/proc/<pid>/comm` string for diagnostics.
    Other(String),
    /// `org.freedesktop.portal.Desktop` is not on the session bus —
    /// classic i3/X11 setup with no xdg-desktop-portal installed.
    None,
}

/// Outcome of [`probe`]. The CLI maps this onto the three-line
/// truth table in `DoD` #10.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    /// Identified backend (or [`BackendDetected::None`]).
    pub backend: BackendDetected,
    /// `true` only when the `GlobalShortcuts` interface answers a
    /// `version` property read.
    pub global_shortcuts_available: bool,
    /// Value of `org.freedesktop.portal.GlobalShortcuts.version`.
    pub portal_version: Option<u32>,
    /// Single-line, human-readable explanation suitable for the
    /// CLI output.
    pub reason: String,
}

// =====================================================================
// Test seams — small traits that wrap the live D-Bus / proc/fs
// interactions so the truth table can be exercised without a real
// portal on the bus.
// =====================================================================

/// Inspects the session bus for the portal owner / its PID / its
/// executable name. Implemented by [`LiveBus`] in production and
/// by `FakeBus` in tests.
///
/// We deliberately use `/proc/<pid>/cmdline` (full executable
/// path in `argv[0]`) instead of `/proc/<pid>/comm` (15-byte
/// truncated thread name) — the three real portal binaries all
/// truncate to `xdg-desktop-por` and become indistinguishable.
#[async_trait]
trait BusInspector: Send + Sync {
    async fn name_has_owner(&self, name: &str) -> bool;
    async fn get_pid(&self, name: &str) -> Option<u32>;
    /// Returns the basename of the process's executable
    /// (`argv[0]`), or `None` when `/proc/<pid>/cmdline` cannot
    /// be read or is empty.
    async fn read_cmdline(&self, pid: u32) -> Option<String>;
}

/// Reads the `version` property of the `GlobalShortcuts`
/// interface. Separate trait so [`probe`] can be tested without
/// reaching for the real portal.
#[async_trait]
trait PortalIntrospector: Send + Sync {
    /// Returns `Some(version)` when the interface exists and the
    /// property read succeeds, `None` otherwise.
    async fn global_shortcuts_version(&self) -> Option<u32>;
}

// =====================================================================
// Live (production) implementations.
// =====================================================================

/// Live `BusInspector` — talks to the real session bus via
/// `zbus::fdo::DBusProxy` and `tokio::fs`.
struct LiveBus {
    conn: zbus::Connection,
}

#[async_trait]
impl BusInspector for LiveBus {
    async fn name_has_owner(&self, name: &str) -> bool {
        let proxy = match zbus::fdo::DBusProxy::new(&self.conn).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "DBusProxy::new failed");
                return false;
            }
        };
        let bus_name = match zbus::names::BusName::try_from(name) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, name, "invalid bus name");
                return false;
            }
        };
        match proxy.name_has_owner(bus_name).await {
            Ok(owned) => owned,
            Err(e) => {
                tracing::debug!(error = %e, "name_has_owner failed");
                false
            }
        }
    }

    async fn get_pid(&self, name: &str) -> Option<u32> {
        let proxy = zbus::fdo::DBusProxy::new(&self.conn).await.ok()?;
        let bus_name = zbus::names::BusName::try_from(name).ok()?;
        match proxy.get_connection_unix_process_id(bus_name).await {
            Ok(pid) => Some(pid),
            Err(e) => {
                tracing::debug!(error = %e, "GetConnectionUnixProcessID failed");
                None
            }
        }
    }

    async fn read_cmdline(&self, pid: u32) -> Option<String> {
        // `/proc/<pid>/cmdline` is the NUL-separated argv. We only
        // care about argv[0] (the executable path); subsequent
        // arguments are irrelevant for backend classification.
        // Reading as bytes to avoid UTF-8 surprises on exotic
        // installs — argv[0] is virtually always ASCII anyway.
        let path = format!("/proc/{pid}/cmdline");
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(error = %e, path, "failed to read /proc cmdline");
                return None;
            }
        };
        // First NUL terminates argv[0]. If no NUL is present, the
        // whole buffer IS argv[0] (kernel sometimes returns the
        // raw command without separators for short cmdlines).
        let argv0_bytes = bytes
            .split(|b| *b == 0)
            .next()
            .filter(|s| !s.is_empty())?;
        let argv0 = std::str::from_utf8(argv0_bytes).ok()?;
        // Trim to basename — portal binaries are usually invoked
        // by absolute path (`/usr/lib/xdg-desktop-portal-kde`).
        let basename = argv0.rsplit('/').next().unwrap_or(argv0);
        if basename.is_empty() {
            None
        } else {
            Some(basename.to_string())
        }
    }
}

/// Live `PortalIntrospector` — issues a real D-Bus
/// `Properties.Get` against the `GlobalShortcuts` interface.
struct LivePortal {
    conn: zbus::Connection,
}

#[async_trait]
impl PortalIntrospector for LivePortal {
    async fn global_shortcuts_version(&self) -> Option<u32> {
        // Build a generic proxy bound to the GlobalShortcuts
        // interface. `get_property` issues a
        // `org.freedesktop.DBus.Properties.Get` under the hood.
        let proxy = match zbus::Proxy::new(
            &self.conn,
            PORTAL_BUS_NAME,
            PORTAL_OBJECT_PATH,
            GLOBAL_SHORTCUTS_IFACE,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "GlobalShortcuts proxy failed");
                return None;
            }
        };
        match proxy.get_property::<u32>("version").await {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!(error = %e, "version property fetch failed");
                None
            }
        }
    }
}

// =====================================================================
// Public API.
// =====================================================================

/// Detects which portal backend (if any) currently owns
/// `org.freedesktop.portal.Desktop` on the session bus.
///
/// Never panics: every D-Bus or filesystem failure is logged and
/// degrades to a sensible variant ([`BackendDetected::None`] when
/// the bus is unreachable, [`BackendDetected::Other`] with
/// `"unknown"` when `/proc/<pid>/comm` can't be read).
pub async fn detect_backend() -> BackendDetected {
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "session bus unreachable");
            return BackendDetected::None;
        }
    };
    detect_backend_with(&LiveBus { conn }).await
}

/// Testable variant of [`detect_backend`] taking a
/// [`BusInspector`].
async fn detect_backend_with(bus: &dyn BusInspector) -> BackendDetected {
    tracing::debug!(name = PORTAL_BUS_NAME, "checking portal owner");
    if !bus.name_has_owner(PORTAL_BUS_NAME).await {
        tracing::debug!("portal not on the bus");
        return BackendDetected::None;
    }

    let Some(pid) = bus.get_pid(PORTAL_BUS_NAME).await else {
        tracing::debug!("could not resolve portal owner PID");
        return BackendDetected::Other("unknown".to_string());
    };
    tracing::debug!(pid, "portal owner PID");

    let Some(exe) = bus.read_cmdline(pid).await else {
        tracing::debug!(pid, "could not read /proc/<pid>/cmdline");
        return BackendDetected::Other("unknown".to_string());
    };
    let exe_trim = exe.trim();
    tracing::debug!(pid, exe = exe_trim, "portal owner executable");

    classify_executable(exe_trim)
}

/// Classifies a `/proc/<pid>/cmdline` argv[0] basename into a
/// [`BackendDetected`] variant.
///
/// Unlike `/proc/<pid>/comm` (which is truncated to 15 bytes by
/// the kernel and collapses every real backend to the same
/// `xdg-desktop-por` prefix), `cmdline` carries the FULL argv[0]
/// so we can reliably match on the complete binary name.
fn classify_executable(exe: &str) -> BackendDetected {
    if exe.contains("xdg-desktop-portal-kde") {
        BackendDetected::Kde
    } else if exe.contains("xdg-desktop-portal-gnome") {
        BackendDetected::Gnome
    } else if exe.contains("xdg-desktop-portal-wlr") {
        BackendDetected::Wlr
    } else if exe.is_empty() {
        BackendDetected::Other("unknown".to_string())
    } else {
        BackendDetected::Other(exe.to_string())
    }
}

/// Friendly portal name for the diagnostic `reason` string.
fn backend_label(backend: &BackendDetected) -> String {
    match backend {
        BackendDetected::Kde => "portal-kde".to_string(),
        BackendDetected::Gnome => "portal-gnome".to_string(),
        BackendDetected::Wlr => "portal-wlr".to_string(),
        BackendDetected::Other(s) => format!("portal={s}"),
        BackendDetected::None => "none".to_string(),
    }
}

/// Probes the session bus for an available `GlobalShortcuts` portal
/// and returns a [`ProbeReport`].
///
/// Three states map onto the truth table from `DoD` #10:
///
/// * `backend = None` → no xdg-desktop-portal (i3/X11),
/// * `backend = Kde/Gnome/Wlr/Other` and `available = false` →
///   portal is up but doesn't expose `GlobalShortcuts`,
/// * `available = true` → fully usable, `portal_version` is set.
pub async fn probe() -> ProbeReport {
    let backend = detect_backend().await;
    if matches!(backend, BackendDetected::None) {
        return probe_unavailable_no_portal();
    }

    // Reuse the connection-establishing path for the live
    // introspector. Failure to open the bus here would be
    // surprising (we just succeeded inside `detect_backend`), but
    // we still degrade gracefully.
    let conn = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "session bus unreachable during probe");
            return ProbeReport {
                backend,
                global_shortcuts_available: false,
                portal_version: None,
                reason: "session bus unreachable".to_string(),
            };
        }
    };
    probe_with(backend, &LivePortal { conn }).await
}

/// Testable variant of [`probe`] — takes an already-detected
/// backend and a [`PortalIntrospector`].
async fn probe_with(backend: BackendDetected, portal: &dyn PortalIntrospector) -> ProbeReport {
    if matches!(backend, BackendDetected::None) {
        return probe_unavailable_no_portal();
    }

    match portal.global_shortcuts_version().await {
        Some(version) => {
            let reason = format!(
                "{} GlobalShortcuts v{version}",
                backend_label(&backend)
            );
            ProbeReport {
                backend,
                global_shortcuts_available: true,
                portal_version: Some(version),
                reason,
            }
        }
        None => ProbeReport {
            backend,
            global_shortcuts_available: false,
            portal_version: None,
            reason: "backend has no GlobalShortcuts interface".to_string(),
        },
    }
}

fn probe_unavailable_no_portal() -> ProbeReport {
    ProbeReport {
        backend: BackendDetected::None,
        global_shortcuts_available: false,
        portal_version: None,
        reason: "no GlobalShortcuts portal — i3/X11 detected; bind via your WM config"
            .to_string(),
    }
}

// =====================================================================
// Tests — drive the truth table through `FakeBus` /
// `FakePortal` so they don't require a live xdg-desktop-portal.
// =====================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Configurable fake `BusInspector` for the truth-table
    /// tests. Each field corresponds to one bus operation.
    struct FakeBus {
        owned: bool,
        pid: Option<u32>,
        /// Synthesised value of `/proc/<pid>/cmdline` argv[0]
        /// basename — i.e. the FULL executable name (no
        /// truncation), since `cmdline` is not subject to the
        /// 15-byte `comm` cap.
        cmdline: Option<String>,
    }

    impl FakeBus {
        fn new(owned: bool, pid: Option<u32>, cmdline: Option<&str>) -> Self {
            Self {
                owned,
                pid,
                cmdline: cmdline.map(str::to_string),
            }
        }
    }

    #[async_trait]
    impl BusInspector for FakeBus {
        async fn name_has_owner(&self, _name: &str) -> bool {
            self.owned
        }
        async fn get_pid(&self, _name: &str) -> Option<u32> {
            self.pid
        }
        async fn read_cmdline(&self, _pid: u32) -> Option<String> {
            self.cmdline.clone()
        }
    }

    /// Configurable fake `PortalIntrospector`. The mutex is only
    /// needed because tests read `version` through a shared
    /// reference; in real code the live introspector is owned.
    struct FakePortal {
        version: Mutex<Option<u32>>,
    }

    impl FakePortal {
        fn with_version(v: u32) -> Self {
            Self {
                version: Mutex::new(Some(v)),
            }
        }
        fn missing() -> Self {
            Self {
                version: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl PortalIntrospector for FakePortal {
        async fn global_shortcuts_version(&self) -> Option<u32> {
            // Mutex poisoning here only happens if a test panicked while holding
            // the lock — surface the prior panic rather than swallow.
            #[allow(clippy::expect_used)]
            {
                *self.version.lock().expect("FakePortal mutex poisoned")
            }
        }
    }

    // ---- detect_backend truth table ------------------------------

    #[tokio::test]
    async fn no_portal_owner_returns_none() {
        let bus = FakeBus::new(false, None, None);
        assert_eq!(detect_backend_with(&bus).await, BackendDetected::None);
    }

    #[tokio::test]
    async fn kde_classified_from_full_cmdline_xdg_desktop_portal_kde() {
        // Real Linux installs run the binary as the FULL name
        // (e.g. `/usr/lib/xdg-desktop-portal-kde`); cmdline is not
        // truncated. After basenaming we see the unabridged name.
        let bus = FakeBus::new(true, Some(1234), Some("xdg-desktop-portal-kde"));
        assert_eq!(detect_backend_with(&bus).await, BackendDetected::Kde);
    }

    #[tokio::test]
    async fn gnome_classified_from_full_cmdline_xdg_desktop_portal_gnome() {
        let bus = FakeBus::new(true, Some(2345), Some("xdg-desktop-portal-gnome"));
        assert_eq!(detect_backend_with(&bus).await, BackendDetected::Gnome);
    }

    #[tokio::test]
    async fn wlr_classified_from_full_cmdline_xdg_desktop_portal_wlr() {
        let bus = FakeBus::new(true, Some(3456), Some("xdg-desktop-portal-wlr"));
        assert_eq!(detect_backend_with(&bus).await, BackendDetected::Wlr);
    }

    #[tokio::test]
    async fn truncated_comm_string_is_unrecognised_and_falls_through() {
        // Regression: the kernel truncates `/proc/<pid>/comm` to
        // 15 bytes, collapsing all three real backends to
        // `xdg-desktop-por`. The new cmdline-based pipeline must
        // NEVER receive that truncated form (we removed the
        // `read_comm` seam) — but we still pin the classifier's
        // behaviour so a future regression can't silently revive
        // the bug. `xdg-desktop-por` does NOT contain any of the
        // full backend names, so the classifier treats it as
        // `Other(...)` rather than misreporting Kde / Gnome / Wlr.
        let bus = FakeBus::new(true, Some(7777), Some("xdg-desktop-por"));
        match detect_backend_with(&bus).await {
            BackendDetected::Other(s) => assert_eq!(s, "xdg-desktop-por"),
            other => panic!("expected Other(\"xdg-desktop-por\"), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn other_portal_returns_other_variant() {
        let bus = FakeBus::new(true, Some(4567), Some("xdg-desktop-portal-foo"));
        assert_eq!(
            detect_backend_with(&bus).await,
            BackendDetected::Other("xdg-desktop-portal-foo".to_string())
        );
    }

    #[tokio::test]
    async fn cmdline_unreadable_falls_back_to_other_unknown() {
        let bus = FakeBus::new(true, Some(5678), None);
        assert_eq!(
            detect_backend_with(&bus).await,
            BackendDetected::Other("unknown".to_string())
        );
    }

    #[tokio::test]
    async fn missing_pid_falls_back_to_other_unknown() {
        let bus = FakeBus::new(true, None, None);
        assert_eq!(
            detect_backend_with(&bus).await,
            BackendDetected::Other("unknown".to_string())
        );
    }

    // ---- probe() truth table -------------------------------------

    #[tokio::test]
    async fn probe_with_no_portal_returns_unavailable_with_i3_message() {
        let portal = FakePortal::missing();
        let report = probe_with(BackendDetected::None, &portal).await;
        assert_eq!(report.backend, BackendDetected::None);
        assert!(!report.global_shortcuts_available);
        assert_eq!(report.portal_version, None);
        assert!(
            report.reason.contains("i3/X11"),
            "reason should mention i3/X11; got: {}",
            report.reason
        );
        assert!(
            report.reason.contains("no GlobalShortcuts portal"),
            "reason should mention missing portal; got: {}",
            report.reason
        );
    }

    #[tokio::test]
    async fn probe_with_kde_returns_available_with_version_2() {
        let portal = FakePortal::with_version(2);
        let report = probe_with(BackendDetected::Kde, &portal).await;
        assert_eq!(report.backend, BackendDetected::Kde);
        assert!(report.global_shortcuts_available);
        assert_eq!(report.portal_version, Some(2));
        assert!(
            report.reason.contains("portal-kde"),
            "reason should label backend; got: {}",
            report.reason
        );
        assert!(
            report.reason.contains("v2"),
            "reason should mention version 2; got: {}",
            report.reason
        );
    }

    #[tokio::test]
    async fn probe_with_kde_but_missing_interface_marks_unavailable() {
        let portal = FakePortal::missing();
        let report = probe_with(BackendDetected::Kde, &portal).await;
        assert_eq!(report.backend, BackendDetected::Kde);
        assert!(!report.global_shortcuts_available);
        assert_eq!(report.portal_version, None);
        assert_eq!(
            report.reason,
            "backend has no GlobalShortcuts interface"
        );
    }

    #[tokio::test]
    async fn probe_with_other_backend_includes_label_in_reason() {
        let portal = FakePortal::with_version(1);
        let report = probe_with(
            BackendDetected::Other("xdg-desktop-portal-foo".to_string()),
            &portal,
        )
        .await;
        assert!(report.global_shortcuts_available);
        assert_eq!(report.portal_version, Some(1));
        assert!(
            report.reason.contains("xdg-desktop-portal-foo"),
            "reason should expose unknown backend name; got: {}",
            report.reason
        );
    }

    // ---- classify_executable direct sanity checks ----------------

    #[test]
    fn classify_executable_handles_empty() {
        assert_eq!(
            classify_executable(""),
            BackendDetected::Other("unknown".to_string())
        );
    }

    #[test]
    fn classify_executable_keeps_raw_other_value() {
        assert_eq!(
            classify_executable("some-random-bin"),
            BackendDetected::Other("some-random-bin".to_string())
        );
    }

    #[test]
    fn classify_executable_kde_full_name() {
        assert_eq!(
            classify_executable("xdg-desktop-portal-kde"),
            BackendDetected::Kde
        );
    }

    #[test]
    fn classify_executable_gnome_full_name() {
        assert_eq!(
            classify_executable("xdg-desktop-portal-gnome"),
            BackendDetected::Gnome
        );
    }

    #[test]
    fn classify_executable_wlr_full_name() {
        assert_eq!(
            classify_executable("xdg-desktop-portal-wlr"),
            BackendDetected::Wlr
        );
    }

    #[test]
    fn classify_executable_does_not_misclassify_truncated_comm() {
        // The whole point of moving from /proc/<pid>/comm to
        // /proc/<pid>/cmdline: the 15-byte comm form
        // `xdg-desktop-por` must NOT be matched as any of
        // Kde/Gnome/Wlr — it is genuinely ambiguous and the
        // classifier honestly reports it as `Other`.
        match classify_executable("xdg-desktop-por") {
            BackendDetected::Other(s) => assert_eq!(s, "xdg-desktop-por"),
            other => panic!(
                "truncated comm must not be classified as a known backend; got {other:?}"
            ),
        }
    }
}
