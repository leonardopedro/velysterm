//! Durable-store status dashboard (operator surface for the unfer H4 store).
//!
//! Polls the kernel's durable store — opened **read-only** from the same
//! `UNFER_DURABLE_DIR` the kernel uses (Loro backend) — and renders a small
//! Bevy-UI status chip in the top-right corner: backend, persist counter,
//! per-stream record counts, and the fail-visible corrupt-snapshot recovery
//! report (`snapshot_load_error`).
//!
//! The panel is a pure consult: it never appends or flushes, so it cannot
//! race the kernel's own writes to the same snapshot. Everything GUI lives
//! in the mathed Bevy stack (per the GUI convention); `kernel_client` and
//! `mathed_mini` stay headless.

use std::path::PathBuf;
use std::sync::Arc;

use bevy::prelude::*;
use unfer_ffi::durable::{Backend, open_store};
use unfer_protocol::durable::{DurableStore, streams};

/// The well-known stream names in status order (matches
/// `unfer_ffi::handles::STREAM_NAMES`).
const STREAM_NAMES: [&str; 6] = [
    streams::AUDIT,
    streams::OWNER_LOG,
    streams::ACTIONS,
    streams::CONFIG,
    streams::SESSION,
    streams::CERTIFICATES,
];

/// Latest consulted status, refreshed on the poll cadence.
#[derive(Resource, Default)]
pub struct DurableStatus {
    pub backend: String,
    pub persist_count: u64,
    pub streams: Vec<(String, u64)>,
    pub snapshot_load_error: Option<String>,
}

/// The panel's store handle (opened lazily) and poll cadence.
#[derive(Resource)]
pub struct DurablePanel {
    store: Option<Arc<dyn DurableStore>>,
    poll: Timer,
}

impl Default for DurablePanel {
    fn default() -> Self {
        Self {
            store: None,
            poll: Timer::from_seconds(2.0, TimerMode::Repeating),
        }
    }
}

/// Marker on the dashboard text node so the update system can find it.
#[derive(Component)]
pub(crate) struct DurablePanelText;

/// Spawn the status chip (top-right corner). Spawned once at startup; the
/// update system rewrites its text.
pub fn spawn_durable_panel(mut commands: Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                right: Val::Px(12.0),
                top: Val::Px(12.0),
                padding: UiRect::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.10, 0.10, 0.13, 0.85)),
            ZIndex(20),
        ))
        .with_children(|parent| {
            parent.spawn((
                Text::new("durable: —"),
                TextColor(Color::srgb(0.72, 0.78, 0.85)),
                DurablePanelText,
            ));
        });
}

/// Open a read-only store from `UNFER_DURABLE_DIR` (only when the directory
/// exists — otherwise `None` reports RAM-only, exactly like the kernel does
/// with no store configured). Loro's `open_store` never fails, so opening
/// cannot error out; it just stays `None` until a store appears.
pub fn open_panel_store() -> Option<Arc<dyn DurableStore>> {
    let dir = std::env::var("UNFER_DURABLE_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .filter(|d| d.is_dir());
    let d = dir?;
    // Loro open is infallible (a corrupt snapshot recovers empty and reports
    // via `snapshot_load_error` — exactly what the chip shows).
    Some(Arc::from(
        open_store(Some(&d), Backend::Loro).expect("loro open_store cannot fail"),
    ))
}

/// Consult the durable store into a fresh status (backend, persist counter,
/// per-stream lengths, corrupt-snapshot recovery report). `None` store =
/// RAM-only (the kernel's no-store shape, stable schema). Pure: extracted
/// from the poll system so the dashboard's reporting path is unit-testable.
pub fn consult_status(store: Option<&Arc<dyn DurableStore>>) -> DurableStatus {
    match store {
        Some(store) => DurableStatus {
            backend: store.backend().to_string(),
            persist_count: store.persist_count(),
            streams: STREAM_NAMES
                .iter()
                .map(|s| (s.to_string(), store.stream_len(s).unwrap_or(u64::MAX)))
                .collect(),
            snapshot_load_error: store.snapshot_load_error(),
        },
        None => DurableStatus {
            backend: "none".to_string(),
            persist_count: 0,
            streams: STREAM_NAMES.iter().map(|s| (s.to_string(), 0)).collect(),
            snapshot_load_error: None,
        },
    }
}

/// Poll the durable store on the cadence: open lazily and refresh the
/// consulted status.
pub fn poll_durable_status(
    time: Res<Time>,
    mut panel: ResMut<DurablePanel>,
    mut status: ResMut<DurableStatus>,
) {
    if !panel.poll.tick(time.delta()).just_finished() {
        return;
    }
    if panel.store.is_none() {
        panel.store = open_panel_store();
    }
    *status = consult_status(panel.store.as_ref());
}

/// Rewrite the chip text when the consulted status changes. A corrupt-snapshot
/// recovery is drawn in warning red so the operator cannot miss it.
pub fn update_durable_panel(
    status: Res<DurableStatus>,
    mut q: Query<(&mut Text, &mut TextColor), With<DurablePanelText>>,
) {
    if !status.is_changed() {
        return;
    }
    let Ok((mut text, mut color)) = q.single_mut() else {
        return;
    };
    let streams_line = status
        .streams
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(s, n)| format!("{s}:{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let first = if status.backend == "none" {
        "durable: none (RAM-only)".to_string()
    } else {
        format!("durable: {} persist {}", status.backend, status.persist_count)
    };
    text.0 = match &status.snapshot_load_error {
        Some(err) => {
            color.0 = Color::srgb(0.95, 0.45, 0.40);
            format!("{first}\n⚠ snapshot recovery: {err}\n{streams_line}")
        }
        None => {
            color.0 = Color::srgb(0.72, 0.78, 0.85);
            format!("{first}\n{streams_line}")
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory, cleaned up on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mathed-durable-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// E2E reporting path: a torn snapshot.bin on disk, the store opened the
    /// way the panel opens it, and the consulted status carries the recovery
    /// report — the exact data the dashboard chip renders in warning red.
    #[test]
    fn corrupt_snapshot_surfaces_in_dashboard_status() {
        let scratch = Scratch::new("corrupt");
        let dir = &scratch.0;
        std::fs::write(
            dir.join("snapshot.bin"),
            b"\x00garbage: not a loro snapshot",
        )
        .unwrap();

        let store = Arc::from(
            open_store(Some(dir), Backend::Loro).expect("loro open cannot fail"),
        );
        let status = consult_status(Some(&store));
        assert_eq!(status.backend, "loro");
        assert!(
            status
                .snapshot_load_error
                .as_deref()
                .unwrap_or_default()
                .contains("snapshot import failed"),
            "dashboard must surface the corruption: {:?}",
            status.snapshot_load_error
        );

        // The chip text is built from the consulted status — assert the
        // warning line appears in what the panel would render.
        let line = match &status.snapshot_load_error {
            Some(err) => format!("⚠ snapshot recovery: {err}"),
            None => String::new(),
        };
        assert!(line.contains("snapshot recovery"), "chip line: {line}");
    }

    /// The kernel's no-store shape: RAM-only, stable schema, no error.
    #[test]
    fn no_store_reports_ram_only() {
        let status = consult_status(None);
        assert_eq!(status.backend, "none");
        assert_eq!(status.persist_count, 0);
        assert_eq!(status.snapshot_load_error, None);
        assert!(status.streams.iter().all(|(_, n)| *n == 0));
    }
}
