use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{NaiveDate, Utc};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use walkdir::{DirEntry, WalkDir};

use crate::config::IndexConfig;
use crate::platform_windows::{is_hidden, is_system, metadata_attributes};

const MAX_WATCHER_EVENT_PATHS: usize = 256;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    pub extension: Option<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub include_hidden: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SearchItemType {
    File,
    Directory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexedEntry {
    pub path: String,
    pub name: String,
    pub path_lower: String,
    pub name_lower: String,
    pub extension: Option<String>,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub created_at: Option<i64>,
    pub modified_at: Option<i64>,
    pub accessed_at: Option<i64>,
    pub attributes: u32,
}

#[derive(Clone, Debug)]
pub struct SearchResult {
    pub entry: IndexedEntry,
    pub score: i64,
    pub item_type: SearchItemType,
}

#[derive(Clone, Debug, Default)]
pub struct IndexStatus {
    pub is_indexing: bool,
    pub last_scan_started: Option<i64>,
    pub last_scan_completed: Option<i64>,
    pub last_error: Option<String>,
    pub indexed_entries: usize,
    pub watched_roots: usize,
    pub snapshot_revision: u64,
    pub snapshot_loaded: bool,
}

pub struct SearchService {
    config: Arc<RwLock<IndexConfig>>,
    snapshot: Arc<RwLock<Vec<IndexedEntry>>>,
    status: Arc<Mutex<IndexStatus>>,
    runtime: Handle,
    watcher_stop: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    scan_in_progress: Arc<AtomicBool>,
    reindex_requested: Arc<AtomicBool>,
    config_revision: Arc<AtomicU64>,
}

impl SearchService {
    pub fn new(runtime: Handle, config: IndexConfig) -> anyhow::Result<Self> {
        initialize_database(&config.db_path)?;
        let indexed_entries = indexed_entry_count(&config.db_path)?;
        let snapshot = Arc::new(RwLock::new(Vec::new()));
        let status = Arc::new(Mutex::new(IndexStatus {
            indexed_entries,
            snapshot_loaded: indexed_entries == 0,
            ..Default::default()
        }));

        let service = Self {
            config: Arc::new(RwLock::new(config)),
            snapshot,
            status,
            runtime,
            watcher_stop: Arc::new(Mutex::new(None)),
            scan_in_progress: Arc::new(AtomicBool::new(false)),
            reindex_requested: Arc::new(AtomicBool::new(false)),
            config_revision: Arc::new(AtomicU64::new(0)),
        };

        service.start_watchers();
        if indexed_entries == 0 {
            service.reindex_now();
        } else {
            service.load_snapshot_async();
        }
        Ok(service)
    }

    pub fn update_config(&self, config: IndexConfig) {
        if let Ok(mut guard) = self.config.write() {
            *guard = config;
        }
        self.config_revision.fetch_add(1, Ordering::SeqCst);
        reset_snapshot_before_reindex(&self.snapshot, &self.status);
        self.start_watchers();
        self.reindex_now();
    }

    pub fn config(&self) -> IndexConfig {
        self.config
            .read()
            .map(|config| config.clone())
            .unwrap_or_else(|_| fallback_index_config())
    }

    pub fn reindex_now(&self) {
        self.reindex_requested.store(true, Ordering::SeqCst);
        if self
            .scan_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let config_lock = Arc::clone(&self.config);
        let snapshot = Arc::clone(&self.snapshot);
        let status = Arc::clone(&self.status);
        let scan_flag = Arc::clone(&self.scan_in_progress);
        let reindex_requested = Arc::clone(&self.reindex_requested);
        let config_revision = Arc::clone(&self.config_revision);
        self.runtime.spawn(async move {
            loop {
                reindex_requested.store(false, Ordering::SeqCst);
                let config = config_lock
                    .read()
                    .map(|guard| guard.clone())
                    .unwrap_or_else(|_| fallback_index_config());
                let scan_revision = config_revision.load(Ordering::SeqCst);

                {
                    if let Ok(mut guard) = status.lock() {
                        guard.is_indexing = true;
                        guard.last_error = None;
                        guard.last_scan_started = Some(Utc::now().timestamp());
                    }
                }

                let db_path = config.db_path.clone();
                let result = tokio::task::spawn_blocking(move || full_scan(&config)).await;
                match result {
                    Ok(Ok(_)) => {
                        let reload_result =
                            tokio::task::spawn_blocking(move || load_snapshot(&db_path)).await;
                        match reload_result {
                            Ok(Ok(items)) => {
                                apply_snapshot_if_current(
                                    &snapshot,
                                    &status,
                                    items,
                                    Some(Utc::now().timestamp()),
                                    &config_revision,
                                    scan_revision,
                                );
                            }
                            Ok(Err(error)) => {
                                set_status_error_if_current(
                                    &status,
                                    error.to_string(),
                                    true,
                                    &config_revision,
                                    scan_revision,
                                );
                            }
                            Err(join_error) => {
                                set_status_error_if_current(
                                    &status,
                                    join_error.to_string(),
                                    true,
                                    &config_revision,
                                    scan_revision,
                                );
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        set_status_error_if_current(
                            &status,
                            error.to_string(),
                            true,
                            &config_revision,
                            scan_revision,
                        );
                    }
                    Err(join_error) => {
                        set_status_error_if_current(
                            &status,
                            join_error.to_string(),
                            true,
                            &config_revision,
                            scan_revision,
                        );
                    }
                }

                if !reindex_requested.load(Ordering::SeqCst) {
                    scan_flag.store(false, Ordering::SeqCst);
                    if reindex_requested.swap(false, Ordering::SeqCst)
                        && scan_flag
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        continue;
                    }
                    break;
                }
            }
        });
    }

    pub fn search(&self, query: &SearchQuery, limit: usize) -> Vec<SearchResult> {
        self.snapshot
            .read()
            .map(|guard| search_snapshot(&guard, query, limit))
            .unwrap_or_default()
    }

    pub fn status(&self) -> IndexStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    pub fn indexed_entries(&self) -> usize {
        self.status
            .lock()
            .map(|status| status.indexed_entries)
            .unwrap_or_default()
    }

    fn start_watchers(&self) {
        let config = self.config();

        let stop_flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut watcher_guard) = self.watcher_stop.lock()
            && let Some(existing) = watcher_guard.replace(Arc::clone(&stop_flag))
        {
            existing.store(true, Ordering::SeqCst);
        }

        if let Ok(mut status_guard) = self.status.lock() {
            status_guard.watched_roots = config.roots.len();
        }

        let runtime = self.runtime.clone();
        let status = Arc::clone(&self.status);
        let snapshot = Arc::clone(&self.snapshot);
        let config_lock = Arc::clone(&self.config);
        let config_revision = Arc::clone(&self.config_revision);

        thread::spawn(move || {
            let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
            let mut watcher = match notify::recommended_watcher(tx) {
                Ok(watcher) => watcher,
                Err(error) => {
                    if let Ok(mut guard) = status.lock() {
                        guard.last_error = Some(format!(
                            "Échec de l'initialisation de la surveillance : {error}"
                        ));
                    }
                    return;
                }
            };

            for root in &config.roots {
                if root.exists()
                    && watcher.watch(root, RecursiveMode::Recursive).is_err()
                    && let Ok(mut guard) = status.lock()
                {
                    guard.last_error =
                        Some(format!("Échec de la surveillance de {}", root.display()));
                }
            }

            keep_watcher_alive(
                &mut watcher,
                rx,
                WatcherRuntime {
                    runtime,
                    config_lock,
                    snapshot,
                    status,
                    config_revision,
                    stop_flag,
                },
            );
        });
    }

    fn load_snapshot_async(&self) {
        let db_path = self.config().db_path;
        let snapshot = Arc::clone(&self.snapshot);
        let status = Arc::clone(&self.status);
        let config_revision = Arc::clone(&self.config_revision);
        let snapshot_revision = config_revision.load(Ordering::SeqCst);
        self.runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || load_snapshot(&db_path)).await;
            match result {
                Ok(Ok(items)) => {
                    apply_snapshot_if_current(
                        &snapshot,
                        &status,
                        items,
                        None,
                        &config_revision,
                        snapshot_revision,
                    );
                }
                Ok(Err(error)) => {
                    set_status_error_if_current(
                        &status,
                        error.to_string(),
                        false,
                        &config_revision,
                        snapshot_revision,
                    );
                }
                Err(join_error) => {
                    set_status_error_if_current(
                        &status,
                        join_error.to_string(),
                        false,
                        &config_revision,
                        snapshot_revision,
                    );
                }
            }
        });
    }
}

