use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, TimeZone, Utc};
use eframe::egui::{self, Color32, CornerRadius, RichText, Stroke, Vec2};
use tokio::runtime::Runtime;

use crate::config::{Settings, app_paths, load_or_create_settings, save_settings};
use crate::monitor::{AlertEvent, AlertEventState, AlertRule, MonitorService, ProcessMetric};
use crate::platform_windows::{PriorityClass, open_path, reveal_in_explorer};
use crate::process::{
    ProcessAction, ProcessActionResult, ProcessBottleneck, ProcessFamily, ProcessKey,
    ProcessManager, ProcessRecommendation, ProcessRow, ProcessSafety, ProcessState,
    SuggestedAction,
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
            Self::Monitor => "Surveillance",
            Self::Temperatures => "Temperatures",
            Self::Settings => "Paramètres",
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
enum ProcessTab {
    Families,
    Instances,
}

impl ProcessTab {
    fn label(self) -> &'static str {
        match self {
            Self::Families => "Familles",
            Self::Instances => "Instances",
        }
    }
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

#[derive(Clone)]
struct SettingsDraft {
    roots_text: String,
    exclusions_text: String,
    include_hidden: bool,
    include_system: bool,
    scan_concurrency: usize,
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

        Ok(Settings {
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
            process_refresh_ms: self.process_refresh_ms.max(250),
            monitor_refresh_ms: self.monitor_refresh_ms.max(250),
            alert_rules: self.alert_rules.clone(),
            thermal,
            saved_at_utc: Utc::now().timestamp(),
        })
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
    search_results: Vec<SearchResult>,
    last_search_fingerprint: String,
    process_filter: String,
    selected_process: Option<ProcessKey>,
    selected_family: Option<String>,
    process_sort: ProcessSort,
    process_tab: ProcessTab,
    hide_windows_processes: bool,
    show_only_suspects: bool,
    show_only_closeable: bool,
    confirm_kill: Option<(ProcessKey, String)>,
    status_message: Option<String>,
}

impl WindowsHelpApp {
    fn build(runtime: Arc<Runtime>, settings: Settings) -> anyhow::Result<Self> {
        let handle = runtime.handle().clone();
        let search_service = SearchService::new(handle.clone(), settings.index.clone())?;
        let process_manager = ProcessManager::new(
            handle.clone(),
            Duration::from_millis(settings.process_refresh_ms),
        );
        let monitor_service = MonitorService::new(
            handle,
            Duration::from_millis(settings.monitor_refresh_ms),
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
            current_view: View::Search,
            search_text: String::new(),
            extension_filter: String::new(),
            min_size_filter: String::new(),
            max_size_filter: String::new(),
            modified_after_filter: String::new(),
            modified_before_filter: String::new(),
            include_hidden_results: false,
            search_results: Vec::new(),
            last_search_fingerprint: String::new(),
            process_filter: String::new(),
            selected_process: None,
            selected_family: None,
            process_sort: ProcessSort::Impact,
            process_tab: ProcessTab::Families,
            hide_windows_processes: true,
            show_only_suspects: false,
            show_only_closeable: false,
            confirm_kill: None,
            status_message: None,
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

        self.search_results = self.search_service.search(&self.search_query(), 500);
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

    fn show_sidebar(&mut self, ctx: &egui::Context, active_alerts: usize) {
        let search_status = self.search_service.status();
        let process_count = self.process_manager.state().summary.total_processes;

        egui::SidePanel::left("navigation")
            .resizable(false)
            .default_width(278.0)
            .frame(theme::sidebar_frame())
            .show(ctx, |ui| {
                theme::panel_card(theme::ORANGE).show(ui, |ui| {
                    ui.label(
                        RichText::new("WINDOWSHELP // CYBER OPS")
                            .text_style(egui::TextStyle::Small)
                            .monospace()
                            .color(theme::ORANGE),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("WindowsHELP")
                            .text_style(egui::TextStyle::Name("Hero".into()))
                            .color(theme::TEXT_PRIMARY),
                    );
                    ui.label(
                        RichText::new(
                            "Recherche locale, supervision système, gestion des processus et suivi thermique dans une seule interface.",
                        )
                        .size(13.0)
                        .color(theme::TEXT_SECONDARY),
                    );
                    ui.add_space(10.0);
                    ui.horizontal_wrapped(|ui| {
                        theme::status_chip(
                            ui,
                            format!("INDEX {}", search_status.indexed_entries),
                            theme::ORANGE,
                        );
                        theme::status_chip(ui, format!("PROC {}", process_count), theme::CYAN);
                        theme::status_chip(
                            ui,
                            format!("ALERT {}", active_alerts),
                            if active_alerts == 0 {
                                theme::ORANGE_SOFT
                            } else {
                                theme::RED
                            },
                        );
                    });
                });

                ui.add_space(10.0);
                theme::panel_card(theme::CYAN).show(ui, |ui| {
                    theme::section_header(ui, "Navigation", "Modules du poste de commande");
                    for (prefix, view) in [
                        ("01", View::Search),
                        ("02", View::Processes),
                        ("03", View::Monitor),
                        ("04", View::Temperatures),
                        ("05", View::Settings),
                    ] {
                        if nav_button(ui, prefix, view.label(), self.current_view == view) {
                            self.current_view = view;
                        }
                        ui.add_space(6.0);
                    }
                });

                ui.add_space(10.0);
                theme::panel_card(theme::ORANGE_SOFT).show(ui, |ui| {
                    theme::section_header(ui, "État de session", "Services actifs et cadence");
                    ui.horizontal_wrapped(|ui| {
                        theme::status_chip(
                            ui,
                            format!("ROOTS {}", search_status.watched_roots),
                            theme::CYAN,
                        );
                        theme::status_chip(
                            ui,
                            format!("PROC {} ms", self.settings.process_refresh_ms),
                            theme::ORANGE_SOFT,
                        );
                        theme::status_chip(
                            ui,
                            format!("MON {} ms", self.settings.monitor_refresh_ms),
                            theme::ORANGE_SOFT,
                        );
                    });
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "Racines surveillées : {}",
                            search_status.watched_roots
                        ))
                        .color(theme::TEXT_PRIMARY),
                    );
                    ui.label(
                        RichText::new(format!("Processus suivis : {process_count}"))
                        .color(theme::TEXT_PRIMARY),
                    );
                    if search_status.is_indexing {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Indexation en cours sur la machine.")
                                .color(theme::ORANGE),
                        );
                    }
                    if !search_status.snapshot_loaded && search_status.indexed_entries > 0 {
                        ui.label(
                            RichText::new("Le snapshot de recherche est en cours de chargement.")
                                .size(12.0)
                                .color(theme::TEXT_SECONDARY),
                        );
                    }
                    if let Some(error) = search_status.last_error.as_deref() {
                        ui.add_space(8.0);
                        theme::banner_frame(theme::RED).show(ui, |ui| {
                            ui.label(RichText::new(error).color(theme::TEXT_PRIMARY));
                        });
                    }
                });

                ui.add_space(10.0);

                theme::banner_frame(theme::BORDER).show(ui, |ui| {
                    ui.label(
                        RichText::new(
                            "Mode local uniquement : pas de cloud, pas de service distant, tout est traité sur la machine.",
                        )
                        .size(12.0)
                        .color(theme::TEXT_SECONDARY),
                    );
                });
            });
    }
    fn show_top_bar(&mut self, ctx: &egui::Context, active_alerts: usize) {
        egui::TopBottomPanel::top("top-bar")
            .frame(theme::topbar_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "MODULE // {}",
                                self.current_view.label().to_uppercase()
                            ))
                            .monospace()
                            .size(12.0)
                            .color(theme::ORANGE),
                        );
                        ui.label(
                            RichText::new(self.current_view.label())
                                .text_style(egui::TextStyle::Heading)
                                .color(theme::TEXT_PRIMARY),
                        );
                        ui.label(
                            RichText::new(self.current_view.description())
                                .size(13.0)
                                .color(theme::TEXT_SECONDARY),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        theme::status_chip(
                            ui,
                            format!("LOCAL {}", Local::now().format("%H:%M:%S")),
                            theme::CYAN,
                        );
                        theme::status_chip(
                            ui,
                            format!("MONITOR {} ms", self.settings.monitor_refresh_ms),
                            theme::ORANGE_SOFT,
                        );
                        theme::status_chip(
                            ui,
                            format!("PROC {} ms", self.settings.process_refresh_ms),
                            theme::CYAN,
                        );
                        theme::status_chip(
                            ui,
                            format!(
                                "ALERTES {}",
                                if active_alerts == 0 {
                                    "OK".to_owned()
                                } else {
                                    active_alerts.to_string()
                                }
                            ),
                            if active_alerts == 0 {
                                theme::ORANGE_SOFT
                            } else {
                                theme::RED
                            },
                        );
                    });
                });

                if let Some(message) = &self.status_message {
                    ui.add_space(10.0);
                    theme::banner_frame(theme::ORANGE).show(ui, |ui| {
                        ui.label(RichText::new(message).color(theme::TEXT_PRIMARY));
                    });
                }
            });
    }
    fn show_search_view(&mut self, ui: &mut egui::Ui) {
        let search_status = self.search_service.status();

        theme::section_header(
            ui,
            "Recherche",
            "Index local, filtres rapides et exploration des resultats.",
        );

        theme::panel_card(theme::ORANGE).show(ui, |ui| {
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
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.checkbox(
                        &mut self.include_hidden_results,
                        "Inclure les elements caches",
                    );
                });
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    if ui.button("Reindexer maintenant").clicked() {
                        self.search_service.reindex_now();
                        self.last_search_fingerprint.clear();
                    }
                });
            });

            self.refresh_search_results();
            ui.add_space(10.0);
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
        });

        ui.add_space(10.0);
        theme::panel_card(theme::RED_SOFT).show(ui, |ui| {
            theme::section_header(ui, "Resultats", "Fichiers et dossiers indexes");
            if self.search_results.is_empty()
                && self.search_text.trim().is_empty()
                && self.extension_filter.trim().is_empty()
                && self.min_size_filter.trim().is_empty()
                && self.max_size_filter.trim().is_empty()
                && self.modified_after_filter.trim().is_empty()
                && self.modified_before_filter.trim().is_empty()
                && !self.include_hidden_results
            {
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
                                    RichText::new(&result.entry.name).color(theme::TEXT_PRIMARY),
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
                                    if ui.small_button("Ouvrir").clicked() {
                                        if let Err(error) = open_path(&path) {
                                            self.status_message =
                                                Some(format!("Ouverture impossible : {error}"));
                                        }
                                    }
                                    if ui.small_button("Explorer").clicked() {
                                        if let Err(error) = reveal_in_explorer(&path) {
                                            self.status_message = Some(format!(
                                                "Affichage dans l'Explorateur impossible : {error}"
                                            ));
                                        }
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
        });
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
            ui.horizontal_wrapped(|ui| {
                metric_card(
                    ui,
                    "CPU",
                    format!("{:.1}%", snapshot.cpu_usage_percent),
                    "Charge systeme globale",
                    if snapshot.cpu_usage_percent >= 90.0 {
                        CardTone::Danger
                    } else if snapshot.cpu_usage_percent >= 75.0 {
                        CardTone::Warning
                    } else {
                        CardTone::Accent
                    },
                );
                metric_card(
                    ui,
                    "Memoire",
                    format!(
                        "{:.1}%",
                        percent(snapshot.used_memory_bytes, snapshot.total_memory_bytes)
                    ),
                    &format!(
                        "{}/{} utilises",
                        format_bytes(snapshot.used_memory_bytes),
                        format_bytes(snapshot.total_memory_bytes)
                    ),
                    CardTone::Info,
                );
                metric_card(
                    ui,
                    "Net In",
                    format!(
                        "{}/s",
                        format_bytes(snapshot.network_received_bytes_per_sec)
                    ),
                    "Trafic reseau entrant",
                    CardTone::Default,
                );
                metric_card(
                    ui,
                    "Net Out",
                    format!(
                        "{}/s",
                        format_bytes(snapshot.network_transmitted_bytes_per_sec)
                    ),
                    "Trafic reseau sortant",
                    CardTone::Default,
                );
            });

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
                ui.horizontal_wrapped(|ui| {
                    for reading in &snapshot.temperatures {
                        temperature_card(ui, reading);
                    }
                });
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
        ctx.request_repaint_after(Duration::from_millis(250));

        // Fond global simple et sûr.
        theme::paint_app_background(ctx);

        let monitor_state = self.monitor_service.snapshot_state();
        let active_alerts = Self::active_alerts(&monitor_state.events);

        self.show_sidebar(ctx, active_alerts.len());
        self.show_top_bar(ctx, active_alerts.len());

        egui::TopBottomPanel::bottom("active-alerts")
            .resizable(false)
            .default_height(82.0)
            .frame(theme::topbar_frame())
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new("ALERTES ACTIVES")
                            .monospace()
                            .size(12.0)
                            .color(theme::ORANGE),
                    );
                    if active_alerts.is_empty() {
                        theme::status_chip(ui, "AUCUNE", theme::CYAN);
                    } else {
                        theme::status_chip(ui, active_alerts.len().to_string(), theme::RED);
                    }
                });
                ui.add_space(6.0);
                if active_alerts.is_empty() {
                    ui.label(
                        RichText::new("Aucune alerte persistante sur le poste.")
                            .color(theme::TEXT_SECONDARY),
                    );
                } else {
                    for event in active_alerts.iter().take(3) {
                        ui.label(
                            RichText::new(format!("{} // {}", event.source_label, event.message))
                                .color(theme::RED),
                        );
                    }

                    if active_alerts.len() > 3 {
                        ui.label(
                            RichText::new(format!(
                                "+ {} autres alertes actives",
                                active_alerts.len() - 3
                            ))
                            .size(12.0)
                            .color(theme::TEXT_SECONDARY),
                        );
                    }
                }
            });

        egui::CentralPanel::default()
            .frame(theme::workspace_frame())
            .show(ctx, |ui| {
                // Important :
                // le décor "hacker" est maintenant peint dans la zone centrale,
                // donc il reste derrière les widgets.
                theme::paint_workspace_background(ui);

                theme::workspace_content_frame().show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| match self.current_view {
                            View::Search => self.show_search_view(ui),
                            View::Processes => self.show_processes_view(ui),
                            View::Monitor => self.show_monitor_view(ui),
                            View::Temperatures => self.show_temperatures_view(ui),
                            View::Settings => self.show_settings_view(ui),
                        });
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

fn render_processes_view(app: &mut WindowsHelpApp, ui: &mut egui::Ui) {
    theme::section_header(
        ui,
        "Processus",
        "Diagnostic guide des applis qui consomment maintenant et dans la duree.",
    );

    let process_state = app.process_manager.state();
    let monitor_state = app.monitor_service.snapshot_state();
    let alerted_pids = process_alerted_pids(&monitor_state.events);
    let bottleneck = monitor_state
        .latest
        .as_ref()
        .map(|snapshot| {
            diagnostic_bottleneck_from_metrics(
                snapshot.cpu_usage_percent,
                percent(snapshot.used_memory_bytes, snapshot.total_memory_bytes),
            )
        })
        .unwrap_or(process_state.summary.bottleneck);

    if let Some(error) = process_state.last_error.as_deref() {
        theme::panel_card(theme::RED).show(ui, |ui| {
            ui.label(RichText::new(error).color(theme::TEXT_PRIMARY));
        });
        ui.add_space(10.0);
    }

    ui.horizontal_wrapped(|ui| {
        metric_card(
            ui,
            "Processus",
            process_state.summary.total_processes.to_string(),
            &format!(
                "{} familles detectees",
                process_state.summary.total_families
            ),
            CardTone::Info,
        );
        metric_card(
            ui,
            "Top impact",
            process_state
                .summary
                .top_impact_name
                .clone()
                .unwrap_or_else(|| "-".into()),
            &format!("Bottleneck courant : {}", bottleneck.label()),
            diagnostic_bottleneck_tone(bottleneck),
        );
        metric_card(
            ui,
            "Top memoire",
            process_state
                .summary
                .top_memory_name
                .clone()
                .unwrap_or_else(|| "-".into()),
            &format!(
                "{:.1}% RAM systeme",
                process_state.summary.current_memory_percent
            ),
            CardTone::Default,
        );
        metric_card(
            ui,
            "Fermeture prudente",
            process_state.summary.closeable_candidates.to_string(),
            "Candidats fermables proprement",
            if process_state.summary.closeable_candidates > 0 {
                CardTone::Accent
            } else {
                CardTone::Info
            },
        );
    });

    ui.add_space(10.0);
    render_process_recommendations(ui, &process_state, bottleneck, &alerted_pids);

    let mut visible_rows = process_state
        .rows
        .iter()
        .filter(|row| {
            process_row_matches_filters(
                row,
                &app.process_filter,
                app.hide_windows_processes,
                app.show_only_suspects,
                app.show_only_closeable,
                &alerted_pids,
            )
        })
        .collect::<Vec<_>>();
    sort_process_rows(&mut visible_rows, app.process_sort);

    let visible_family_ids = visible_rows
        .iter()
        .map(|row| row.family_id.clone())
        .collect::<HashSet<_>>();
    let mut visible_families = process_state
        .families
        .iter()
        .filter(|family| visible_family_ids.contains(&family.id))
        .collect::<Vec<_>>();
    sort_process_families(&mut visible_families, app.process_sort);

    app.sync_process_selection(&visible_families, &visible_rows);

    ui.add_space(10.0);
    render_process_filters(app, ui);

    ui.add_space(10.0);
    render_process_explorer(app, ui, &visible_rows, &visible_families, &alerted_pids);
}

fn render_process_recommendations(
    ui: &mut egui::Ui,
    process_state: &ProcessState,
    bottleneck: ProcessBottleneck,
    alerted_pids: &HashSet<u32>,
) {
    theme::panel_card(theme::ORANGE).show(ui, |ui| {
        theme::section_header(
            ui,
            "Pourquoi le PC rame ?",
            "Lecture rapide du moment avant de descendre dans la liste.",
        );
        ui.horizontal_wrapped(|ui| {
            theme::status_chip(
                ui,
                format!("CPU {:.1}%", process_state.summary.current_cpu_percent),
                theme::ORANGE,
            );
            theme::status_chip(
                ui,
                format!("RAM {:.1}%", process_state.summary.current_memory_percent),
                theme::CYAN,
            );
            theme::status_chip(
                ui,
                format!("FOCUS {}", bottleneck.label()),
                bottleneck_color(bottleneck),
            );
        });
        ui.add_space(8.0);

        if process_state.recommendations.is_empty() {
            ui.label(
                RichText::new("Aucune recommandation process forte pour le moment.")
                    .color(theme::TEXT_SECONDARY),
            );
        } else {
            for recommendation in process_state.recommendations.iter().take(3) {
                theme::banner_frame(recommendation_color(recommendation)).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        theme::status_chip(
                            ui,
                            format!("IMPACT {}", recommendation.impact_score),
                            recommendation_color(recommendation),
                        );
                        theme::status_chip(
                            ui,
                            recommendation.safety.label(),
                            safety_color(recommendation.safety),
                        );
                        theme::status_chip(
                            ui,
                            recommendation.suggested_action.label(),
                            action_color(recommendation.suggested_action),
                        );
                        if recommendation
                            .target
                            .as_ref()
                            .map(|key| alerted_pids.contains(&key.pid))
                            .unwrap_or(false)
                        {
                            theme::status_chip(ui, "Alerte active", theme::RED);
                        }
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new(&recommendation.title).color(theme::TEXT_PRIMARY));
                    ui.label(
                        RichText::new(&recommendation.details)
                            .size(12.0)
                            .color(theme::TEXT_SECONDARY),
                    );
                });
                ui.add_space(8.0);
            }
        }
    });
}

