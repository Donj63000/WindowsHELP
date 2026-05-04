use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, TimeZone, Utc};
use eframe::egui::{self, Color32, CornerRadius, RichText, Stroke, Vec2};
use tokio::runtime::Runtime;

use crate::config::{PerformanceMode, Settings, app_paths, load_or_create_settings, save_settings};
use crate::monitor::{AlertEvent, AlertEventState, AlertRule, MonitorService, ProcessMetric};
use crate::platform_windows::{PriorityClass, open_path, primary_work_area, reveal_in_explorer};
use crate::process::{
    ProcessAction, ProcessActionResult, ProcessFamily, ProcessKey, ProcessManager, ProcessRow,
    ProcessSafety, ProcessState, SuggestedAction,
};
use crate::search::{SearchQuery, SearchResult, SearchService, parse_date_filter};
use crate::theme::{self, CardTone};
use crate::thermal::{
    TemperatureReading, TemperatureSensorKind, ThermalSettings, ThermalState, ThermalThresholdMode,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Search,
    Processes,
    Monitor,
    Temperatures,
    Settings,
}

impl View {
    fn label(self) -> &'static str {
        match self {
            Self::Search => "Recherche",
            Self::Processes => "Processus",
            Self::Monitor => "Monitoring",
            Self::Temperatures => "Thermique",
            Self::Settings => "Réglages",
        }
    }

    /// Sous-titre affiche dans la top bar.
    fn description(self) -> &'static str {
        match self {
            Self::Search => "Recherche instantanée dans l'index local sans bloquer l'interface.",
            Self::Processes => "Inspection, tri et contrôle des processus Windows en direct.",
            Self::Monitor => {
                "Vue temps réel du CPU, de la mémoire, du réseau, des disques et des alertes."
            }
            Self::Temperatures => {
                "Suivi thermique, capteurs disponibles et automatisation du refroidissement."
            }
            Self::Settings => {
                "Configuration persistante de l'indexation, de la surveillance et des seuils."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppIcon {
    Logo,
    Search,
    Processes,
    Monitor,
    Thermal,
    Settings,
    Cpu,
    Memory,
    Network,
    Storage,
    Alerts,
    Shield,
    More,
    Chrome,
    Code,
    Music,
    Chat,
    Terminal,
    GenericProcess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessTab {
    Families,
    Instances,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessSort {
    Impact,
    CpuNow,
    CpuAverage,
    Memory,
    Name,
}

impl ProcessSort {
    fn label(self) -> &'static str {
        match self {
            Self::Impact => "Impact",
            Self::CpuNow => "CPU instantane",
            Self::CpuAverage => "CPU moyen 10s",
            Self::Memory => "Memoire",
            Self::Name => "Nom",
        }
    }

    fn all() -> [Self; 5] {
        [
            Self::Impact,
            Self::CpuNow,
            Self::CpuAverage,
            Self::Memory,
            Self::Name,
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessDetailTab {
    Details,
    Performance,
    OpenFiles,
    Connections,
}

impl ProcessDetailTab {
    fn label(self) -> &'static str {
        match self {
            Self::Details => "Détails",
            Self::Performance => "Performance",
            Self::OpenFiles => "Fichiers ouverts",
            Self::Connections => "Connexions",
        }
    }

    fn all() -> [Self; 4] {
        [
            Self::Details,
            Self::Performance,
            Self::OpenFiles,
            Self::Connections,
        ]
    }
}

const PROCESS_PAGE_SIZE: usize = 10;
const PROCESS_PANEL_GAP: f32 = 12.0;
const PROCESS_KPI_COUNT: usize = 6;
const PROCESS_KPI_GAP: f32 = 10.0;
const PROCESS_KPI_HEIGHT: f32 = 118.0;
const PROCESS_KPI_MIN_CARD_WIDTH: f32 = 222.0;
const SEARCH_RESULT_LIMIT: usize = 200;
const SUMMARY_CARD_GAP: f32 = 10.0;
const SUMMARY_CARD_HEIGHT: f32 = 116.0;
const SUMMARY_CARD_MIN_WIDTH: f32 = 224.0;
const TEMPERATURE_CARD_GAP: f32 = 10.0;
const TEMPERATURE_CARD_HEIGHT: f32 = 160.0;
const TEMPERATURE_CARD_MIN_WIDTH: f32 = 300.0;

const TARGET_WINDOW_SIZE: Vec2 = Vec2::new(1600.0, 900.0);
const DESIRED_MIN_WINDOW_SIZE: Vec2 = Vec2::new(1180.0, 720.0);
const WINDOW_SCREEN_MARGIN: Vec2 = Vec2::new(60.0, 56.0);
const WORKSPACE_CONTENT_MAX_WIDTH: f32 = 1020.0;

#[derive(Clone, Copy, Debug)]
struct NativeWindowSizes {
    initial: Vec2,
    minimum: Vec2,
}

#[derive(Clone, Copy, Debug)]
struct TopBarLayout {
    search_width: f32,
    drag_width: f32,
    brand_gap: f32,
    view_gap: f32,
    show_view_label: bool,
    show_shortcut: bool,
    show_statuses: bool,
}

#[derive(Clone, Copy, Debug)]
struct KpiGridLayout {
    columns: usize,
    rows: usize,
    card_width: f32,
    total_height: f32,
}

#[derive(Clone, Copy, Debug)]
struct ResponsiveCardGridLayout {
    columns: usize,
    rows: usize,
    card_width: f32,
    total_height: f32,
}

struct KpiCardSpec {
    icon: AppIcon,
    title: &'static str,
    value: String,
    subtitle: String,
    tone: CardTone,
    samples: Vec<f32>,
}

struct SummaryCardSpec {
    title: &'static str,
    value: String,
    subtitle: String,
    tone: CardTone,
}

#[derive(Clone, Copy, Debug)]
struct ProcessDashboardLayout {
    width: f32,
    kpi_height: f32,
    main_height: f32,
    events_height: f32,
    gap: f32,
    left_width: f32,
    right_width: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessRowsCacheKey {
    revision: u64,
    filter: String,
    hide_windows_processes: bool,
    show_only_suspects: bool,
    show_only_closeable: bool,
    sort: ProcessSort,
    alerted_pids: Vec<u32>,
}

#[derive(Default)]
struct ProcessRowsCache {
    key: Option<ProcessRowsCacheKey>,
    indices: Vec<usize>,
    #[cfg(test)]
    rebuilds: u64,
}

struct ProcessRowsCacheInput<'a> {
    filter: &'a str,
    hide_windows_processes: bool,
    show_only_suspects: bool,
    show_only_closeable: bool,
    sort: ProcessSort,
    alerted_pids: &'a HashSet<u32>,
}

#[derive(Clone)]
struct SettingsDraft {
    roots_text: String,
    exclusions_text: String,
    include_hidden: bool,
    include_system: bool,
    scan_concurrency: usize,
    performance_mode: PerformanceMode,
    process_refresh_ms: u64,
    monitor_refresh_ms: u64,
    alert_rules: Vec<AlertRule>,
    thermal_enabled: bool,
    thermal_notifications_enabled: bool,
    thermal_auto_cooling_enabled: bool,
    thermal_threshold_mode: ThermalThresholdMode,
    cpu_warning_celsius: f32,
    cpu_critical_celsius: f32,
    gpu_warning_celsius: f32,
    gpu_critical_celsius: f32,
}

impl SettingsDraft {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            roots_text: settings
                .index
                .roots
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            exclusions_text: settings.index.exclusions.join("\n"),
            include_hidden: settings.index.include_hidden,
            include_system: settings.index.include_system,
            scan_concurrency: settings.index.scan_concurrency,
            performance_mode: settings.performance_mode,
            process_refresh_ms: settings.process_refresh_ms,
            monitor_refresh_ms: settings.monitor_refresh_ms,
            alert_rules: settings.alert_rules.clone(),
            thermal_enabled: settings.thermal.enabled,
            thermal_notifications_enabled: settings.thermal.notifications_enabled,
            thermal_auto_cooling_enabled: settings.thermal.auto_cooling_enabled,
            thermal_threshold_mode: settings.thermal.threshold_mode,
            cpu_warning_celsius: settings.thermal.cpu_thresholds.warning_celsius,
            cpu_critical_celsius: settings.thermal.cpu_thresholds.critical_celsius,
            gpu_warning_celsius: settings.thermal.gpu_thresholds.warning_celsius,
            gpu_critical_celsius: settings.thermal.gpu_thresholds.critical_celsius,
        }
    }

    fn to_settings(&self, current: &Settings) -> anyhow::Result<Settings> {
        let paths = app_paths()?;
        let roots = self
            .roots_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect::<Vec<_>>();

        let exclusions = self
            .exclusions_text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        let thermal = ThermalSettings {
            enabled: self.thermal_enabled,
            notifications_enabled: self.thermal_notifications_enabled,
            auto_cooling_enabled: self.thermal_auto_cooling_enabled,
            threshold_mode: self.thermal_threshold_mode,
            cpu_thresholds: crate::thermal::ThermalThresholdPair {
                warning_celsius: self.cpu_warning_celsius,
                critical_celsius: self.cpu_critical_celsius,
            },
            gpu_thresholds: crate::thermal::ThermalThresholdPair {
                warning_celsius: self.gpu_warning_celsius,
                critical_celsius: self.gpu_critical_celsius,
            },
        };

        if !thermal.cpu_thresholds.is_valid() {
            anyhow::bail!("Les seuils CPU doivent respecter warning < critical");
        }
        if !thermal.gpu_thresholds.is_valid() {
            anyhow::bail!("Les seuils GPU doivent respecter warning < critical");
        }

        let mut settings = Settings {
            index: crate::config::IndexConfig {
                roots: if roots.is_empty() {
                    current.index.roots.clone()
                } else {
                    roots
                },
                exclusions,
                include_hidden: self.include_hidden,
                include_system: self.include_system,
                scan_concurrency: self.scan_concurrency.max(1),
                db_path: paths.index_db_path,
            },
            performance_mode: self.performance_mode,
            process_refresh_ms: self.process_refresh_ms.max(250),
            monitor_refresh_ms: self.monitor_refresh_ms.max(250),
            alert_rules: self.alert_rules.clone(),
            thermal,
            saved_at_utc: Utc::now().timestamp(),
        };
        settings.sanitize();
        Ok(settings)
    }

    fn apply_performance_profile(&mut self) {
        let profile = self.performance_mode.profile();
        self.process_refresh_ms = profile.process_refresh_ms;
        self.monitor_refresh_ms = profile.monitor_refresh_ms;
    }
}

pub struct WindowsHelpApp {
    _runtime: Arc<Runtime>,
    settings: Settings,
    settings_draft: SettingsDraft,
    search_service: SearchService,
    process_manager: ProcessManager,
    monitor_service: MonitorService,
    current_view: View,
    search_text: String,
    extension_filter: String,
    min_size_filter: String,
    max_size_filter: String,
    modified_after_filter: String,
    modified_before_filter: String,
    include_hidden_results: bool,
    global_search: String,
    search_results: Vec<SearchResult>,
    last_search_fingerprint: String,
    process_filter: String,
    selected_process: Option<ProcessKey>,
    selected_family: Option<String>,
    process_sort: ProcessSort,
    process_tab: ProcessTab,
    process_page: usize,
    process_detail_tab: ProcessDetailTab,
    hide_windows_processes: bool,
    show_only_suspects: bool,
    show_only_closeable: bool,
    process_rows_cache: ProcessRowsCache,
    confirm_kill: Option<(ProcessKey, String)>,
    status_message: Option<String>,
    window_maximized: bool,
}

impl WindowsHelpApp {
    fn build(runtime: Arc<Runtime>, settings: Settings) -> anyhow::Result<Self> {
        let handle = runtime.handle().clone();
        let search_service = SearchService::new(handle.clone(), settings.index.clone())?;
        let process_manager = ProcessManager::new(
            handle.clone(),
            Duration::from_millis(settings.process_refresh_ms),
        );
        let process_state = process_manager.shared_state();
        let performance_profile = settings.performance_mode.profile();
        let monitor_service = MonitorService::new(
            handle,
            Duration::from_millis(settings.monitor_refresh_ms),
            Duration::from_millis(performance_profile.thermal_refresh_ms),
            performance_profile.history_capacity,
            process_state,
            settings.alert_rules.clone(),
            settings.thermal.clone(),
        );
        let settings_draft = SettingsDraft::from_settings(&settings);

        let mut app = Self {
            _runtime: runtime,
            settings,
            settings_draft,
            search_service,
            process_manager,
            monitor_service,
            current_view: View::Processes,
            search_text: String::new(),
            extension_filter: String::new(),
            min_size_filter: String::new(),
            max_size_filter: String::new(),
            modified_after_filter: String::new(),
            modified_before_filter: String::new(),
            include_hidden_results: false,
            global_search: String::new(),
            search_results: Vec::new(),
            last_search_fingerprint: String::new(),
            process_filter: String::new(),
            selected_process: None,
            selected_family: None,
            process_sort: ProcessSort::CpuNow,
            process_tab: ProcessTab::Instances,
            process_page: 0,
            process_detail_tab: ProcessDetailTab::Details,
            hide_windows_processes: true,
            show_only_suspects: false,
            show_only_closeable: false,
            process_rows_cache: ProcessRowsCache::default(),
            confirm_kill: None,
            status_message: None,
            window_maximized: false,
        };
        app.refresh_search_results();
        Ok(app)
    }

    fn search_query(&self) -> SearchQuery {
        SearchQuery {
            text: self.search_text.clone(),
            extension: Some(self.extension_filter.clone()).filter(|value| !value.trim().is_empty()),
            min_size: self.min_size_filter.trim().parse::<u64>().ok(),
            max_size: self.max_size_filter.trim().parse::<u64>().ok(),
            modified_after: parse_date_filter(&self.modified_after_filter),
            modified_before: parse_date_filter(&self.modified_before_filter)
                .map(|value| value + 86_399),
            include_hidden: self.include_hidden_results,
        }
    }

    fn refresh_search_results(&mut self) {
        let search_status = self.search_service.status();
        let fingerprint = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{:?}",
            self.search_text,
            self.extension_filter,
            self.min_size_filter,
            self.max_size_filter,
            self.modified_after_filter,
            self.modified_before_filter,
            self.include_hidden_results,
            search_status.snapshot_revision,
            search_status.last_scan_completed,
        );
        if fingerprint == self.last_search_fingerprint {
            return;
        }
        self.last_search_fingerprint = fingerprint;

        let has_active_filters = !self.search_text.trim().is_empty()
            || !self.extension_filter.trim().is_empty()
            || !self.min_size_filter.trim().is_empty()
            || !self.max_size_filter.trim().is_empty()
            || !self.modified_after_filter.trim().is_empty()
            || !self.modified_before_filter.trim().is_empty()
            || self.include_hidden_results;

        if !has_active_filters {
            self.search_results.clear();
            return;
        }

        if !search_status.snapshot_loaded {
            self.search_results.clear();
            return;
        }

        self.search_results = self
            .search_service
            .search(&self.search_query(), SEARCH_RESULT_LIMIT);
    }

    fn active_alerts(events: &[AlertEvent]) -> Vec<AlertEvent> {
        let mut latest_by_source: HashMap<(String, Option<u32>, String), AlertEvent> =
            HashMap::new();
        for event in events {
            latest_by_source.insert(
                (
                    event.rule_id.clone(),
                    event.source_pid,
                    event.source_label.clone(),
                ),
                event.clone(),
            );
        }
        latest_by_source
            .into_values()
            .filter(|event| {
                matches!(event.state, AlertEventState::Active) && event.is_persistent_alert()
            })
            .collect()
    }

    fn save_settings(&mut self) {
        match self.settings_draft.to_settings(&self.settings) {
            Ok(settings) => {
                if let Err(error) = save_settings(&settings) {
                    self.status_message = Some(format!(
                        "Échec de l'enregistrement des paramètres : {error}"
                    ));
                    return;
                }
                self.search_service.update_config(settings.index.clone());
                self.process_manager
                    .update_refresh_interval(Duration::from_millis(settings.process_refresh_ms));
                self.monitor_service
                    .update_refresh_interval(Duration::from_millis(settings.monitor_refresh_ms));
                let profile = settings.performance_mode.profile();
                self.monitor_service
                    .update_thermal_refresh_interval(Duration::from_millis(
                        profile.thermal_refresh_ms,
                    ));
                self.monitor_service
                    .update_history_capacity(profile.history_capacity);
                self.monitor_service
                    .update_rules(settings.alert_rules.clone());
                self.monitor_service
                    .update_thermal_settings(settings.thermal.clone());
                self.settings = settings.clone();
                self.settings_draft = SettingsDraft::from_settings(&settings);
                self.status_message = Some("Paramètres enregistrés et services mis à jour.".into());
            }
            Err(error) => {
                self.status_message = Some(format!("Paramètres invalides : {error}"));
            }
        }
    }

    fn repaint_interval(&self) -> Duration {
        let profile = self.settings.performance_mode.profile();
        let millis = match self.current_view {
            View::Processes => profile.ui_idle_ms.min(profile.process_refresh_ms),
            View::Monitor => profile.ui_idle_ms.min(profile.monitor_refresh_ms),
            View::Temperatures => profile.ui_idle_ms.min(profile.thermal_refresh_ms),
            View::Search | View::Settings => profile.ui_idle_ms,
        };
        Duration::from_millis(millis)
    }

    fn show_sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("navigation")
            .resizable(false)
            .default_width(124.0)
            .frame(theme::sidebar_frame())
            .show(ctx, |ui| {
                ui.set_width(124.0);
                ui.add_space(26.0);

                for (view, icon) in [
                    (View::Search, AppIcon::Search),
                    (View::Processes, AppIcon::Processes),
                    (View::Monitor, AppIcon::Monitor),
                    (View::Temperatures, AppIcon::Thermal),
                    (View::Settings, AppIcon::Settings),
                ] {
                    if nav_button(ui, icon, view.label(), self.current_view == view) {
                        self.current_view = view;
                    }
                    ui.add_space(8.0);
                }

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(22.0);
                    ui.label(RichText::new("≪").size(22.0).color(theme::TEXT_SECONDARY));
                });
            });
    }

    fn show_top_bar(&mut self, ctx: &egui::Context, active_alerts: usize) {
        if ctx.input(|input| input.modifiers.ctrl && input.key_pressed(egui::Key::K)) {
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("global-search")));
        }

        let search_status = self.search_service.status();
        egui::TopBottomPanel::top("top-bar")
            .exact_height(62.0)
            .frame(theme::topbar_frame())
            .show(ctx, |ui| {
                let full_rect = ui.max_rect();
                let layout = top_bar_layout(full_rect.width());
                let drag_rect =
                    egui::Rect::from_min_size(full_rect.min, egui::vec2(layout.drag_width, 62.0));
                let drag_response =
                    ui.interact(drag_rect, egui::Id::new("title-drag"), egui::Sense::drag());
                if drag_response.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                ui.horizontal(|ui| {
                    ui.set_height(62.0);
                    ui.add_space(16.0);
                    draw_icon(ui, AppIcon::Logo, 28.0, theme::ORANGE);
                    ui.label(
                        RichText::new("WindowsHELP")
                            .text_style(egui::TextStyle::Name("Hero".into()))
                            .strong()
                            .color(theme::TEXT_PRIMARY),
                    );
                    ui.add_space(layout.brand_gap);
                    if layout.show_view_label {
                        draw_icon(ui, view_icon(self.current_view), 26.0, theme::TEXT_PRIMARY);
                        ui.label(
                            RichText::new(self.current_view.label())
                                .text_style(egui::TextStyle::Name("Hero".into()))
                                .strong()
                                .color(theme::TEXT_PRIMARY),
                        )
                        .on_hover_text(self.current_view.description());
                        ui.add_space(layout.view_gap);
                    }

                    let search_response = ui.add_sized(
                        [layout.search_width, 40.0],
                        egui::TextEdit::singleline(&mut self.global_search)
                            .id(egui::Id::new("global-search"))
                            .hint_text("Rechercher (processus, fichier, chemin...)"),
                    );
                    if search_response.changed() {
                        match self.current_view {
                            View::Processes => {
                                self.process_filter = self.global_search.clone();
                                self.process_page = 0;
                            }
                            View::Search => {
                                self.search_text = self.global_search.clone();
                                self.last_search_fingerprint.clear();
                            }
                            _ => {}
                        }
                    }
                    ui.add_space(8.0);
                    if layout.show_shortcut {
                        ui.label(
                            RichText::new("Ctrl+K")
                                .size(13.0)
                                .color(theme::TEXT_SECONDARY),
                        );
                    }
                    top_icon_count(ui, AppIcon::Alerts, active_alerts);
                    if layout.show_statuses {
                        top_status(
                            ui,
                            "Monitoring",
                            true,
                            if active_alerts == 0 {
                                theme::ORANGE_SOFT
                            } else {
                                theme::WARNING
                            },
                        );
                        top_status(
                            ui,
                            "Indexé",
                            search_status.snapshot_loaded,
                            if search_status.snapshot_loaded {
                                theme::ORANGE_SOFT
                            } else {
                                theme::WARNING
                            },
                        );
                    }
                    window_button(ui, "−", theme::TEXT_PRIMARY, || {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                    });
                    window_button(ui, "□", theme::TEXT_PRIMARY, || {
                        self.window_maximized = !self.window_maximized;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(
                            self.window_maximized,
                        ));
                    });
                    window_button(ui, "×", theme::TEXT_PRIMARY, || {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    });
                });
            });

        if let Some(message) = &self.status_message {
            egui::TopBottomPanel::top("status-message")
                .exact_height(34.0)
                .frame(theme::topbar_frame())
                .show(ctx, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.label(RichText::new(message).size(13.0).color(theme::WARNING));
                    });
                });
        }
    }

    fn show_footer(
        &self,
        ctx: &egui::Context,
        monitor_state: &crate::monitor::MonitorSnapshotState,
    ) {
        egui::TopBottomPanel::bottom("footer")
            .exact_height(44.0)
            .frame(theme::topbar_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(28.0);
                    if let Some(snapshot) = monitor_state.latest.as_ref() {
                        footer_metric(ui, "CPU", format!("{:.0}%", snapshot.cpu_usage_percent));
                        footer_metric(
                            ui,
                            "RAM",
                            format!(
                                "{:.0}%",
                                percent(snapshot.used_memory_bytes, snapshot.total_memory_bytes)
                            ),
                        );
                        footer_metric(
                            ui,
                            "Réseau ↓",
                            format!(
                                "{}/s",
                                format_bytes(snapshot.network_received_bytes_per_sec)
                            ),
                        );
                        footer_metric(
                            ui,
                            "↑",
                            format!(
                                "{}/s",
                                format_bytes(snapshot.network_transmitted_bytes_per_sec)
                            ),
                        );
                        footer_metric(
                            ui,
                            "Stockage",
                            snapshot
                                .disks
                                .first()
                                .map(|disk| format!("{:.0}%", disk.used_percent))
                                .unwrap_or_else(|| "-".into()),
                        );
                    } else {
                        footer_metric(ui, "CPU", "-");
                        footer_metric(ui, "RAM", "-");
                        footer_metric(ui, "Réseau", "-");
                        footer_metric(ui, "Stockage", "-");
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(28.0);
                        ui.label(
                            RichText::new("v1.0.0")
                                .size(13.0)
                                .color(theme::TEXT_SECONDARY),
                        );
                        ui.separator();
                        ui.label(
                            RichText::new("Mode sombre")
                                .size(13.0)
                                .color(theme::TEXT_SECONDARY),
                        );
                        ui.separator();
                        ui.label(
                            RichText::new(format!(
                                "Mode {}",
                                self.settings.performance_mode.label()
                            ))
                            .size(13.0)
                            .color(theme::TEXT_SECONDARY),
                        );
                        ui.label(RichText::new("◐").size(18.0).color(theme::TEXT_SECONDARY));
                    });
                });
            });
    }
    fn show_search_view(&mut self, ui: &mut egui::Ui) {
        let search_status = self.search_service.status();

        theme::section_header(
            ui,
            "Recherche",
            "Index local, filtres rapides et exploration des resultats.",
        );

        render_fixed_panel(
            ui,
            "search-filter-panel",
            236.0,
            theme::panel_card(theme::ORANGE),
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("REQUETE")
                                .monospace()
                                .color(theme::TEXT_SECONDARY),
                        );
                        ui.add_sized(
                            [260.0, 34.0],
                            egui::TextEdit::singleline(&mut self.search_text),
                        );
                    });
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("EXT")
                                .monospace()
                                .color(theme::TEXT_SECONDARY),
                        );
                        ui.add_sized(
                            [120.0, 34.0],
                            egui::TextEdit::singleline(&mut self.extension_filter),
                        );
                    });
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("MIN OCTETS")
                                .monospace()
                                .color(theme::TEXT_SECONDARY),
                        );
                        ui.add_sized(
                            [120.0, 34.0],
                            egui::TextEdit::singleline(&mut self.min_size_filter),
                        );
                    });
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("MAX OCTETS")
                                .monospace()
                                .color(theme::TEXT_SECONDARY),
                        );
                        ui.add_sized(
                            [120.0, 34.0],
                            egui::TextEdit::singleline(&mut self.max_size_filter),
                        );
                    });
                });

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("MODIFIE APRES")
                                .monospace()
                                .color(theme::TEXT_SECONDARY),
                        );
                        ui.add_sized(
                            [150.0, 34.0],
                            egui::TextEdit::singleline(&mut self.modified_after_filter),
                        );
                    });
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("MODIFIE AVANT")
                                .monospace()
                                .color(theme::TEXT_SECONDARY),
                        );
                        ui.add_sized(
                            [150.0, 34.0],
                            egui::TextEdit::singleline(&mut self.modified_before_filter),
                        );
                    });
                });

                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.checkbox(
                        &mut self.include_hidden_results,
                        "Inclure les elements caches",
                    );
                    if ui.button("Reindexer maintenant").clicked() {
                        self.search_service.reindex_now();
                        self.last_search_fingerprint.clear();
                    }
                });

                self.refresh_search_results();
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    theme::status_chip(
                        ui,
                        format!("RESULTATS {}", self.search_results.len()),
                        theme::ORANGE,
                    );
                    theme::status_chip(
                        ui,
                        format!("INDEX {}", search_status.indexed_entries),
                        theme::CYAN,
                    );
                    if !search_status.snapshot_loaded && search_status.indexed_entries > 0 {
                        theme::status_chip(ui, "CHARGEMENT INDEX", theme::ORANGE_SOFT);
                    }
                    if search_status.is_indexing {
                        theme::status_chip(ui, "INDEX EN COURS", theme::ORANGE_SOFT);
                    }
                });
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Format de date : AAAA-MM-JJ")
                        .text_style(egui::TextStyle::Small)
                        .color(theme::TEXT_SECONDARY),
                );
            },
        );

        ui.add_space(10.0);
        let has_active_search_filters = !self.search_text.trim().is_empty()
            || !self.extension_filter.trim().is_empty()
            || !self.min_size_filter.trim().is_empty()
            || !self.max_size_filter.trim().is_empty()
            || !self.modified_after_filter.trim().is_empty()
            || !self.modified_before_filter.trim().is_empty()
            || self.include_hidden_results;
        let result_panel_height = if has_active_search_filters {
            ui.available_height().clamp(220.0, 520.0)
        } else {
            128.0
        };
        render_fixed_panel(
            ui,
            "search-results-panel",
            result_panel_height,
            theme::panel_card(theme::RED_SOFT),
            |ui| {
                theme::section_header(ui, "Resultats", "Fichiers et dossiers indexes");
                if self.search_results.is_empty() && !has_active_search_filters {
                    ui.label(
                    RichText::new(
                        "Saisis une requete ou un filtre pour afficher des resultats sans bloquer l'interface.",
                    )
                    .color(theme::TEXT_SECONDARY),
                );
                    return;
                }
                let results = self.search_results.clone();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("search-results-grid")
                            .num_columns(7)
                            .striped(true)
                            .spacing([12.0, 10.0])
                            .show(ui, |ui| {
                                ui.strong("Nom");
                                ui.strong("Chemin");
                                ui.strong("Type");
                                ui.strong("Taille");
                                ui.strong("Modifie le");
                                ui.strong("Score");
                                ui.strong("Actions");
                                ui.end_row();

                                for result in &results {
                                    ui.label(
                                        RichText::new(&result.entry.name)
                                            .color(theme::TEXT_PRIMARY),
                                    );
                                    ui.label(
                                        RichText::new(&result.entry.path)
                                            .monospace()
                                            .color(theme::TEXT_SECONDARY),
                                    );
                                    ui.label(match result.item_type {
                                        crate::search::SearchItemType::File => "Fichier",
                                        crate::search::SearchItemType::Directory => "Dossier",
                                    });
                                    ui.label(format_bytes(result.entry.size_bytes));
                                    ui.label(format_timestamp(result.entry.modified_at));
                                    ui.label(result.score.to_string());
                                    ui.horizontal(|ui| {
                                        let path = PathBuf::from(&result.entry.path);
                                        if ui.small_button("Ouvrir").clicked()
                                            && let Err(error) = open_path(&path)
                                        {
                                            self.status_message =
                                                Some(format!("Ouverture impossible : {error}"));
                                        }
                                        if ui.small_button("Explorer").clicked()
                                            && let Err(error) = reveal_in_explorer(&path)
                                        {
                                            self.status_message = Some(format!(
                                                "Affichage dans l'Explorateur impossible : {error}"
                                            ));
                                        }
                                        if ui.small_button("Copier").clicked() {
                                            ui.ctx().copy_text(result.entry.path.clone());
                                            self.status_message =
                                                Some("Chemin copie dans le presse-papiers.".into());
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                    });
            },
        );
    }
    fn show_processes_view(&mut self, ui: &mut egui::Ui) {
        render_processes_view(self, ui);
    }

    fn apply_process_action(&mut self, key: &ProcessKey, name: &str, action: ProcessAction) {
        match self.process_manager.perform_action(key, action) {
            Ok(result) => {
                self.status_message = Some(process_action_message(name, key.pid, &result));
            }
            Err(error) => {
                self.status_message = Some(format!(
                    "Action impossible sur {name} ({}) : {error}",
                    key.pid
                ));
            }
        }
    }

    fn sync_process_selection(&mut self, families: &[&ProcessFamily], rows: &[&ProcessRow]) {
        if let Some(selected_key) = self.selected_process.as_ref()
            && let Some(row) = rows.iter().copied().find(|row| &row.key == selected_key)
        {
            self.selected_family = Some(row.family_id.clone());
        }

        let visible_family_ids = families
            .iter()
            .map(|family| family.id.as_str())
            .collect::<HashSet<_>>();
        if self
            .selected_family
            .as_deref()
            .map(|family| !visible_family_ids.contains(family))
            .unwrap_or(true)
        {
            self.selected_family = families.first().map(|family| family.id.clone());
        }

        let relevant_rows = if self.process_tab == ProcessTab::Families {
            if let Some(selected_family) = self.selected_family.as_deref() {
                rows.iter()
                    .copied()
                    .filter(|row| row.family_id == selected_family)
                    .collect::<Vec<_>>()
            } else {
                rows.to_vec()
            }
        } else {
            rows.to_vec()
        };

        if self
            .selected_process
            .as_ref()
            .map(|selected| !relevant_rows.iter().any(|row| &row.key == selected))
            .unwrap_or(true)
        {
            self.selected_process = relevant_rows.first().map(|row| row.key.clone());
        }
    }

    fn show_monitor_view(&mut self, ui: &mut egui::Ui) {
        theme::section_header(
            ui,
            "Surveillance",
            "Dashboard temps reel des ressources systeme et de l'historique d'alertes.",
        );

        let monitor_state = self.monitor_service.snapshot_state();
        if let Some(error) = monitor_state.last_error.as_deref() {
            theme::panel_card(theme::RED).show(ui, |ui| {
                ui.label(RichText::new(error).color(theme::TEXT_PRIMARY));
            });
            ui.add_space(10.0);
        }

        let latest_snapshot = monitor_state.latest.clone();
        if let Some(snapshot) = latest_snapshot.as_ref() {
            let summary_cards = vec![
                SummaryCardSpec {
                    title: "CPU",
                    value: format!("{:.1}%", snapshot.cpu_usage_percent),
                    subtitle: "Charge systeme globale".into(),
                    tone: if snapshot.cpu_usage_percent >= 90.0 {
                        CardTone::Danger
                    } else if snapshot.cpu_usage_percent >= 75.0 {
                        CardTone::Warning
                    } else {
                        CardTone::Accent
                    },
                },
                SummaryCardSpec {
                    title: "Memoire",
                    value: format!(
                        "{:.1}%",
                        percent(snapshot.used_memory_bytes, snapshot.total_memory_bytes)
                    ),
                    subtitle: format!(
                        "{}/{} utilises",
                        format_bytes(snapshot.used_memory_bytes),
                        format_bytes(snapshot.total_memory_bytes)
                    ),
                    tone: CardTone::Info,
                },
                SummaryCardSpec {
                    title: "Net In",
                    value: format!(
                        "{}/s",
                        format_bytes(snapshot.network_received_bytes_per_sec)
                    ),
                    subtitle: "Trafic reseau entrant".into(),
                    tone: CardTone::Default,
                },
                SummaryCardSpec {
                    title: "Net Out",
                    value: format!(
                        "{}/s",
                        format_bytes(snapshot.network_transmitted_bytes_per_sec)
                    ),
                    subtitle: "Trafic reseau sortant".into(),
                    tone: CardTone::Default,
                },
            ];
            render_summary_card_grid(ui, &summary_cards);

            if !monitor_state.history.is_empty() {
                ui.add_space(10.0);
                ui.columns(2, |columns| {
                    theme::panel_card(theme::ORANGE).show(&mut columns[0], |ui| {
                        draw_line_chart(
                            ui,
                            "CPU % (5 min)",
                            monitor_state
                                .history
                                .iter()
                                .map(|sample| sample.cpu_usage_percent)
                                .collect(),
                            100.0,
                            theme::ORANGE,
                        );
                    });
                    theme::panel_card(theme::CYAN).show(&mut columns[1], |ui| {
                        draw_line_chart(
                            ui,
                            "Memoire % (5 min)",
                            monitor_state
                                .history
                                .iter()
                                .map(|sample| {
                                    percent(sample.used_memory_bytes, sample.total_memory_bytes)
                                })
                                .collect(),
                            100.0,
                            theme::CYAN,
                        );
                    });
                });
            }

            ui.add_space(10.0);
            ui.columns(2, |columns| {
                theme::panel_card(theme::CYAN).show(&mut columns[0], |ui| {
                    theme::section_header(ui, "Disques", "Occupation des volumes");
                    for disk in &snapshot.disks {
                        ui.horizontal_wrapped(|ui| {
                            theme::status_chip(
                                ui,
                                format!("{:.1}%", disk.used_percent),
                                if disk.used_percent >= 95.0 {
                                    theme::RED
                                } else {
                                    theme::ORANGE_SOFT
                                },
                            );
                            ui.label(
                                RichText::new(format!(
                                    "{} ({})",
                                    disk.mount_point.display(),
                                    disk.name
                                ))
                                .color(theme::TEXT_PRIMARY),
                            );
                        });
                        ui.label(
                            RichText::new(format!(
                                "{} libres sur {}",
                                format_bytes(disk.available_space_bytes),
                                format_bytes(disk.total_space_bytes)
                            ))
                            .size(12.0)
                            .color(theme::TEXT_SECONDARY),
                        );
                        ui.add_space(6.0);
                    }
                });

                theme::panel_card(theme::ORANGE_SOFT).show(&mut columns[1], |ui| {
                    theme::section_header(ui, "Top CPU", "Processus les plus gourmands en calcul");
                    process_metric_grid(ui, &snapshot.top_cpu_processes, true);
                });
            });

            ui.add_space(10.0);
            theme::panel_card(theme::RED_SOFT).show(ui, |ui| {
                theme::section_header(ui, "Top Memoire", "Processus les plus gourmands en RAM");
                process_metric_grid(ui, &snapshot.top_memory_processes, false);
            });
        } else {
            theme::panel_card(theme::ORANGE_SOFT).show(ui, |ui| {
                ui.label("En attente des premieres donnees de surveillance...");
            });
        }

        ui.add_space(10.0);
        theme::panel_card(theme::RED).show(ui, |ui| {
            theme::section_header(ui, "Historique des alertes", "Derniers evenements");
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if monitor_state.events.is_empty() {
                        ui.label(
                            RichText::new("Aucune alerte enregistree.")
                                .color(theme::TEXT_SECONDARY),
                        );
                    }
                    for event in monitor_state.events.iter().rev() {
                        let color = match event.state {
                            AlertEventState::Active => theme::RED,
                            AlertEventState::Resolved => theme::CYAN,
                        };
                        ui.label(
                            RichText::new(format!(
                                "{} // {} // {}",
                                format_timestamp(Some(event.triggered_at_utc)),
                                event.source_label,
                                event.message
                            ))
                            .color(color),
                        );
                    }
                });
        });
    }
    fn show_temperatures_view(&mut self, ui: &mut egui::Ui) {
        theme::section_header(
            ui,
            "Temperatures",
            "Suivi thermique, alertes et refroidissement automatique Acer Nitro.",
        );

        let monitor_state = self.monitor_service.snapshot_state();
        if let Some(error) = monitor_state.last_error.as_deref() {
            theme::panel_card(theme::RED).show(ui, |ui| {
                ui.label(RichText::new(error).color(theme::TEXT_PRIMARY));
            });
            ui.add_space(10.0);
        }

        let latest_snapshot = monitor_state.latest.clone();
        if let Some(snapshot) = latest_snapshot.as_ref() {
            theme::panel_card(theme::ORANGE).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    theme::status_chip(
                        ui,
                        format!("SOURCE {}", snapshot.thermal.source.label()),
                        theme::CYAN,
                    );
                    theme::status_chip(
                        ui,
                        format!("ETAT {}", snapshot.thermal.state.label()),
                        thermal_state_color(snapshot.thermal.state),
                    );
                    theme::status_chip(
                        ui,
                        if snapshot.thermal.auto_cooling_enabled {
                            "AUTO COOLING ON"
                        } else {
                            "AUTO COOLING OFF"
                        },
                        if snapshot.thermal.auto_cooling_enabled {
                            theme::ORANGE
                        } else {
                            theme::ORANGE_SOFT
                        },
                    );
                    theme::status_chip(
                        ui,
                        if snapshot.thermal.control_available {
                            "CTRL MATERIEL OK"
                        } else {
                            "CTRL MATERIEL OFF"
                        },
                        if snapshot.thermal.control_available {
                            theme::CYAN
                        } else {
                            theme::RED_SOFT
                        },
                    );
                });
                ui.add_space(8.0);
                if !snapshot.thermal.monitoring_enabled {
                    ui.label(
                        RichText::new(
                            "La surveillance thermique est desactivee dans les parametres.",
                        )
                        .color(theme::ORANGE),
                    );
                }
                if let Some(error) = &snapshot.thermal.last_error {
                    ui.label(RichText::new(error).color(theme::RED));
                }
                if let Some(last_action) = &snapshot.thermal.last_action {
                    ui.label(
                        RichText::new(format!(
                            "Derniere action : {} a {}",
                            last_action.action.label(),
                            format_timestamp(Some(last_action.applied_at_utc))
                        ))
                        .color(theme::TEXT_PRIMARY),
                    );
                    ui.label(
                        RichText::new(&last_action.detail)
                            .size(12.0)
                            .color(theme::TEXT_SECONDARY),
                    );
                    if let Some(restored_at) = last_action.restored_at_utc {
                        ui.label(
                            RichText::new(format!(
                                "Etat restaure le {}",
                                format_timestamp(Some(restored_at))
                            ))
                            .size(12.0)
                            .color(theme::CYAN),
                        );
                    }
                }
            });

            ui.add_space(10.0);
            if snapshot.temperatures.is_empty() {
                theme::panel_card(theme::RED_SOFT).show(ui, |ui| {
                    ui.label("Aucun capteur thermique disponible.");
                });
            } else {
                render_temperature_grid(ui, &snapshot.temperatures);
            }
        } else {
            theme::panel_card(theme::ORANGE_SOFT).show(ui, |ui| {
                ui.label("En attente des premieres donnees thermiques...");
            });
        }

        let history = monitor_state.history;
        if !history.is_empty() {
            let cpu_series = temperature_series(&history, TemperatureSensorKind::Cpu);
            let gpu_series = temperature_series(&history, TemperatureSensorKind::Gpu);
            let system_series = temperature_series(&history, TemperatureSensorKind::System);

            if !cpu_series.is_empty() || !gpu_series.is_empty() {
                ui.add_space(10.0);
                ui.columns(2, |columns| {
                    if !cpu_series.is_empty() {
                        theme::panel_card(theme::RED_SOFT).show(&mut columns[0], |ui| {
                            draw_line_chart(ui, "CPU (5 min)", cpu_series, 110.0, theme::RED);
                        });
                    }
                    if !gpu_series.is_empty() {
                        theme::panel_card(theme::CYAN).show(&mut columns[1], |ui| {
                            draw_line_chart(ui, "GPU (5 min)", gpu_series, 110.0, theme::CYAN);
                        });
                    }
                });
            }

            if !system_series.is_empty() {
                ui.add_space(10.0);
                theme::panel_card(theme::ORANGE).show(ui, |ui| {
                    draw_line_chart(ui, "Systeme (5 min)", system_series, 110.0, theme::ORANGE);
                });
            }
        }
    }
    fn show_settings_view(&mut self, ui: &mut egui::Ui) {
        theme::section_header(
            ui,
            "Parametres",
            "Configuration persistante de l'indexation, du monitoring et des alertes.",
        );

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::panel_card(theme::ORANGE).show(ui, |ui| {
                    theme::section_header(ui, "Indexation", "Sources locales et exclusions");
                    ui.label("Racines indexees (une par ligne)");
                    ui.add_sized(
                        [ui.available_width(), 120.0],
                        egui::TextEdit::multiline(&mut self.settings_draft.roots_text)
                            .desired_rows(5)
                            .code_editor(),
                    );
                    ui.add_space(8.0);
                    ui.label("Exclusions (un nom par ligne)");
                    ui.add_sized(
                        [ui.available_width(), 96.0],
                        egui::TextEdit::multiline(&mut self.settings_draft.exclusions_text)
                            .desired_rows(4)
                            .code_editor(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(
                            &mut self.settings_draft.include_hidden,
                            "Inclure les elements caches",
                        );
                        ui.checkbox(
                            &mut self.settings_draft.include_system,
                            "Inclure les elements systeme",
                        );
                        ui.label("Parallellisme d'analyse");
                        ui.add(
                            egui::DragValue::new(&mut self.settings_draft.scan_concurrency)
                                .range(1..=32),
                        );
                    });
                });

                ui.add_space(10.0);
                theme::panel_card(theme::CYAN).show(ui, |ui| {
                    theme::section_header(ui, "Cadence", "Intervalles de rafraichissement");
                    let previous_mode = self.settings_draft.performance_mode;
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Mode performance");
                        for mode in PerformanceMode::all() {
                            ui.radio_value(
                                &mut self.settings_draft.performance_mode,
                                mode,
                                mode.label(),
                            );
                        }
                    });
                    if self.settings_draft.performance_mode != previous_mode {
                        self.settings_draft.apply_performance_profile();
                    }
                    let profile = self.settings_draft.performance_mode.profile();
                    ui.label(
                        RichText::new(format!(
                            "Profil actif : processus {} ms, monitoring {} ms, thermique {} ms, UI {} ms",
                            profile.process_refresh_ms,
                            profile.monitor_refresh_ms,
                            profile.thermal_refresh_ms,
                            profile.ui_idle_ms
                        ))
                        .text_style(egui::TextStyle::Small)
                        .color(theme::TEXT_SECONDARY),
                    );
                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Actualisation processus (ms)");
                        ui.add(
                            egui::DragValue::new(&mut self.settings_draft.process_refresh_ms)
                                .speed(50),
                        );
                        ui.label("Actualisation surveillance (ms)");
                        ui.add(
                            egui::DragValue::new(&mut self.settings_draft.monitor_refresh_ms)
                                .speed(50),
                        );
                    });
                    ui.add_space(10.0);
                    theme::section_header(ui, "Alertes", "Seuils et maintien");
                    for rule in &mut self.settings_draft.alert_rules {
                        ui.horizontal_wrapped(|ui| {
                            ui.checkbox(&mut rule.enabled, &rule.label);
                            ui.label("Seuil %");
                            ui.add(egui::DragValue::new(&mut rule.threshold_percent).speed(1.0));
                            ui.label("Maintien (s)");
                            ui.add(egui::DragValue::new(&mut rule.sustain_seconds).speed(1.0));
                        });
                        ui.add_space(4.0);
                    }
                });

                ui.add_space(10.0);
                theme::panel_card(theme::RED_SOFT).show(ui, |ui| {
                    theme::section_header(
                        ui,
                        "Thermique",
                        "Monitoring, notifications et refroidissement automatique",
                    );
                    ui.horizontal_wrapped(|ui| {
                        ui.checkbox(
                            &mut self.settings_draft.thermal_enabled,
                            "Activer la surveillance thermique",
                        );
                        ui.checkbox(
                            &mut self.settings_draft.thermal_notifications_enabled,
                            "Notifications toast Windows",
                        );
                        ui.checkbox(
                            &mut self.settings_draft.thermal_auto_cooling_enabled,
                            "Refroidissement automatique",
                        );
                    });
                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Mode de seuils");
                        ui.radio_value(
                            &mut self.settings_draft.thermal_threshold_mode,
                            ThermalThresholdMode::Auto,
                            "Auto",
                        );
                        ui.radio_value(
                            &mut self.settings_draft.thermal_threshold_mode,
                            ThermalThresholdMode::Custom,
                            "Personnalise",
                        );
                    });

                    if matches!(
                        self.settings_draft.thermal_threshold_mode,
                        ThermalThresholdMode::Custom
                    ) {
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label("CPU warning C");
                            ui.add(
                                egui::DragValue::new(&mut self.settings_draft.cpu_warning_celsius)
                                    .speed(1.0),
                            );
                            ui.label("CPU critical C");
                            ui.add(
                                egui::DragValue::new(
                                    &mut self.settings_draft.cpu_critical_celsius,
                                )
                                .speed(1.0),
                            );
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.label("GPU warning C");
                            ui.add(
                                egui::DragValue::new(&mut self.settings_draft.gpu_warning_celsius)
                                    .speed(1.0),
                            );
                            ui.label("GPU critical C");
                            ui.add(
                                egui::DragValue::new(
                                    &mut self.settings_draft.gpu_critical_celsius,
                                )
                                .speed(1.0),
                            );
                        });
                        ui.label(
                            RichText::new(
                                "Regle : warning doit rester strictement inferieur a critical.",
                            )
                            .text_style(egui::TextStyle::Small)
                            .color(theme::TEXT_SECONDARY),
                        );
                    }
                });

                ui.add_space(10.0);
                theme::panel_card(theme::ORANGE_SOFT).show(ui, |ui| {
                    theme::section_header(ui, "Actions", "Sauvegarde et restauration");
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Enregistrer les parametres").clicked() {
                            self.save_settings();
                        }
                        if ui.button("Reinitialiser les modifications").clicked() {
                            self.settings_draft = SettingsDraft::from_settings(&self.settings);
                            self.status_message = Some(
                                "Les modifications ont ete reinitialisees aux parametres enregistres."
                                    .into(),
                            );
                        }
                    });
                });
            });
    }
}
impl eframe::App for WindowsHelpApp {
    /// Fond opaque de la fenêtre native.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG_GRAPHITE.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(self.repaint_interval());