fn fallback_index_config() -> IndexConfig {
    IndexConfig {
        roots: Vec::new(),
        exclusions: Vec::new(),
        include_hidden: false,
        include_system: false,
        scan_concurrency: 1,
        db_path: PathBuf::new(),
    }
}

impl Drop for SearchService {
    fn drop(&mut self) {
        stop_current_watcher(&self.watcher_stop);
    }
}

struct WatcherRuntime {
    runtime: Handle,
    config_lock: Arc<RwLock<IndexConfig>>,
    snapshot: Arc<RwLock<Vec<IndexedEntry>>>,
    status: Arc<Mutex<IndexStatus>>,
    config_revision: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
}

fn keep_watcher_alive(
    _watcher: &mut RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<Event>>,
    context: WatcherRuntime,
) {
    while !context.stop_flag.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(event)) => {
                let mut paths = event.paths.clone();
                let debounce_until = Instant::now() + Duration::from_millis(350);
                while !context.stop_flag.load(Ordering::SeqCst) {
                    let remaining = debounce_until.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
                        Ok(Ok(event)) => paths.extend(event.paths),
                        Ok(Err(error)) => {
                            if let Ok(mut guard) = context.status.lock() {
                                guard.last_error =
                                    Some(format!("Erreur de surveillance : {error}"));
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                let config = context.config_lock.read().map(|guard| guard.clone());
                if let Ok(config) = config {
                    let paths = coalesce_changed_paths(paths, &config.roots);
                    if paths.is_empty() {
                        continue;
                    }
                    let event_revision = context.config_revision.load(Ordering::SeqCst);
                    let snapshot = Arc::clone(&context.snapshot);
                    let status = Arc::clone(&context.status);
                    let config_revision = Arc::clone(&context.config_revision);
                    context.runtime.spawn(async move {
                        let db_path = config.db_path.clone();
                        let result =
                            tokio::task::spawn_blocking(move || sync_paths(&config, &paths)).await;
                        if let Ok(Ok(())) = result {
                            let reload_result =
                                tokio::task::spawn_blocking(move || load_snapshot(&db_path)).await;
                            match reload_result {
                                Ok(Ok(items)) => {
                                    apply_snapshot_if_current(
                                        &snapshot,
                                        &status,
                                        items,
                                        None,
                                        &config_revision,
                                        event_revision,
                                    );
                                }
                                Ok(Err(error)) => {
                                    set_status_error_if_current(
                                        &status,
                                        error.to_string(),
                                        false,
                                        &config_revision,
                                        event_revision,
                                    );
                                }
                                Err(join_error) => {
                                    set_status_error_if_current(
                                        &status,
                                        join_error.to_string(),
                                        false,
                                        &config_revision,
                                        event_revision,
                                    );
                                }
                            }
                        } else if let Ok(Err(error)) = result {
                            set_status_error_if_current(
                                &status,
                                error.to_string(),
                                false,
                                &config_revision,
                                event_revision,
                            );
                        }
                    });
                }
            }
            Ok(Err(error)) => {
                if let Ok(mut guard) = context.status.lock() {
                    guard.last_error = Some(format!("Erreur de surveillance : {error}"));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn stop_current_watcher(watcher_stop: &Arc<Mutex<Option<Arc<AtomicBool>>>>) -> bool {
    watcher_stop
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
        .map(|stop_flag| {
            stop_flag.store(true, Ordering::SeqCst);
            true
        })
        .unwrap_or(false)
}

fn coalesce_changed_paths(paths: Vec<PathBuf>, roots: &[PathBuf]) -> Vec<PathBuf> {
    let root_prefixes = roots
        .iter()
        .map(|root| normalized_path_string(root).to_lowercase())
        .collect::<Vec<_>>();
    let mut unique = paths
        .into_iter()
        .filter_map(|path| {
            let normalized = normalized_path_string(&path).to_lowercase();
            if !root_prefixes.is_empty() && !path_is_under_any_root(&normalized, &root_prefixes) {
                return None;
            }
            Some((normalized, path))
        })
        .collect::<Vec<_>>();

    unique.sort_by(|left, right| left.0.cmp(&right.0));
    unique.dedup_by(|left, right| left.0 == right.0);

    let mut coalesced: Vec<(String, PathBuf)> = Vec::new();
    for (normalized, path) in unique {
        if coalesced
            .iter()
            .any(|(ancestor, _)| path_is_under_root(&normalized, ancestor))
        {
            continue;
        }
        coalesced.push((normalized, path));
        if coalesced.len() >= MAX_WATCHER_EVENT_PATHS {
            break;
        }
    }

    coalesced.into_iter().map(|(_, path)| path).collect()
}

pub fn initialize_database(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("échec de la création de {}", parent.display()))?;
    }
    let connection = open_connection(path)?;
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS file_index (
            path TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            path_lower TEXT NOT NULL,
            name_lower TEXT NOT NULL,
            extension TEXT,
            is_dir INTEGER NOT NULL,
            size_bytes INTEGER NOT NULL,
            created_at INTEGER,
            modified_at INTEGER,
            accessed_at INTEGER,
            attributes INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_file_index_name_lower ON file_index(name_lower);
        CREATE INDEX IF NOT EXISTS idx_file_index_path_lower ON file_index(path_lower);
        ",
    )?;
    Ok(())
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let connection = Connection::open(path)
        .with_context(|| format!("échec de l'ouverture de {}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .context("échec de la configuration du délai SQLite")?;
    Ok(connection)
}

fn indexed_entry_count(path: &Path) -> anyhow::Result<usize> {
    let connection = open_connection(path)?;
    let count = connection.query_row("SELECT COUNT(*) FROM file_index", [], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(count.max(0) as usize)
}

fn apply_snapshot(
    snapshot: &Arc<RwLock<Vec<IndexedEntry>>>,
    status: &Arc<Mutex<IndexStatus>>,
    items: Vec<IndexedEntry>,
    completed_at: Option<i64>,
) {
    let indexed_entries = items.len();
    if let Ok(mut snapshot_guard) = snapshot.write() {
        *snapshot_guard = items;
    }
    if let Ok(mut status_guard) = status.lock() {
        status_guard.indexed_entries = indexed_entries;
        status_guard.snapshot_loaded = true;
        status_guard.snapshot_revision = status_guard.snapshot_revision.wrapping_add(1);
        status_guard.last_error = None;
        if let Some(timestamp) = completed_at {
            status_guard.last_scan_completed = Some(timestamp);
            status_guard.is_indexing = false;
        }
    }
}

fn apply_snapshot_if_current(
    snapshot: &Arc<RwLock<Vec<IndexedEntry>>>,
    status: &Arc<Mutex<IndexStatus>>,
    items: Vec<IndexedEntry>,
    completed_at: Option<i64>,
    config_revision: &AtomicU64,
    expected_revision: u64,
) -> bool {
    if config_revision.load(Ordering::SeqCst) != expected_revision {
        return false;
    }
    apply_snapshot(snapshot, status, items, completed_at);
    true
}

fn reset_snapshot_before_reindex(
    snapshot: &Arc<RwLock<Vec<IndexedEntry>>>,
    status: &Arc<Mutex<IndexStatus>>,
) {
    if let Ok(mut snapshot_guard) = snapshot.write() {
        snapshot_guard.clear();
    }
    if let Ok(mut status_guard) = status.lock() {
        status_guard.indexed_entries = 0;
        status_guard.last_error = None;
        status_guard.snapshot_loaded = false;
        status_guard.snapshot_revision = status_guard.snapshot_revision.wrapping_add(1);
    }
}

fn set_status_error(status: &Arc<Mutex<IndexStatus>>, error: String, clear_indexing: bool) {
    if let Ok(mut status_guard) = status.lock() {
        status_guard.last_error = Some(error);
        if clear_indexing {
            status_guard.is_indexing = false;
        }
    }
}

fn set_status_error_if_current(
    status: &Arc<Mutex<IndexStatus>>,
    error: String,
    clear_indexing: bool,
    config_revision: &AtomicU64,
    expected_revision: u64,
) -> bool {
    if config_revision.load(Ordering::SeqCst) != expected_revision {
        return false;
    }
    set_status_error(status, error, clear_indexing);
    true
}

pub fn full_scan(config: &IndexConfig) -> anyhow::Result<usize> {
    initialize_database(&config.db_path)?;
    let mut connection = open_connection(&config.db_path)?;
    let mut indexed_entries = 0usize;
    let active_roots = config
        .roots
        .iter()
        .filter(|root| root.exists())
        .cloned()
        .collect::<Vec<_>>();

    for root in &active_roots {
        let existing = existing_paths_under_root(&connection, root)?;
        let mut seen = HashSet::new();
        let transaction = connection.transaction()?;
        {
            let mut statement = transaction.prepare(
                "
                INSERT INTO file_index (
                    path, name, path_lower, name_lower, extension, is_dir, size_bytes,
                    created_at, modified_at, accessed_at, attributes
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(path) DO UPDATE SET
                    name = excluded.name,
                    path_lower = excluded.path_lower,
                    name_lower = excluded.name_lower,
                    extension = excluded.extension,
                    is_dir = excluded.is_dir,
                    size_bytes = excluded.size_bytes,
                    created_at = excluded.created_at,
                    modified_at = excluded.modified_at,
                    accessed_at = excluded.accessed_at,
                    attributes = excluded.attributes
                ",
            )?;

            for entry in WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|dir_entry| should_descend(dir_entry, config))
            {
                let Ok(entry) = entry else {
                    continue;
                };

                let Ok(indexed) = entry_to_indexed_entry(entry.path(), config) else {
                    continue;
                };

                seen.insert(indexed.path.clone());
                indexed_entries += 1;
                upsert_entry(&mut statement, &indexed)?;
            }
        }
        transaction.commit()?;

        let missing: Vec<String> = existing.difference(&seen).cloned().collect();
        delete_paths(&mut connection, &missing)?;
    }
    delete_paths_outside_roots(&mut connection, &active_roots)?;

    Ok(indexed_entries)
}

pub fn sync_paths(config: &IndexConfig, paths: &[PathBuf]) -> anyhow::Result<()> {
    initialize_database(&config.db_path)?;
    let mut connection = open_connection(&config.db_path)?;
    let transaction = connection.transaction()?;
    let paths = coalesce_changed_paths(paths.to_vec(), &config.roots);
    {
        let mut statement = transaction.prepare(
            "
            INSERT INTO file_index (
                path, name, path_lower, name_lower, extension, is_dir, size_bytes,
                created_at, modified_at, accessed_at, attributes
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(path) DO UPDATE SET
                name = excluded.name,
                path_lower = excluded.path_lower,
                name_lower = excluded.name_lower,
                extension = excluded.extension,
                is_dir = excluded.is_dir,
                size_bytes = excluded.size_bytes,
                created_at = excluded.created_at,
                modified_at = excluded.modified_at,
                accessed_at = excluded.accessed_at,
                attributes = excluded.attributes
            ",
        )?;

        for path in &paths {
            sync_single_path(&transaction, &mut statement, config, path)?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn sync_single_path(
    transaction: &rusqlite::Transaction<'_>,
    statement: &mut rusqlite::Statement<'_>,
    config: &IndexConfig,
    path: &Path,
) -> anyhow::Result<()> {
    if !path.exists() {
        remove_path_prefix(transaction, path)?;
        return Ok(());
    }

    if should_skip_path(path, config) {
        remove_path_prefix(transaction, path)?;
        return Ok(());
    }

    if path.is_dir() {
        let prefix = normalized_path_string(path);
        let existing = existing_paths_under_prefix(transaction, &prefix)?;
        let mut seen = HashSet::new();
        for entry in WalkDir::new(path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|dir_entry| should_descend(dir_entry, config))
        {
            let Ok(entry) = entry else {
                continue;
            };
            let Ok(indexed) = entry_to_indexed_entry(entry.path(), config) else {
                continue;
            };
            seen.insert(indexed.path.clone());
            upsert_entry(statement, &indexed)?;
        }
        let missing: Vec<String> = existing.difference(&seen).cloned().collect();
        for missing_path in missing {
            transaction.execute(
                "DELETE FROM file_index WHERE path = ?1",
                params![missing_path],
            )?;
        }
        return Ok(());
    }

    if let Ok(indexed) = entry_to_indexed_entry(path, config) {
        upsert_entry(statement, &indexed)?;
    }
    Ok(())
}

fn upsert_entry(
    statement: &mut rusqlite::Statement<'_>,
    entry: &IndexedEntry,
) -> anyhow::Result<()> {
    statement.execute(params![
        entry.path,
        entry.name,
        entry.path_lower,
        entry.name_lower,
        entry.extension,
        entry.is_dir as i32,
        entry.size_bytes as i64,
        entry.created_at,
        entry.modified_at,
        entry.accessed_at,
        entry.attributes as i64,
    ])?;
    Ok(())
}

fn load_snapshot(path: &Path) -> anyhow::Result<Vec<IndexedEntry>> {
    initialize_database(path)?;
    let connection = open_connection(path)?;
    let mut statement = connection.prepare(
        "
        SELECT path, name, path_lower, name_lower, extension, is_dir, size_bytes,
               created_at, modified_at, accessed_at, attributes
        FROM file_index
        ",
    )?;

    let rows = statement.query_map([], |row| {
        Ok(IndexedEntry {
            path: row.get(0)?,
            name: row.get(1)?,
            path_lower: row.get(2)?,
            name_lower: row.get(3)?,
            extension: row.get(4)?,
            is_dir: row.get::<_, i32>(5)? != 0,
            size_bytes: row.get::<_, i64>(6)? as u64,
            created_at: row.get(7)?,
            modified_at: row.get(8)?,
            accessed_at: row.get(9)?,
            attributes: row.get::<_, i64>(10)? as u32,
        })
    })?;

    let mut items = Vec::new();
    for item in rows {
        items.push(item?);
    }
    Ok(items)
}

fn existing_paths_under_root(
    connection: &Connection,
    root: &Path,
) -> anyhow::Result<HashSet<String>> {
    existing_paths_under_prefix(connection, &normalized_path_string(root))
}

fn existing_paths_under_prefix(
    connection: &Connection,
    prefix: &str,
) -> anyhow::Result<HashSet<String>> {
    let mut statement = connection
        .prepare("SELECT path FROM file_index WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'")?;
    let like = child_prefix_like(prefix);
    let rows = statement.query_map(params![prefix, like], |row| row.get::<_, String>(0))?;

    let mut values = HashSet::new();
    for row in rows {
        values.insert(row?);
    }
    Ok(values)
}

fn delete_paths(connection: &mut Connection, paths: &[String]) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    {
        let mut statement = transaction.prepare("DELETE FROM file_index WHERE path = ?1")?;
        for path in paths {
            statement.execute(params![path])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn delete_paths_outside_roots(
    connection: &mut Connection,
    roots: &[PathBuf],
) -> anyhow::Result<()> {
    let root_prefixes = roots
        .iter()
        .map(|root| normalized_path_string(root).to_lowercase())
        .collect::<Vec<_>>();
    let stale_paths = {
        let mut statement = connection.prepare("SELECT path, path_lower FROM file_index")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut stale_paths = Vec::new();
        for row in rows {
            let (path, path_lower) = row?;
            if !path_is_under_any_root(&path_lower, &root_prefixes) {
                stale_paths.push(path);
            }
        }
        stale_paths
    };
    delete_paths(connection, &stale_paths)
}

fn remove_path_prefix(connection: &Connection, path: &Path) -> anyhow::Result<()> {
    let prefix = normalized_path_string(path);
    let like = child_prefix_like(&prefix);
    connection.execute(
        "DELETE FROM file_index WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
        params![prefix, like],
    )?;
    Ok(())
}

fn should_descend(entry: &DirEntry, config: &IndexConfig) -> bool {
    !should_skip_path(entry.path(), config)
}

fn should_skip_path(path: &Path, config: &IndexConfig) -> bool {
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };

    if config
        .exclusions
        .iter()
        .any(|pattern| file_name.eq_ignore_ascii_case(pattern))
    {
        return true;
    }

    if let Ok(metadata) = fs::metadata(path) {
        let attributes = metadata_attributes(&metadata);
        if !config.include_hidden && is_hidden(attributes) {
            return true;
        }
        if !config.include_system && is_system(attributes) {
            return true;
        }
    }

    false
}

fn entry_to_indexed_entry(path: &Path, config: &IndexConfig) -> anyhow::Result<IndexedEntry> {
    if should_skip_path(path, config) {
        anyhow::bail!("chemin ignoré");
    }

    let metadata = fs::metadata(path)
        .with_context(|| format!("échec de la lecture des métadonnées de {}", path.display()))?;
    let attributes = metadata_attributes(&metadata);
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| path.display().to_string());
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_lowercase);
    let path_str = normalized_path_string(path);
    Ok(IndexedEntry {
        path_lower: path_str.to_lowercase(),
        name_lower: name.to_lowercase(),
        path: path_str,
        name,
        extension,
        is_dir: metadata.is_dir(),
        size_bytes: metadata.len(),
        created_at: metadata.created().ok().and_then(system_time_to_unix),
        modified_at: metadata.modified().ok().and_then(system_time_to_unix),
        accessed_at: metadata.accessed().ok().and_then(system_time_to_unix),
        attributes,
    })
}

pub fn search_snapshot(
    snapshot: &[IndexedEntry],
    query: &SearchQuery,
    limit: usize,
) -> Vec<SearchResult> {
    let normalized_text = normalize_query_text(&query.text);
    let normalized_extension = query
        .extension
        .as_ref()
        .map(|extension| {
            extension
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .filter(|extension| !extension.is_empty());

    if limit == 0 {
        return Vec::new();
    }

    let mut results = Vec::<SearchResult>::with_capacity(limit.min(200));
    for entry in snapshot {
        if !matches_query(
            entry,
            query,
            normalized_text.as_deref(),
            normalized_extension.as_deref(),
        ) {
            continue;
        }

        let candidate = SearchResult {
            score: score_entry(entry, normalized_text.as_deref()),
            item_type: if entry.is_dir {
                SearchItemType::Directory
            } else {
                SearchItemType::File
            },
            entry: entry.clone(),
        };

        if results.len() < limit {
            results.push(candidate);
            results.sort_by(compare_search_results);
        } else if results
            .last()
            .map(|worst| compare_search_results(&candidate, worst).is_lt())
            .unwrap_or(true)
        {
            results.pop();
            results.push(candidate);
            results.sort_by(compare_search_results);
        }
    }

    results
}

fn compare_search_results(left: &SearchResult, right: &SearchResult) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.entry.name.cmp(&right.entry.name))
        .then_with(|| left.entry.path.cmp(&right.entry.path))
}

pub fn normalize_query_text(value: &str) -> Option<String> {
    let normalized = value.trim().to_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub fn parse_date_filter(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|datetime| datetime.and_utc().timestamp())
}

pub fn score_entry(entry: &IndexedEntry, normalized_text: Option<&str>) -> i64 {
    let Some(needle) = normalized_text else {
        return 0;
    };
    if entry.name_lower.starts_with(needle) {
        return 300 - (entry.name_lower.len().saturating_sub(needle.len()) as i64);
    }
    if entry.name_lower.contains(needle) {
        return 200;
    }
    if entry.path_lower.contains(needle) {
        return 100;
    }
    -1
}

pub fn matches_query(
    entry: &IndexedEntry,
    query: &SearchQuery,
    normalized_text: Option<&str>,
    normalized_extension: Option<&str>,
) -> bool {
    if let Some(needle) = normalized_text
        && !entry.name_lower.contains(needle)
        && !entry.path_lower.contains(needle)
    {
        return false;
    }

    if let Some(extension) = normalized_extension
        && entry.extension.as_deref() != Some(extension)
    {
        return false;
    }

    if let Some(min_size) = query.min_size
        && entry.size_bytes < min_size
    {
        return false;
    }

    if let Some(max_size) = query.max_size
        && entry.size_bytes > max_size
    {
        return false;
    }

    if let Some(modified_after) = query.modified_after
        && entry.modified_at.unwrap_or_default() < modified_after
    {
        return false;
    }

    if let Some(modified_before) = query.modified_before
        && entry.modified_at.unwrap_or(i64::MAX) > modified_before
    {
        return false;
    }

    if !query.include_hidden && is_hidden(entry.attributes) {
        return false;
    }

    true
}

fn normalized_path_string(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

fn path_is_under_any_root(path_lower: &str, root_prefixes: &[String]) -> bool {
    root_prefixes
        .iter()
        .any(|root| path_is_under_root(path_lower, root))
}

fn path_is_under_root(path_lower: &str, root_lower: &str) -> bool {
    if path_lower == root_lower {
        return true;
    }
    if root_lower.ends_with('\\') {
        return path_lower.starts_with(root_lower);
    }
    path_lower
        .strip_prefix(root_lower)
        .map(|remaining| remaining.starts_with('\\'))
        .unwrap_or(false)
}

fn system_time_to_unix(time: std::time::SystemTime) -> Option<i64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

fn child_prefix_like(prefix: &str) -> String {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    if prefix.ends_with('\\') {
        format!("{escaped}%")
    } else {
        format!("{escaped}\\\\%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use tempfile::tempdir;

    fn test_config(root: &Path, db_path: &Path) -> IndexConfig {
        IndexConfig {
            roots: vec![root.to_path_buf()],
            exclusions: vec!["ignore-me".into()],
            include_hidden: true,
            include_system: true,
            scan_concurrency: 1,
            db_path: db_path.to_path_buf(),
        }
    }

    fn sample_entry(name: &str, path: &str) -> IndexedEntry {
        IndexedEntry {
            path: path.to_owned(),
            name: name.to_owned(),
            path_lower: path.to_lowercase(),
            name_lower: name.to_lowercase(),
            extension: Some("txt".to_owned()),
            is_dir: false,
            size_bytes: 128,
            created_at: Some(100),
            modified_at: Some(100),
            accessed_at: Some(100),
            attributes: 0,
        }
    }

    #[test]
    fn query_normalization_trims_and_lowercases() {
        assert_eq!(normalize_query_text("  Foo Bar "), Some("foo bar".into()));
        assert_eq!(normalize_query_text("  École "), Some("école".into()));
        assert_eq!(normalize_query_text("   "), None);
    }

    #[test]
    fn scoring_prefers_name_prefix_then_name_then_path() {
        let prefix = sample_entry("report.txt", "C:\\logs\\report.txt");
        let contains = sample_entry("yearly-report.txt", "C:\\logs\\yearly-report.txt");
        let path_only = sample_entry("archive.txt", "C:\\reports\\archive.txt");

        assert!(score_entry(&prefix, Some("rep")) > score_entry(&contains, Some("rep")));
        assert!(score_entry(&contains, Some("rep")) > score_entry(&path_only, Some("rep")));
    }

    #[test]
    fn search_snapshot_keeps_bounded_top_results_in_score_order() {
        let snapshot = vec![
            sample_entry("yearly-report.txt", "C:\\logs\\yearly-report.txt"),
            sample_entry("report.txt", "C:\\logs\\report.txt"),
            sample_entry("archive.txt", "C:\\reports\\archive.txt"),
            sample_entry("report-final.txt", "C:\\logs\\report-final.txt"),
        ];
        let query = SearchQuery {
            text: "rep".into(),
            include_hidden: true,
            ..SearchQuery::default()
        };

        let results = search_snapshot(&snapshot, &query, 2);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].entry.name, "report.txt");
        assert_eq!(results[1].entry.name, "report-final.txt");
        assert!(results[0].score >= results[1].score);
    }

    #[test]
    fn filters_apply_extension_size_and_date() {
        let query = SearchQuery {
            text: "report".into(),
            extension: Some("txt".into()),
            min_size: Some(100),
            max_size: Some(200),
            modified_after: Some(50),
            modified_before: Some(150),
            include_hidden: false,
        };
        let entry = sample_entry("report.txt", "C:\\logs\\report.txt");
        assert!(matches_query(&entry, &query, Some("report"), Some("txt")));
    }

    #[test]
    fn prefix_matching_respects_directory_boundaries() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let db_path = temp.path().join("index.db");
        initialize_database(&db_path)?;
        let mut connection = open_connection(&db_path)?;
        let transaction = connection.transaction()?;
        {
            let mut statement = transaction.prepare(
                "
                INSERT INTO file_index (
                    path, name, path_lower, name_lower, extension, is_dir, size_bytes,
                    created_at, modified_at, accessed_at, attributes
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ",
            )?;

            for entry in [
                sample_entry("alpha.txt", r"C:\Users\alpha.txt"),
                sample_entry("beta.txt", r"C:\Users\team\beta.txt"),
                sample_entry("wrong.txt", r"C:\Users2\wrong.txt"),
            ] {
                upsert_entry(&mut statement, &entry)?;
            }
        }
        transaction.commit()?;

        let matches = existing_paths_under_prefix(&connection, r"C:\Users")?;
        assert!(matches.contains(r"C:\Users\alpha.txt"));
        assert!(matches.contains(r"C:\Users\team\beta.txt"));
        assert!(!matches.contains(r"C:\Users2\wrong.txt"));
        Ok(())
    }

    #[test]
    fn changed_paths_are_deduplicated_bounded_and_pruned_to_roots() {
        let root = PathBuf::from(r"C:\Data");
        let paths = vec![
            PathBuf::from(r"C:\Data\folder\child.txt"),
            PathBuf::from(r"C:\Data\folder"),
            PathBuf::from(r"C:\Data\FOLDER"),
            PathBuf::from(r"C:\Outside\ignored.txt"),
        ];

        let coalesced = coalesce_changed_paths(paths, &[root]);

        assert_eq!(coalesced.len(), 1);
        assert_eq!(
            normalized_path_string(&coalesced[0]).to_lowercase(),
            r"c:\data\folder"
        );
    }

    #[test]
    fn reset_snapshot_before_reindex_marks_snapshot_unloaded() {
        let snapshot = Arc::new(RwLock::new(vec![sample_entry(
            "alpha.txt",
            r"C:\Users\alpha.txt",
        )]));
        let status = Arc::new(Mutex::new(IndexStatus {
            indexed_entries: 1,
            last_error: Some("ancien probleme".into()),
            snapshot_revision: 7,
            snapshot_loaded: true,
            ..IndexStatus::default()
        }));

        reset_snapshot_before_reindex(&snapshot, &status);

        assert!(snapshot.read().unwrap().is_empty());
        let status = status.lock().unwrap();
        assert_eq!(status.indexed_entries, 0);
        assert!(status.last_error.is_none());
        assert!(!status.snapshot_loaded);
        assert_eq!(status.snapshot_revision, 8);
    }

    #[test]
    fn stale_snapshot_and_errors_do_not_override_current_revision() {
        let snapshot = Arc::new(RwLock::new(vec![sample_entry(
            "current.txt",
            r"C:\Users\current.txt",
        )]));
        let status = Arc::new(Mutex::new(IndexStatus {
            indexed_entries: 1,
            snapshot_loaded: true,
            ..IndexStatus::default()
        }));
        let config_revision = AtomicU64::new(2);

        let applied = apply_snapshot_if_current(
            &snapshot,
            &status,
            vec![sample_entry("old.txt", r"C:\Users\old.txt")],
            Some(100),
            &config_revision,
            1,
        );
        assert!(!applied);
        assert_eq!(snapshot.read().unwrap()[0].name, "current.txt");

        let error_applied = set_status_error_if_current(
            &status,
            "ancienne erreur".into(),
            true,
            &config_revision,
            1,
        );
        assert!(!error_applied);
        assert!(status.lock().unwrap().last_error.is_none());
    }

    #[test]
    fn stop_current_watcher_signals_thread_shutdown() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let watcher_stop = Arc::new(Mutex::new(Some(Arc::clone(&stop_flag))));

        assert!(stop_current_watcher(&watcher_stop));
        assert!(stop_flag.load(Ordering::SeqCst));
        assert!(watcher_stop.lock().unwrap().is_none());
        assert!(!stop_current_watcher(&watcher_stop));
    }

    #[test]
    fn full_scan_builds_db_and_sync_updates_it() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("scan-root");
        let db_path = temp.path().join("index.db");
        fs::create_dir_all(&root)?;
        fs::write(root.join("alpha.txt"), "alpha")?;
        fs::create_dir_all(root.join("folder"))?;
        fs::write(root.join("folder").join("beta.log"), "beta")?;

        let config = test_config(&root, &db_path);
        let indexed = full_scan(&config)?;
        assert!(indexed >= 3);

        let snapshot = load_snapshot(&db_path)?;
        assert!(snapshot.iter().any(|entry| entry.name == "alpha.txt"));

        fs::remove_file(root.join("alpha.txt"))?;
        fs::write(root.join("folder").join("gamma.txt"), "gamma")?;
        sync_paths(&config, &[root.join("alpha.txt"), root.join("folder")])?;

        let snapshot = load_snapshot(&db_path)?;
        assert!(!snapshot.iter().any(|entry| entry.name == "alpha.txt"));
        assert!(snapshot.iter().any(|entry| entry.name == "gamma.txt"));
        Ok(())
    }

    #[test]
    fn full_scan_removes_entries_outside_active_roots() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let first_root = temp.path().join("first-root");
        let second_root = temp.path().join("second-root");
        let db_path = temp.path().join("index.db");
        fs::create_dir_all(&first_root)?;
        fs::create_dir_all(&second_root)?;
        fs::write(first_root.join("alpha.txt"), "alpha")?;
        fs::write(second_root.join("beta.txt"), "beta")?;

        let mut config = test_config(&first_root, &db_path);
        config.roots.push(second_root.clone());
        full_scan(&config)?;
        let snapshot = load_snapshot(&db_path)?;
        assert!(snapshot.iter().any(|entry| entry.name == "alpha.txt"));
        assert!(snapshot.iter().any(|entry| entry.name == "beta.txt"));

        config.roots = vec![second_root];
        full_scan(&config)?;
        let snapshot = load_snapshot(&db_path)?;
        assert!(!snapshot.iter().any(|entry| entry.name == "alpha.txt"));
        assert!(snapshot.iter().any(|entry| entry.name == "beta.txt"));
        Ok(())
    }

    #[test]
    fn full_scan_clears_index_when_no_configured_root_is_active() -> anyhow::Result<()> {
        let temp = tempdir()?;
        let root = temp.path().join("scan-root");
        let db_path = temp.path().join("index.db");
        fs::create_dir_all(&root)?;
        fs::write(root.join("alpha.txt"), "alpha")?;

        let config = test_config(&root, &db_path);
        full_scan(&config)?;
        assert!(!load_snapshot(&db_path)?.is_empty());

        fs::remove_dir_all(&root)?;
        full_scan(&config)?;
        assert!(load_snapshot(&db_path)?.is_empty());
        Ok(())
    }
}
