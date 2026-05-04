use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sysinfo::{Disks, Networks, System};
use tokio::runtime::Handle;

use crate::platform_windows::show_toast_notification;
use crate::process::ProcessState;
use crate::thermal::{
    CapturedControlState, CoolingActionRecord, TemperatureReading, ThermalAutomationController,
    ThermalCapabilities, ThermalManager, ThermalSettings, ThermalState, ThermalStatusSnapshot,
    next_thermal_state, thresholds_for_reading,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AlertRuleKind {
    SystemCpu,
    SystemMemory,
    DiskUsage,
    ProcessCpu,
    ProcessMemory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    pub kind: AlertRuleKind,
    pub threshold_percent: f32,
    pub sustain_seconds: u64,
}

impl AlertRule {
    pub fn default_label_for_id(id: &str) -> Option<&'static str> {
        match id {
            "system-cpu" => Some("CPU systeme"),
            "system-memory" => Some("Memoire systeme"),
            "disk-usage" => Some("Utilisation des disques"),
            "process-cpu" => Some("CPU des processus"),
            "process-memory" => Some("Memoire des processus"),
            _ => None,
        }
    }

    pub fn refresh_label(&mut self) {
        if let Some(label) = Self::default_label_for_id(&self.id) {
            self.label = label.to_owned();
        }
    }

    pub fn default_rules() -> Vec<Self> {
        vec![
            Self {
                id: "system-cpu".into(),
                label: Self::default_label_for_id("system-cpu")
                    .expect("system-cpu must have a default label")
                    .into(),
                enabled: true,
                kind: AlertRuleKind::SystemCpu,
                threshold_percent: 90.0,
                sustain_seconds: 10,
            },
            Self {
                id: "system-memory".into(),
                label: Self::default_label_for_id("system-memory")
                    .expect("system-memory must have a default label")
                    .into(),
                enabled: true,
                kind: AlertRuleKind::SystemMemory,
                threshold_percent: 90.0,
                sustain_seconds: 10,
            },
            Self {
                id: "disk-usage".into(),
                label: Self::default_label_for_id("disk-usage")
                    .expect("disk-usage must have a default label")
                    .into(),
                enabled: true,
                kind: AlertRuleKind::DiskUsage,
                threshold_percent: 95.0,
                sustain_seconds: 10,
            },
            Self {
                id: "process-cpu".into(),
                label: Self::default_label_for_id("process-cpu")
                    .expect("process-cpu must have a default label")
                    .into(),
                enabled: true,
                kind: AlertRuleKind::ProcessCpu,
                threshold_percent: 85.0,
                sustain_seconds: 10,
            },
            Self {
                id: "process-memory".into(),
                label: Self::default_label_for_id("process-memory")
                    .expect("process-memory must have a default label")
                    .into(),
                enabled: true,
                kind: AlertRuleKind::ProcessMemory,
                threshold_percent: 20.0,
                sustain_seconds: 10,
            },
        ]
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AlertEventState {
    Active,
    Resolved,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AlertEventKind {
    MetricThreshold,
    TemperatureWarning,
    TemperatureCritical,
    CoolingActionApplied,
    CoolingActionFailed,
    CoolingActionRestored,
}

impl AlertEventKind {
    pub fn is_persistent(self) -> bool {
        matches!(
            self,
            Self::MetricThreshold | Self::TemperatureWarning | Self::TemperatureCritical
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AlertEvent {
    pub kind: AlertEventKind,
    pub rule_id: String,
    pub source_label: String,
    pub source_pid: Option<u32>,
    pub message: String,
    pub state: AlertEventState,
    pub value_percent: f32,
    pub threshold_percent: f32,
    pub triggered_at_utc: i64,
    pub resolved_at_utc: Option<i64>,
}

impl AlertEvent {
    pub fn is_persistent_alert(&self) -> bool {
        self.kind.is_persistent()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessMetric {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub memory_bytes: u64,
    pub memory_percent: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiskMetric {
    pub name: String,
    pub mount_point: PathBuf,
    pub total_space_bytes: u64,
    pub available_space_bytes: u64,
    pub used_percent: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricSnapshot {
    pub timestamp_utc: i64,
    pub cpu_usage_percent: f32,
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    pub network_received_bytes_per_sec: u64,
    pub network_transmitted_bytes_per_sec: u64,
    pub disks: Vec<DiskMetric>,
    pub top_cpu_processes: Vec<ProcessMetric>,
    pub top_memory_processes: Vec<ProcessMetric>,
    pub temperatures: Vec<TemperatureReading>,
    pub thermal: ThermalStatusSnapshot,
}

#[derive(Clone, Debug)]
pub struct MetricHistory {
    samples: VecDeque<MetricSnapshot>,
    capacity: usize,
}

impl MetricHistory {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, snapshot: MetricSnapshot) {
        self.samples.push_back(snapshot);
        while self.samples.len() > self.capacity {
            self.samples.pop_front();
        }
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        while self.samples.len() > self.capacity {
            self.samples.pop_front();
        }
    }

    pub fn samples(&self) -> Arc<[MetricSnapshot]> {
        Arc::from(self.samples.iter().cloned().collect::<Vec<_>>())
    }
}

#[derive(Clone, Debug)]
pub struct MonitorSnapshotState {
    pub latest: Option<Arc<MetricSnapshot>>,
    pub history: Arc<[MetricSnapshot]>,
    pub events: Arc<[AlertEvent]>,
    pub last_error: Option<String>,
}

pub struct MonitorService {
    state: Arc<Mutex<MonitorSnapshotState>>,
    rules: Arc<RwLock<Vec<AlertRule>>>,
    thermal_settings: Arc<RwLock<ThermalSettings>>,
    refresh_interval: Arc<RwLock<Duration>>,
    thermal_refresh_interval: Arc<RwLock<Duration>>,
    history_capacity: Arc<RwLock<usize>>,
    process_state: Arc<RwLock<ProcessState>>,
}

#[derive(Default)]
struct AlertTracker {
    first_exceeded_at: Option<i64>,
    active: bool,
    rule_id: String,
    source_label: String,
    source_pid: Option<u32>,
    last_value_percent: f32,
    threshold_percent: f32,
}

#[derive(Clone)]
struct AlertEvaluation {
    key: String,
    source_label: String,
    source_pid: Option<u32>,
    value_percent: f32,
    threshold_percent: f32,
    exceeded: bool,
}

#[derive(Default)]
struct ThermalRuntimeState {
    sensor_states: HashMap<String, ThermalState>,
    global_state: ThermalState,
    previous_control_state: Option<CapturedControlState>,
    control_applied_by_app: bool,
    last_action: Option<CoolingActionRecord>,
    last_error: Option<String>,
}

impl MonitorService {
    pub fn new(
        runtime: Handle,
        refresh_interval: Duration,
        thermal_refresh_interval: Duration,
        history_capacity: usize,
        process_state: Arc<RwLock<ProcessState>>,
        rules: Vec<AlertRule>,
        thermal_settings: ThermalSettings,
    ) -> Self {
        let state = Arc::new(Mutex::new(MonitorSnapshotState {
            latest: None,
            history: Arc::from(Vec::<MetricSnapshot>::new()),
            events: Arc::from(Vec::<AlertEvent>::new()),
            last_error: None,
        }));
        let service = Self {
            state,
            rules: Arc::new(RwLock::new(rules)),
            thermal_settings: Arc::new(RwLock::new(thermal_settings)),
            refresh_interval: Arc::new(RwLock::new(refresh_interval)),
            thermal_refresh_interval: Arc::new(RwLock::new(thermal_refresh_interval)),
            history_capacity: Arc::new(RwLock::new(history_capacity.max(1))),
            process_state,
        };
        service.spawn_loop(runtime);
        service
    }

    pub fn snapshot_state(&self) -> MonitorSnapshotState {
        self.state
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or(MonitorSnapshotState {
                latest: None,
                history: Arc::from(Vec::<MetricSnapshot>::new()),
                events: Arc::from(Vec::<AlertEvent>::new()),
                last_error: Some("Etat de la surveillance indisponible".into()),
            })
    }

    pub fn update_rules(&self, rules: Vec<AlertRule>) {
        if let Ok(mut guard) = self.rules.write() {
            *guard = rules;
        }
    }

    pub fn update_thermal_settings(&self, settings: ThermalSettings) {
        if let Ok(mut guard) = self.thermal_settings.write() {
            *guard = settings;
        }
    }

    pub fn update_refresh_interval(&self, refresh_interval: Duration) {
        if let Ok(mut guard) = self.refresh_interval.write() {
            *guard = refresh_interval;
        }
    }

    pub fn update_thermal_refresh_interval(&self, thermal_refresh_interval: Duration) {
        if let Ok(mut guard) = self.thermal_refresh_interval.write() {
            *guard = thermal_refresh_interval;
        }
    }

    pub fn update_history_capacity(&self, history_capacity: usize) {
        if let Ok(mut guard) = self.history_capacity.write() {
            *guard = history_capacity.max(1);
        }
    }

    fn spawn_loop(&self, runtime: Handle) {
        let state = Arc::clone(&self.state);
        let rules = Arc::clone(&self.rules);
        let thermal_settings = Arc::clone(&self.thermal_settings);
        let refresh_interval = Arc::clone(&self.refresh_interval);
        let thermal_refresh_interval = Arc::clone(&self.thermal_refresh_interval);
        let history_capacity = Arc::clone(&self.history_capacity);
        let process_state = Arc::clone(&self.process_state);

        runtime.spawn(async move {
            let mut system = System::new();
            let mut disks = Disks::new_with_refreshed_list();
            let mut networks = Networks::new_with_refreshed_list();
            let mut history =
                MetricHistory::new(history_capacity.read().map(|guard| *guard).unwrap_or(180));
            let mut event_log = Vec::<AlertEvent>::new();
            let mut alert_trackers: HashMap<String, AlertTracker> = HashMap::new();
            let mut thermal_manager = ThermalManager::new();
            let mut thermal_runtime = ThermalRuntimeState::default();
            let mut last_thermal_at: Option<Instant> = None;
            let mut last_temperatures = Vec::<TemperatureReading>::new();
            let mut last_thermal = ThermalStatusSnapshot::default();

            loop {
                let current_rules = rules.read().map(|guard| guard.clone()).unwrap_or_default();
                let current_thermal_settings = thermal_settings
                    .read()
                    .map(|guard| guard.clone())
                    .unwrap_or_default();
                let current_history_capacity =
                    history_capacity.read().map(|guard| *guard).unwrap_or(180);
                history.set_capacity(current_history_capacity);
                let thermal_interval = thermal_refresh_interval
                    .read()
                    .map(|guard| *guard)
                    .unwrap_or_else(|_| Duration::from_secs(3));
                let now = Instant::now();
                let thermal_due = last_thermal_at
                    .map(|last| now.duration_since(last) >= thermal_interval)
                    .unwrap_or(true);

                let mut collectors = MetricCollectors {
                    system: &mut system,
                    disks: &mut disks,
                    networks: &mut networks,
                };
                let mut thermal_context = ThermalCollectionContext {
                    settings: &current_thermal_settings,
                    manager: &mut thermal_manager,
                    runtime: &mut thermal_runtime,
                    due: thermal_due,
                    last_temperatures: &mut last_temperatures,
                    last_thermal: &mut last_thermal,
                };

                match collect_metrics(&mut collectors, &process_state, &mut thermal_context) {
                    Ok((snapshot, process_metrics, mut thermal_events)) => {
                        if thermal_due {
                            last_thermal_at = Some(now);
                        }
                        history.push(snapshot.clone());
                        let mut metric_events = evaluate_alerts(
                            &current_rules,
                            &snapshot,
                            &process_metrics,
                            &mut alert_trackers,
                        );
                        metric_events.append(&mut thermal_events);
                        maybe_notify_thermal_events(&current_thermal_settings, &metric_events);

                        if let Ok(mut guard) = state.lock() {
                            guard.latest = Some(Arc::new(snapshot));
                            guard.history = history.samples();
                            event_log.extend(metric_events);
                            if event_log.len() > 150 {
                                let start = event_log.len() - 150;
                                event_log = event_log.split_off(start);
                            }
                            guard.events = Arc::from(event_log.clone());
                            guard.last_error = None;
                        }
                    }
                    Err(error) => {
                        if let Ok(mut guard) = state.lock() {
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

struct MetricCollectors<'a> {
    system: &'a mut System,
    disks: &'a mut Disks,
    networks: &'a mut Networks,
}

struct ThermalCollectionContext<'a> {
    settings: &'a ThermalSettings,
    manager: &'a mut ThermalManager,
    runtime: &'a mut ThermalRuntimeState,
    due: bool,
    last_temperatures: &'a mut Vec<TemperatureReading>,
    last_thermal: &'a mut ThermalStatusSnapshot,
}

fn collect_metrics(
    collectors: &mut MetricCollectors<'_>,
    process_state: &Arc<RwLock<ProcessState>>,
    thermal_context: &mut ThermalCollectionContext<'_>,
) -> anyhow::Result<(MetricSnapshot, Vec<ProcessMetric>, Vec<AlertEvent>)> {
    collectors.system.refresh_cpu_usage();
    collectors.system.refresh_memory();
    collectors.disks.refresh(false);
    collectors.networks.refresh(true);

    let timestamp_utc = Utc::now().timestamp();
    let total_memory = collectors.system.total_memory();
    let used_memory = collectors.system.used_memory();

    let process_metrics = process_metrics_from_state(process_state);
    let top_cpu_processes = top_processes_by_cpu(&process_metrics, 8);
    let top_memory_processes = top_processes_by_memory(&process_metrics, 8);

    let disks_metrics = collectors
        .disks
        .list()
        .iter()
        .map(|disk| DiskMetric {
            name: disk.name().to_string_lossy().to_string(),
            mount_point: disk.mount_point().to_path_buf(),
            total_space_bytes: disk.total_space(),
            available_space_bytes: disk.available_space(),
            used_percent: percent(
                disk.total_space().saturating_sub(disk.available_space()),
                disk.total_space(),
            ),
        })
        .collect::<Vec<_>>();

    let network_received = collectors
        .networks
        .values()
        .map(|network| network.received())
        .sum();
    let network_transmitted = collectors
        .networks
        .values()
        .map(|network| network.transmitted())
        .sum();

    let (temperatures, thermal, thermal_events) = if thermal_context.due {
        match thermal_context.manager.collect() {
            Ok(mut collection) => {
                let (thermal_status, thermal_events) = evaluate_thermal_cycle(
                    timestamp_utc,
                    thermal_context.settings,
                    &mut collection.readings,
                    collection.capabilities,
                    thermal_context.runtime,
                    thermal_context.manager,
                );
                *thermal_context.last_temperatures = collection.readings.clone();
                *thermal_context.last_thermal = thermal_status.clone();
                (collection.readings, thermal_status, thermal_events)
            }
            Err(error) => {
                thermal_context.runtime.last_error = Some(error.to_string());
                let thermal_status = build_thermal_status_snapshot(
                    thermal_context.settings,
                    thermal_context.manager.capabilities(),
                    thermal_context.runtime,
                );
                *thermal_context.last_temperatures = Vec::new();
                *thermal_context.last_thermal = thermal_status.clone();
                (Vec::new(), thermal_status, Vec::new())
            }
        }
    } else {
        (
            thermal_context.last_temperatures.clone(),
            thermal_context.last_thermal.clone(),
            Vec::new(),
        )
    };

    let snapshot = MetricSnapshot {
        timestamp_utc,
        cpu_usage_percent: collectors.system.global_cpu_usage(),
        total_memory_bytes: total_memory,
        used_memory_bytes: used_memory,
        network_received_bytes_per_sec: network_received,
        network_transmitted_bytes_per_sec: network_transmitted,
        disks: disks_metrics,
        top_cpu_processes,
        top_memory_processes,
        temperatures,
        thermal,
    };

    Ok((snapshot, process_metrics, thermal_events))
}

fn top_processes_by_cpu(processes: &[ProcessMetric], limit: usize) -> Vec<ProcessMetric> {
    top_processes_by(processes, limit, |left, right| {
        right
            .cpu
            .partial_cmp(&left.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.memory_bytes.cmp(&left.memory_bytes))
            .then_with(|| left.name.cmp(&right.name))
    })
}

fn top_processes_by_memory(processes: &[ProcessMetric], limit: usize) -> Vec<ProcessMetric> {
    top_processes_by(processes, limit, |left, right| {
        right
            .memory_bytes
            .cmp(&left.memory_bytes)
            .then_with(|| {
                right
                    .cpu
                    .partial_cmp(&left.cpu)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.name.cmp(&right.name))
    })
}

fn top_processes_by(
    processes: &[ProcessMetric],
    limit: usize,
    compare: impl Fn(&ProcessMetric, &ProcessMetric) -> std::cmp::Ordering,
) -> Vec<ProcessMetric> {
    if limit == 0 {
        return Vec::new();
    }

    let mut top = Vec::with_capacity(limit.min(processes.len()));
    for process in processes {
        if top.len() < limit {
            top.push(process.clone());
            top.sort_by(&compare);
        } else if top
            .last()
            .map(|worst| compare(process, worst).is_lt())
            .unwrap_or(true)
        {
            top.pop();
            top.push(process.clone());
            top.sort_by(&compare);
        }
    }
    top
}

fn process_metrics_from_state(process_state: &Arc<RwLock<ProcessState>>) -> Vec<ProcessMetric> {
    process_state
        .read()
        .map(|guard| {
            guard
                .rows
                .iter()
                .map(|row| ProcessMetric {
                    pid: row.key.pid,
                    name: row.name.clone(),
                    cpu: row.cpu_now,
                    memory_bytes: row.memory_bytes,
                    memory_percent: row.insight.memory_percent,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn maybe_notify_thermal_events(settings: &ThermalSettings, events: &[AlertEvent]) {
    if !settings.notifications_enabled {
        return;
    }

    for event in events {
        let should_notify = match event.kind {
            AlertEventKind::TemperatureWarning | AlertEventKind::TemperatureCritical => {
                matches!(event.state, AlertEventState::Active)
            }
            AlertEventKind::CoolingActionFailed => true,
            AlertEventKind::MetricThreshold
            | AlertEventKind::CoolingActionApplied
            | AlertEventKind::CoolingActionRestored => false,
        };

        if !should_notify {
            continue;
        }

        let title = match event.kind {
            AlertEventKind::TemperatureWarning => "WindowsHELP - Temperature elevee",
            AlertEventKind::TemperatureCritical => "WindowsHELP - Temperature critique",
            AlertEventKind::CoolingActionFailed => "WindowsHELP - Refroidissement automatique",
            AlertEventKind::MetricThreshold
            | AlertEventKind::CoolingActionApplied
            | AlertEventKind::CoolingActionRestored => continue,
        };
        let body = format!("{} - {}", event.source_label, event.message);
        let _ = show_toast_notification(title, &body);
    }
}

fn build_thermal_status_snapshot(
    settings: &ThermalSettings,
    capabilities: ThermalCapabilities,
    runtime: &ThermalRuntimeState,
) -> ThermalStatusSnapshot {
    ThermalStatusSnapshot {
        monitoring_enabled: settings.enabled,
        auto_cooling_enabled: settings.auto_cooling_enabled,
        control_available: capabilities.control_supported,
        state: runtime.global_state,
        source: capabilities.source,
        last_action: runtime.last_action.clone(),
        last_error: runtime.last_error.clone(),
        capabilities,
    }
}

fn evaluate_thermal_cycle<C: ThermalAutomationController>(
    timestamp_utc: i64,
    settings: &ThermalSettings,
    readings: &mut [TemperatureReading],
    capabilities: ThermalCapabilities,
    runtime: &mut ThermalRuntimeState,
    controller: &mut C,
) -> (ThermalStatusSnapshot, Vec<AlertEvent>) {
    let mut events = Vec::new();
    let previous_global_state = runtime.global_state;
    let mut next_sensor_states = HashMap::new();
    let mut seen_sensor_ids = HashSet::new();
    let mut next_global_state = ThermalState::Normal;

    for reading in readings.iter_mut() {
        reading.state = ThermalState::Normal;
        let Some(current_temperature) = reading.temperature_celsius else {
            continue;
        };

        let thresholds = thresholds_for_reading(reading, settings);
        reading.warning_limit_celsius = Some(thresholds.warning_celsius);
        reading.critical_limit_celsius = Some(thresholds.critical_celsius);
        seen_sensor_ids.insert(reading.sensor_id.clone());

        let previous_state = runtime
            .sensor_states
            .get(&reading.sensor_id)
            .copied()
            .unwrap_or(ThermalState::Normal);
        let next_state = if settings.enabled {
            next_thermal_state(previous_state, current_temperature, thresholds)
        } else {
            ThermalState::Normal
        };
        reading.state = next_state;

        if next_state != ThermalState::Normal {
            next_sensor_states.insert(reading.sensor_id.clone(), next_state);
        }
        if next_state > next_global_state {
            next_global_state = next_state;
        }
        push_temperature_transition_events(
            &mut events,
            timestamp_utc,
            reading,
            previous_state,
            next_state,
            current_temperature,
            thresholds.warning_celsius,
            thresholds.critical_celsius,
        );
    }

    for (sensor_id, previous_state) in runtime.sensor_states.iter() {
        if seen_sensor_ids.contains(sensor_id) || *previous_state == ThermalState::Normal {
            continue;
        }
        let source_label = sensor_id.clone();
        match previous_state {
            ThermalState::Warning => events.push(AlertEvent {
                kind: AlertEventKind::TemperatureWarning,
                rule_id: "temperature-warning".into(),
                source_label,
                source_pid: None,
                message: "temperature revenue a la normale".into(),
                state: AlertEventState::Resolved,
                value_percent: 0.0,
                threshold_percent: 0.0,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: Some(timestamp_utc),
            }),
            ThermalState::Critical => events.push(AlertEvent {
                kind: AlertEventKind::TemperatureCritical,
                rule_id: "temperature-critical".into(),
                source_label,
                source_pid: None,
                message: "temperature critique terminee".into(),
                state: AlertEventState::Resolved,
                value_percent: 0.0,
                threshold_percent: 0.0,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: Some(timestamp_utc),
            }),
            ThermalState::Normal => {}
        }
    }

    runtime.sensor_states = next_sensor_states;
    runtime.global_state = if settings.enabled {
        next_global_state
    } else {
        ThermalState::Normal
    };

    if !settings.enabled {
        maybe_restore_previous_cooling(
            timestamp_utc,
            runtime,
            controller,
            &mut events,
            "surveillance thermique desactivee",
        );
        let status = build_thermal_status_snapshot(settings, capabilities, runtime);
        return (status, events);
    }

    if previous_global_state != ThermalState::Critical
        && runtime.global_state == ThermalState::Critical
        && settings.auto_cooling_enabled
    {
        if controller.control_available() {
            runtime.previous_control_state = controller.capture_control_state();
            match controller.apply_max_cooling() {
                Ok(action) => {
                    runtime.control_applied_by_app = true;
                    runtime.last_error = None;
                    runtime.last_action = Some(CoolingActionRecord {
                        action,
                        detail: if runtime.previous_control_state.is_some() {
                            "refroidissement automatique applique".into()
                        } else {
                            "refroidissement automatique applique, restauration incertaine".into()
                        },
                        applied_at_utc: timestamp_utc,
                        restored_at_utc: None,
                    });
                    events.push(AlertEvent {
                        kind: AlertEventKind::CoolingActionApplied,
                        rule_id: "cooling-action-applied".into(),
                        source_label: "Refroidissement automatique".into(),
                        source_pid: None,
                        message: format!("{} active", action.label()),
                        state: AlertEventState::Resolved,
                        value_percent: 0.0,
                        threshold_percent: 0.0,
                        triggered_at_utc: timestamp_utc,
                        resolved_at_utc: Some(timestamp_utc),
                    });
                }
                Err(error) => {
                    runtime.control_applied_by_app = false;
                    runtime.last_error =
                        Some(format!("echec du refroidissement automatique: {error}"));
                    events.push(AlertEvent {
                        kind: AlertEventKind::CoolingActionFailed,
                        rule_id: "cooling-action-failed".into(),
                        source_label: "Refroidissement automatique".into(),
                        source_pid: None,
                        message: runtime
                            .last_error
                            .clone()
                            .unwrap_or_else(|| "echec du refroidissement automatique".into()),
                        state: AlertEventState::Active,
                        value_percent: 0.0,
                        threshold_percent: 0.0,
                        triggered_at_utc: timestamp_utc,
                        resolved_at_utc: None,
                    });
                }
            }
        } else {
            runtime.last_error = Some("controle thermique automatique indisponible".into());
            events.push(AlertEvent {
                kind: AlertEventKind::CoolingActionFailed,
                rule_id: "cooling-action-failed".into(),
                source_label: "Refroidissement automatique".into(),
                source_pid: None,
                message: runtime
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "controle thermique indisponible".into()),
                state: AlertEventState::Active,
                value_percent: 0.0,
                threshold_percent: 0.0,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: None,
            });
        }
    }

    if runtime.control_applied_by_app && runtime.global_state != ThermalState::Critical {
        let restore_reason = if previous_global_state == ThermalState::Critical {
            "temperature revenue sous le seuil critique"
        } else {
            "restauration precedente a reessayer"
        };
        maybe_restore_previous_cooling(
            timestamp_utc,
            runtime,
            controller,
            &mut events,
            restore_reason,
        );
    }

    let status = build_thermal_status_snapshot(settings, capabilities, runtime);
    (status, events)
}

fn maybe_restore_previous_cooling<C: ThermalAutomationController>(
    timestamp_utc: i64,
    runtime: &mut ThermalRuntimeState,
    controller: &mut C,
    events: &mut Vec<AlertEvent>,
    reason: &str,
) {
    if !runtime.control_applied_by_app {
        return;
    }

    let Some(previous_state) = runtime.previous_control_state.clone() else {
        runtime.control_applied_by_app = false;
        runtime.last_error = Some(format!(
            "restauration impossible apres {reason}: etat precedent inconnu"
        ));
        events.push(AlertEvent {
            kind: AlertEventKind::CoolingActionFailed,
            rule_id: "cooling-action-failed".into(),
            source_label: "Refroidissement automatique".into(),
            source_pid: None,
            message: runtime
                .last_error
                .clone()
                .unwrap_or_else(|| "restauration impossible".into()),
            state: AlertEventState::Active,
            value_percent: 0.0,
            threshold_percent: 0.0,
            triggered_at_utc: timestamp_utc,
            resolved_at_utc: None,
        });
        return;
    };

    match controller.restore_previous_state(&previous_state) {
        Ok(action) => {
            runtime.control_applied_by_app = false;
            runtime.previous_control_state = None;
            runtime.last_error = None;
            if let Some(last_action) = runtime.last_action.as_mut() {
                last_action.restored_at_utc = Some(timestamp_utc);
            } else {
                runtime.last_action = Some(CoolingActionRecord {
                    action,
                    detail: format!("etat precedent restaure apres {reason}"),
                    applied_at_utc: timestamp_utc,
                    restored_at_utc: Some(timestamp_utc),
                });
            }
            events.push(AlertEvent {
                kind: AlertEventKind::CoolingActionRestored,
                rule_id: "cooling-action-restored".into(),
                source_label: "Refroidissement automatique".into(),
                source_pid: None,
                message: format!("etat precedent restaure apres {reason}"),
                state: AlertEventState::Resolved,
                value_percent: 0.0,
                threshold_percent: 0.0,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: Some(timestamp_utc),
            });
        }
        Err(error) => {
            // Je garde l'etat capture pour pouvoir retenter la restauration au cycle suivant.
            runtime.last_error = Some(format!("echec de la restauration: {error}"));
            events.push(AlertEvent {
                kind: AlertEventKind::CoolingActionFailed,
                rule_id: "cooling-action-failed".into(),
                source_label: "Refroidissement automatique".into(),
                source_pid: None,
                message: runtime
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "echec de la restauration".into()),
                state: AlertEventState::Active,
                value_percent: 0.0,
                threshold_percent: 0.0,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: None,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_temperature_transition_events(
    events: &mut Vec<AlertEvent>,
    timestamp_utc: i64,
    reading: &TemperatureReading,
    previous_state: ThermalState,
    next_state: ThermalState,
    current_temperature: f32,
    warning_celsius: f32,
    critical_celsius: f32,
) {
    match (previous_state, next_state) {
        (ThermalState::Normal, ThermalState::Warning) => events.push(AlertEvent {
            kind: AlertEventKind::TemperatureWarning,
            rule_id: "temperature-warning".into(),
            source_label: reading.name.clone(),
            source_pid: None,
            message: format!(
                "temperature elevee: {:.1} C (seuil {:.1} C)",
                current_temperature, warning_celsius
            ),
            state: AlertEventState::Active,
            value_percent: current_temperature,
            threshold_percent: warning_celsius,
            triggered_at_utc: timestamp_utc,
            resolved_at_utc: None,
        }),
        (ThermalState::Warning, ThermalState::Normal) => events.push(AlertEvent {
            kind: AlertEventKind::TemperatureWarning,
            rule_id: "temperature-warning".into(),
            source_label: reading.name.clone(),
            source_pid: None,
            message: format!("temperature revenue a {:.1} C", current_temperature),
            state: AlertEventState::Resolved,
            value_percent: current_temperature,
            threshold_percent: warning_celsius,
            triggered_at_utc: timestamp_utc,
            resolved_at_utc: Some(timestamp_utc),
        }),
        (ThermalState::Normal, ThermalState::Critical) => events.push(AlertEvent {
            kind: AlertEventKind::TemperatureCritical,
            rule_id: "temperature-critical".into(),
            source_label: reading.name.clone(),
            source_pid: None,
            message: format!(
                "temperature critique: {:.1} C (seuil {:.1} C)",
                current_temperature, critical_celsius
            ),
            state: AlertEventState::Active,
            value_percent: current_temperature,
            threshold_percent: critical_celsius,
            triggered_at_utc: timestamp_utc,
            resolved_at_utc: None,
        }),
        (ThermalState::Warning, ThermalState::Critical) => {
            events.push(AlertEvent {
                kind: AlertEventKind::TemperatureWarning,
                rule_id: "temperature-warning".into(),
                source_label: reading.name.clone(),
                source_pid: None,
                message: format!(
                    "temperature warning remplacee par un etat critique ({:.1} C)",
                    current_temperature
                ),
                state: AlertEventState::Resolved,
                value_percent: current_temperature,
                threshold_percent: warning_celsius,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: Some(timestamp_utc),
            });
            events.push(AlertEvent {
                kind: AlertEventKind::TemperatureCritical,
                rule_id: "temperature-critical".into(),
                source_label: reading.name.clone(),
                source_pid: None,
                message: format!(
                    "temperature critique: {:.1} C (seuil {:.1} C)",
                    current_temperature, critical_celsius
                ),
                state: AlertEventState::Active,
                value_percent: current_temperature,
                threshold_percent: critical_celsius,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: None,
            });
        }
        (ThermalState::Critical, ThermalState::Warning) => {
            events.push(AlertEvent {
                kind: AlertEventKind::TemperatureCritical,
                rule_id: "temperature-critical".into(),
                source_label: reading.name.clone(),
                source_pid: None,
                message: format!(
                    "temperature critique quittee a {:.1} C",
                    current_temperature
                ),
                state: AlertEventState::Resolved,
                value_percent: current_temperature,
                threshold_percent: critical_celsius,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: Some(timestamp_utc),
            });
            events.push(AlertEvent {
                kind: AlertEventKind::TemperatureWarning,
                rule_id: "temperature-warning".into(),
                source_label: reading.name.clone(),
                source_pid: None,
                message: format!(
                    "temperature encore elevee: {:.1} C (seuil {:.1} C)",
                    current_temperature, warning_celsius
                ),
                state: AlertEventState::Active,
                value_percent: current_temperature,
                threshold_percent: warning_celsius,
                triggered_at_utc: timestamp_utc,
                resolved_at_utc: None,
            });
        }
        (ThermalState::Critical, ThermalState::Normal) => events.push(AlertEvent {
            kind: AlertEventKind::TemperatureCritical,
            rule_id: "temperature-critical".into(),
            source_label: reading.name.clone(),
            source_pid: None,
            message: format!(
                "temperature critique terminee a {:.1} C",
                current_temperature
            ),
            state: AlertEventState::Resolved,
            value_percent: current_temperature,
            threshold_percent: critical_celsius,
            triggered_at_utc: timestamp_utc,
            resolved_at_utc: Some(timestamp_utc),
        }),
        (ThermalState::Normal, ThermalState::Normal)
        | (ThermalState::Warning, ThermalState::Warning)
        | (ThermalState::Critical, ThermalState::Critical) => {}
    }
}

fn evaluate_alerts(
    rules: &[AlertRule],
    snapshot: &MetricSnapshot,
    processes: &[ProcessMetric],
    trackers: &mut HashMap<String, AlertTracker>,
) -> Vec<AlertEvent> {
    let now = snapshot.timestamp_utc;
    let mut events = Vec::new();
    let mut expected_keys_by_rule: HashMap<String, HashSet<String>> = HashMap::new();

    for rule in rules.iter().filter(|rule| rule.enabled) {
        let evaluations = build_evaluations(rule, snapshot, processes);
        let rule_expected = expected_keys_by_rule.entry(rule.id.clone()).or_default();
        for evaluation in evaluations {
            rule_expected.insert(evaluation.key.clone());
            let tracker = trackers.entry(evaluation.key.clone()).or_default();
            tracker.rule_id = rule.id.clone();
            tracker.source_label = evaluation.source_label.clone();
            tracker.source_pid = evaluation.source_pid;
            tracker.last_value_percent = evaluation.value_percent;
            tracker.threshold_percent = evaluation.threshold_percent;
            if evaluation.exceeded {
                if tracker.first_exceeded_at.is_none() {
                    tracker.first_exceeded_at = Some(now);
                }
                if !tracker.active
                    && now.saturating_sub(tracker.first_exceeded_at.unwrap_or(now))
                        >= sustain_seconds_i64(rule.sustain_seconds)
                {
                    tracker.active = true;
                    events.push(AlertEvent {
                        kind: AlertEventKind::MetricThreshold,
                        rule_id: rule.id.clone(),
                        source_label: evaluation.source_label.clone(),
                        source_pid: evaluation.source_pid,
                        message: format!(
                            "{} a depasse {:.1}% (actuel : {:.1}%)",
                            evaluation.source_label,
                            evaluation.threshold_percent,
                            evaluation.value_percent
                        ),
                        state: AlertEventState::Active,
                        value_percent: evaluation.value_percent,
                        threshold_percent: evaluation.threshold_percent,
                        triggered_at_utc: now,
                        resolved_at_utc: None,
                    });
                }
            } else if tracker.active {
                tracker.active = false;
                tracker.first_exceeded_at = None;
                events.push(AlertEvent {
                    kind: AlertEventKind::MetricThreshold,
                    rule_id: rule.id.clone(),
                    source_label: evaluation.source_label.clone(),
                    source_pid: evaluation.source_pid,
                    message: format!(
                        "{} est revenu sous {:.1}% (actuel : {:.1}%)",
                        evaluation.source_label,
                        evaluation.threshold_percent,
                        evaluation.value_percent
                    ),
                    state: AlertEventState::Resolved,
                    value_percent: evaluation.value_percent,
                    threshold_percent: evaluation.threshold_percent,
                    triggered_at_utc: now,
                    resolved_at_utc: Some(now),
                });
            } else {
                tracker.first_exceeded_at = None;
            }
        }
    }

    let keys_to_resolve: Vec<String> = trackers
        .iter()
        .filter(|(key, tracker)| {
            tracker.active
                && !expected_keys_by_rule
                    .get(&tracker.rule_id)
                    .map(|keys| keys.contains(*key))
                    .unwrap_or(false)
        })
        .map(|(key, _)| key.clone())
        .collect();

    for key in keys_to_resolve {
        if let Some(tracker) = trackers.get_mut(&key)
            && tracker.active
        {
            tracker.active = false;
            tracker.first_exceeded_at = None;
            events.push(AlertEvent {
                kind: AlertEventKind::MetricThreshold,
                rule_id: tracker.rule_id.clone(),
                source_label: tracker.source_label.clone(),
                source_pid: tracker.source_pid,
                message: format!("{} est revenu a la normale", tracker.source_label),
                state: AlertEventState::Resolved,
                value_percent: tracker.last_value_percent,
                threshold_percent: tracker.threshold_percent,
                triggered_at_utc: now,
                resolved_at_utc: Some(now),
            });
        }
    }

    prune_inactive_alert_trackers(trackers, &expected_keys_by_rule);

    events
}

fn prune_inactive_alert_trackers(
    trackers: &mut HashMap<String, AlertTracker>,
    expected_keys_by_rule: &HashMap<String, HashSet<String>>,
) {
    trackers.retain(|key, tracker| {
        tracker.active
            || expected_keys_by_rule
                .get(&tracker.rule_id)
                .map(|keys| keys.contains(key))
                .unwrap_or(false)
    });
}

fn build_evaluations(
    rule: &AlertRule,
    snapshot: &MetricSnapshot,
    processes: &[ProcessMetric],
) -> Vec<AlertEvaluation> {
    match rule.kind {
        AlertRuleKind::SystemCpu => vec![AlertEvaluation {
            key: format!("{}:system-cpu", rule.id),
            source_label: "CPU systeme".into(),
            source_pid: None,
            value_percent: snapshot.cpu_usage_percent,
            threshold_percent: rule.threshold_percent,
            exceeded: snapshot.cpu_usage_percent >= rule.threshold_percent,
        }],
        AlertRuleKind::SystemMemory => vec![AlertEvaluation {
            key: format!("{}:system-memory", rule.id),
            source_label: "Memoire systeme".into(),
            source_pid: None,
            value_percent: percent(snapshot.used_memory_bytes, snapshot.total_memory_bytes),
            threshold_percent: rule.threshold_percent,
            exceeded: percent(snapshot.used_memory_bytes, snapshot.total_memory_bytes)
                >= rule.threshold_percent,
        }],
        AlertRuleKind::DiskUsage => snapshot
            .disks
            .iter()
            .map(|disk| AlertEvaluation {
                key: format!("{}:{}", rule.id, disk.mount_point.display()),
                source_label: format!("Disque {}", disk.mount_point.display()),
                source_pid: None,
                value_percent: disk.used_percent,
                threshold_percent: rule.threshold_percent,
                exceeded: disk.used_percent >= rule.threshold_percent,
            })
            .collect(),
        AlertRuleKind::ProcessCpu => processes
            .iter()
            .map(|process| AlertEvaluation {
                key: format!("{}:{} ({})", rule.id, process.pid, process.name),
                source_label: format!("{} ({})", process.name, process.pid),
                source_pid: Some(process.pid),
                value_percent: process.cpu,
                threshold_percent: rule.threshold_percent,
                exceeded: process.cpu >= rule.threshold_percent,
            })
            .collect(),
        AlertRuleKind::ProcessMemory => processes
            .iter()
            .map(|process| AlertEvaluation {
                key: format!("{}:{} ({})", rule.id, process.pid, process.name),
                source_label: format!("{} ({})", process.name, process.pid),
                source_pid: Some(process.pid),
                value_percent: process.memory_percent,
                threshold_percent: rule.threshold_percent,
                exceeded: process.memory_percent >= rule.threshold_percent,
            })
            .collect(),
    }
}

fn percent(value: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (value as f64 / total as f64 * 100.0) as f32
    }
}

fn sustain_seconds_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform_windows::PriorityClass;
    use crate::process::{
        ProcessInsight, ProcessKey, ProcessRow, ProcessSafety, ProcessState, ProcessTrend,
        SuggestedAction,
    };
    use crate::thermal::{
        CoolingAction, TemperatureSensorKind, TemperatureSource, ThermalThresholdMode,
        ThermalThresholdPair,
    };
    use anyhow::anyhow;

    struct MockThermalController {
        control_available: bool,
        capture_calls: usize,
        apply_calls: usize,
        restore_calls: usize,
        captured_state: Option<CapturedControlState>,
        apply_result: anyhow::Result<CoolingAction>,
        restore_result: anyhow::Result<CoolingAction>,
    }

    impl ThermalAutomationController for MockThermalController {
        fn control_available(&self) -> bool {
            self.control_available
        }

        fn capture_control_state(&mut self) -> Option<CapturedControlState> {
            self.capture_calls += 1;
            self.captured_state.clone()
        }

        fn apply_max_cooling(&mut self) -> anyhow::Result<CoolingAction> {
            self.apply_calls += 1;
            self.apply_result
                .as_ref()
                .map(|value| *value)
                .map_err(|error| anyhow!(error.to_string()))
        }

        fn restore_previous_state(
            &mut self,
            _state: &CapturedControlState,
        ) -> anyhow::Result<CoolingAction> {
            self.restore_calls += 1;
            self.restore_result
                .as_ref()
                .map(|value| *value)
                .map_err(|error| anyhow!(error.to_string()))
        }
    }

    fn snapshot_with_cpu(timestamp_utc: i64, cpu_usage_percent: f32) -> MetricSnapshot {
        MetricSnapshot {
            timestamp_utc,
            cpu_usage_percent,
            total_memory_bytes: 100,
            used_memory_bytes: 10,
            network_received_bytes_per_sec: 0,
            network_transmitted_bytes_per_sec: 0,
            disks: Vec::new(),
            top_cpu_processes: Vec::new(),
            top_memory_processes: Vec::new(),
            temperatures: Vec::new(),
            thermal: ThermalStatusSnapshot::default(),
        }
    }

    fn process_row_for_metrics(pid: u32, name: &str, cpu: f32, memory_bytes: u64) -> ProcessRow {
        ProcessRow {
            key: ProcessKey {
                pid,
                started_at: Some(pid as u64),
            },
            family_id: name.to_ascii_lowercase(),
            name: name.to_owned(),
            path: None,
            parent_pid: None,
            cpu_now: cpu,
            memory_bytes,
            threads: 1,
            priority: PriorityClass::Normal,
            status: "En cours".into(),
            run_time_secs: 10,
            has_visible_window: true,
            insight: ProcessInsight {
                impact_score: 12,
                cpu_avg_10s: cpu,
                cpu_peak_60s: cpu,
                memory_percent: memory_bytes as f32 / 100.0,
                disk_io_bytes_per_sec: 0,
                safety: ProcessSafety::LikelyClosable,
                suggested_action: SuggestedAction::CloseGracefully,
                trend: ProcessTrend::Stable,
                reasons: Vec::new(),
            },
        }
    }

    #[test]
    fn monitor_process_metrics_are_read_from_shared_process_state() {
        let process_state = Arc::new(RwLock::new(ProcessState {
            revision: 7,
            rows: Arc::from(vec![
                process_row_for_metrics(10, "alpha.exe", 12.5, 2048),
                process_row_for_metrics(11, "beta.exe", 4.0, 4096),
            ]),
            ..ProcessState::default()
        }));

        let metrics = process_metrics_from_state(&process_state);

        assert_eq!(metrics.len(), 2);
        assert_eq!(metrics[0].pid, 10);
        assert_eq!(metrics[0].name, "alpha.exe");
        assert!((metrics[0].cpu - 12.5).abs() < f32::EPSILON);
        assert_eq!(metrics[1].memory_bytes, 4096);
    }

    #[test]
    fn top_process_helpers_keep_limit_and_order() {
        let metrics = vec![
            ProcessMetric {
                pid: 1,
                name: "slow.exe".into(),
                cpu: 1.0,
                memory_bytes: 900,
                memory_percent: 1.0,
            },
            ProcessMetric {
                pid: 2,
                name: "hot.exe".into(),
                cpu: 40.0,
                memory_bytes: 100,
                memory_percent: 2.0,
            },
            ProcessMetric {
                pid: 3,
                name: "warm.exe".into(),
                cpu: 20.0,
                memory_bytes: 2_000,
                memory_percent: 3.0,
            },
        ];

        let top_cpu = top_processes_by_cpu(&metrics, 2);
        assert_eq!(
            top_cpu.iter().map(|metric| metric.pid).collect::<Vec<_>>(),
            vec![2, 3]
        );

        let top_memory = top_processes_by_memory(&metrics, 2);
        assert_eq!(
            top_memory
                .iter()
                .map(|metric| metric.pid)
                .collect::<Vec<_>>(),
            vec![3, 1]
        );
    }

    fn default_thermal_settings() -> ThermalSettings {
        ThermalSettings {
            enabled: true,
            notifications_enabled: true,
            auto_cooling_enabled: true,
            threshold_mode: ThermalThresholdMode::Custom,
            cpu_thresholds: ThermalThresholdPair {
                warning_celsius: 85.0,
                critical_celsius: 95.0,
            },
            gpu_thresholds: ThermalThresholdPair {
                warning_celsius: 85.0,
                critical_celsius: 95.0,
            },
        }
    }

    fn cpu_temperature(current: f32) -> TemperatureReading {
        TemperatureReading {
            sensor_id: "cpu".into(),
            name: "CPU".into(),
            kind: TemperatureSensorKind::Cpu,
            temperature_celsius: Some(current),
            max_temperature_celsius: None,
            critical_temperature_celsius: None,
            warning_limit_celsius: None,
            critical_limit_celsius: None,
            fan_speed_rpm: None,
            source: TemperatureSource::AcerNitro,
            available: true,
            state: ThermalState::Normal,
        }
    }

    #[test]
    fn history_retains_only_capacity() {
        let mut history = MetricHistory::new(3);
        history.push(snapshot_with_cpu(1, 10.0));
        history.push(snapshot_with_cpu(2, 20.0));
        history.push(snapshot_with_cpu(3, 30.0));
        history.push(snapshot_with_cpu(4, 40.0));

        let samples = history.samples();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0].timestamp_utc, 2);
        assert_eq!(samples[2].timestamp_utc, 4);
    }

    #[test]
    fn history_keeps_at_least_one_sample_with_zero_capacity() {
        let mut history = MetricHistory::new(0);
        history.push(snapshot_with_cpu(1, 10.0));
        history.push(snapshot_with_cpu(2, 20.0));

        let samples = history.samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].timestamp_utc, 2);
    }

    #[test]
    fn sustain_seconds_conversion_does_not_wrap() {
        assert_eq!(sustain_seconds_i64(10), 10);
        assert_eq!(sustain_seconds_i64(u64::MAX), i64::MAX);
    }

    #[test]
    fn alerts_debounce_before_triggering_and_then_resolve() {
        let rule = AlertRule {
            id: "system-cpu".into(),
            label: "CPU systeme".into(),
            enabled: true,
            kind: AlertRuleKind::SystemCpu,
            threshold_percent: 80.0,
            sustain_seconds: 10,
        };

        let mut trackers = HashMap::new();
        let no_event = evaluate_alerts(
            std::slice::from_ref(&rule),
            &snapshot_with_cpu(0, 85.0),
            &[],
            &mut trackers,
        );
        assert!(no_event.is_empty());

        let still_no_event = evaluate_alerts(
            std::slice::from_ref(&rule),
            &snapshot_with_cpu(9, 85.0),
            &[],
            &mut trackers,
        );
        assert!(still_no_event.is_empty());

        let active_event = evaluate_alerts(
            std::slice::from_ref(&rule),
            &snapshot_with_cpu(10, 85.0),
            &[],
            &mut trackers,
        );
        assert_eq!(active_event.len(), 1);
        assert!(matches!(active_event[0].state, AlertEventState::Active));

        let resolved = evaluate_alerts(&[rule], &snapshot_with_cpu(11, 20.0), &[], &mut trackers);
        assert_eq!(resolved.len(), 1);
        assert!(matches!(resolved[0].state, AlertEventState::Resolved));
    }

    #[test]
    fn process_alert_evaluations_keep_source_pid() {
        let rule = AlertRule {
            id: "process-cpu".into(),
            label: "CPU process".into(),
            enabled: true,
            kind: AlertRuleKind::ProcessCpu,
            threshold_percent: 50.0,
            sustain_seconds: 1,
        };
        let snapshot = snapshot_with_cpu(0, 10.0);
        let processes = vec![ProcessMetric {
            pid: 42,
            name: "discord.exe".into(),
            cpu: 75.0,
            memory_bytes: 10,
            memory_percent: 1.0,
        }];

        let evaluations = build_evaluations(&rule, &snapshot, &processes);
        assert_eq!(evaluations.len(), 1);
        assert_eq!(evaluations[0].source_pid, Some(42));
    }

    #[test]
    fn process_alert_resolves_with_source_pid_when_process_disappears() {
        let rule = AlertRule {
            id: "process-cpu".into(),
            label: "CPU process".into(),
            enabled: true,
            kind: AlertRuleKind::ProcessCpu,
            threshold_percent: 50.0,
            sustain_seconds: 0,
        };
        let mut trackers = HashMap::new();
        let hot_processes = vec![ProcessMetric {
            pid: 42,
            name: "worker.exe".into(),
            cpu: 75.0,
            memory_bytes: 10,
            memory_percent: 1.0,
        }];

        let active = evaluate_alerts(
            std::slice::from_ref(&rule),
            &snapshot_with_cpu(1, 10.0),
            &hot_processes,
            &mut trackers,
        );
        assert_eq!(active.len(), 1);
        assert!(matches!(active[0].state, AlertEventState::Active));
        assert_eq!(active[0].source_pid, Some(42));

        let resolved = evaluate_alerts(&[rule], &snapshot_with_cpu(2, 10.0), &[], &mut trackers);
        assert_eq!(resolved.len(), 1);
        assert!(matches!(resolved[0].state, AlertEventState::Resolved));
        assert_eq!(resolved[0].source_pid, Some(42));
        assert_eq!(resolved[0].source_label, "worker.exe (42)");
        assert!(trackers.is_empty());
    }

    #[test]
    fn inactive_process_alert_trackers_are_pruned_when_process_disappears() {
        let rule = AlertRule {
            id: "process-cpu".into(),
            label: "CPU process".into(),
            enabled: true,
            kind: AlertRuleKind::ProcessCpu,
            threshold_percent: 50.0,
            sustain_seconds: 10,
        };
        let mut trackers = HashMap::new();
        let hot_processes = vec![ProcessMetric {
            pid: 42,
            name: "short-lived.exe".into(),
            cpu: 75.0,
            memory_bytes: 10,
            memory_percent: 1.0,
        }];

        let pending = evaluate_alerts(
            std::slice::from_ref(&rule),
            &snapshot_with_cpu(1, 10.0),
            &hot_processes,
            &mut trackers,
        );
        assert!(pending.is_empty());
        assert_eq!(trackers.len(), 1);

        let disappeared = evaluate_alerts(&[rule], &snapshot_with_cpu(2, 10.0), &[], &mut trackers);
        assert!(disappeared.is_empty());
        assert!(trackers.is_empty());
    }

    #[test]
    fn active_alert_resolves_when_rule_is_disabled_or_removed() {
        let mut rule = AlertRule {
            id: "system-cpu".into(),
            label: "CPU systeme".into(),
            enabled: true,
            kind: AlertRuleKind::SystemCpu,
            threshold_percent: 80.0,
            sustain_seconds: 0,
        };
        let mut trackers = HashMap::new();

        let active = evaluate_alerts(
            std::slice::from_ref(&rule),
            &snapshot_with_cpu(1, 85.0),
            &[],
            &mut trackers,
        );
        assert_eq!(active.len(), 1);
        assert!(matches!(active[0].state, AlertEventState::Active));

        rule.enabled = false;
        let resolved = evaluate_alerts(&[rule], &snapshot_with_cpu(2, 85.0), &[], &mut trackers);
        assert_eq!(resolved.len(), 1);
        assert!(matches!(resolved[0].state, AlertEventState::Resolved));
        assert_eq!(resolved[0].rule_id, "system-cpu");

        let rule = AlertRule {
            id: "system-memory".into(),
            label: "Memoire systeme".into(),
            enabled: true,
            kind: AlertRuleKind::SystemMemory,
            threshold_percent: 5.0,
            sustain_seconds: 0,
        };
        let active = evaluate_alerts(
            std::slice::from_ref(&rule),
            &snapshot_with_cpu(3, 10.0),
            &[],
            &mut trackers,
        );
        assert_eq!(active.len(), 1);

        let resolved = evaluate_alerts(&[], &snapshot_with_cpu(4, 10.0), &[], &mut trackers);
        assert_eq!(resolved.len(), 1);
        assert!(matches!(resolved[0].state, AlertEventState::Resolved));
        assert_eq!(resolved[0].rule_id, "system-memory");
    }

    #[test]
    fn thermal_cycle_does_not_reapply_cooling_while_state_stays_critical() {
        let settings = default_thermal_settings();
        let mut runtime = ThermalRuntimeState::default();
        let mut controller = MockThermalController {
            control_available: true,
            capture_calls: 0,
            apply_calls: 0,
            restore_calls: 0,
            captured_state: Some(CapturedControlState::AcerNitro {
                fan_control: None,
                operating_mode: Some(1),
            }),
            apply_result: Ok(CoolingAction::FanMax),
            restore_result: Ok(CoolingAction::TurboMode),
        };

        let mut first_readings = vec![cpu_temperature(97.0)];
        let _ = evaluate_thermal_cycle(
            1,
            &settings,
            &mut first_readings,
            ThermalCapabilities {
                source: TemperatureSource::AcerNitro,
                read_supported: true,
                control_supported: true,
                fan_control_supported: true,
                operating_mode_supported: true,
            },
            &mut runtime,
            &mut controller,
        );

        let mut second_readings = vec![cpu_temperature(98.0)];
        let _ = evaluate_thermal_cycle(
            2,
            &settings,
            &mut second_readings,
            ThermalCapabilities {
                source: TemperatureSource::AcerNitro,
                read_supported: true,
                control_supported: true,
                fan_control_supported: true,
                operating_mode_supported: true,
            },
            &mut runtime,
            &mut controller,
        );

        assert_eq!(controller.apply_calls, 1);
        assert_eq!(controller.capture_calls, 1);
    }

    #[test]
    fn thermal_cycle_restores_only_when_previous_state_is_known() {
        let settings = default_thermal_settings();
        let mut runtime = ThermalRuntimeState {
            sensor_states: HashMap::new(),
            global_state: ThermalState::Critical,
            previous_control_state: Some(CapturedControlState::AcerNitro {
                fan_control: None,
                operating_mode: Some(1),
            }),
            control_applied_by_app: true,
            last_action: None,
            last_error: None,
        };
        let mut controller = MockThermalController {
            control_available: true,
            capture_calls: 0,
            apply_calls: 0,
            restore_calls: 0,
            captured_state: None,
            apply_result: Ok(CoolingAction::FanMax),
            restore_result: Ok(CoolingAction::TurboMode),
        };

        let mut readings = vec![cpu_temperature(89.0)];
        let _ = evaluate_thermal_cycle(
            10,
            &settings,
            &mut readings,
            ThermalCapabilities {
                source: TemperatureSource::AcerNitro,
                read_supported: true,
                control_supported: true,
                fan_control_supported: true,
                operating_mode_supported: true,
            },
            &mut runtime,
            &mut controller,
        );

        assert_eq!(controller.restore_calls, 1);

        let mut no_state_runtime = ThermalRuntimeState {
            sensor_states: HashMap::new(),
            global_state: ThermalState::Critical,
            previous_control_state: None,
            control_applied_by_app: true,
            last_action: None,
            last_error: None,
        };
        let mut second_controller = MockThermalController {
            control_available: true,
            capture_calls: 0,
            apply_calls: 0,
            restore_calls: 0,
            captured_state: None,
            apply_result: Ok(CoolingAction::FanMax),
            restore_result: Ok(CoolingAction::TurboMode),
        };

        let mut second_readings = vec![cpu_temperature(89.0)];
        let (_, events) = evaluate_thermal_cycle(
            11,
            &settings,
            &mut second_readings,
            ThermalCapabilities {
                source: TemperatureSource::AcerNitro,
                read_supported: true,
                control_supported: true,
                fan_control_supported: true,
                operating_mode_supported: true,
            },
            &mut no_state_runtime,
            &mut second_controller,
        );

        assert_eq!(second_controller.restore_calls, 0);
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, AlertEventKind::CoolingActionFailed))
        );
    }

    #[test]
    fn thermal_cycle_resolves_active_sensor_when_monitoring_is_disabled() {
        let mut settings = default_thermal_settings();
        settings.enabled = false;
        let mut runtime = ThermalRuntimeState {
            sensor_states: HashMap::from([("cpu".into(), ThermalState::Critical)]),
            global_state: ThermalState::Critical,
            previous_control_state: None,
            control_applied_by_app: false,
            last_action: None,
            last_error: None,
        };
        let mut controller = MockThermalController {
            control_available: true,
            capture_calls: 0,
            apply_calls: 0,
            restore_calls: 0,
            captured_state: None,
            apply_result: Ok(CoolingAction::FanMax),
            restore_result: Ok(CoolingAction::TurboMode),
        };

        let mut readings = vec![cpu_temperature(80.0)];
        let (status, events) = evaluate_thermal_cycle(
            20,
            &settings,
            &mut readings,
            ThermalCapabilities {
                source: TemperatureSource::AcerNitro,
                read_supported: true,
                control_supported: true,
                fan_control_supported: true,
                operating_mode_supported: true,
            },
            &mut runtime,
            &mut controller,
        );

        assert_eq!(status.state, ThermalState::Normal);
        assert!(runtime.sensor_states.is_empty());
        assert_eq!(readings[0].state, ThermalState::Normal);
        assert!(events.iter().any(|event| {
            matches!(event.kind, AlertEventKind::TemperatureCritical)
                && matches!(event.state, AlertEventState::Resolved)
                && event.source_label == "CPU"
        }));
    }

    #[test]
    fn thermal_cycle_retries_restore_after_transient_failure() {
        let settings = default_thermal_settings();
        let mut runtime = ThermalRuntimeState {
            sensor_states: HashMap::new(),
            global_state: ThermalState::Critical,
            previous_control_state: Some(CapturedControlState::AcerNitro {
                fan_control: None,
                operating_mode: Some(1),
            }),
            control_applied_by_app: true,
            last_action: None,
            last_error: None,
        };
        let mut controller = MockThermalController {
            control_available: true,
            capture_calls: 0,
            apply_calls: 0,
            restore_calls: 0,
            captured_state: None,
            apply_result: Ok(CoolingAction::FanMax),
            restore_result: Err(anyhow!("erreur transitoire")),
        };

        let mut readings = vec![cpu_temperature(80.0)];
        let (_, failed_events) = evaluate_thermal_cycle(
            30,
            &settings,
            &mut readings,
            ThermalCapabilities {
                source: TemperatureSource::AcerNitro,
                read_supported: true,
                control_supported: true,
                fan_control_supported: true,
                operating_mode_supported: true,
            },
            &mut runtime,
            &mut controller,
        );

        assert_eq!(controller.restore_calls, 1);
        assert!(runtime.control_applied_by_app);
        assert!(runtime.previous_control_state.is_some());
        assert!(
            failed_events
                .iter()
                .any(|event| matches!(event.kind, AlertEventKind::CoolingActionFailed))
        );

        controller.restore_result = Ok(CoolingAction::TurboMode);
        let mut retry_readings = vec![cpu_temperature(78.0)];
        let (_, retry_events) = evaluate_thermal_cycle(
            31,
            &settings,
            &mut retry_readings,
            ThermalCapabilities {
                source: TemperatureSource::AcerNitro,
                read_supported: true,
                control_supported: true,
                fan_control_supported: true,
                operating_mode_supported: true,
            },
            &mut runtime,
            &mut controller,
        );

        assert_eq!(controller.restore_calls, 2);
        assert!(!runtime.control_applied_by_app);
        assert!(runtime.previous_control_state.is_none());
        assert!(
            retry_events
                .iter()
                .any(|event| matches!(event.kind, AlertEventKind::CoolingActionRestored))
        );
    }
}