        // Fond global simple et sûr.
        theme::paint_app_background(ctx);

        let monitor_state = self.monitor_service.snapshot_state();
        let active_alerts = Self::active_alerts(&monitor_state.events);

        self.show_sidebar(ctx);
        self.show_top_bar(ctx, active_alerts.len());

        self.show_footer(ctx, &monitor_state);

        egui::CentralPanel::default()
            .frame(theme::workspace_frame())
            .show(ctx, |ui| {
                // Important :
                // le décor "hacker" est maintenant peint dans la zone centrale,
                // donc il reste derrière les widgets.
                theme::paint_workspace_background(ui);

                theme::workspace_content_frame().show(ui, |ui| match self.current_view {
                    View::Search => scrollable_workspace(ui, "search-view-scroll", |ui| {
                        self.show_search_view(ui)
                    }),
                    View::Processes => self.show_processes_view(ui),
                    View::Monitor => scrollable_workspace(ui, "monitor-view-scroll", |ui| {
                        self.show_monitor_view(ui)
                    }),
                    View::Temperatures => scrollable_workspace(ui, "thermal-view-scroll", |ui| {
                        self.show_temperatures_view(ui)
                    }),
                    View::Settings => self.show_settings_view(ui),
                });
            });

        if let Some((key, name)) = self.confirm_kill.clone() {
            let pid = key.pid;
            egui::Window::new("Confirmer l'arrêt")
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .collapsible(false)
                .resizable(false)
                .frame(theme::panel_card(theme::RED))
                .show(ctx, |ui| {
                    ui.label(
                        RichText::new(format!("Terminer {name} ({}) ?", key.pid))
                            .color(theme::TEXT_PRIMARY),
                    );
                    ui.add_space(8.0);

                    ui.label(
                        RichText::new(
                            "Cette action est immédiate et le processus sera stoppé côté Windows.",
                        )
                        .size(12.0)
                        .color(theme::TEXT_SECONDARY),
                    );

                    ui.add_space(10.0);

                    ui.horizontal(|ui| {
                        if ui.button("Annuler").clicked() {
                            self.confirm_kill = None;
                        }
                        if ui.button("Terminer le processus").clicked() {
                            match self
                                .process_manager
                                .perform_action(&key, ProcessAction::Kill)
                            {
                                Ok(_result) => {
                                    self.status_message =
                                        Some(format!("Processus {name} ({pid}) terminé."));
                                }
                                Err(error) => {
                                    self.status_message =
                                        Some(format!("Échec de la terminaison : {error}"));
                                }
                            }
                            self.confirm_kill = None;
                        }
                    });
                });
        }
    }
}