fn render_process_filters(app: &mut WindowsHelpApp, ui: &mut egui::Ui) {
    theme::panel_card(theme::ORANGE_SOFT).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("FILTRE")
                        .monospace()
                        .color(theme::TEXT_SECONDARY),
                );
                ui.add_sized(
                    [320.0, 34.0],
                    egui::TextEdit::singleline(&mut app.process_filter),
                );
            });

            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("VUE")
                        .monospace()
                        .color(theme::TEXT_SECONDARY),
                );
                ui.horizontal_wrapped(|ui| {
                    for tab in [ProcessTab::Families, ProcessTab::Instances] {
                        if ui
                            .selectable_label(app.process_tab == tab, tab.label())
                            .clicked()
                        {
                            app.process_tab = tab;
                        }
                    }
                });
            });

            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("TRI")
                        .monospace()
                        .color(theme::TEXT_SECONDARY),
                );
                egui::ComboBox::from_id_salt("process-sort")
                    .selected_text(app.process_sort.label())
                    .show_ui(ui, |ui| {
                        for sort in ProcessSort::all() {
                            ui.selectable_value(&mut app.process_sort, sort, sort.label());
                        }
                    });
            });

            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("FILTRES")
                        .monospace()
                        .color(theme::TEXT_SECONDARY),
                );
                ui.checkbox(&mut app.hide_windows_processes, "Masquer Windows critique");
                ui.checkbox(&mut app.show_only_suspects, "Suspects seulement");
                ui.checkbox(&mut app.show_only_closeable, "Fermeture prudente seulement");
            });
        });
    });
}

