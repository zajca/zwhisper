//! `zwhisper profile {list,show,clone,migrate}` dispatcher.
//!
//! `list` and `show` consult the daemon (so the active daemon stays
//! the source of truth for "what profiles exist"), with a graceful
//! fallback to the local listing when the daemon is not on the bus.
//! `clone` and `migrate` stay local — they touch user TOML files
//! under `${XDG_CONFIG_HOME}/zwhisper/profiles/` and a cross-process
//! daemon RPC for them is M4 territory.

use color_eyre::eyre::eyre;
use tracing::{debug, info};
use zwhisper_ipc::{Profiles1Proxy, types::ProfileEntry};

use crate::cli::ProfileCmd;
use crate::profile_commands;

use super::{build_runtime, is_daemon_down};

pub(crate) fn run(cmd: &ProfileCmd) -> color_eyre::Result<()> {
    match cmd {
        ProfileCmd::List => list(),
        ProfileCmd::Set { name } => set_active(name),
        ProfileCmd::Show { name } => show(name),
        // Clone + migrate stay local — see the M3-plan rationale in
        // `commands/mod.rs`.
        ProfileCmd::Clone { src, dst } => profile_commands::clone(src, dst),
        ProfileCmd::Migrate { name } => profile_commands::migrate(name),
    }
}

fn set_active(name: &str) -> color_eyre::Result<()> {
    // Validate locally before touching the daemon so typos get the
    // profile loader's detailed file-path diagnostics.
    zwhisper_core::profile::load(name).map_err(|e| eyre!("{e}"))?;

    let rt = build_runtime()?;
    match rt.block_on(set_active_via_dbus(name)) {
        Ok(()) => {
            println!("active profile: {name}");
            Ok(())
        }
        Err(err) => {
            if is_daemon_down(&err) {
                Err(eyre!(
                    "daemon not running; could not persist active profile through Profiles1.SetActive. \
                     Start it with `systemctl --user start zwhisperd` and retry."
                ))
            } else {
                Err(eyre!("Profiles1.SetActive failed: {err}"))
            }
        }
    }
}

fn list() -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    match rt.block_on(fetch_list_via_dbus()) {
        Ok(entries) => {
            print_dbus_table(&entries);
            Ok(())
        }
        Err(err) => {
            // Daemon down → emit a notice on stderr and fall back to
            // the local listing. Per DoD: still exit 0 in this path
            // — the user got their answer, just from the on-disk
            // source rather than the daemon's in-memory cache.
            if is_daemon_down(&err) {
                fallback_to_local("list", &err);
                profile_commands::list()
            } else {
                Err(eyre!("Profiles1.List failed: {err}"))
            }
        }
    }
}

fn show(name: &str) -> color_eyre::Result<()> {
    let rt = build_runtime()?;
    match rt.block_on(fetch_list_via_dbus()) {
        Ok(entries) => {
            let entry = entries.iter().find(|e| e.name == name);
            if let Some(entry) = entry {
                println!("source: daemon ({})", zwhisper_ipc::BUS_NAME);
                println!("name: {}", entry.name);
                println!("description: {}", entry.description);
                println!("schema_version: {}", entry.schema_version);
                Ok(())
            } else {
                // Daemon answered, but the profile is unknown to it.
                // Fall through to the local resolver — it gives a
                // typed `not found` error with the searched paths,
                // which is more useful than "missing in daemon list".
                debug!(
                    name,
                    "profile missing from daemon list, falling back to local resolver"
                );
                profile_commands::show(name)
            }
        }
        Err(err) => {
            if is_daemon_down(&err) {
                fallback_to_local("show", &err);
                profile_commands::show(name)
            } else {
                Err(eyre!("Profiles1.List failed: {err}"))
            }
        }
    }
}

async fn set_active_via_dbus(name: &str) -> Result<(), zbus::Error> {
    let conn = zbus::Connection::session().await?;
    let proxy = Profiles1Proxy::new(&conn).await?;
    proxy.set_active(name).await
}

async fn fetch_list_via_dbus() -> Result<Vec<ProfileEntry>, zbus::Error> {
    let conn = zbus::Connection::session().await?;
    let proxy = Profiles1Proxy::new(&conn).await?;
    proxy.list().await
}

#[allow(clippy::print_stderr)]
fn fallback_to_local(op: &str, err: &zbus::Error) {
    info!(
        ?op,
        "daemon unreachable, falling back to local profile {op}"
    );
    eprintln!("daemon not running, listing from local files only ({err})");
}

fn print_dbus_table(entries: &[ProfileEntry]) {
    if entries.is_empty() {
        println!("(no profiles found)");
        return;
    }
    println!("{:<24}  {:<6}  description", "name", "ver");
    println!("{}", "-".repeat(60));
    for entry in entries {
        println!(
            "{:<24}  {:<6}  {}",
            entry.name, entry.schema_version, entry.description
        );
    }
}