fn scrollable_workspace(
    ui: &mut egui::Ui,
    id: &'static str,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::ScrollArea::vertical()
        .id_salt(id)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui);
        });
}

fn render_fixed_panel(
    ui: &mut egui::Ui,
    id: &'static str,
    height: f32,
    frame: egui::Frame,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let width = bounded_available_width(ui);
    if width <= 1.0 || height <= 1.0 {
        return;
    }

    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id)
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect);
    child.set_width(rect.width());
    child.set_max_width(rect.width());

    frame.show(&mut child, |ui| {
        let inner_width = (rect.width() - 24.0).max(0.0);
        ui.set_width(inner_width);
        ui.set_max_width(inner_width);
        ui.set_min_height((rect.height() - 24.0).max(0.0));
        add_contents(ui);
    });
}

fn bounded_available_width(ui: &egui::Ui) -> f32 {
    let available = ui.available_rect_before_wrap();
    let clip = ui.clip_rect();
    let content = ui.ctx().input(|input| input.content_rect());
    let right = available.right().min(clip.right()).min(content.right());
    (right - available.left()).clamp(0.0, WORKSPACE_CONTENT_MAX_WIDTH)
}

fn bounded_available_size(ui: &egui::Ui) -> Vec2 {
    let available = ui.available_rect_before_wrap();
    let clip = ui.clip_rect();
    let content = ui.ctx().input(|input| input.content_rect());
    Vec2::new(
        bounded_available_width(ui),
        (available.bottom().min(clip.bottom()).min(content.bottom()) - available.top()).max(0.0),
    )
}

