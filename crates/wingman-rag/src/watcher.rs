//! Background file watcher: runs an initial reindex pass, then incrementally
//! re-embeds files as they change. Powered by `notify-debouncer-mini` so we
//! don't get hammered by editor save bursts.
//!
//! Spawn once at startup with [`spawn_background_indexer`]. The returned
//! [`WatcherHandle`] holds the debouncer alive — drop it to stop watching.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebounceEventResult, Debouncer};
use tokio::sync::mpsc;

use crate::Indexer;

/// Holds the debouncer + the indexing task. Dropping it stops the watcher;
/// the indexing task naturally finishes when its channel closes.
pub struct WatcherHandle {
    _debouncer: Debouncer<notify_debouncer_mini::notify::RecommendedWatcher>,
    _task: tokio::task::JoinHandle<()>,
}

/// Spawn the background indexer. Returns immediately; the initial reindex
/// happens inside the spawned task so startup isn't blocked.
pub fn spawn_background_indexer(
    indexer: Arc<Indexer>,
    root: PathBuf,
) -> std::result::Result<WatcherHandle, String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();

    let mut debouncer = new_debouncer(
        Duration::from_millis(500),
        move |res: DebounceEventResult| match res {
            Ok(events) => {
                let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
                if !paths.is_empty() {
                    let _ = tx.send(paths);
                }
            }
            Err(e) => tracing::warn!("watch error: {e:?}"),
        },
    )
    .map_err(|e| format!("notify: {e}"))?;

    debouncer
        .watcher()
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|e| format!("watch root: {e}"))?;

    let indexer_for_task = indexer.clone();
    let task = tokio::spawn(async move {
        // Initial pass.
        match indexer_for_task.reindex_repo().await {
            Ok(stats) => tracing::info!(
                "rag: initial index complete — {} files indexed, {} chunks",
                stats.files_indexed,
                stats.chunks_written
            ),
            Err(e) => tracing::warn!("rag: initial index failed: {e}"),
        }

        // Watch loop.
        while let Some(paths) = rx.recv().await {
            for path in paths {
                if let Err(e) = indexer_for_task.reindex_file(&path).await {
                    tracing::debug!("reindex {}: {}", path.display(), e);
                }
            }
        }
    });

    Ok(WatcherHandle {
        _debouncer: debouncer,
        _task: task,
    })
}