fn render_process_explorer(
    app: &mut WindowsHelpApp,
    ui: &mut egui::Ui,
    visible_rows: &[&ProcessRow],
    visible_families: &[&ProcessFamily],
    _alerted_pids: &HashSet<u32>,
) {
    ui.columns(2, |columns| {
        theme::panel_card(theme::RED_SOFT).show(&mut columns[0], |ui| {
            theme::section_header(
                ui,
                app.process_tab.label(),
                "Liste principale pour explorer par famille ou par instance.",
            );

            if app.process_tab == ProcessTab::Families {
                render_process_family_list(app, ui, visible_rows, visible_families);
            } else {
                render_process_instance_list(app, ui, visible_rows);
            }
        });

        theme::panel_card(theme::CYAN).show(&mut columns[1], |ui| {
            theme::section_header(
                ui,
                "Detail du processus",
                "Contexte, raisons et actions prudentes sur la selection courante.",
            );
            render_process_detail_panel(app, ui, visible_rows);
        });
    });
}

fn render_process_family_list(
    app: &mut WindowsHelpApp,
    ui: &mut egui::Ui,
    visible_rows: &[&ProcessRow],
    visible_families: &[&ProcessFamily],
) {
    if visible_families.is_empty() {
        ui.label(
            RichText::new("Aucune famille ne correspond aux filtres.").color(theme::TEXT_SECONDARY),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, 94.0, visible_families.len(), |ui, row_range| {
            for index in row_range {
                let family = visible_families[index];
                let selected = app.selected_family.as_deref() == Some(family.id.as_str());
                let response = theme::banner_frame(if selected {
                    theme::ORANGE
                } else {
                    recommendation_family_color(family)
                })
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        theme::status_chip(
                            ui,
                            format!("IMPACT {}", family.max_impact_score),
                            recommendation_family_color(family),
                        );
                        theme::status_chip(
                            ui,
                            format!("{} proc", family.instance_count),
                            theme::CYAN,
                        );
                        theme::status_chip(ui, family.safety.label(), safety_color(family.safety));
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new(&family.label).color(theme::TEXT_PRIMARY));
                    ui.label(
                        RichText::new(format!(
                            "CPU moyen {:.1}% // memoire {} // {}",
                            family.cpu_avg_10s_total,
                            format_bytes(family.memory_bytes_total),
                            family.primary_reason
                        ))
                        .size(12.0)
                        .color(theme::TEXT_SECONDARY),
                    );
                })
                .response;

                if response.clicked() {
                    app.selected_family = Some(family.id.clone());
                    app.selected_process = visible_rows
                        .iter()
                        .copied()
                        .find(|row| row.family_id == family.id)
                        .map(|row| row.key.clone());
                }
                ui.add_space(8.0);
            }
        });
}