fn render_processes_view(app: &mut WindowsHelpApp, ui: &mut egui::Ui) {
    let process_state = app.process_manager.state();
    let monitor_state = app.monitor_service.snapshot_state();
    let alerted_pids = process_alerted_pids(&monitor_state.events);
    let active_alerts = WindowsHelpApp::active_alerts(&monitor_state.events);

    if let Some(error) = process_state.last_error.as_deref() {
        theme::panel_card(theme::RED).show(ui, |ui| {
            ui.label(RichText::new(error).color(theme::TEXT_PRIMARY));
        });
        ui.add_space(10.0);
    }

    let process_filter = app.process_filter.clone();
    let hide_windows_processes = app.hide_windows_processes;
    let show_only_suspects = app.show_only_suspects;
    let show_only_closeable = app.show_only_closeable;
    let process_sort = app.process_sort;
    let visible_indices = cached_process_row_indices(
        &mut app.process_rows_cache,
        &process_state,
        ProcessRowsCacheInput {
            filter: &process_filter,
            hide_windows_processes,
            show_only_suspects,
            show_only_closeable,
            sort: process_sort,
            alerted_pids: &alerted_pids,
        },
    )
    .to_vec();
    let visible_rows = visible_indices
        .iter()
        .filter_map(|index| process_state.rows.get(*index))
        .collect::<Vec<_>>();

    app.sync_process_selection(&[], &visible_rows);

    let max_page = max_page_for_len(visible_rows.len(), PROCESS_PAGE_SIZE);
    app.process_page = app.process_page.min(max_page);

    let layout = process_dashboard_layout(bounded_available_size(ui));
    render_process_kpi_row(
        ui,
        layout.width,
        layout.kpi_height,
        &process_state,
        &monitor_state,
        active_alerts.len(),
    );

    ui.add_space(layout.gap);
    let (row_rect, _) = ui.allocate_exact_size(
        Vec2::new(layout.width, layout.main_height),
        egui::Sense::hover(),
    );
    let left_rect = egui::Rect::from_min_size(
        row_rect.min,
        Vec2::new(layout.left_width, layout.main_height),
    );
    let right_rect = egui::Rect::from_min_size(
        egui::pos2(left_rect.right() + layout.gap, row_rect.top()),
        Vec2::new(layout.right_width, layout.main_height),
    );

    render_bounded_process_panel(ui, "process-table-panel", left_rect, |ui| {
        render_process_table_panel(app, ui, &visible_rows, &process_state);
    });
    render_bounded_process_panel(ui, "process-detail-panel", right_rect, |ui| {
        render_process_detail_dashboard(app, ui, &visible_rows);
    });

    ui.add_space(layout.gap);
    let (events_rect, _) = ui.allocate_exact_size(
        Vec2::new(layout.width, layout.events_height),
        egui::Sense::hover(),
    );
    render_bounded_process_panel(ui, "process-events-panel", events_rect, |ui| {
        render_events_panel(ui, &monitor_state.events);
    });
}

fn render_bounded_process_panel(
    ui: &mut egui::Ui,
    id: &'static str,
    rect: egui::Rect,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    if rect.width() <= 1.0 || rect.height() <= 1.0 {
        return;
    }

    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id)
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect);
    child.set_width(rect.width());
    child.set_max_width(rect.width());
    theme::panel_frame().show(&mut child, |ui| {
        let inner_width = (rect.width() - 24.0).max(0.0);
        ui.set_width(inner_width);
        ui.set_max_width(inner_width);
        ui.set_min_height((rect.height() - 24.0).max(0.0));
        add_contents(ui);
    });
}

fn process_dashboard_layout(available: Vec2) -> ProcessDashboardLayout {
    let width = available.x.max(0.0);
    let height = available.y.max(0.0);
    let gap = PROCESS_PANEL_GAP.min(height / 4.0);
    let kpi_height = kpi_grid_layout(width).total_height.min(height);
    let remaining = (height - kpi_height - gap * 2.0).max(0.0);
    let min_main = if height < 620.0 { 260.0_f32 } else { 340.0_f32 }.min(remaining);
    let ideal_events = (height * 0.20).clamp(112.0, 156.0).min(remaining);
    let events_height = if remaining <= min_main {
        (remaining * 0.30).min(remaining)
    } else {
        ideal_events
            .min((remaining - min_main).max(0.0))
            .max(96.0_f32.min(remaining))
            .min(remaining)
    };
    let main_height = (remaining - events_height).max(0.0);
    let (left_width, right_width) = process_panel_widths(width, PROCESS_PANEL_GAP);

    ProcessDashboardLayout {
        width,
        kpi_height,
        main_height,
        events_height,
        gap,
        left_width,
        right_width,
    }
}

fn native_window_sizes_for_work_area(work_area: Vec2) -> NativeWindowSizes {
    let available = Vec2::new(
        (work_area.x - WINDOW_SCREEN_MARGIN.x).max(1.0),
        (work_area.y - WINDOW_SCREEN_MARGIN.y).max(1.0),
    );
    let initial = Vec2::new(
        TARGET_WINDOW_SIZE.x.min(available.x),
        TARGET_WINDOW_SIZE.y.min(available.y),
    );
    let minimum = Vec2::new(
        DESIRED_MIN_WINDOW_SIZE.x.min(initial.x),
        DESIRED_MIN_WINDOW_SIZE.y.min(initial.y),
    );

    NativeWindowSizes { initial, minimum }
}

fn top_bar_layout(width: f32) -> TopBarLayout {
    let width = width.max(0.0);
    let narrow = width < 980.0;
    let compact = width < 1180.0;
    let show_shortcut = width >= 980.0;
    let show_statuses = width >= 1440.0;
    let reserved_left = if narrow {
        214.0
    } else if compact {
        282.0
    } else {
        342.0
    };
    let right_controls_width: f32 = if show_statuses {
        438.0
    } else if show_shortcut {
        292.0
    } else {
        178.0
    };
    let reserved_right = if narrow {
        186.0
    } else if compact {
        right_controls_width.max(246.0)
    } else {
        right_controls_width.max(300.0)
    };
    let available_search = (width - reserved_left - reserved_right).max(96.0);
    let min_search = (if narrow { 150.0_f32 } else { 240.0_f32 }).min(available_search);
    let max_search = if show_statuses { 490.0 } else { 400.0 };
    let search_width = available_search.clamp(min_search, max_search);

    TopBarLayout {
        search_width,
        drag_width: reserved_left.min(width),
        brand_gap: if compact { 18.0 } else { 34.0 },
        view_gap: if compact { 18.0 } else { 38.0 },
        show_view_label: width >= 780.0,
        show_shortcut,
        show_statuses,
    }
}

fn kpi_grid_layout(total_width: f32) -> KpiGridLayout {
    let total_width = total_width.max(0.0);
    let six_column_min_width = PROCESS_KPI_MIN_CARD_WIDTH * 6.0 + PROCESS_KPI_GAP * (6.0 - 1.0);
    let columns = if total_width >= six_column_min_width {
        6
    } else if total_width >= 760.0 {
        3
    } else {
        2
    };
    let rows = PROCESS_KPI_COUNT.div_ceil(columns);
    let total_gap = PROCESS_KPI_GAP * (columns.saturating_sub(1) as f32);
    let card_width = ((total_width - total_gap).max(0.0)) / columns as f32;
    let total_height =
        PROCESS_KPI_HEIGHT * rows as f32 + PROCESS_KPI_GAP * rows.saturating_sub(1) as f32;

    KpiGridLayout {
        columns,
        rows,
        card_width,
        total_height,
    }
}

fn responsive_card_grid_layout(
    total_width: f32,
    item_count: usize,
    min_card_width: f32,
    max_columns: usize,
    gap: f32,
    row_height: f32,
) -> ResponsiveCardGridLayout {
    if item_count == 0 {
        return ResponsiveCardGridLayout {
            columns: 0,
            rows: 0,
            card_width: 0.0,
            total_height: 0.0,
        };
    }

    let total_width = total_width.max(0.0);
    let max_columns = max_columns.max(1).min(item_count);
    let columns_that_fit = (((total_width + gap) / (min_card_width + gap)).floor() as usize)
        .max(1)
        .min(max_columns);
    let rows = item_count.div_ceil(columns_that_fit);
    let total_gap = gap * columns_that_fit.saturating_sub(1) as f32;
    let card_width = ((total_width - total_gap).max(0.0)) / columns_that_fit as f32;
    let total_height = row_height * rows as f32 + gap * rows.saturating_sub(1) as f32;

    ResponsiveCardGridLayout {
        columns: columns_that_fit,
        rows,
        card_width,
        total_height,
    }
}

fn process_panel_widths(width: f32, gap: f32) -> (f32, f32) {
    if width <= gap {
        return (width.max(0.0), 0.0);
    }

    let available = width - gap;
    let max_right = available * 0.44;
    let right_width = (width * 0.32)
        .clamp(300.0, 480.0)
        .min(max_right)
        .max((available * 0.28).min(max_right));
    ((available - right_width).max(0.0), right_width.max(0.0))
}

fn render_process_kpi_row(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    process_state: &ProcessState,
    monitor_state: &crate::monitor::MonitorSnapshotState,
    active_alerts: usize,
) {
    let latest = monitor_state.latest.as_ref();
    let cpu = latest
        .map(|snapshot| snapshot.cpu_usage_percent)
        .unwrap_or(process_state.summary.current_cpu_percent);
    let memory = latest
        .map(|snapshot| percent(snapshot.used_memory_bytes, snapshot.total_memory_bytes))
        .unwrap_or(process_state.summary.current_memory_percent);
    let network_in = latest
        .map(|snapshot| snapshot.network_received_bytes_per_sec)
        .unwrap_or_default();
    let network_out = latest
        .map(|snapshot| snapshot.network_transmitted_bytes_per_sec)
        .unwrap_or_default();
    let disk = latest.and_then(|snapshot| snapshot.disks.first());

    let specs = vec![
        KpiCardSpec {
            icon: AppIcon::Cpu,
            title: "CPU",
            value: format!("{cpu:.0}%"),
            subtitle: "Charge système".into(),
            tone: CardTone::Info,
            samples: monitor_state
                .history
                .iter()
                .map(|sample| sample.cpu_usage_percent)
                .collect(),
        },
        KpiCardSpec {
            icon: AppIcon::Memory,
            title: "RAM",
            value: format!("{memory:.0}%"),
            subtitle: latest
                .map(|snapshot| {
                    format!(
                        "{} / {}",
                        format_bytes(snapshot.used_memory_bytes),
                        format_bytes(snapshot.total_memory_bytes)
                    )
                })
                .unwrap_or_else(|| "En attente".into()),
            tone: CardTone::Info,
            samples: monitor_state
                .history
                .iter()
                .map(|sample| percent(sample.used_memory_bytes, sample.total_memory_bytes))
                .collect(),
        },
        KpiCardSpec {
            icon: AppIcon::Network,
            title: "Réseau",
            value: format!("{}/s", format_bytes(network_in.saturating_add(network_out))),
            subtitle: format!(
                "↓ {}/s  ↑ {}/s",
                format_bytes(network_in),
                format_bytes(network_out)
            ),
            tone: CardTone::Accent,
            samples: monitor_state
                .history
                .iter()
                .map(|sample| {
                    (sample
                        .network_received_bytes_per_sec
                        .saturating_add(sample.network_transmitted_bytes_per_sec)
                        / 1024) as f32
                })
                .collect(),
        },
        KpiCardSpec {
            icon: AppIcon::Storage,
            title: "Stockage",
            value: disk
                .map(|disk| format!("{:.0}%", disk.used_percent))
                .unwrap_or_else(|| "-".into()),
            subtitle: disk
                .map(|disk| {
                    format!(
                        "{} / {}",
                        format_bytes(
                            disk.total_space_bytes
                                .saturating_sub(disk.available_space_bytes)
                        ),
                        format_bytes(disk.total_space_bytes)
                    )
                })
                .unwrap_or_else(|| "Aucun volume".into()),
            tone: CardTone::Info,
            samples: Vec::new(),
        },
        KpiCardSpec {
            icon: AppIcon::Processes,
            title: "Processus",
            value: process_state.summary.total_processes.to_string(),
            subtitle: "En cours d'exécution".into(),
            tone: CardTone::Default,
            samples: Vec::new(),
        },
        KpiCardSpec {
            icon: AppIcon::Alerts,
            title: "Alertes",
            value: active_alerts.to_string(),
            subtitle: if active_alerts == 0 {
                "Aucune alerte active".into()
            } else {
                "Action requise".into()
            },
            tone: if active_alerts == 0 {
                CardTone::Accent
            } else {
                CardTone::Danger
            },
            samples: Vec::new(),
        },
    ];

    let row_width = width.max(0.0);
    let row_height = height.max(0.0);
    let grid = kpi_grid_layout(row_width);
    let card_width = grid.card_width;
    let card_height = if grid.rows > 1 {
        ((row_height - PROCESS_KPI_GAP * (grid.rows - 1) as f32).max(0.0)) / grid.rows as f32
    } else {
        row_height
    };
    let (row_rect, _) =
        ui.allocate_exact_size(Vec2::new(row_width, row_height), egui::Sense::hover());

    for (index, spec) in specs.into_iter().enumerate() {
        let column = index % grid.columns;
        let row = index / grid.columns;
        let x = row_rect.left() + column as f32 * (card_width + PROCESS_KPI_GAP);
        let y = row_rect.top() + row as f32 * (card_height + PROCESS_KPI_GAP);
        let rect = egui::Rect::from_min_size(egui::pos2(x, y), Vec2::new(card_width, card_height));
        kpi_card(ui, rect, spec);
    }
}

fn render_process_table_panel(
    app: &mut WindowsHelpApp,
    ui: &mut egui::Ui,
    visible_rows: &[&ProcessRow],
    process_state: &ProcessState,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "Processus ({})",
                process_state.summary.total_processes
            ))
            .strong()
            .size(16.0)
            .color(theme::TEXT_PRIMARY),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            egui::ComboBox::from_id_salt("process-sort")
                .selected_text(app.process_sort.label())
                .width(128.0)
                .show_ui(ui, |ui| {
                    for sort in ProcessSort::all() {
                        if ui
                            .selectable_value(&mut app.process_sort, sort, sort.label())
                            .clicked()
                        {
                            app.process_page = 0;
                        }
                    }
                });
            if ui
                .checkbox(&mut app.show_only_closeable, "Fermables")
                .changed()
            {
                app.process_page = 0;
            }
            if ui
                .checkbox(&mut app.show_only_suspects, "Suspects")
                .changed()
            {
                app.process_page = 0;
            }
            if ui
                .checkbox(&mut app.hide_windows_processes, "Masquer Windows")
                .changed()
            {
                app.process_page = 0;
            }
        });
    });
    ui.add_space(8.0);

    let filter_response = ui.add_sized(
        [ui.available_width(), 34.0],
        egui::TextEdit::singleline(&mut app.process_filter).hint_text("Filtrer..."),
    );
    if filter_response.changed() {
        app.process_page = 0;
    }
    ui.add_space(8.0);

    render_process_table_header(ui);
    let (start, end, max_page) =
        page_bounds(visible_rows.len(), app.process_page, PROCESS_PAGE_SIZE);
    let rows_height = (ui.available_height() - 38.0).max(48.0);
    egui::ScrollArea::vertical()
        .id_salt("process-table-rows-scroll")
        .auto_shrink([false, false])
        .max_height(rows_height)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for row in visible_rows[start..end].iter().copied() {
                render_process_table_row(app, ui, row);
            }

            if visible_rows.is_empty() {
                ui.add_space(24.0);
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new("Aucun processus ne correspond aux filtres.")
                            .color(theme::TEXT_SECONDARY),
                    );
                });
            }
        });

    ui.add_space(6.0);
    render_process_pagination(ui, app, visible_rows.len(), start, end, max_page);
}

