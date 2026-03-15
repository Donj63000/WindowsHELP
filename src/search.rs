use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use chrono::{NaiveDate, Utc};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use walkdir::{DirEntry, WalkDir};

use crate::config::IndexConfig;
use crate::platform_windows::{is_hidden, is_system, metadata_attributes};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    pub extension: Option<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub include_hidden: bool,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            extension: None,
            min_size: None,
            max_size: None,
            modified_after: None,
            modified_before: None,
            include_hidden: false,
        }
    }
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
        if let Ok(mut snapshot_guard) = self.snapshot.write() {
            snapshot_guard.clear();
        }
        if let Ok(mut status_guard) = self.status.lock() {
            status_guard.indexed_entries = 0;
            status_guard.last_error = None;
            status_guard.snapshot_loaded = true;
            status_guard.snapshot_revision = status_guard.snapshot_revision.wrapping_add(1);
        }
        self.start_watchers();
        self.reindex_now();
    }

    pub fn config(&self) -> IndexConfig {
        self.config
            .read()
            .map(|config| config.clone())
            .unwrap_or_else(|_| IndexConfig {
                roots: Vec::new(),
                exclusions: Vec::new(),
                include_hidden: false,
                include_system: false,
                scan_concurrency: 1,
                db_path: PathBuf::new(),
            })
    }

    pub fn reindex_now(&self) {
        if self
            .scan_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let config = self.config();
        let snapshot = Arc::clone(&self.snapshot);
        let status = Arc::clone(&self.status);
        let scan_flag = Arc::clone(&self.scan_in_progress);
        self.runtime.spawn(async move {
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
                            apply_snapshot(
                                &snapshot,
                                &status,
                                items,
                                Some(Utc::now().timestamp()),
                            );
                        }
                        Ok(Err(error)) => set_status_error(&status, error.to_string(), true),
                        Err(join_error) => {
                            set_status_error(&status, join_error.to_string(), true);
                        }
                    }
                }
                Ok(Err(error)) => {
                    set_status_error(&status, error.to_string(), true);
                }
                Err(join_error) => {
                    set_status_error(&status, join_error.to_string(), true);
                }
            }
            scan_flag.store(false, Ordering::SeqCst);
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
        if let Ok(mut watcher_guard) = self.watcher_stop.lock() {
            if let Some(existing) = watcher_guard.replace(Arc::clone(&stop_flag)) {
                existing.store(true, Ordering::SeqCst);
            }
        }

        if let Ok(mut status_guard) = self.status.lock() {
            status_guard.watched_roots = config.roots.len();
        }

        let runtime = self.runtime.clone();
        let status = Arc::clone(&self.status);
        let snapshot = Arc::clone(&self.snapshot);
        let config_lock = Arc::clone(&self.config);

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
                if root.exists() && watcher.watch(root, RecursiveMode::Recursive).is_err() {
                    if let Ok(mut guard) = status.lock() {
                        guard.last_error =
                            Some(format!("Échec de la surveillance de {}", root.display()));
                    }
                }
            }

            keep_watcher_alive(
                &mut watcher,
                rx,
                runtime,
                config_lock,
                snapshot,
                status,
                stop_flag,
            );
        });
    }

    fn load_snapshot_async(&self) {
        let db_path = self.config().db_path;
        let snapshot = Arc::clone(&self.snapshot);
        let status = Arc::clone(&self.status);
        self.runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || load_snapshot(&db_path)).await;
            match result {
                Ok(Ok(items)) => apply_snapshot(&snapshot, &status, items, None),
                Ok(Err(error)) => set_status_error(&status, error.to_string(), false),
                Err(join_error) => set_status_error(&status, join_error.to_string(), false),
            }
        });
    }
}

fn keep_watcher_alive(
    _watcher: &mut RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<Event>>,
    runtime: Handle,
    config_lock: Arc<RwLock<IndexConfig>>,
    snapshot: Arc<RwLock<Vec<IndexedEntry>>>,
    status: Arc<Mutex<IndexStatus>>,
    stop_flag: Arc<AtomicBool>,
) {
    while !stop_flag.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(event)) => {
                let paths = event.paths.clone();
                let config = config_lock.read().map(|guard| guard.clone());
                if let Ok(config) = config {
                    let snapshot = Arc::clone(&snapshot);
                    let status = Arc::clone(&status);
                    runtime.spawn(async move {
                        let db_path = config.db_path.clone();
                        let result =
                            tokio::task::spawn_blocking(move || sync_paths(&config, &paths)).await;
                        if let Ok(Ok(())) = result {
                            let reload_result =
                                tokio::task::spawn_blocking(move || load_snapshot(&db_path)).await;
                            match reload_result {
                                Ok(Ok(items)) => apply_snapshot(&snapshot, &status, items, None),
                                Ok(Err(error)) => {
                                    set_status_error(&status, error.to_string(), false);
                                }
                                Err(join_error) => {
                                    set_status_error(&status, join_error.to_string(), false);
                                }
                            }
                        } else if let Ok(Err(error)) = result {
                            set_status_error(&status, error.to_string(), false);
                        }
                    });
                }
            }
            Ok(Err(error)) => {
                if let Ok(mut guard) = status.lock() {
                    guard.last_error = Some(format!("Erreur de surveillance : {error}"));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
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

fn set_status_error(status: &Arc<Mutex<IndexStatus>>, error: String, clear_indexing: bool) {
    if let Ok(mut status_guard) = status.lock() {
        status_guard.last_error = Some(error);
        if clear_indexing {
            status_guard.is_indexing = false;
        }
    }
}

pub fn full_scan(config: &IndexConfig) -> anyhow::Result<usize> {
    initialize_database(&config.db_path)?;
    let mut connection = open_connection(&config.db_path)?;
    let mut indexed_entries = 0usize;

    for root in &config.roots {
        if !root.exists() {
            continue;
        }

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

    Ok(indexed_entries)
}

pub fn sync_paths(config: &IndexConfig, paths: &[PathBuf]) -> anyhow::Result<()> {
    initialize_database(&config.db_path)?;
    let mut connection = open_connection(&config.db_path)?;
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

        for path in paths {
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
    let mut statement = connection.prepare(
        "SELECT path FROM file_index WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
    )?;
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
        .map(|value| value.to_ascii_lowercase());
    let path_str = normalized_path_string(path);
    Ok(IndexedEntry {
        path_lower: path_str.to_ascii_lowercase(),
        name_lower: name.to_ascii_lowercase(),
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

    let mut results: Vec<SearchResult> = snapshot
        .iter()
        .filter_map(|entry| {
            if !matches_query(
                entry,
                query,
                normalized_text.as_deref(),
                normalized_extension.as_deref(),
            ) {
                return None;
            }
            Some(SearchResult {
                score: score_entry(entry, normalized_text.as_deref()),
                item_type: if entry.is_dir {
                    SearchItemType::Directory
                } else {
                    SearchItemType::File
                },
                entry: entry.clone(),
            })
        })
        .collect();

    results.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.entry.name.cmp(&right.entry.name))
            .then_with(|| left.entry.path.cmp(&right.entry.path))
    });
    results.truncate(limit);
    results
}

pub fn normalize_query_text(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase();
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
            path_lower: path.to_ascii_lowercase(),
            name_lower: name.to_ascii_lowercase(),
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
}