fn render_process_instance_list(
    app: &mut WindowsHelpApp,
    ui: &mut egui::Ui,
    visible_rows: &[&ProcessRow],
) {
    if visible_rows.is_empty() {
        ui.label(
            RichText::new("Aucune instance ne correspond aux filtres.")
                .color(theme::TEXT_SECONDARY),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, 88.0, visible_rows.len(), |ui, row_range| {
            for index in row_range {
                let row = visible_rows[index];
                let selected = app
                    .selected_process
                    .as_ref()
                    .map(|key| key == &row.key)
                    .unwrap_or(false);
                let response = theme::banner_frame(if selected {
                    theme::CYAN
                } else {
                    recommendation_row_color(row)
                })
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        theme::status_chip(ui, format!("PID {}", row.key.pid), theme::CYAN);
                        theme::status_chip(
                            ui,
                            format!("IMPACT {}", row.insight.impact_score),
                            recommendation_row_color(row),
                        );
                        theme::status_chip(
                            ui,
                            row.insight.safety.label(),
                            safety_color(row.insight.safety),
                        );
                    });
                    ui.add_space(4.0);
                    ui.label(RichText::new(&row.name).color(theme::TEXT_PRIMARY));
                    ui.label(
                        RichText::new(format!(
                            "CPU now {:.1}% // CPU moyen {:.1}% // memoire {}",
                            row.cpu_now,
                            row.insight.cpu_avg_10s,
                            format_bytes(row.memory_bytes)
                        ))
                        .size(12.0)
                        .color(theme::TEXT_SECONDARY),
                    );
                })
                .response;

                if response.clicked() {
                    app.selected_family = Some(row.family_id.clone());
                    app.selected_process = Some(row.key.clone());
                }
                ui.add_space(8.0);
            }
        });
}