fn render_process_pagination(
    ui: &mut egui::Ui,
    app: &mut WindowsHelpApp,
    total_rows: usize,
    start: usize,
    end: usize,
    max_page: usize,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "Affichage {}-{} sur {}",
                if total_rows == 0 { 0 } else { start + 1 },
                end,
                total_rows
            ))
            .size(13.0)
            .color(theme::TEXT_SECONDARY),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(app.process_page < max_page, egui::Button::new("›"))
                .clicked()
            {
                app.process_page += 1;
            }
            let last_label = (max_page + 1).to_string();
            if ui
                .selectable_label(app.process_page == max_page, last_label)
                .clicked()
            {
                app.process_page = max_page;
            }
            ui.label(RichText::new("...").color(theme::TEXT_MUTED));
            for page in 0..=max_page.min(2) {
                if ui
                    .selectable_label(app.process_page == page, (page + 1).to_string())
                    .clicked()
                {
                    app.process_page = page;
                }
            }
            if ui
                .add_enabled(app.process_page > 0, egui::Button::new("‹"))
                .clicked()
            {
                app.process_page = app.process_page.saturating_sub(1);
            }
        });
    });
}

fn render_process_table_header(ui: &mut egui::Ui) {
    let widths = process_table_widths(ui.available_width());
    theme::table_header_frame().show(ui, |ui| {
        ui.horizontal(|ui| {
            table_header_cell(ui, widths[0], "Nom");
            table_header_cell(ui, widths[1], "PID");
            table_header_cell(ui, widths[2], "CPU ↓");
            table_header_cell(ui, widths[3], "RAM");
            table_header_cell(ui, widths[4], "Impact");
            table_header_cell(ui, widths[5], "Act.");
        });
    });
}

fn render_process_table_row(app: &mut WindowsHelpApp, ui: &mut egui::Ui, row: &ProcessRow) {
    let selected = app
        .selected_process
        .as_ref()
        .map(|key| key == &row.key)
        .unwrap_or(false);

    let response = theme::table_row_frame(selected)
        .show(ui, |ui| {
            let widths = process_table_widths(ui.available_width());
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(widths[0], 32.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        draw_process_icon(ui, process_icon_for_name(&row.name), 22.0);
                        label_truncated(
                            ui,
                            (widths[0] - 30.0).max(42.0),
                            &row.name,
                            14.0,
                            theme::TEXT_PRIMARY,
                            false,
                        );
                    },
                );
                table_value_cell(ui, widths[1], row.key.pid.to_string());
                table_value_cell(ui, widths[2], format!("{:.1}%", row.cpu_now));
                table_value_cell(ui, widths[3], format_bytes(row.memory_bytes));
                ui.allocate_ui_with_layout(
                    Vec2::new(widths[4], 32.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.colored_label(impact_color(row.insight.impact_score), "●");
                        ui.label(
                            RichText::new(impact_label(row.insight.impact_score))
                                .size(13.0)
                                .color(theme::TEXT_PRIMARY),
                        );
                    },
                );
                ui.allocate_ui_with_layout(
                    Vec2::new(widths[5], 32.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        icon_badge(ui, AppIcon::Shield, "Niveau de securite calcule");
                        icon_badge(ui, AppIcon::More, "Actions disponibles dans le panneau");
                    },
                );
            });
        })
        .response;

    if response.clicked() {
        app.selected_family = Some(row.family_id.clone());
        app.selected_process = Some(row.key.clone());
    }
    ui.add_space(2.0);
}

fn render_process_detail_dashboard(
    app: &mut WindowsHelpApp,
    ui: &mut egui::Ui,
    visible_rows: &[&ProcessRow],
) {
    let selected_row = visible_rows
        .iter()
        .copied()
        .find(|row| {
            app.selected_process
                .as_ref()
                .map(|selected| selected == &row.key)
                .unwrap_or(false)
        })
        .or_else(|| visible_rows.first().copied());

    let Some(row) = selected_row else {
        ui.label(
            RichText::new("Selectionnez une famille ou une instance pour voir le detail.")
                .color(theme::TEXT_SECONDARY),
        );
        return;
    };

    let family_members = visible_rows
        .iter()
        .copied()
        .filter(|candidate| candidate.family_id == row.family_id)
        .collect::<Vec<_>>();

    let header_width = ui.available_width();
    ui.horizontal(|ui| {
        draw_process_icon(ui, process_icon_for_name(&row.name), 44.0);
        ui.allocate_ui_with_layout(
            Vec2::new((header_width - 156.0).max(90.0), 48.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                label_truncated(
                    ui,
                    ui.available_width(),
                    &row.name,
                    18.0,
                    theme::TEXT_PRIMARY,
                    true,
                );
                label_truncated(
                    ui,
                    ui.available_width(),
                    process_exe_label(row),
                    13.0,
                    theme::TEXT_SECONDARY,
                    false,
                );
            },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            ui.add_sized(
                [92.0, 48.0],
                egui::Label::new(
                    RichText::new(format!(
                        "PID  {}\nUtilisateur\n{}",
                        row.key.pid,
                        std::env::var("USERNAME").unwrap_or_else(|_| "-".into())
                    ))
                    .size(13.0)
                    .color(theme::TEXT_SECONDARY),
                )
                .truncate(),
            );
        });
    });
    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        for tab in ProcessDetailTab::all() {
            if ui
                .selectable_label(app.process_detail_tab == tab, tab.label())
                .clicked()
            {
                app.process_detail_tab = tab;
            }
        }
    });

    ui.add_space(12.0);
    let detail_height = ui.available_height().max(48.0);
    egui::ScrollArea::vertical()
        .id_salt("process-detail-scroll")
        .auto_shrink([false, false])
        .max_height(detail_height)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            render_detail_cards(
                ui,
                |ui| detail_status_card(app, ui, row),
                |ui| detail_impact_card(ui, row),
            );

            ui.add_space(10.0);
            render_detail_cards(
                ui,
                |ui| detail_resources_card(ui, row),
                |ui| detail_actions_card(app, ui, row),
            );

            if !family_members.is_empty() && app.process_detail_tab == ProcessDetailTab::Details {
                ui.add_space(12.0);
                theme::section_header(ui, "Autres instances de la famille", "");
                for member in family_members.iter().take(8) {
                    let selected = app
                        .selected_process
                        .as_ref()
                        .map(|key| key == &member.key)
                        .unwrap_or(false);
                    if ui
                        .selectable_label(
                            selected,
                            format!(
                                "{}   PID {}   impact {}   {}",
                                member.name,
                                member.key.pid,
                                member.insight.impact_score,
                                format_bytes(member.memory_bytes)
                            ),
                        )
                        .clicked()
                    {
                        app.selected_process = Some(member.key.clone());
                        app.selected_family = Some(member.family_id.clone());
                    }
                }
            }
        });
}

fn render_detail_cards(
    ui: &mut egui::Ui,
    add_left: impl FnOnce(&mut egui::Ui),
    add_right: impl FnOnce(&mut egui::Ui),
) {
    let width = ui.available_width();
    if width < 680.0 {
        ui.set_width(width);
        ui.set_max_width(width);
        add_left(ui);
        ui.add_space(10.0);
        add_right(ui);
    } else {
        ui.columns(2, |columns| {
            let column_width = columns[0].available_width();
            columns[0].set_width(column_width);
            columns[0].set_max_width(column_width);
            columns[1].set_width(column_width);
            columns[1].set_max_width(column_width);
            add_left(&mut columns[0]);
            add_right(&mut columns[1]);
        });
    }
}

fn detail_status_card(app: &mut WindowsHelpApp, ui: &mut egui::Ui, row: &ProcessRow) {
    theme::banner_frame(safety_color(row.insight.safety)).show(ui, |ui| {
        ui.label(RichText::new("Statut").strong().color(theme::TEXT_PRIMARY));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            draw_icon(ui, AppIcon::Shield, 34.0, safety_color(row.insight.safety));
            theme::status_chip(
                ui,
                safety_short_label(row.insight.safety),
                safety_color(row.insight.safety),
            );
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!("Priorité {}", row.priority.label()))
                .color(theme::TEXT_SECONDARY),
        );
        ui.label(RichText::new(format!("État {}", row.status)).color(theme::TEXT_SECONDARY));
        if let Some(path) = row.path.as_ref() {
            ui.add_space(8.0);
            if ui.button("Ouvrir l'emplacement").clicked()
                && let Err(error) = reveal_in_explorer(path)
            {
                app.status_message =
                    Some(format!("Affichage dans l'Explorateur impossible : {error}"));
            }
        }
    });
}

fn detail_impact_card(ui: &mut egui::Ui, row: &ProcessRow) {
    theme::banner_frame(process_tone_color(row.insight.impact_score)).show(ui, |ui| {
        ui.label(
            RichText::new("Impact système")
                .strong()
                .color(theme::TEXT_PRIMARY),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            impact_gauge(ui, row.insight.impact_score, 82.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(impact_label(row.insight.impact_score))
                        .size(20.0)
                        .strong()
                        .color(process_tone_color(row.insight.impact_score)),
                );
                ui.label(
                    RichText::new(format!("Score {}/100", row.insight.impact_score))
                        .size(13.0)
                        .color(theme::TEXT_SECONDARY),
                );
            });
        });
        ui.separator();
        detail_meta_line(
            ui,
            "Démarré le",
            format_timestamp(row.key.started_at.map(|value| value as i64)),
        );
        detail_meta_line(ui, "Temps d'exécution", format_duration(row.run_time_secs));
    });
}

fn detail_meta_line(ui: &mut egui::Ui, label: &str, value: String) {
    ui.horizontal(|ui| {
        label_truncated(ui, 128.0, label, 12.0, theme::TEXT_SECONDARY, false);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            label_truncated(
                ui,
                ui.available_width(),
                value,
                13.0,
                theme::TEXT_PRIMARY,
                false,
            );
        });
    });
}

fn detail_resources_card(ui: &mut egui::Ui, row: &ProcessRow) {
    theme::banner_frame(theme::BORDER).show(ui, |ui| {
        ui.label(
            RichText::new("Ressources")
                .strong()
                .color(theme::TEXT_PRIMARY),
        );
        ui.add_space(8.0);
        resource_line(
            ui,
            "CPU",
            format!("{:.1}%", row.cpu_now),
            row.cpu_now / 100.0,
        );
        resource_line(
            ui,
            "RAM",
            format_bytes(row.memory_bytes),
            row.insight.memory_percent / 100.0,
        );
        resource_line(
            ui,
            "Disque (E/S)",
            format!("{}/s", format_bytes(row.insight.disk_io_bytes_per_sec)),
            (row.insight.disk_io_bytes_per_sec as f32 / 8_000_000.0).clamp(0.0, 1.0),
        );
        resource_line(
            ui,
            "Threads",
            row.threads.to_string(),
            (row.threads as f32 / 128.0).clamp(0.0, 1.0),
        );
    });
}

fn detail_actions_card(app: &mut WindowsHelpApp, ui: &mut egui::Ui, row: &ProcessRow) {
    theme::banner_frame(theme::BORDER).show(ui, |ui| {
        ui.label(RichText::new("Actions").strong().color(theme::TEXT_PRIMARY));
        ui.add_space(8.0);
        let protected = is_protected_process(row);
        if protected {
            ui.label(
                RichText::new("Actions système bloquées pour ce processus protégé.")
                    .size(12.0)
                    .color(theme::WARNING),
            );
            ui.add_space(6.0);
        }
        ui.add_enabled_ui(!protected, |ui| {
            action_button(ui, theme::RED, "Fin de tâche", || {
                app.confirm_kill = Some((row.key.clone(), row.name.clone()));
            });
            action_button(ui, theme::TEXT_SECONDARY, "Réduire la priorité", || {
                app.apply_process_action(
                    &row.key,
                    &row.name,
                    ProcessAction::SetPriority(PriorityClass::BelowNormal),
                );
            });
            action_button(ui, theme::ORANGE_SOFT, "Définir priorité faible", || {
                app.apply_process_action(
                    &row.key,
                    &row.name,
                    ProcessAction::SetPriority(PriorityClass::Idle),
                );
            });
        });
        let ctx = ui.ctx().clone();
        action_button(ui, theme::TEXT_PRIMARY, "Copier le chemin", || {
            if let Some(path) = row.path.as_ref() {
                ctx.copy_text(path.display().to_string());
                app.status_message = Some("Chemin copié dans le presse-papiers.".into());
            }
        });
    });
}

fn render_events_panel(ui: &mut egui::Ui, events: &[AlertEvent]) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Alertes et événements")
                .strong()
                .size(15.5)
                .color(theme::TEXT_PRIMARY),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{} evenements", events.len()))
                    .size(13.0)
                    .color(theme::TEXT_SECONDARY),
            );
        });
    });
    ui.add_space(8.0);

    if events.is_empty() {
        ui.label(
            RichText::new("Aucun événement enregistré.")
                .size(13.0)
                .color(theme::TEXT_SECONDARY),
        );
        return;
    }

    let list_height = ui.available_height().max(40.0);
    egui::ScrollArea::vertical()
        .id_salt("events-panel-scroll")
        .auto_shrink([false, false])
        .max_height(list_height)
        .show(ui, |ui| {
            let widths = event_column_widths(ui.available_width());
            egui::Grid::new("events-grid")
                .num_columns(5)
                .striped(true)
                .spacing([14.0, 6.0])
                .show(ui, |ui| {
                    for event in events.iter().rev() {
                        let color = match event.state {
                            AlertEventState::Active => theme::WARNING,
                            AlertEventState::Resolved => theme::ORANGE_SOFT,
                        };
                        ui.colored_label(color, "●");
                        label_truncated(
                            ui,
                            widths[1],
                            format_timestamp(Some(event.triggered_at_utc)),
                            13.0,
                            theme::TEXT_PRIMARY,
                            false,
                        );
                        label_truncated(
                            ui,
                            widths[2],
                            match event.state {
                                AlertEventState::Active => "Avertissement",
                                AlertEventState::Resolved => "Info",
                            },
                            13.0,
                            theme::TEXT_PRIMARY,
                            false,
                        );
                        label_truncated(
                            ui,
                            widths[3],
                            &event.message,
                            13.0,
                            theme::TEXT_PRIMARY,
                            false,
                        );
                        label_truncated(
                            ui,
                            widths[4],
                            &event.source_label,
                            13.0,
                            theme::TEXT_PRIMARY,
                            false,
                        );
                        ui.end_row();
                    }
                });
        });
}

fn event_column_widths(total: f32) -> [f32; 5] {
    let total = total.max(0.0);
    let dot = 18.0;
    let time = 160.0_f32.min(total * 0.20);
    let level = 120.0_f32.min(total * 0.16);
    let source = 160.0_f32.min(total * 0.18);
    let spacing = 56.0;
    let message = (total - dot - time - level - source - spacing).max(120.0);
    [dot, time, level, message, source]
}

fn process_row_matches_filters(
    row: &ProcessRow,
    filter: &str,
    hide_windows_processes: bool,
    show_only_suspects: bool,
    show_only_closeable: bool,
    alerted_pids: &HashSet<u32>,
) -> bool {
    if hide_windows_processes
        && matches!(
            row.insight.safety,
            ProcessSafety::CriticalSystem | ProcessSafety::WindowsComponent
        )
    {
        return false;
    }

    if show_only_suspects && !is_suspect_row(row, alerted_pids) {
        return false;
    }

    if show_only_closeable && row.insight.suggested_action != SuggestedAction::CloseGracefully {
        return false;
    }

    let trimmed_filter = filter.trim().to_ascii_lowercase();
    if trimmed_filter.is_empty() {
        return true;
    }

    row.name.to_ascii_lowercase().contains(&trimmed_filter)
        || row.family_id.to_ascii_lowercase().contains(&trimmed_filter)
        || row.key.pid.to_string().contains(&trimmed_filter)
        || row
            .path
            .as_ref()
            .map(|path| {
                path.display()
                    .to_string()
                    .to_ascii_lowercase()
                    .contains(&trimmed_filter)
            })
            .unwrap_or(false)
}

fn is_suspect_row(row: &ProcessRow, alerted_pids: &HashSet<u32>) -> bool {
    row.insight.impact_score >= 40
        || row.insight.cpu_avg_10s >= 10.0
        || row.insight.memory_percent >= 5.0
        || alerted_pids.contains(&row.key.pid)
}

