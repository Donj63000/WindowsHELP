use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};
use tokio::runtime::Handle;

use crate::platform_windows::{
    PriorityClass, close_process_gracefully, collect_thread_counts, collect_visible_window_pids,
    get_process_priority, kill_process, set_process_priority, wait_for_process_exit,
};

const HISTORY_RETENTION: Duration = Duration::from_secs(60);
const SLOW_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const SHORT_WINDOW: Duration = Duration::from_secs(10);
const CLOSE_WAIT_TIMEOUT_MS: u32 = 2_500;

#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessKey {
    pub pid: u32,
    pub started_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessSafety {
    CriticalSystem,
    WindowsComponent,
    Caution,
    LikelyClosable,
    Unknown,
}

impl ProcessSafety {
    pub fn label(self) -> &'static str {
        match self {
            Self::CriticalSystem => "Systeme critique",
            Self::WindowsComponent => "Composant Windows",
            Self::Caution => "Prudence",
            Self::LikelyClosable => "Fermeture prudente possible",
            Self::Unknown => "Inconnu",
        }
    }

    fn protection_rank(self) -> u8 {
        match self {
            Self::CriticalSystem => 0,
            Self::WindowsComponent => 1,
            Self::Caution => 2,
            Self::Unknown => 3,
            Self::LikelyClosable => 4,
        }
    }

    fn most_protective(self, other: Self) -> Self {
        if self.protection_rank() <= other.protection_rank() {
            self
        } else {
            other
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestedAction {
    None,
    CloseGracefully,
    LowerPriority,
    ReviewOnly,
}

impl SuggestedAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Ne rien faire",
            Self::CloseGracefully => "Fermer proprement",
            Self::LowerPriority => "Baisser la priorite",
            Self::ReviewOnly => "Verifier avant action",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::ReviewOnly => 1,
            Self::LowerPriority => 2,
            Self::CloseGracefully => 3,
        }
    }

    fn strongest(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessTrend {
    Rising,
    Stable,
    CoolingDown,
}

impl ProcessTrend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rising => "Hausse",
            Self::Stable => "Stable",
            Self::CoolingDown => "Retombe",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessBottleneck {
    Quiet,
    Cpu,
    Memory,
    Mixed,
}

impl ProcessBottleneck {
    pub fn label(self) -> &'static str {
        match self {
            Self::Quiet => "RAS",
            Self::Cpu => "CPU",
            Self::Memory => "Memoire",
            Self::Mixed => "CPU + Memoire",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessInsight {
    pub impact_score: u8,
    pub cpu_avg_10s: f32,
    pub cpu_peak_60s: f32,
    pub memory_percent: f32,
    pub disk_io_bytes_per_sec: u64,
    pub safety: ProcessSafety,
    pub suggested_action: SuggestedAction,
    pub trend: ProcessTrend,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessRow {
    pub key: ProcessKey,
    pub family_id: String,
    pub name: String,
    pub path: Option<PathBuf>,
    pub parent_pid: Option<u32>,
    pub cpu_now: f32,
    pub memory_bytes: u64,
    pub threads: u32,
    pub priority: PriorityClass,
    pub status: String,
    pub run_time_secs: u64,
    pub has_visible_window: bool,
    pub insight: ProcessInsight,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessFamily {
    pub id: String,
    pub label: String,
    pub instance_count: usize,
    pub cpu_now_total: f32,
    pub cpu_avg_10s_total: f32,
    pub memory_bytes_total: u64,
    pub memory_percent_total: f32,
    pub max_impact_score: u8,
    pub visible_instances: usize,
    pub closeable_instances: usize,
    pub safety: ProcessSafety,
    pub suggested_action: SuggestedAction,
    pub primary_reason: String,
    pub top_process: Option<ProcessKey>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessRecommendation {
    pub title: String,
    pub details: String,
    pub family_id: String,
    pub target: Option<ProcessKey>,
    pub impact_score: u8,
    pub suggested_action: SuggestedAction,
    pub safety: ProcessSafety,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub total_processes: usize,
    pub total_families: usize,
    pub closeable_candidates: usize,
    pub current_cpu_percent: f32,
    pub current_memory_percent: f32,
    pub bottleneck: ProcessBottleneck,
    pub top_impact_name: Option<String>,
    pub top_memory_name: Option<String>,
    pub updated_at_utc: i64,
}

impl Default for ProcessSummary {
    fn default() -> Self {
        Self {
            total_processes: 0,
            total_families: 0,
            closeable_candidates: 0,
            current_cpu_percent: 0.0,
            current_memory_percent: 0.0,
            bottleneck: ProcessBottleneck::Quiet,
            top_impact_name: None,
            top_memory_name: None,
            updated_at_utc: Utc::now().timestamp(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessState {
    pub revision: u64,
    pub rows: Arc<[ProcessRow]>,
    pub families: Arc<[ProcessFamily]>,
    pub recommendations: Arc<[ProcessRecommendation]>,
    pub summary: ProcessSummary,
    pub last_error: Option<String>,
}

impl Default for ProcessState {
    fn default() -> Self {
        Self {
            revision: 0,
            rows: Arc::from(Vec::<ProcessRow>::new()),
            families: Arc::from(Vec::<ProcessFamily>::new()),
            recommendations: Arc::from(Vec::<ProcessRecommendation>::new()),
            summary: ProcessSummary::default(),
            last_error: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ProcessAction {
    CloseGracefully,
    Kill,
    SetPriority(PriorityClass),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessActionResult {
    CloseRequested,
    ClosedGracefully,
    ForceTerminated,
    PriorityUpdated(PriorityClass),
}

pub struct ProcessManager {
    state: Arc<RwLock<ProcessState>>,
    refresh_interval: Arc<RwLock<Duration>>,
}

impl ProcessManager {
    pub fn new(runtime: Handle, refresh_interval: Duration) -> Self {
        let manager = Self {
            state: Arc::new(RwLock::new(ProcessState::default())),
            refresh_interval: Arc::new(RwLock::new(refresh_interval)),
        };
        manager.spawn_refresh_loop(runtime);
        manager
    }

    pub fn state(&self) -> ProcessState {
        self.state
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn last_error(&self) -> Option<String> {
        self.state
            .read()
            .ok()
            .and_then(|guard| guard.last_error.clone())
    }

    pub fn update_refresh_interval(&self, refresh_interval: Duration) {
        if let Ok(mut guard) = self.refresh_interval.write() {
            *guard = refresh_interval;
        }
    }

    pub fn perform_action(
        &self,
        key: &ProcessKey,
        action: ProcessAction,
    ) -> anyhow::Result<ProcessActionResult> {
        validate_process_key(key)?;
        match action {
            ProcessAction::CloseGracefully => {
                close_process_gracefully(key.pid)?;
                if wait_for_process_exit(key.pid, CLOSE_WAIT_TIMEOUT_MS)? {
                    Ok(ProcessActionResult::ClosedGracefully)
                } else {
                    Ok(ProcessActionResult::CloseRequested)
                }
            }
            ProcessAction::Kill => {
                kill_process(key.pid)?;
                Ok(ProcessActionResult::ForceTerminated)
            }
            ProcessAction::SetPriority(priority) => {
                set_process_priority(key.pid, priority)?;
                Ok(ProcessActionResult::PriorityUpdated(priority))
            }
        }
    }

    fn spawn_refresh_loop(&self, runtime: Handle) {
        let state = Arc::clone(&self.state);
        let refresh_interval = Arc::clone(&self.refresh_interval);

        runtime.spawn(async move {
            let mut system = System::new_all();
            let mut context = ProcessRefreshContext::default();
            let mut revision = 0u64;
            system.refresh_all();

            loop {
                revision = revision.saturating_add(1);
                match refresh_process_state_with_system(&mut system, &mut context, revision) {
                    Ok(process_state) => {
                        if let Ok(mut guard) = state.write() {
                            *guard = process_state;
                        }
                    }
                    Err(error) => {
                        if let Ok(mut guard) = state.write() {
                            guard.last_error = Some(error.to_string());
                        }
                    }
                }

                let sleep_for = refresh_interval
                    .read()
                    .map(|guard| *guard)
                    .unwrap_or_else(|_| Duration::from_secs(1));
                tokio::time::sleep(sleep_for).await;
            }
        });
    }
}

pub fn refresh_process_state() -> anyhow::Result<ProcessState> {
    let mut system = System::new_all();
    let mut context = ProcessRefreshContext::default();
    system.refresh_all();
    refresh_process_state_with_system(&mut system, &mut context, 1)
}

#[derive(Clone, Debug)]
struct ProcessSample {
    captured_at: Instant,
    cpu_now: f32,
    memory_bytes: u64,
    disk_total_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct CachedMetadata {
    threads: u32,
    priority: PriorityClass,
    has_visible_window: bool,
}

impl Default for CachedMetadata {
    fn default() -> Self {
        Self {
            threads: 0,
            priority: PriorityClass::Normal,
            has_visible_window: false,
        }
    }
}

#[derive(Default)]
struct ProcessRefreshContext {
    history: HashMap<ProcessKey, VecDeque<ProcessSample>>,
    metadata: HashMap<ProcessKey, CachedMetadata>,
    last_slow_refresh_at: Option<Instant>,
}

fn refresh_process_state_with_system(
    system: &mut System,
    context: &mut ProcessRefreshContext,
    revision: u64,
) -> anyhow::Result<ProcessState> {
    let now = Instant::now();
    system.refresh_cpu_usage();
    system.refresh_memory();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cpu()
            .with_disk_usage()
            .with_exe(UpdateKind::OnlyIfNotSet),
    );

    let slow_due = context
        .last_slow_refresh_at
        .map(|last| now.duration_since(last) >= SLOW_REFRESH_INTERVAL)
        .unwrap_or(true);

    let thread_counts = if slow_due {
        collect_thread_counts().unwrap_or_default()
    } else {
        HashMap::new()
    };
    let visible_window_pids = if slow_due {
        collect_visible_window_pids().unwrap_or_default()
    } else {
        HashSet::new()
    };

    let total_memory = system.total_memory();
    let current_cpu_percent = system.global_cpu_usage();
    let current_memory_percent = percent(system.used_memory(), total_memory);

    let mut active_keys = HashSet::new();
    let mut rows = Vec::with_capacity(system.processes().len());

    for (pid, process) in system.processes() {
        let name = process.name().to_string_lossy().to_string();
        let path = process.exe().map(Path::to_path_buf);
        let key = ProcessKey {
            pid: pid.as_u32(),
            started_at: Some(process.start_time()).filter(|value| *value > 0),
        };
        active_keys.insert(key.clone());

        let is_new_process = !context.metadata.contains_key(&key);
        let metadata = context.metadata.entry(key.clone()).or_default();
        if slow_due || is_new_process {
            metadata.threads = thread_counts
                .get(&pid.as_u32())
                .copied()
                .unwrap_or(metadata.threads);
            metadata.priority = get_process_priority(pid.as_u32()).unwrap_or(metadata.priority);
            metadata.has_visible_window = visible_window_pids.contains(&pid.as_u32());
        }

        let history = context.history.entry(key.clone()).or_default();
        history.push_back(ProcessSample {
            captured_at: now,
            cpu_now: process.cpu_usage(),
            memory_bytes: process.memory(),
            disk_total_bytes: total_disk_bytes(process),
        });
        trim_history(history, now);

        let insight = build_process_insight(
            &name,
            path.as_deref(),
            metadata.has_visible_window,
            process.run_time(),
            process.cpu_usage(),
            process.memory(),
            total_memory,
            history,
            current_cpu_percent,
            current_memory_percent,
        );

        rows.push(ProcessRow {
            key,
            family_id: family_id_for_process(&name, path.as_deref()),
            name,
            path,
            parent_pid: process.parent().map(Pid::as_u32),
            cpu_now: process.cpu_usage(),
            memory_bytes: process.memory(),
            threads: metadata.threads,
            priority: metadata.priority,
            status: translate_process_status(process.status()).to_owned(),
            run_time_secs: process.run_time(),
            has_visible_window: metadata.has_visible_window,
            insight,
        });
    }

    context.history.retain(|key, _| active_keys.contains(key));
    context.metadata.retain(|key, _| active_keys.contains(key));

    if slow_due {
        context.last_slow_refresh_at = Some(now);
    }

    rows.sort_by(compare_rows);

    let families = build_families(&rows);
    let summary = build_summary(
        &rows,
        &families,
        current_cpu_percent,
        current_memory_percent,
    );
    let recommendations = build_recommendations(&rows, &summary);

    Ok(ProcessState {
        revision,
        rows: Arc::from(rows),
        families: Arc::from(families),
        recommendations: Arc::from(recommendations),
        summary,
        last_error: None,
    })
}

fn compare_rows(left: &ProcessRow, right: &ProcessRow) -> Ordering {
    right
        .insight
        .impact_score
        .cmp(&left.insight.impact_score)
        .then_with(|| {
            right
                .insight
                .cpu_avg_10s
                .partial_cmp(&left.insight.cpu_avg_10s)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| right.memory_bytes.cmp(&left.memory_bytes))
        .then_with(|| {
            right
                .cpu_now
                .partial_cmp(&left.cpu_now)
                .unwrap_or(Ordering::Equal)
        })
}

fn build_process_insight(
    name: &str,
    path: Option<&Path>,
    has_visible_window: bool,
    run_time_secs: u64,
    cpu_now: f32,
    memory_bytes: u64,
    total_memory: u64,
    history: &VecDeque<ProcessSample>,
    current_cpu_percent: f32,
    current_memory_percent: f32,
) -> ProcessInsight {
    let cpu_avg_10s = average_cpu_over(history, SHORT_WINDOW);
    let cpu_peak_60s = history
        .iter()
        .map(|sample| sample.cpu_now)
        .fold(cpu_now, f32::max);
    let stable_memory_bytes = average_memory_over(history, SHORT_WINDOW).round() as u64;
    let memory_percent = percent(stable_memory_bytes.max(memory_bytes), total_memory);
    let disk_io_bytes_per_sec = disk_bytes_per_second(history);
    let trend = compute_trend(history);
    let safety = classify_process_safety(name, path, has_visible_window);
    let impact_score = impact_score(
        cpu_avg_10s,
        cpu_peak_60s,
        memory_percent,
        disk_io_bytes_per_sec,
        current_cpu_percent,
        current_memory_percent,
        run_time_secs,
    );
    let suggested_action = suggest_action(safety, has_visible_window, impact_score);
    let reasons = build_reasons(
        name,
        path,
        cpu_avg_10s,
        cpu_peak_60s,
        memory_percent,
        disk_io_bytes_per_sec,
        safety,
        suggested_action,
        trend,
        run_time_secs,
    );

    ProcessInsight {
        impact_score,
        cpu_avg_10s,
        cpu_peak_60s,
        memory_percent,
        disk_io_bytes_per_sec,
        safety,
        suggested_action,
        trend,
        reasons,
    }
}

fn impact_score(
    cpu_avg_10s: f32,
    cpu_peak_60s: f32,
    memory_percent: f32,
    disk_io_bytes_per_sec: u64,
    current_cpu_percent: f32,
    current_memory_percent: f32,
    run_time_secs: u64,
) -> u8 {
    let cpu_weight = if current_cpu_percent >= 80.0 {
        0.50
    } else {
        0.38
    };
    let memory_weight = if current_memory_percent >= 80.0 {
        0.30
    } else {
        0.24
    };
    let disk_weight = 0.16;
    let peak_weight = 0.10;

    let cpu_component = (cpu_avg_10s / 80.0).clamp(0.0, 1.0) * cpu_weight;
    let memory_component = (memory_percent / 25.0).clamp(0.0, 1.0) * memory_weight;
    let disk_component =
        (disk_io_bytes_per_sec as f32 / (150.0 * 1024.0 * 1024.0)).clamp(0.0, 1.0) * disk_weight;
    let peak_component = (cpu_peak_60s / 100.0).clamp(0.0, 1.0) * peak_weight;

    let sustain_bonus = if run_time_secs >= 60 && (cpu_avg_10s >= 20.0 || memory_percent >= 8.0) {
        0.08
    } else {
        0.0
    };

    ((cpu_component + memory_component + disk_component + peak_component + sustain_bonus)
        .clamp(0.0, 1.0)
        * 100.0)
        .round() as u8
}

fn build_reasons(
    name: &str,
    path: Option<&Path>,
    cpu_avg_10s: f32,
    cpu_peak_60s: f32,
    memory_percent: f32,
    disk_io_bytes_per_sec: u64,
    safety: ProcessSafety,
    suggested_action: SuggestedAction,
    trend: ProcessTrend,
    run_time_secs: u64,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if cpu_avg_10s >= 20.0 {
        reasons.push(format!(
            "charge CPU moyenne de {:.1}% sur 10 s",
            cpu_avg_10s
        ));
    } else if cpu_peak_60s >= 35.0 {
        reasons.push(format!("pic CPU de {:.1}% sur 60 s", cpu_peak_60s));
    }

    if memory_percent >= 8.0 {
        reasons.push(format!("utilise {:.1}% de la RAM systeme", memory_percent));
    }

    if disk_io_bytes_per_sec >= 16 * 1024 * 1024 {
        reasons.push(format!(
            "genere environ {}/s d'I/O",
            format_bytes(disk_io_bytes_per_sec)
        ));
    }

    if run_time_secs >= 300 && (cpu_avg_10s >= 8.0 || memory_percent >= 4.0) {
        reasons.push("la charge dure depuis plusieurs minutes".into());
    }

    match trend {
        ProcessTrend::Rising => reasons.push("la tendance est a la hausse".into()),
        ProcessTrend::CoolingDown => reasons.push("la charge retombe".into()),
        ProcessTrend::Stable => {}
    }

    match safety {
        ProcessSafety::CriticalSystem => {
            reasons.push("processus critique a ne pas terminer".into())
        }
        ProcessSafety::WindowsComponent => {
            reasons.push("composant Windows a inspecter seulement".into())
        }
        ProcessSafety::Caution => {
            reasons.push("service ou outil de fond a traiter prudemment".into())
        }
        ProcessSafety::LikelyClosable => {
            if suggested_action == SuggestedAction::CloseGracefully {
                reasons.push("application utilisateur avec fermeture prudente possible".into());
            }
        }
        ProcessSafety::Unknown => {
            reasons.push("classification incertaine, verification conseillee".into())
        }
    }

    if reasons.is_empty() {
        if let Some(path) = path {
            reasons.push(format!("surveille via {}", path.display()));
        } else {
            reasons.push(format!("{name} a un impact faible ou bref"));
        }
    }

    reasons.truncate(4);
    reasons
}

fn build_families(rows: &[ProcessRow]) -> Vec<ProcessFamily> {
    #[derive(Clone)]
    struct FamilyAccumulator {
        label: String,
        instance_count: usize,
        cpu_now_total: f32,
        cpu_avg_10s_total: f32,
        memory_bytes_total: u64,
        memory_percent_total: f32,
        max_impact_score: u8,
        visible_instances: usize,
        closeable_instances: usize,
        safety: ProcessSafety,
        suggested_action: SuggestedAction,
        primary_reason: String,
        top_process: Option<ProcessKey>,
    }

    let mut families: HashMap<String, FamilyAccumulator> = HashMap::new();

    for row in rows {
        let entry = families
            .entry(row.family_id.clone())
            .or_insert_with(|| FamilyAccumulator {
                label: row.name.clone(),
                instance_count: 0,
                cpu_now_total: 0.0,
                cpu_avg_10s_total: 0.0,
                memory_bytes_total: 0,
                memory_percent_total: 0.0,
                max_impact_score: 0,
                visible_instances: 0,
                closeable_instances: 0,
                safety: row.insight.safety,
                suggested_action: row.insight.suggested_action,
                primary_reason: row
                    .insight
                    .reasons
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "aucune raison forte".into()),
                top_process: Some(row.key.clone()),
            });

        entry.instance_count += 1;
        entry.cpu_now_total += row.cpu_now;
        entry.cpu_avg_10s_total += row.insight.cpu_avg_10s;
        entry.memory_bytes_total = entry.memory_bytes_total.saturating_add(row.memory_bytes);
        entry.memory_percent_total += row.insight.memory_percent;
        entry.visible_instances += usize::from(row.has_visible_window);
        entry.closeable_instances +=
            usize::from(row.insight.suggested_action == SuggestedAction::CloseGracefully);
        entry.safety = entry.safety.most_protective(row.insight.safety);
        entry.suggested_action = entry
            .suggested_action
            .strongest(row.insight.suggested_action);

        if row.insight.impact_score >= entry.max_impact_score {
            entry.max_impact_score = row.insight.impact_score;
            entry.label = row.name.clone();
            entry.primary_reason = row
                .insight
                .reasons
                .first()
                .cloned()
                .unwrap_or_else(|| "aucune raison forte".into());
            entry.top_process = Some(row.key.clone());
        }
    }

    let mut families = families
        .into_iter()
        .map(|(id, family)| ProcessFamily {
            id,
            label: family.label,
            instance_count: family.instance_count,
            cpu_now_total: family.cpu_now_total,
            cpu_avg_10s_total: family.cpu_avg_10s_total,
            memory_bytes_total: family.memory_bytes_total,
            memory_percent_total: family.memory_percent_total,
            max_impact_score: family.max_impact_score,
            visible_instances: family.visible_instances,
            closeable_instances: family.closeable_instances,
            safety: family.safety,
            suggested_action: family.suggested_action,
            primary_reason: family.primary_reason,
            top_process: family.top_process,
        })
        .collect::<Vec<_>>();

    families.sort_by(|left, right| {
        right
            .max_impact_score
            .cmp(&left.max_impact_score)
            .then_with(|| {
                right
                    .cpu_avg_10s_total
                    .partial_cmp(&left.cpu_avg_10s_total)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| right.memory_bytes_total.cmp(&left.memory_bytes_total))
    });
    families
}

fn build_summary(
    rows: &[ProcessRow],
    families: &[ProcessFamily],
    current_cpu_percent: f32,
    current_memory_percent: f32,
) -> ProcessSummary {
    let top_memory_name = rows
        .iter()
        .max_by_key(|row| row.memory_bytes)
        .map(|row| row.name.clone());

    ProcessSummary {
        total_processes: rows.len(),
        total_families: families.len(),
        closeable_candidates: rows
            .iter()
            .filter(|row| {
                row.insight.suggested_action == SuggestedAction::CloseGracefully
                    && row.insight.impact_score >= 25
            })
            .count(),
        current_cpu_percent,
        current_memory_percent,
        bottleneck: determine_bottleneck(current_cpu_percent, current_memory_percent),
        top_impact_name: rows.first().map(|row| row.name.clone()),
        top_memory_name,
        updated_at_utc: Utc::now().timestamp(),
    }
}

fn build_recommendations(
    rows: &[ProcessRow],
    summary: &ProcessSummary,
) -> Vec<ProcessRecommendation> {
    let mut picks = Vec::new();

    if let Some(primary) = pick_primary_suspect(rows, summary.bottleneck) {
        picks.push(primary);
    }
    if let Some(memory) = rows
        .iter()
        .filter(|row| row.insight.memory_percent >= 6.0)
        .find(|row| !picks.iter().any(|picked| picked.family_id == row.family_id))
    {
        picks.push(memory);
    }
    if let Some(closeable) = rows.iter().find(|row| {
        row.insight.suggested_action == SuggestedAction::CloseGracefully
            && row.insight.impact_score >= 20
            && !picks.iter().any(|picked| picked.family_id == row.family_id)
    }) {
        picks.push(closeable);
    }
    if picks.is_empty()
        && let Some(row) = rows.first()
    {
        picks.push(row);
    }

    picks
        .into_iter()
        .take(3)
        .map(|row| ProcessRecommendation {
            title: recommendation_title(row, summary.bottleneck),
            details: recommendation_details(row),
            family_id: row.family_id.clone(),
            target: Some(row.key.clone()),
            impact_score: row.insight.impact_score,
            suggested_action: row.insight.suggested_action,
            safety: row.insight.safety,
        })
        .collect()
}

fn pick_primary_suspect<'a>(
    rows: &'a [ProcessRow],
    bottleneck: ProcessBottleneck,
) -> Option<&'a ProcessRow> {
    match bottleneck {
        ProcessBottleneck::Cpu => rows.iter().find(|row| row.insight.cpu_avg_10s >= 10.0),
        ProcessBottleneck::Memory => rows.iter().find(|row| row.insight.memory_percent >= 5.0),
        ProcessBottleneck::Mixed => rows
            .iter()
            .find(|row| row.insight.cpu_avg_10s >= 8.0 || row.insight.memory_percent >= 4.0),
        ProcessBottleneck::Quiet => rows.first(),
    }
}

fn recommendation_title(row: &ProcessRow, bottleneck: ProcessBottleneck) -> String {
    match bottleneck {
        ProcessBottleneck::Cpu => format!("{} pese sur le CPU", row.name),
        ProcessBottleneck::Memory => format!("{} occupe la RAM", row.name),
        ProcessBottleneck::Mixed => format!("{} revient parmi les suspects", row.name),
        ProcessBottleneck::Quiet => format!("{} reste le plus visible", row.name),
    }
}

fn recommendation_details(row: &ProcessRow) -> String {
    let mut parts = vec![format!(
        "impact {} / 100, CPU moyen {:.1}%, memoire {:.1}%",
        row.insight.impact_score, row.insight.cpu_avg_10s, row.insight.memory_percent
    )];

    if row.insight.disk_io_bytes_per_sec > 0 {
        parts.push(format!(
            "I/O {} /s",
            format_bytes(row.insight.disk_io_bytes_per_sec)
        ));
    }

    parts.push(format!(
        "niveau de prudence: {}",
        row.insight.safety.label().to_ascii_lowercase()
    ));
    parts.join(" // ")
}

fn determine_bottleneck(
    current_cpu_percent: f32,
    current_memory_percent: f32,
) -> ProcessBottleneck {
    if current_cpu_percent < 65.0 && current_memory_percent < 70.0 {
        ProcessBottleneck::Quiet
    } else if current_cpu_percent >= 80.0 && current_memory_percent >= 80.0 {
        ProcessBottleneck::Mixed
    } else if current_cpu_percent >= current_memory_percent + 8.0 {
        ProcessBottleneck::Cpu
    } else if current_memory_percent >= current_cpu_percent + 5.0 {
        ProcessBottleneck::Memory
    } else {
        ProcessBottleneck::Mixed
    }
}

fn classify_process_safety(
    name: &str,
    path: Option<&Path>,
    has_visible_window: bool,
) -> ProcessSafety {
    let lower_name = name.to_ascii_lowercase();
    if is_critical_system_process(&lower_name) {
        return ProcessSafety::CriticalSystem;
    }

    if is_windows_component_path(path) || is_known_windows_component(&lower_name) {
        return ProcessSafety::WindowsComponent;
    }

    if lower_name.contains("defender")
        || lower_name.contains("antimalware")
        || lower_name.contains("update")
        || lower_name.contains("service")
        || lower_name.contains("nvidia")
        || lower_name.contains("amd")
        || lower_name.contains("realtek")
        || lower_name.contains("intel")
    {
        return ProcessSafety::Caution;
    }

    if has_visible_window && !is_windows_component_path(path) {
        return ProcessSafety::LikelyClosable;
    }

    if let Some(path) = path
        && is_user_profile_or_program_files(path)
    {
        return ProcessSafety::LikelyClosable;
    }

    ProcessSafety::Unknown
}

fn suggest_action(
    safety: ProcessSafety,
    has_visible_window: bool,
    impact_score: u8,
) -> SuggestedAction {
    match safety {
        ProcessSafety::CriticalSystem => SuggestedAction::None,
        ProcessSafety::WindowsComponent => SuggestedAction::ReviewOnly,
        ProcessSafety::Caution => {
            if impact_score >= 40 {
                SuggestedAction::LowerPriority
            } else {
                SuggestedAction::ReviewOnly
            }
        }
        ProcessSafety::LikelyClosable => {
            if has_visible_window && impact_score >= 20 {
                SuggestedAction::CloseGracefully
            } else if impact_score >= 45 {
                SuggestedAction::LowerPriority
            } else {
                SuggestedAction::ReviewOnly
            }
        }
        ProcessSafety::Unknown => {
            if impact_score >= 35 {
                SuggestedAction::ReviewOnly
            } else {
                SuggestedAction::None
            }
        }
    }
}

fn compute_trend(history: &VecDeque<ProcessSample>) -> ProcessTrend {
    if history.len() < 4 {
        return ProcessTrend::Stable;
    }

    let midpoint = history.len() / 2;
    let older_avg = history
        .iter()
        .take(midpoint)
        .map(|sample| sample.cpu_now)
        .sum::<f32>()
        / midpoint as f32;
    let newer_len = history.len() - midpoint;
    let newer_avg = history
        .iter()
        .skip(midpoint)
        .map(|sample| sample.cpu_now)
        .sum::<f32>()
        / newer_len as f32;

    if newer_avg >= older_avg + 5.0 {
        ProcessTrend::Rising
    } else if older_avg >= newer_avg + 5.0 {
        ProcessTrend::CoolingDown
    } else {
        ProcessTrend::Stable
    }
}

fn trim_history(history: &mut VecDeque<ProcessSample>, now: Instant) {
    while history
        .front()
        .map(|sample| now.duration_since(sample.captured_at) > HISTORY_RETENTION)
        .unwrap_or(false)
    {
        history.pop_front();
    }
}

fn average_cpu_over(history: &VecDeque<ProcessSample>, window: Duration) -> f32 {
    let Some(last) = history.back() else {
        return 0.0;
    };
    let cutoff = last
        .captured_at
        .checked_sub(window)
        .unwrap_or(last.captured_at);
    let relevant = history
        .iter()
        .filter(|sample| sample.captured_at >= cutoff)
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        0.0
    } else {
        relevant.iter().map(|sample| sample.cpu_now).sum::<f32>() / relevant.len() as f32
    }
}

fn average_memory_over(history: &VecDeque<ProcessSample>, window: Duration) -> f64 {
    let Some(last) = history.back() else {
        return 0.0;
    };
    let cutoff = last
        .captured_at
        .checked_sub(window)
        .unwrap_or(last.captured_at);
    let relevant = history
        .iter()
        .filter(|sample| sample.captured_at >= cutoff)
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        0.0
    } else {
        relevant
            .iter()
            .map(|sample| sample.memory_bytes as f64)
            .sum::<f64>()
            / relevant.len() as f64
    }
}

fn disk_bytes_per_second(history: &VecDeque<ProcessSample>) -> u64 {
    let Some(last) = history.back() else {
        return 0;
    };
    let cutoff = last
        .captured_at
        .checked_sub(SHORT_WINDOW)
        .unwrap_or(last.captured_at);
    let Some(first) = history.iter().find(|sample| sample.captured_at >= cutoff) else {
        return 0;
    };
    let elapsed = last
        .captured_at
        .duration_since(first.captured_at)
        .as_secs_f64();
    if elapsed <= 0.0 {
        0
    } else {
        (last.disk_total_bytes.saturating_sub(first.disk_total_bytes) as f64 / elapsed) as u64
    }
}

fn total_disk_bytes(process: &sysinfo::Process) -> u64 {
    let usage = process.disk_usage();
    usage
        .total_read_bytes
        .saturating_add(usage.total_written_bytes)
}

fn family_id_for_process(name: &str, path: Option<&Path>) -> String {
    let base = path
        .and_then(|value| value.file_stem())
        .and_then(|value| value.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase();
    base.trim().to_string()
}

fn is_critical_system_process(name: &str) -> bool {
    matches!(
        name,
        "system"
            | "registry"
            | "smss.exe"
            | "csrss.exe"
            | "wininit.exe"
            | "services.exe"
            | "lsass.exe"
            | "winlogon.exe"
            | "fontdrvhost.exe"
    )
}

fn is_known_windows_component(name: &str) -> bool {
    matches!(
        name,
        "svchost.exe"
            | "taskhostw.exe"
            | "dllhost.exe"
            | "dwm.exe"
            | "explorer.exe"
            | "sihost.exe"
            | "runtimebroker.exe"
            | "startmenuexperiencehost.exe"
            | "searchhost.exe"
    )
}

fn is_windows_component_path(path: Option<&Path>) -> bool {
    path.map(path_as_ascii_lowercase)
        .map(|value| value.contains("\\windows\\"))
        .unwrap_or(false)
}

fn is_user_profile_or_program_files(path: &Path) -> bool {
    let value = path_as_ascii_lowercase(path);
    value.contains("\\users\\") || value.contains("\\program files")
}

fn path_as_ascii_lowercase(path: &Path) -> String {
    path.display().to_string().to_ascii_lowercase()
}

fn validate_process_key(key: &ProcessKey) -> anyhow::Result<()> {
    let pid = Pid::from_u32(key.pid);
    let mut system = System::new();
    let pids = [pid];
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_memory(),
    );

    let process = system
        .process(pid)
        .ok_or_else(|| anyhow::anyhow!("le processus {} n'existe plus", key.pid))?;
    let current_started_at = Some(process.start_time()).filter(|value| *value > 0);
    if key.started_at.is_some() && key.started_at != current_started_at {
        anyhow::bail!(
            "le PID {} a ete reutilise par un autre processus, action annulee",
            key.pid
        );
    }
    Ok(())
}

fn translate_process_status(status: ProcessStatus) -> &'static str {
    match status {
        ProcessStatus::Idle => "Inactif",
        ProcessStatus::Run => "En cours",
        ProcessStatus::Sleep => "En veille",
        ProcessStatus::Stop => "Arrete",
        ProcessStatus::Zombie => "Zombie",
        ProcessStatus::Tracing => "Trace",
        ProcessStatus::Dead => "Bloque",
        ProcessStatus::Wakekill => "Reveil force",
        ProcessStatus::Waking => "Reveil",
        ProcessStatus::Parked => "En pause",
        ProcessStatus::LockBlocked => "Bloque par verrou",
        ProcessStatus::UninterruptibleDiskSleep => "Attente disque",
        ProcessStatus::Suspended => "Suspendu",
        ProcessStatus::Unknown(_) => "Inconnu",
    }
}

fn percent(value: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (value as f64 / total as f64 * 100.0) as f32
    }
}

fn format_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = value as f64;
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    format!("{value:.1} {}", UNITS[unit_index])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn safety_classification_is_conservative() {
        assert_eq!(
            classify_process_safety("System", None, false),
            ProcessSafety::CriticalSystem
        );
        assert_eq!(
            classify_process_safety(
                "svchost.exe",
                Some(Path::new("C:\\Windows\\System32\\svchost.exe")),
                false
            ),
            ProcessSafety::WindowsComponent
        );
        assert_eq!(
            classify_process_safety(
                "Discord.exe",
                Some(Path::new(
                    "C:\\Users\\me\\AppData\\Local\\Discord\\Discord.exe"
                )),
                true
            ),
            ProcessSafety::LikelyClosable
        );
    }

    #[test]
    fn history_metrics_compute_trend_and_average() {
        let now = Instant::now();
        let history = VecDeque::from(vec![
            ProcessSample {
                captured_at: now - Duration::from_secs(9),
                cpu_now: 5.0,
                memory_bytes: 10,
                disk_total_bytes: 100,
            },
            ProcessSample {
                captured_at: now - Duration::from_secs(6),
                cpu_now: 7.0,
                memory_bytes: 10,
                disk_total_bytes: 220,
            },
            ProcessSample {
                captured_at: now - Duration::from_secs(3),
                cpu_now: 20.0,
                memory_bytes: 10,
                disk_total_bytes: 520,
            },
            ProcessSample {
                captured_at: now,
                cpu_now: 25.0,
                memory_bytes: 10,
                disk_total_bytes: 820,
            },
        ]);

        let average = average_cpu_over(&history, SHORT_WINDOW);
        assert!((average - 14.25).abs() < f32::EPSILON);
        assert_eq!(compute_trend(&history), ProcessTrend::Rising);
        assert!(disk_bytes_per_second(&history) > 0);
    }

    #[test]
    fn family_aggregation_keeps_top_process_and_counts_closeable() {
        let key_a = ProcessKey {
            pid: 1,
            started_at: Some(10),
        };
        let key_b = ProcessKey {
            pid: 2,
            started_at: Some(11),
        };
        let rows = vec![
            ProcessRow {
                key: key_a.clone(),
                family_id: "chrome".into(),
                name: "chrome.exe".into(),
                path: None,
                parent_pid: None,
                cpu_now: 10.0,
                memory_bytes: 100,
                threads: 1,
                priority: PriorityClass::Normal,
                status: "En cours".into(),
                run_time_secs: 10,
                has_visible_window: true,
                insight: ProcessInsight {
                    impact_score: 60,
                    cpu_avg_10s: 20.0,
                    cpu_peak_60s: 30.0,
                    memory_percent: 5.0,
                    disk_io_bytes_per_sec: 0,
                    safety: ProcessSafety::LikelyClosable,
                    suggested_action: SuggestedAction::CloseGracefully,
                    trend: ProcessTrend::Stable,
                    reasons: vec!["CPU".into()],
                },
            },
            ProcessRow {
                key: key_b.clone(),
                family_id: "chrome".into(),
                name: "chrome.exe".into(),
                path: None,
                parent_pid: None,
                cpu_now: 2.0,
                memory_bytes: 50,
                threads: 1,
                priority: PriorityClass::Normal,
                status: "En cours".into(),
                run_time_secs: 10,
                has_visible_window: false,
                insight: ProcessInsight {
                    impact_score: 25,
                    cpu_avg_10s: 5.0,
                    cpu_peak_60s: 8.0,
                    memory_percent: 2.0,
                    disk_io_bytes_per_sec: 0,
                    safety: ProcessSafety::LikelyClosable,
                    suggested_action: SuggestedAction::ReviewOnly,
                    trend: ProcessTrend::Stable,
                    reasons: vec!["Memoire".into()],
                },
            },
        ];

        let families = build_families(&rows);
        assert_eq!(families.len(), 1);
        assert_eq!(families[0].instance_count, 2);
        assert_eq!(families[0].closeable_instances, 1);
        assert_eq!(families[0].top_process, Some(key_a));
    }

    #[test]
    fn process_state_sees_spawned_process_and_validates_key() -> anyhow::Result<()> {
        let mut child = Command::new("cmd")
            .args(["/C", "ping", "127.0.0.1", "-n", "30"])
            .spawn()?;
        let pid = child.id();

        let state = refresh_process_state()?;
        let row = state
            .rows
            .iter()
            .find(|row| row.key.pid == pid)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("process {pid} not found"))?;

        assert_eq!(row.key.pid, pid);
        validate_process_key(&row.key)?;
        set_process_priority(pid, PriorityClass::BelowNormal)?;
        kill_process(pid)?;

        for _ in 0..20 {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("child process {pid} was not terminated in time")
    }
}