fn render_process_detail_panel(
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

    ui.horizontal_wrapped(|ui| {
        theme::status_chip(ui, format!("PID {}", row.key.pid), theme::CYAN);
        theme::status_chip(
            ui,
            row.insight.safety.label(),
            safety_color(row.insight.safety),
        );
        theme::status_chip(
            ui,
            row.insight.suggested_action.label(),
            action_color(row.insight.suggested_action),
        );
        theme::status_chip(ui, row.insight.trend.label(), recommendation_row_color(row));
    });
    ui.add_space(8.0);
    ui.label(
        RichText::new(&row.name)
            .size(22.0)
            .color(theme::TEXT_PRIMARY),
    );
    ui.label(
        RichText::new(format!(
            "Etat {} // priorite {} // runtime {}",
            row.status,
            row.priority.label(),
            format_duration(row.run_time_secs)
        ))
        .size(12.0)
        .color(theme::TEXT_SECONDARY),
    );
    ui.add_space(10.0);

    ui.horizontal_wrapped(|ui| {
        metric_card(
            ui,
            "Impact",
            row.insight.impact_score.to_string(),
            "Score de pression globale",
            process_tone(row.insight.impact_score),
        );
        metric_card(
            ui,
            "CPU",
            format!("{:.1}% / {:.1}%", row.cpu_now, row.insight.cpu_avg_10s),
            "Instantane / moyenne 10 s",
            process_tone(row.insight.impact_score),
        );
        metric_card(
            ui,
            "Memoire",
            format_bytes(row.memory_bytes),
            &format!("{:.1}% de la RAM systeme", row.insight.memory_percent),
            CardTone::Info,
        );
        metric_card(
            ui,
            "Disque",
            format!("{}/s", format_bytes(row.insight.disk_io_bytes_per_sec)),
            "Debit observe sur la fenetre courte",
            CardTone::Default,
        );
    });

    ui.add_space(10.0);
    theme::banner_frame(theme::BORDER).show(ui, |ui| {
        ui.label(
            RichText::new("Pourquoi il remonte ici")
                .monospace()
                .color(theme::TEXT_SECONDARY),
        );
        ui.add_space(6.0);
        for reason in &row.insight.reasons {
            ui.label(RichText::new(format!("- {reason}")).color(theme::TEXT_PRIMARY));
        }
    });

    ui.add_space(10.0);
    theme::banner_frame(theme::CYAN).show(ui, |ui| {
        ui.label(
            RichText::new("Contexte")
                .monospace()
                .color(theme::TEXT_SECONDARY),
        );
        ui.add_space(6.0);
        ui.label(RichText::new(format!("Famille : {}", row.family_id)).color(theme::TEXT_PRIMARY));
        ui.label(
            RichText::new(format!("Parent PID : {}", row.parent_pid.unwrap_or(0)))
                .color(theme::TEXT_PRIMARY),
        );
        ui.label(RichText::new(format!("Threads : {}", row.threads)).color(theme::TEXT_PRIMARY));
        ui.label(
            RichText::new(format!(
                "Demarre le : {}",
                format_timestamp(row.key.started_at.map(|value| value as i64))
            ))
            .color(theme::TEXT_PRIMARY),
        );
        ui.label(
            RichText::new(
                row.path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "Chemin indisponible".into()),
            )
            .size(12.0)
            .color(theme::TEXT_SECONDARY),
        );
    });

    ui.add_space(10.0);
    ui.horizontal_wrapped(|ui| {
        if (row.has_visible_window
            || row.insight.suggested_action == SuggestedAction::CloseGracefully)
            && ui.button("Fermer proprement").clicked()
        {
            app.apply_process_action(&row.key, &row.name, ProcessAction::CloseGracefully);
        }

        if ui.button("Priorite basse").clicked() {
            app.apply_process_action(
                &row.key,
                &row.name,
                ProcessAction::SetPriority(PriorityClass::BelowNormal),
            );
        }

        if row.priority != PriorityClass::Normal && ui.button("Priorite normale").clicked() {
            app.apply_process_action(
                &row.key,
                &row.name,
                ProcessAction::SetPriority(PriorityClass::Normal),
            );
        }

        if let Some(path) = row.path.as_ref()
            && ui.button("Ouvrir le dossier").clicked()
        {
            if let Err(error) = reveal_in_explorer(path) {
                app.status_message =
                    Some(format!("Affichage dans l'Explorateur impossible : {error}"));
            }
        }

        ui.menu_button("Danger", |ui| {
            if ui.button("Terminer de force").clicked() {
                app.confirm_kill = Some((row.key.clone(), row.name.clone()));
                ui.close();
            }
        });
    });

    if !family_members.is_empty() {
        ui.add_space(12.0);
        theme::section_header(
            ui,
            "Autres instances de la famille",
            "Pratique pour naviguer dans les sous-processus du meme groupe.",
        );
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
                        "{} // PID {} // impact {} // memoire {}",
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

fn sort_process_rows(rows: &mut Vec<&ProcessRow>, sort: ProcessSort) {
    rows.sort_by(|left, right| match sort {
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
    });
}

fn sort_process_families(families: &mut Vec<&ProcessFamily>, sort: ProcessSort) {
    families.sort_by(|left, right| match sort {
        ProcessSort::Impact => right
            .max_impact_score
            .cmp(&left.max_impact_score)
            .then_with(|| {
                right
                    .cpu_avg_10s_total
                    .partial_cmp(&left.cpu_avg_10s_total)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
        ProcessSort::CpuNow => right
            .cpu_now_total
            .partial_cmp(&left.cpu_now_total)
            .unwrap_or(std::cmp::Ordering::Equal),
        ProcessSort::CpuAverage => right
            .cpu_avg_10s_total
            .partial_cmp(&left.cpu_avg_10s_total)
            .unwrap_or(std::cmp::Ordering::Equal),
        ProcessSort::Memory => right.memory_bytes_total.cmp(&left.memory_bytes_total),
        ProcessSort::Name => left.label.cmp(&right.label),
    });
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

fn diagnostic_bottleneck_from_metrics(cpu_percent: f32, memory_percent: f32) -> ProcessBottleneck {
    if cpu_percent < 65.0 && memory_percent < 70.0 {
        ProcessBottleneck::Quiet
    } else if cpu_percent >= 80.0 && memory_percent >= 80.0 {
        ProcessBottleneck::Mixed
    } else if cpu_percent >= memory_percent + 8.0 {
        ProcessBottleneck::Cpu
    } else if memory_percent >= cpu_percent + 5.0 {
        ProcessBottleneck::Memory
    } else {
        ProcessBottleneck::Mixed
    }
}

fn process_tone(impact_score: u8) -> CardTone {
    if impact_score >= 75 {
        CardTone::Danger
    } else if impact_score >= 50 {
        CardTone::Warning
    } else if impact_score >= 25 {
        CardTone::Accent
    } else {
        CardTone::Info
    }
}

fn diagnostic_bottleneck_tone(bottleneck: ProcessBottleneck) -> CardTone {
    match bottleneck {
        ProcessBottleneck::Quiet => CardTone::Info,
        ProcessBottleneck::Cpu | ProcessBottleneck::Memory => CardTone::Warning,
        ProcessBottleneck::Mixed => CardTone::Danger,
    }
}

fn recommendation_row_color(row: &ProcessRow) -> Color32 {
    impact_color(row.insight.impact_score)
}

fn recommendation_family_color(family: &ProcessFamily) -> Color32 {
    impact_color(family.max_impact_score)
}

fn recommendation_color(recommendation: &ProcessRecommendation) -> Color32 {
    impact_color(recommendation.impact_score)
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

fn action_color(action: SuggestedAction) -> Color32 {
    match action {
        SuggestedAction::None => theme::TEXT_SECONDARY,
        SuggestedAction::CloseGracefully => theme::CYAN,
        SuggestedAction::LowerPriority => theme::ORANGE,
        SuggestedAction::ReviewOnly => theme::WARNING,
    }
}

fn bottleneck_color(bottleneck: ProcessBottleneck) -> Color32 {
    match bottleneck {
        ProcessBottleneck::Quiet => theme::CYAN,
        ProcessBottleneck::Cpu => theme::ORANGE,
        ProcessBottleneck::Memory => theme::WARNING,
        ProcessBottleneck::Mixed => theme::RED,
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

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1500.0, 920.0])
            .with_min_inner_size([1200.0, 760.0])
            .with_transparent(false),
        ..Default::default()
    };

    eframe::run_native(
        "WindowsHELP",
        native_options,
        Box::new(move |cc| {
            theme::apply_hacker_theme(&cc.egui_ctx);
            Ok(Box::new(app_slot.take().expect(
                "WindowsHELP application should only be created once",
            )))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn nav_button(ui: &mut egui::Ui, prefix: &str, label: &str, active: bool) -> bool {
    let fill = if active {
        theme::BG_SURFACE
    } else {
        theme::BG_PANEL_ALT
    };
    let stroke = if active {
        theme::ORANGE
    } else {
        theme::BORDER.gamma_multiply(0.9)
    };
    let text_color = if active {
        theme::TEXT_PRIMARY
    } else {
        theme::TEXT_SECONDARY
    };

    let response = ui.add_sized(
        [ui.available_width(), 44.0],
        egui::Button::new(
            RichText::new(format!("{prefix}   {label}"))
                .monospace()
                .size(14.0)
                .color(text_color),
        )
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(16)),
    );

    if active {
        let indicator = egui::Rect::from_min_size(
            response.rect.min + egui::vec2(6.0, 8.0),
            egui::vec2(4.0, response.rect.height() - 16.0),
        );

        ui.painter()
            .rect_filled(indicator, CornerRadius::same(255), theme::ORANGE);
    }

    response.clicked()
}

fn metric_card(ui: &mut egui::Ui, title: &str, value: String, subtitle: &str, tone: CardTone) {
    let accent = match tone {
        CardTone::Default => theme::ORANGE_SOFT,
        CardTone::Accent => theme::ORANGE,
        CardTone::Warning => theme::WARNING,
        CardTone::Danger => theme::RED,
        CardTone::Info => theme::CYAN,
    };

    theme::metric_card_variant(tone).show(ui, |ui| {
        ui.set_min_size(Vec2::new(220.0, 104.0));
        ui.label(
            RichText::new(title)
                .monospace()
                .size(12.0)
                .color(theme::TEXT_SECONDARY),
        );
        ui.add_space(2.0);
        ui.label(
            RichText::new(value)
                .text_style(egui::TextStyle::Name("Metric".into()))
                .color(theme::TEXT_PRIMARY),
        );
        ui.add_space(2.0);
        ui.label(
            RichText::new(subtitle)
                .size(12.0)
                .color(theme::TEXT_SECONDARY),
        );

        let width = ui.available_width().max(42.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), egui::Sense::hover());
        let painter = ui.painter_at(rect);

        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - 1.0),
            egui::vec2(rect.width() * 0.42, 2.0),
        );

        painter.rect_filled(bar, CornerRadius::same(255), accent.gamma_multiply(0.95));
    });
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

    #[test]
    fn view_descriptions_match_expected_modules() {
        let cases = [
            (View::Search, "Recherche", "index local"),
            (View::Processes, "Processus", "processus Windows"),
            (View::Monitor, "Surveillance", "temps réel"),
            (View::Temperatures, "Temperatures", "thermique"),
            (View::Settings, "Paramètres", "Configuration"),
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