fn cached_process_row_indices<'a>(
    cache: &'a mut ProcessRowsCache,
    state: &ProcessState,
    input: ProcessRowsCacheInput<'_>,
) -> &'a [usize] {
    let key = ProcessRowsCacheKey {
        revision: state.revision,
        filter: input.filter.trim().to_ascii_lowercase(),
        hide_windows_processes: input.hide_windows_processes,
        show_only_suspects: input.show_only_suspects,
        show_only_closeable: input.show_only_closeable,
        sort: input.sort,
        alerted_pids: alerted_pid_fingerprint(input.alerted_pids),
    };

    if cache.key.as_ref() != Some(&key) {
        cache.indices = state
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                process_row_matches_filters(
                    row,
                    &key.filter,
                    input.hide_windows_processes,
                    input.show_only_suspects,
                    input.show_only_closeable,
                    input.alerted_pids,
                )
                .then_some(index)
            })
            .collect();
        cache.indices.sort_by(|left, right| {
            compare_process_rows(&state.rows[*left], &state.rows[*right], input.sort)
        });
        cache.key = Some(key);
        note_process_cache_rebuild(cache);
    }

    &cache.indices
}

#[cfg(test)]
fn note_process_cache_rebuild(cache: &mut ProcessRowsCache) {
    cache.rebuilds = cache.rebuilds.wrapping_add(1);
}

#[cfg(not(test))]
fn note_process_cache_rebuild(_cache: &mut ProcessRowsCache) {}

fn alerted_pid_fingerprint(alerted_pids: &HashSet<u32>) -> Vec<u32> {
    let mut values = alerted_pids.iter().copied().collect::<Vec<_>>();
    values.sort_unstable();
    values
}

fn compare_process_rows(
    left: &ProcessRow,
    right: &ProcessRow,
    sort: ProcessSort,
) -> std::cmp::Ordering {
    let ordering = match sort {
        ProcessSort::Impact => right
            .insight
            .impact_score
            .cmp(&left.insight.impact_score)
            .then_with(|| {
                right
                    .insight
                    .cpu_avg_10s
                    .partial_cmp(&left.insight.cpu_avg_10s)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
        ProcessSort::CpuNow => right
            .cpu_now
            .partial_cmp(&left.cpu_now)
            .unwrap_or(std::cmp::Ordering::Equal),
        ProcessSort::CpuAverage => right
            .insight
            .cpu_avg_10s
            .partial_cmp(&left.insight.cpu_avg_10s)
            .unwrap_or(std::cmp::Ordering::Equal),
        ProcessSort::Memory => right.memory_bytes.cmp(&left.memory_bytes),
        ProcessSort::Name => left.name.cmp(&right.name),
    };

    ordering
        .then_with(|| left.name.cmp(&right.name))
        .then_with(|| left.key.pid.cmp(&right.key.pid))
}

fn process_alerted_pids(events: &[AlertEvent]) -> HashSet<u32> {
    events
        .iter()
        .filter(|event| matches!(event.state, AlertEventState::Active))
        .filter_map(|event| event.source_pid)
        .collect()
}

fn process_action_message(name: &str, pid: u32, result: &ProcessActionResult) -> String {
    match result {
        ProcessActionResult::CloseRequested => {
            format!("Fermeture demandee a {name} ({pid}). Verification en cours.")
        }
        ProcessActionResult::ClosedGracefully => {
            format!("Processus {name} ({pid}) ferme proprement.")
        }
        ProcessActionResult::ForceTerminated => {
            format!("Processus {name} ({pid}) termine de force.")
        }
        ProcessActionResult::PriorityUpdated(priority) => {
            format!("Priorite {} appliquee a {name} ({pid}).", priority.label())
        }
    }
}

fn impact_color(impact_score: u8) -> Color32 {
    if impact_score >= 75 {
        theme::RED
    } else if impact_score >= 50 {
        theme::WARNING
    } else if impact_score >= 25 {
        theme::ORANGE
    } else {
        theme::CYAN
    }
}

fn safety_color(safety: ProcessSafety) -> Color32 {
    match safety {
        ProcessSafety::CriticalSystem => theme::RED,
        ProcessSafety::WindowsComponent => theme::ORANGE,
        ProcessSafety::Caution => theme::WARNING,
        ProcessSafety::LikelyClosable => theme::CYAN,
        ProcessSafety::Unknown => theme::TEXT_SECONDARY,
    }
}

fn format_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{} h", seconds / 3600)
    } else if seconds >= 60 {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

pub fn run() -> anyhow::Result<()> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?,
    );
    let settings = load_or_create_settings()?;
    let app = WindowsHelpApp::build(Arc::clone(&runtime), settings)?;
    let mut app_slot = Some(app);
    let work_area = primary_work_area();
    let work_area_size = work_area
        .map(|area| Vec2::new(area.width, area.height))
        .unwrap_or(TARGET_WINDOW_SIZE);
    let window_sizes = native_window_sizes_for_work_area(work_area_size);
    let window_position = work_area.map(|area| {
        [
            area.left + (area.width - window_sizes.initial.x).max(0.0) / 2.0,
            area.top + (area.height - window_sizes.initial.y).max(0.0) / 2.0,
        ]
    });

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([window_sizes.initial.x, window_sizes.initial.y])
        .with_min_inner_size([window_sizes.minimum.x, window_sizes.minimum.y])
        .with_decorations(false)
        .with_transparent(false);
    if let Some(position) = window_position {
        viewport = viewport.with_position(position);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let run_result = eframe::run_native(
        "WindowsHELP",
        native_options,
        Box::new(move |cc| {
            theme::apply_hacker_theme(&cc.egui_ctx);
            Ok(Box::new(app_slot.take().expect(
                "WindowsHELP application should only be created once",
            )))
        }),
    );

    if let Ok(runtime) = Arc::try_unwrap(runtime) {
        runtime.shutdown_background();
    }

    run_result.map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn nav_button(ui: &mut egui::Ui, icon: AppIcon, label: &str, active: bool) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 92.0), egui::Sense::click());
    let fill = if active {
        theme::BG_SURFACE
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if active {
        Stroke::new(1.0, theme::BORDER)
    } else {
        Stroke::NONE
    };
    ui.painter().rect_filled(rect.shrink(7.0), 6.0, fill);
    ui.painter()
        .rect_stroke(rect.shrink(7.0), 6.0, stroke, egui::StrokeKind::Outside);

    if active {
        let indicator = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + 8.0),
            egui::vec2(4.0, rect.height() - 16.0),
        );
        ui.painter()
            .rect_filled(indicator, CornerRadius::same(2), theme::ORANGE);
    }

    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + 34.0),
        Vec2::splat(34.0),
    );
    paint_icon(
        ui,
        icon_rect,
        icon,
        if active {
            theme::ORANGE
        } else {
            theme::TEXT_SECONDARY
        },
    );

    ui.painter().text(
        egui::pos2(rect.center().x, rect.top() + 64.0),
        egui::Align2::CENTER_TOP,
        label,
        egui::FontId::new(13.0, egui::FontFamily::Proportional),
        if active {
            theme::ORANGE
        } else {
            theme::TEXT_SECONDARY
        },
    );

    response.clicked()
}

fn view_icon(view: View) -> AppIcon {
    match view {
        View::Search => AppIcon::Search,
        View::Processes => AppIcon::Processes,
        View::Monitor => AppIcon::Monitor,
        View::Temperatures => AppIcon::Thermal,
        View::Settings => AppIcon::Settings,
    }
}

fn draw_icon(ui: &mut egui::Ui, icon: AppIcon, size: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    paint_icon(ui, rect, icon, color);
}

fn icon_badge(ui: &mut egui::Ui, icon: AppIcon, tooltip: &str) {
    ui.add_sized(
        [32.0, 30.0],
        egui::Label::new(
            RichText::new(icon_text(icon))
                .size(15.0)
                .color(theme::TEXT_SECONDARY),
        ),
    )
    .on_hover_text(tooltip);
}

fn window_button(ui: &mut egui::Ui, label: &str, color: Color32, action: impl FnOnce()) {
    if ui
        .add_sized(
            [42.0, 42.0],
            egui::Button::new(RichText::new(label).size(20.0).color(color))
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::NONE),
        )
        .clicked()
    {
        action();
    }
}

fn top_status(ui: &mut egui::Ui, label: &str, enabled: bool, color: Color32) {
    theme::banner_frame(theme::BORDER).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(if enabled { color } else { theme::TEXT_MUTED }, "●");
            ui.label(RichText::new(label).size(13.0).color(theme::TEXT_SECONDARY));
        });
    });
}

fn top_icon_count(ui: &mut egui::Ui, icon: AppIcon, count: usize) {
    ui.horizontal(|ui| {
        draw_icon(ui, icon, 24.0, theme::TEXT_PRIMARY);
        ui.label(
            RichText::new(count.to_string())
                .strong()
                .color(theme::TEXT_PRIMARY),
        );
    });
}

fn footer_metric(ui: &mut egui::Ui, label: &str, value: impl Into<String>) {
    ui.label(
        RichText::new(format!("{label} {}", value.into()))
            .size(13.0)
            .color(theme::TEXT_SECONDARY),
    );
    ui.add_space(18.0);
}

fn kpi_card(ui: &mut egui::Ui, rect: egui::Rect, spec: KpiCardSpec) {
    let accent = theme::tone_color(spec.tone);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("kpi-card", spec.title))
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect);
    child.set_width(rect.width());
    child.set_max_width(rect.width());

    theme::metric_card_variant(spec.tone).show(&mut child, |ui| {
        let inner_width = (rect.width() - 24.0).max(0.0);
        ui.set_width(inner_width);
        ui.set_max_width(inner_width);
        ui.set_min_height((rect.height() - 24.0).max(0.0));
        let icon_width = 34.0;
        let row_gap = 8.0;
        let remaining = (inner_width - icon_width - row_gap * 2.0).max(0.0);
        let value_width = (remaining * 0.48).clamp(62.0, 140.0).min(remaining);
        let text_width = (remaining - value_width).max(42.0);
        ui.horizontal(|ui| {
            draw_icon(ui, spec.icon, icon_width, theme::TEXT_PRIMARY);
            ui.allocate_ui_with_layout(
                Vec2::new(text_width, 42.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    label_truncated(ui, text_width, spec.title, 14.0, theme::TEXT_PRIMARY, true);
                    label_truncated(
                        ui,
                        text_width,
                        spec.subtitle,
                        12.0,
                        theme::TEXT_SECONDARY,
                        false,
                    );
                },
            );
            ui.allocate_ui_with_layout(
                Vec2::new(value_width, 34.0),
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    ui.add_sized(
                        [value_width, 30.0],
                        egui::Label::new(
                            RichText::new(spec.value)
                                .text_style(egui::TextStyle::Name("Metric".into()))
                                .strong()
                                .color(theme::TEXT_PRIMARY),
                        )
                        .truncate(),
                    );
                },
            );
        });
        ui.add_space(4.0);
        draw_sparkline(ui, spec.samples, accent, 24.0);
    });
}

fn draw_sparkline(ui: &mut egui::Ui, values: Vec<f32>, color: Color32, height: f32) {
    let desired = Vec2::new(ui.available_width(), height);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let baseline = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y),
        egui::vec2(rect.width(), 1.0),
    );
    painter.rect_filled(baseline, 1.0, theme::BORDER);

    if values.len() < 2 {
        painter.line_segment(
            [
                egui::pos2(rect.left() + 4.0, rect.center().y),
                egui::pos2(rect.right() - 4.0, rect.center().y),
            ],
            Stroke::new(2.0, color),
        );
        return;
    }

    let max_value = values
        .iter()
        .copied()
        .fold(1.0_f32, |left, right| left.max(right));
    let points = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let x = rect.left() + index as f32 / (values.len() - 1) as f32 * rect.width();
            let y = rect.bottom() - (value / max_value).clamp(0.0, 1.0) * rect.height();
            egui::pos2(x, y)
        })
        .collect::<Vec<_>>();
    painter.add(egui::Shape::line(points, Stroke::new(2.0, color)));
}

fn process_table_widths(total: f32) -> [f32; 6] {
    let total = total.max(0.0);
    let compact = total < 760.0;
    let action = if compact { 66.0 } else { 82.0 };
    let pid = if compact { 62.0 } else { 78.0 };
    let cpu = if compact { 72.0 } else { 92.0 };
    let ram = if compact { 92.0 } else { 126.0 };
    let impact = if compact { 92.0 } else { 120.0 };
    let spacing = if compact { 18.0 } else { 32.0 };
    let name = (total - action - pid - cpu - ram - impact - spacing).max(116.0);
    let mut widths = [name, pid, cpu, ram, impact, action];
    let sum = widths.iter().sum::<f32>() + spacing;
    if sum > total && total > 0.0 {
        let scale = (total - spacing).max(1.0) / (sum - spacing);
        for width in &mut widths {
            *width = (*width * scale).max(42.0);
        }
    }
    widths
}

fn table_header_cell(ui: &mut egui::Ui, width: f32, text: &str) {
    ui.add_sized(
        [width, 18.0],
        egui::Label::new(
            RichText::new(text)
                .size(13.0)
                .strong()
                .color(theme::TEXT_SECONDARY),
        ),
    );
}

fn table_value_cell(ui: &mut egui::Ui, width: f32, text: impl Into<String>) {
    ui.add_sized(
        [width, 30.0],
        egui::Label::new(
            RichText::new(text.into())
                .size(13.0)
                .color(theme::TEXT_PRIMARY),
        ),
    );
}

fn label_truncated(
    ui: &mut egui::Ui,
    width: f32,
    text: impl Into<String>,
    size: f32,
    color: Color32,
    strong: bool,
) -> egui::Response {
    let text = text.into();
    let mut rich = RichText::new(text.clone()).size(size).color(color);
    if strong {
        rich = rich.strong();
    }
    ui.add_sized(
        [width.max(0.0), (size + 6.0).max(18.0)],
        egui::Label::new(rich).truncate(),
    )
    .on_hover_text(text)
}

fn action_button(ui: &mut egui::Ui, color: Color32, label: &str, action: impl FnOnce()) {
    let clicked = ui
        .add_sized(
            [ui.available_width(), 32.0],
            egui::Button::new(RichText::new(label).size(13.0).strong().color(color)),
        )
        .clicked();
    if clicked {
        action();
    }
}

fn resource_line(ui: &mut egui::Ui, label: &str, value: impl Into<String>, ratio: f32) {
    ui.horizontal(|ui| {
        ui.set_height(28.0);
        ui.label(RichText::new(label).size(13.0).color(theme::TEXT_SECONDARY));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(value.into())
                    .size(14.0)
                    .strong()
                    .color(theme::TEXT_PRIMARY),
            );
        });
    });
    progress_bar(ui, ratio.clamp(0.0, 1.0), theme::ORANGE);
}

fn progress_bar(ui: &mut egui::Ui, ratio: f32, color: Color32) {
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, theme::BORDER);
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * ratio, rect.height()));
    ui.painter().rect_filled(fill, 2.0, color);
    ui.add_space(6.0);
}

fn impact_gauge(ui: &mut egui::Ui, score: u8, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(size, size * 0.64), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let center = egui::pos2(rect.center().x, rect.bottom() - 2.0);
    let radius = rect.width() * 0.42;
    let color = process_tone_color(score);
    let steps = 36;
    for index in 0..steps {
        let t0 = std::f32::consts::PI + index as f32 / steps as f32 * std::f32::consts::PI;
        let t1 = std::f32::consts::PI + (index + 1) as f32 / steps as f32 * std::f32::consts::PI;
        let active = index as f32 / steps as f32 <= score as f32 / 100.0;
        let stroke = Stroke::new(7.0, if active { color } else { theme::BORDER });
        painter.line_segment(
            [
                center + egui::vec2(t0.cos() * radius, t0.sin() * radius),
                center + egui::vec2(t1.cos() * radius, t1.sin() * radius),
            ],
            stroke,
        );
    }
    let angle = std::f32::consts::PI + (score as f32 / 100.0) * std::f32::consts::PI;
    painter.line_segment(
        [
            center,
            center + egui::vec2(angle.cos() * (radius - 10.0), angle.sin() * (radius - 10.0)),
        ],
        Stroke::new(3.0, theme::TEXT_PRIMARY),
    );
    painter.circle_filled(center, 4.0, theme::TEXT_PRIMARY);
}

fn draw_process_icon(ui: &mut egui::Ui, icon: AppIcon, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::hover());
    let color = match icon {
        AppIcon::Chrome => Color32::from_rgb(252, 184, 41),
        AppIcon::Code => Color32::from_rgb(38, 166, 255),
        AppIcon::Music => Color32::from_rgb(45, 212, 96),
        AppIcon::Chat => Color32::from_rgb(112, 137, 255),
        AppIcon::Terminal => Color32::from_rgb(94, 210, 112),
        _ => theme::TEXT_SECONDARY,
    };
    ui.painter()
        .circle_filled(rect.center(), rect.width() * 0.46, color);
    paint_icon(ui, rect.shrink(size * 0.2), icon, Color32::WHITE);
}

fn process_icon_for_name(name: &str) -> AppIcon {
    let lower = name.to_ascii_lowercase();
    if lower.contains("chrome") {
        AppIcon::Chrome
    } else if lower.contains("code") || lower.contains("studio") {
        AppIcon::Code
    } else if lower.contains("spotify") {
        AppIcon::Music
    } else if lower.contains("discord") || lower.contains("telegram") {
        AppIcon::Chat
    } else if lower.contains("powershell") || lower.contains("cmd") || lower.contains("terminal") {
        AppIcon::Terminal
    } else {
        AppIcon::GenericProcess
    }
}

fn process_exe_label(row: &ProcessRow) -> String {
    row.path
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| row.name.clone())
}

fn safety_short_label(safety: ProcessSafety) -> &'static str {
    match safety {
        ProcessSafety::CriticalSystem | ProcessSafety::WindowsComponent => "Protégé",
        ProcessSafety::Caution | ProcessSafety::Unknown => "Prudence",
        ProcessSafety::LikelyClosable => "Sûr",
    }
}

fn is_protected_process(row: &ProcessRow) -> bool {
    matches!(
        row.insight.safety,
        ProcessSafety::CriticalSystem | ProcessSafety::WindowsComponent
    )
}

fn impact_label(score: u8) -> &'static str {
    match score {
        0..=24 => "Faible",
        25..=59 => "Moyen",
        60..=84 => "Élevé",
        _ => "Critique",
    }
}

fn process_tone_color(score: u8) -> Color32 {
    match score {
        0..=24 => theme::ORANGE_SOFT,
        25..=59 => theme::WARNING,
        60..=84 => theme::RED_SOFT,
        _ => theme::RED,
    }
}

fn page_bounds(total: usize, page: usize, page_size: usize) -> (usize, usize, usize) {
    if total == 0 || page_size == 0 {
        return (0, 0, 0);
    }
    let max_page = max_page_for_len(total, page_size);
    let safe_page = page.min(max_page);
    let start = safe_page * page_size;
    let end = (start + page_size).min(total);
    (start, end, max_page)
}

fn max_page_for_len(total: usize, page_size: usize) -> usize {
    if total == 0 || page_size == 0 {
        0
    } else {
        (total - 1) / page_size
    }
}

fn icon_text(icon: AppIcon) -> &'static str {
    match icon {
        AppIcon::Logo => "▣",
        AppIcon::Search => "⌕",
        AppIcon::Processes => "▤",
        AppIcon::Monitor => "▧",
        AppIcon::Thermal => "♨",
        AppIcon::Settings => "⚙",
        AppIcon::Cpu => "□",
        AppIcon::Memory => "▥",
        AppIcon::Network => "↕",
        AppIcon::Storage => "▱",
        AppIcon::Alerts => "!",
        AppIcon::Shield => "◇",
        AppIcon::More => "⋮",
        AppIcon::Chrome => "C",
        AppIcon::Code => "<>",
        AppIcon::Music => "♪",
        AppIcon::Chat => "●",
        AppIcon::Terminal => ">_",
        AppIcon::GenericProcess => "•",
    }
}

fn paint_icon(ui: &mut egui::Ui, rect: egui::Rect, icon: AppIcon, color: Color32) {
    let painter = ui.painter();
    let stroke = Stroke::new((rect.width() / 14.0).clamp(1.4, 2.4), color);
    let c = rect.center();
    let w = rect.width();
    let h = rect.height();

    match icon {
        AppIcon::Logo => {
            painter.rect_filled(
                rect.shrink(w * 0.12),
                4.0,
                theme::ORANGE.gamma_multiply(0.25),
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.18, c.y),
                    egui::pos2(rect.left() + w * 0.35, c.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.35, c.y),
                    egui::pos2(rect.left() + w * 0.45, rect.top() + h * 0.28),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.45, rect.top() + h * 0.28),
                    egui::pos2(rect.left() + w * 0.58, rect.bottom() - h * 0.25),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.58, rect.bottom() - h * 0.25),
                    egui::pos2(rect.left() + w * 0.68, c.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.68, c.y),
                    egui::pos2(rect.right() - w * 0.18, c.y),
                ],
                stroke,
            );
        }
        AppIcon::Search => {
            painter.circle_stroke(c - egui::vec2(w * 0.08, h * 0.08), w * 0.27, stroke);
            painter.line_segment(
                [
                    c + egui::vec2(w * 0.13, h * 0.13),
                    c + egui::vec2(w * 0.32, h * 0.32),
                ],
                stroke,
            );
        }
        AppIcon::Processes => {
            for offset in [-0.22, 0.0, 0.22] {
                let y = c.y + h * offset;
                painter.line_segment(
                    [
                        egui::pos2(rect.left() + w * 0.2, y),
                        egui::pos2(rect.right() - w * 0.2, y),
                    ],
                    stroke,
                );
                painter.circle_filled(egui::pos2(rect.left() + w * 0.22, y), 2.0, color);
            }
        }
        AppIcon::Monitor => {
            painter.rect_stroke(
                rect.shrink(w * 0.18),
                3.0,
                stroke,
                egui::StrokeKind::Outside,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.26, c.y),
                    egui::pos2(rect.left() + w * 0.42, c.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.42, c.y),
                    egui::pos2(rect.left() + w * 0.5, rect.top() + h * 0.32),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.5, rect.top() + h * 0.32),
                    egui::pos2(rect.left() + w * 0.62, rect.bottom() - h * 0.32),
                ],
                stroke,
            );
        }
        AppIcon::Thermal => {
            painter.line_segment(
                [
                    egui::pos2(c.x, rect.top() + h * 0.18),
                    egui::pos2(c.x, rect.bottom() - h * 0.32),
                ],
                stroke,
            );
            painter.circle_stroke(egui::pos2(c.x, rect.bottom() - h * 0.28), w * 0.18, stroke);
        }
        AppIcon::Settings => {
            painter.circle_stroke(c, w * 0.26, stroke);
            for index in 0..8 {
                let angle = index as f32 / 8.0 * std::f32::consts::TAU;
                painter.line_segment(
                    [
                        c + egui::vec2(angle.cos() * w * 0.34, angle.sin() * h * 0.34),
                        c + egui::vec2(angle.cos() * w * 0.43, angle.sin() * h * 0.43),
                    ],
                    stroke,
                );
            }
        }
        AppIcon::Cpu => {
            painter.rect_stroke(
                rect.shrink(w * 0.24),
                2.0,
                stroke,
                egui::StrokeKind::Outside,
            );
            painter.rect_stroke(
                rect.shrink(w * 0.35),
                1.0,
                stroke,
                egui::StrokeKind::Outside,
            );
        }
        AppIcon::Memory => {
            painter.rect_stroke(
                rect.shrink(w * 0.18),
                2.0,
                stroke,
                egui::StrokeKind::Outside,
            );
            for index in 0..4 {
                let x = rect.left() + w * (0.28 + index as f32 * 0.14);
                painter.line_segment(
                    [
                        egui::pos2(x, rect.top() + h * 0.35),
                        egui::pos2(x, rect.bottom() - h * 0.35),
                    ],
                    stroke,
                );
            }
        }
        AppIcon::Network => {
            painter.line_segment(
                [
                    egui::pos2(c.x, rect.top() + h * 0.16),
                    egui::pos2(c.x, rect.bottom() - h * 0.16),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x, rect.top() + h * 0.16),
                    egui::pos2(c.x - w * 0.16, rect.top() + h * 0.32),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x, rect.top() + h * 0.16),
                    egui::pos2(c.x + w * 0.16, rect.top() + h * 0.32),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x, rect.bottom() - h * 0.16),
                    egui::pos2(c.x - w * 0.16, rect.bottom() - h * 0.32),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x, rect.bottom() - h * 0.16),
                    egui::pos2(c.x + w * 0.16, rect.bottom() - h * 0.32),
                ],
                stroke,
            );
        }
        AppIcon::Storage => {
            painter.rect_stroke(
                rect.shrink(w * 0.18),
                3.0,
                stroke,
                egui::StrokeKind::Outside,
            );
            painter.line_segment(
                [
                    egui::pos2(rect.left() + w * 0.28, rect.bottom() - h * 0.34),
                    egui::pos2(rect.right() - w * 0.28, rect.bottom() - h * 0.34),
                ],
                stroke,
            );
        }
        AppIcon::Alerts => {
            painter.circle_stroke(c, w * 0.27, stroke);
            painter.line_segment(
                [egui::pos2(c.x, rect.top() + h * 0.28), egui::pos2(c.x, c.y)],
                stroke,
            );
            painter.circle_filled(egui::pos2(c.x, rect.bottom() - h * 0.28), 2.0, color);
        }
        AppIcon::Shield => {
            let points = vec![
                egui::pos2(c.x, rect.top() + h * 0.16),
                egui::pos2(rect.right() - w * 0.2, rect.top() + h * 0.28),
                egui::pos2(rect.right() - w * 0.27, rect.bottom() - h * 0.26),
                egui::pos2(c.x, rect.bottom() - h * 0.12),
                egui::pos2(rect.left() + w * 0.27, rect.bottom() - h * 0.26),
                egui::pos2(rect.left() + w * 0.2, rect.top() + h * 0.28),
            ];
            painter.add(egui::Shape::closed_line(points, stroke));
        }
        AppIcon::More => {
            for offset in [-0.22, 0.0, 0.22] {
                painter.circle_filled(egui::pos2(c.x, c.y + h * offset), w * 0.055, color);
            }
        }
        _ => {
            painter.text(
                c,
                egui::Align2::CENTER_CENTER,
                icon_text(icon),
                egui::FontId::new((w * 0.42).max(11.0), egui::FontFamily::Proportional),
                color,
            );
        }
    }
}

fn metric_tone_accent(tone: CardTone) -> Color32 {
    match tone {
        CardTone::Default => theme::ORANGE_SOFT,
        CardTone::Accent => theme::ORANGE,
        CardTone::Warning => theme::WARNING,
        CardTone::Danger => theme::RED,
        CardTone::Info => theme::CYAN,
    }
}

fn render_summary_card_grid(ui: &mut egui::Ui, specs: &[SummaryCardSpec]) {
    let layout = responsive_card_grid_layout(
        bounded_available_width(ui),
        specs.len(),
        SUMMARY_CARD_MIN_WIDTH,
        4,
        SUMMARY_CARD_GAP,
        SUMMARY_CARD_HEIGHT,
    );
    if layout.columns == 0 || layout.rows == 0 {
        return;
    }

    let width = bounded_available_width(ui);
    let (grid_rect, _) =
        ui.allocate_exact_size(Vec2::new(width, layout.total_height), egui::Sense::hover());
    for (index, spec) in specs.iter().enumerate() {
        let column = index % layout.columns;
        let row = index / layout.columns;
        let x = grid_rect.left() + column as f32 * (layout.card_width + SUMMARY_CARD_GAP);
        let y = grid_rect.top() + row as f32 * (SUMMARY_CARD_HEIGHT + SUMMARY_CARD_GAP);
        let rect = egui::Rect::from_min_size(
            egui::pos2(x, y),
            Vec2::new(layout.card_width, SUMMARY_CARD_HEIGHT),
        );
        summary_card_at(ui, index, rect, spec);
    }
}

fn summary_card_at(ui: &mut egui::Ui, index: usize, rect: egui::Rect, spec: &SummaryCardSpec) {
    let accent = metric_tone_accent(spec.tone);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("summary-card", index, spec.title))
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect);
    child.set_width(rect.width());
    child.set_max_width(rect.width());

    theme::metric_card_variant(spec.tone).show(&mut child, |ui| {
        let inner_width = (rect.width() - 24.0).max(0.0);
        ui.set_width(inner_width);
        ui.set_max_width(inner_width);
        ui.set_min_height((rect.height() - 24.0).max(0.0));
        label_truncated(
            ui,
            inner_width,
            spec.title,
            12.0,
            theme::TEXT_SECONDARY,
            false,
        );
        ui.add_space(2.0);
        label_truncated(
            ui,
            inner_width,
            &spec.value,
            25.0,
            theme::TEXT_PRIMARY,
            true,
        );
        ui.add_space(2.0);
        label_truncated(
            ui,
            inner_width,
            &spec.subtitle,
            12.0,
            theme::TEXT_SECONDARY,
            false,
        );

        let bar_width = ui.available_width().max(42.0);
        let (bar_rect, _) = ui.allocate_exact_size(Vec2::new(bar_width, 8.0), egui::Sense::hover());
        let bar = egui::Rect::from_min_size(
            egui::pos2(bar_rect.left(), bar_rect.center().y - 1.0),
            egui::vec2(bar_rect.width() * 0.42, 2.0),
        );
        ui.painter_at(bar_rect).rect_filled(
            bar,
            CornerRadius::same(255),
            accent.gamma_multiply(0.95),
        );
    });
}

fn render_temperature_grid(ui: &mut egui::Ui, readings: &[TemperatureReading]) {
    let layout = responsive_card_grid_layout(
        bounded_available_width(ui),
        readings.len(),
        TEMPERATURE_CARD_MIN_WIDTH,
        2,
        TEMPERATURE_CARD_GAP,
        TEMPERATURE_CARD_HEIGHT,
    );
    if layout.columns == 0 || layout.rows == 0 {
        return;
    }

    let width = bounded_available_width(ui);
    let (grid_rect, _) =
        ui.allocate_exact_size(Vec2::new(width, layout.total_height), egui::Sense::hover());
    for (index, reading) in readings.iter().enumerate() {
        let column = index % layout.columns;
        let row = index / layout.columns;
        let x = grid_rect.left() + column as f32 * (layout.card_width + TEMPERATURE_CARD_GAP);
        let y = grid_rect.top() + row as f32 * (TEMPERATURE_CARD_HEIGHT + TEMPERATURE_CARD_GAP);
        let rect = egui::Rect::from_min_size(
            egui::pos2(x, y),
            Vec2::new(layout.card_width, TEMPERATURE_CARD_HEIGHT),
        );
        temperature_card_at(ui, index, rect, reading);
    }
}

fn temperature_card_at(
    ui: &mut egui::Ui,
    index: usize,
    rect: egui::Rect,
    reading: &TemperatureReading,
) {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("temperature-card", index))
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(rect);
    child.set_width(rect.width());
    child.set_max_width(rect.width());
    temperature_card(&mut child, reading);
}

fn temperature_card(ui: &mut egui::Ui, reading: &TemperatureReading) {
    let accent = thermal_state_color(reading.state);
    let tone = thermal_state_tone(reading.state);

    theme::metric_card_variant(tone).show(ui, |ui| {
        ui.set_min_size(Vec2::new(236.0, 148.0));
        ui.horizontal_wrapped(|ui| {
            theme::status_chip(ui, reading.kind.label(), accent);
            theme::status_chip(ui, reading.state.label(), accent);
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(&reading.name)
                .size(14.0)
                .color(theme::TEXT_PRIMARY),
        );
        ui.label(
            RichText::new(
                reading
                    .temperature_celsius
                    .map(|value| format!("{value:.1} °C"))
                    .unwrap_or_else(|| "Indisponible".into()),
            )
            .text_style(egui::TextStyle::Name("Metric".into()))
            .color(theme::TEXT_PRIMARY),
        );
        if let Some(fan_speed_rpm) = reading.fan_speed_rpm {
            ui.label(
                RichText::new(format!("Ventilo : {fan_speed_rpm} RPM"))
                    .size(12.0)
                    .color(theme::TEXT_SECONDARY),
            );
        }
        if let (Some(warning), Some(critical)) = (
            reading.warning_limit_celsius,
            reading.critical_limit_celsius,
        ) {
            ui.label(
                RichText::new(format!("Seuils : {warning:.0} / {critical:.0} °C"))
                    .size(12.0)
                    .color(theme::TEXT_SECONDARY),
            );
        }
        ui.label(
            RichText::new(format!("Source : {}", reading.source.label()))
                .size(12.0)
                .color(theme::TEXT_SECONDARY),
        );
    });
}

fn temperature_series(
    history: &[crate::monitor::MetricSnapshot],
    kind: TemperatureSensorKind,
) -> Vec<f32> {
    history
        .iter()
        .filter_map(|sample| {
            sample
                .temperatures
                .iter()
                .find(|reading| reading.kind == kind)
                .and_then(|reading| reading.temperature_celsius)
        })
        .collect()
}

fn process_metric_grid(ui: &mut egui::Ui, metrics: &[ProcessMetric], show_cpu: bool) {
    egui::Grid::new(ui.next_auto_id())
        .num_columns(4)
        .striped(true)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.strong("Nom");
            ui.strong("PID");
            ui.strong(if show_cpu { "CPU %" } else { "Memoire" });
            ui.strong("Memoire %");
            ui.end_row();

            for metric in metrics {
                ui.label(RichText::new(&metric.name).color(theme::TEXT_PRIMARY));
                ui.label(
                    RichText::new(metric.pid.to_string())
                        .monospace()
                        .color(theme::TEXT_SECONDARY),
                );
                if show_cpu {
                    ui.label(
                        RichText::new(format!("{:.1}", metric.cpu))
                            .monospace()
                            .color(theme::TEXT_PRIMARY),
                    );
                } else {
                    ui.label(
                        RichText::new(format_bytes(metric.memory_bytes))
                            .monospace()
                            .color(theme::TEXT_PRIMARY),
                    );
                }
                ui.label(
                    RichText::new(format!("{:.1}", metric.memory_percent))
                        .monospace()
                        .color(theme::TEXT_SECONDARY),
                );
                ui.end_row();
            }
        });
}

fn draw_line_chart(
    ui: &mut egui::Ui,
    title: &str,
    values: Vec<f32>,
    max_value: f32,
    color: Color32,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(title)
                .monospace()
                .size(12.0)
                .color(theme::TEXT_SECONDARY),
        );

        if let Some(last) = values.last() {
            theme::status_chip(ui, format!("{last:.1}"), color);
        }
    });

    let desired_size = Vec2::new(ui.available_width(), 150.0);
    let (rect, _) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, CornerRadius::same(14), theme::BG_PANEL);
    painter.rect_stroke(
        rect,
        CornerRadius::same(14),
        Stroke::new(1.0, theme::BORDER.gamma_multiply(0.9)),
        egui::StrokeKind::Outside,
    );

    for step in 1..4 {
        let y = rect.top() + rect.height() * (step as f32 / 4.0);
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(1.0, theme::BORDER.gamma_multiply(0.35)),
        );
    }

    if values.len() < 2 {
        return;
    }

    let points = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let x = rect.left()
                + (index as f32 / (values.len().saturating_sub(1)) as f32) * rect.width();
            let normalized = if max_value <= 0.0 {
                0.0
            } else {
                (*value / max_value).clamp(0.0, 1.0)
            };
            let y = rect.bottom() - normalized * rect.height();
            egui::pos2(x, y)
        })
        .collect::<Vec<_>>();

    let mut fill_points = Vec::with_capacity(points.len() + 2);
    fill_points.push(egui::pos2(rect.left(), rect.bottom()));
    fill_points.extend(points.iter().copied());
    fill_points.push(egui::pos2(rect.right(), rect.bottom()));

    painter.add(egui::Shape::convex_polygon(
        fill_points,
        color.gamma_multiply(0.10),
        Stroke::new(0.0, Color32::TRANSPARENT),
    ));

    painter.add(egui::Shape::line(
        points.clone(),
        Stroke::new(6.0, color.gamma_multiply(0.15)),
    ));
    painter.add(egui::Shape::line(points.clone(), Stroke::new(2.0, color)));

    let marker_stride = (points.len() / 6).max(1);
    for point in points.iter().step_by(marker_stride) {
        painter.circle_filled(*point, 2.6, color);
    }

    if let Some(last_point) = points.last() {
        painter.circle_filled(*last_point, 4.0, color);
    }
}

fn thermal_state_color(state: ThermalState) -> Color32 {
    match state {
        ThermalState::Normal => theme::CYAN,
        ThermalState::Warning => theme::ORANGE,
        ThermalState::Critical => theme::RED,
    }
}

fn thermal_state_tone(state: ThermalState) -> CardTone {
    match state {
        ThermalState::Normal => CardTone::Info,
        ThermalState::Warning => CardTone::Warning,
        ThermalState::Critical => CardTone::Danger,
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

fn format_timestamp(timestamp: Option<i64>) -> String {
    let Some(timestamp) = timestamp else {
        return "-".into();
    };
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".into())
}

fn percent(value: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (value as f64 / total as f64 * 100.0) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{AlertEvent, AlertEventKind, AlertEventState};

    fn test_process_row(pid: u32, name: &str, cpu_now: f32) -> ProcessRow {
        ProcessRow {
            key: ProcessKey {
                pid,
                started_at: Some(pid as u64),
            },
            family_id: name.to_ascii_lowercase(),
            name: name.to_owned(),
            path: None,
            parent_pid: None,
            cpu_now,
            memory_bytes: 1024,
            threads: 1,
            priority: PriorityClass::Normal,
            status: "En cours".into(),
            run_time_secs: 10,
            has_visible_window: true,
            insight: crate::process::ProcessInsight {
                impact_score: cpu_now.round().clamp(0.0, 100.0) as u8,
                cpu_avg_10s: cpu_now,
                cpu_peak_60s: cpu_now,
                memory_percent: 1.0,
                disk_io_bytes_per_sec: 0,
                safety: ProcessSafety::LikelyClosable,
                suggested_action: SuggestedAction::CloseGracefully,
                trend: crate::process::ProcessTrend::Stable,
                reasons: Vec::new(),
            },
        }
    }

    fn test_process_state(revision: u64, rows: Vec<ProcessRow>) -> ProcessState {
        ProcessState {
            revision,
            rows: Arc::from(rows),
            ..ProcessState::default()
        }
    }

    fn test_cache_input<'a>(
        filter: &'a str,
        sort: ProcessSort,
        alerted_pids: &'a HashSet<u32>,
    ) -> ProcessRowsCacheInput<'a> {
        ProcessRowsCacheInput {
            filter,
            hide_windows_processes: false,
            show_only_suspects: false,
            show_only_closeable: false,
            sort,
            alerted_pids,
        }
    }

    #[test]
    fn view_descriptions_match_expected_modules() {
        let cases = [
            (View::Search, "Recherche", "index local"),
            (View::Processes, "Processus", "processus Windows"),
            (View::Monitor, "Monitoring", "temps réel"),
            (View::Temperatures, "Thermique", "thermique"),
            (View::Settings, "Réglages", "Configuration"),
        ];

        for (view, label, description_fragment) in cases {
            assert_eq!(view.label(), label);
            assert!(
                view.description().contains(description_fragment),
                "description for {label} should contain {description_fragment:?}"
            );
        }
    }

    #[test]
    fn format_bytes_scales_values() {
        assert_eq!(format_bytes(512), "512.0 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
    }

    #[test]
    fn percent_handles_zero_total() {
        assert_eq!(percent(10, 0), 0.0);
        assert!((percent(25, 200) - 12.5).abs() < f32::EPSILON);
    }

    #[test]
    fn process_pagination_clamps_to_available_rows() {
        assert_eq!(page_bounds(0, 3, PROCESS_PAGE_SIZE), (0, 0, 0));
        assert_eq!(page_bounds(184, 0, PROCESS_PAGE_SIZE), (0, 10, 18));
        assert_eq!(page_bounds(184, 18, PROCESS_PAGE_SIZE), (180, 184, 18));
        assert_eq!(page_bounds(184, 99, PROCESS_PAGE_SIZE), (180, 184, 18));
    }

    #[test]
    fn process_rows_cache_rebuilds_only_when_inputs_change() {
        let state = test_process_state(
            1,
            vec![
                test_process_row(10, "alpha.exe", 4.0),
                test_process_row(20, "beta.exe", 18.0),
                test_process_row(30, "gamma.exe", 7.0),
            ],
        );
        let alerted_pids = HashSet::new();
        let mut cache = ProcessRowsCache::default();

        let first = cached_process_row_indices(
            &mut cache,
            &state,
            test_cache_input("", ProcessSort::CpuNow, &alerted_pids),
        )
        .to_vec();
        assert_eq!(first, vec![1, 2, 0]);
        assert_eq!(cache.rebuilds, 1);

        let second = cached_process_row_indices(
            &mut cache,
            &state,
            test_cache_input("", ProcessSort::CpuNow, &alerted_pids),
        )
        .to_vec();
        assert_eq!(second, first);
        assert_eq!(cache.rebuilds, 1);

        let by_name = cached_process_row_indices(
            &mut cache,
            &state,
            test_cache_input("", ProcessSort::Name, &alerted_pids),
        )
        .to_vec();
        assert_eq!(by_name, vec![0, 1, 2]);
        assert_eq!(cache.rebuilds, 2);

        let filtered = cached_process_row_indices(
            &mut cache,
            &state,
            test_cache_input("alpha", ProcessSort::CpuNow, &alerted_pids),
        )
        .to_vec();
        assert_eq!(filtered, vec![0]);
        assert_eq!(cache.rebuilds, 3);

        let next_revision = test_process_state(2, state.rows.iter().cloned().collect());
        let _ = cached_process_row_indices(
            &mut cache,
            &next_revision,
            test_cache_input("alpha", ProcessSort::CpuNow, &alerted_pids),
        );
        assert_eq!(cache.rebuilds, 4);
    }

    #[test]
    fn process_dashboard_layout_never_exceeds_available_height() {
        for size in [
            Vec2::new(1120.0, 620.0),
            Vec2::new(1352.0, 746.0),
            Vec2::new(1360.0, 760.0),
            Vec2::new(1600.0, 900.0),
        ] {
            let layout = process_dashboard_layout(size);
            let used_height = layout.kpi_height
                + layout.gap
                + layout.main_height
                + layout.gap
                + layout.events_height;
            assert!(
                used_height <= size.y + f32::EPSILON,
                "process layout height {used_height} should fit in {}",
                size.y
            );
        }
    }

    #[test]
    fn window_size_adapts_to_work_area() {
        let current_screen = native_window_sizes_for_work_area(Vec2::new(1536.0, 912.0));
        assert_eq!(current_screen.initial, Vec2::new(1476.0, 856.0));
        assert!(current_screen.minimum.x <= current_screen.initial.x);
        assert!(current_screen.minimum.y <= current_screen.initial.y);

        let small_screen = native_window_sizes_for_work_area(Vec2::new(1024.0, 720.0));
        assert_eq!(small_screen.initial, Vec2::new(964.0, 664.0));
        assert_eq!(small_screen.minimum, small_screen.initial);

        let ultra_wide = native_window_sizes_for_work_area(Vec2::new(3440.0, 1400.0));
        assert_eq!(ultra_wide.initial, TARGET_WINDOW_SIZE);
        assert_eq!(ultra_wide.minimum, DESIRED_MIN_WINDOW_SIZE);
    }

    #[test]
    fn top_bar_layout_keeps_search_inside_available_width() {
        for width in [760.0, 980.0, 1180.0, 1412.0] {
            let layout = top_bar_layout(width);
            assert!(layout.search_width >= 96.0);
            assert!(layout.search_width <= 490.0);
            assert!(layout.drag_width <= width);
            assert!(layout.drag_width + layout.search_width < width);
            if width < 980.0 {
                assert!(!layout.show_shortcut);
            }
            if width < 1440.0 {
                assert!(!layout.show_statuses);
                assert!(layout.search_width <= 400.0);
            }
        }
    }

    #[test]
    fn process_kpi_grid_fits_inside_row() {
        for (width, expected_columns) in [(720.0, 2), (1120.0, 3), (1388.0, 6), (1600.0, 6)] {
            let layout = kpi_grid_layout(width);
            assert_eq!(layout.columns, expected_columns);
            let used_width = layout.card_width * layout.columns as f32
                + PROCESS_KPI_GAP * (layout.columns - 1) as f32;
            assert!(
                used_width <= width + f32::EPSILON,
                "kpi row width {used_width} should fit in {width}"
            );
            if layout.columns == 6 {
                assert!(layout.card_width >= PROCESS_KPI_MIN_CARD_WIDTH);
            }
            assert_eq!(layout.rows, PROCESS_KPI_COUNT.div_ceil(layout.columns));
        }
    }

    #[test]
    fn responsive_card_grid_uses_bounded_columns() {
        let narrow = responsive_card_grid_layout(
            520.0,
            4,
            SUMMARY_CARD_MIN_WIDTH,
            4,
            SUMMARY_CARD_GAP,
            SUMMARY_CARD_HEIGHT,
        );
        assert_eq!(narrow.columns, 2);
        assert_eq!(narrow.rows, 2);

        let wide = responsive_card_grid_layout(
            1200.0,
            4,
            SUMMARY_CARD_MIN_WIDTH,
            4,
            SUMMARY_CARD_GAP,
            SUMMARY_CARD_HEIGHT,
        );
        assert_eq!(wide.columns, 4);
        let used_width =
            wide.card_width * wide.columns as f32 + SUMMARY_CARD_GAP * (wide.columns - 1) as f32;
        assert!(used_width <= 1200.0 + f32::EPSILON);
    }

    #[test]
    fn temperature_grid_prefers_two_readable_columns() {
        let layout = responsive_card_grid_layout(
            1280.0,
            3,
            TEMPERATURE_CARD_MIN_WIDTH,
            2,
            TEMPERATURE_CARD_GAP,
            TEMPERATURE_CARD_HEIGHT,
        );
        assert_eq!(layout.columns, 2);
        assert_eq!(layout.rows, 2);
        assert!(layout.card_width >= TEMPERATURE_CARD_MIN_WIDTH);
    }

    #[test]
    fn process_panel_split_stays_bounded() {
        for width in [720.0, 1120.0, 1600.0] {
            let (left, right) = process_panel_widths(width, PROCESS_PANEL_GAP);
            assert!(left >= 0.0);
            assert!(right >= 0.0);
            assert!(left + PROCESS_PANEL_GAP + right <= width + f32::EPSILON);
        }
    }

    #[test]
    fn process_table_columns_fit_available_width() {
        for width in [520.0, 720.0, 980.0] {
            let columns = process_table_widths(width);
            let used = columns.iter().sum::<f32>() + if width < 760.0 { 18.0 } else { 32.0 };
            assert!(
                used <= width + f32::EPSILON,
                "table columns width {used} should fit in {width}"
            );
        }
    }

    #[test]
    fn event_columns_keep_message_space() {
        for width in [520.0, 900.0, 1388.0] {
            let columns = event_column_widths(width);
            assert!(columns.iter().all(|column| *column >= 0.0));
            assert!(columns[3] >= 120.0);
        }
    }

    #[test]
    fn process_icon_mapping_uses_custom_fallbacks() {
        assert_eq!(process_icon_for_name("Google Chrome"), AppIcon::Chrome);
        assert_eq!(process_icon_for_name("Visual Studio Code"), AppIcon::Code);
        assert_eq!(process_icon_for_name("Spotify"), AppIcon::Music);
        assert_eq!(
            process_icon_for_name("unknown.exe"),
            AppIcon::GenericProcess
        );
    }

    #[test]
    fn impact_labels_match_dashboard_thresholds() {
        assert_eq!(impact_label(12), "Faible");
        assert_eq!(impact_label(45), "Moyen");
        assert_eq!(impact_label(70), "Élevé");
        assert_eq!(impact_label(91), "Critique");
    }

    #[test]
    fn active_alerts_keep_latest_persistent_active_events() {
        let events = vec![
            AlertEvent {
                kind: AlertEventKind::MetricThreshold,
                rule_id: "cpu".into(),
                source_label: "CPU".into(),
                source_pid: Some(1),
                message: "first".into(),
                state: AlertEventState::Active,
                value_percent: 95.0,
                threshold_percent: 90.0,
                triggered_at_utc: 1,
                resolved_at_utc: None,
            },
            AlertEvent {
                kind: AlertEventKind::MetricThreshold,
                rule_id: "cpu".into(),
                source_label: "CPU".into(),
                source_pid: Some(1),
                message: "latest".into(),
                state: AlertEventState::Active,
                value_percent: 97.0,
                threshold_percent: 90.0,
                triggered_at_utc: 2,
                resolved_at_utc: None,
            },
            AlertEvent {
                kind: AlertEventKind::CoolingActionApplied,
                rule_id: "cooling".into(),
                source_label: "Fan".into(),
                source_pid: None,
                message: "transient".into(),
                state: AlertEventState::Active,
                value_percent: 0.0,
                threshold_percent: 0.0,
                triggered_at_utc: 3,
                resolved_at_utc: None,
            },
            AlertEvent {
                kind: AlertEventKind::MetricThreshold,
                rule_id: "memory".into(),
                source_label: "RAM".into(),
                source_pid: None,
                message: "resolved".into(),
                state: AlertEventState::Resolved,
                value_percent: 91.0,
                threshold_percent: 90.0,
                triggered_at_utc: 4,
                resolved_at_utc: Some(5),
            },
        ];

        let mut active_alerts = WindowsHelpApp::active_alerts(&events);
        active_alerts.sort_by(|left, right| left.rule_id.cmp(&right.rule_id));

        assert_eq!(active_alerts.len(), 1);
        assert_eq!(active_alerts[0].message, "latest");
        assert_eq!(active_alerts[0].source_label, "CPU");
    }

    #[test]
    fn active_alerts_keep_distinct_process_pids_separate() {
        let events = vec![
            AlertEvent {
                kind: AlertEventKind::MetricThreshold,
                rule_id: "process-cpu".into(),
                source_label: "chrome.exe".into(),
                source_pid: Some(10),
                message: "pid10".into(),
                state: AlertEventState::Active,
                value_percent: 90.0,
                threshold_percent: 80.0,
                triggered_at_utc: 1,
                resolved_at_utc: None,
            },
            AlertEvent {
                kind: AlertEventKind::MetricThreshold,
                rule_id: "process-cpu".into(),
                source_label: "chrome.exe".into(),
                source_pid: Some(11),
                message: "pid11".into(),
                state: AlertEventState::Active,
                value_percent: 85.0,
                threshold_percent: 80.0,
                triggered_at_utc: 2,
                resolved_at_utc: None,
            },
        ];

        let active_alerts = WindowsHelpApp::active_alerts(&events);
        assert_eq!(active_alerts.len(), 2);
    }
}
