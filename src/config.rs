use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use chrono::Utc;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::monitor::AlertRule;
use crate::platform_windows::list_fixed_drive_roots;
use crate::thermal::ThermalSettings;

const MIN_REFRESH_INTERVAL_MS: u64 = 250;
const MAX_REFRESH_INTERVAL_MS: u64 = 60_000;
const MIN_ALERT_THRESHOLD_PERCENT: f32 = 1.0;
const MAX_ALERT_THRESHOLD_PERCENT: f32 = 100.0;
const MIN_ALERT_SUSTAIN_SECONDS: u64 = 1;
const MAX_ALERT_SUSTAIN_SECONDS: u64 = 3_600;
const MAX_SCAN_CONCURRENCY: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PerformanceMode {
    Economy,
    #[default]
    Balanced,
    RealTime,
}

impl PerformanceMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Economy => "Économie",
            Self::Balanced => "Équilibré",
            Self::RealTime => "Temps réel",
        }
    }

    pub fn all() -> [Self; 3] {
        [Self::Economy, Self::Balanced, Self::RealTime]
    }

    pub fn profile(self) -> PerformanceProfile {
        match self {
            Self::Economy => PerformanceProfile {
                process_refresh_ms: 3_000,
                monitor_refresh_ms: 3_000,
                thermal_refresh_ms: 5_000,
                ui_idle_ms: 1_000,
                history_capacity: 120,
            },
            Self::Balanced => PerformanceProfile {
                process_refresh_ms: 1_500,
                monitor_refresh_ms: 2_000,
                thermal_refresh_ms: 3_000,
                ui_idle_ms: 500,
                history_capacity: 180,
            },
            Self::RealTime => PerformanceProfile {
                process_refresh_ms: 750,
                monitor_refresh_ms: 1_000,
                thermal_refresh_ms: 1_500,
                ui_idle_ms: 250,
                history_capacity: 300,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerformanceProfile {
    pub process_refresh_ms: u64,
    pub monitor_refresh_ms: u64,
    pub thermal_refresh_ms: u64,
    pub ui_idle_ms: u64,
    pub history_capacity: usize,
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub settings_path: PathBuf,
    pub index_db_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexConfig {
    pub roots: Vec<PathBuf>,
    pub exclusions: Vec<String>,
    pub include_hidden: bool,
    pub include_system: bool,
    pub scan_concurrency: usize,
    pub db_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub index: IndexConfig,
    #[serde(default)]
    pub performance_mode: PerformanceMode,
    pub process_refresh_ms: u64,
    pub monitor_refresh_ms: u64,
    pub alert_rules: Vec<AlertRule>,
    #[serde(default)]
    pub thermal: ThermalSettings,
    pub saved_at_utc: i64,
}

pub fn app_paths() -> anyhow::Result<AppPaths> {
    let dirs = ProjectDirs::from("com", "nodig", "WindowsHELP").ok_or_else(|| {
        anyhow!("impossible de résoudre les dossiers de l'application WindowsHELP")
    })?;
    let data_dir = dirs.data_local_dir().to_path_buf();
    let config_dir = dirs.config_local_dir().to_path_buf();
    let settings_path = config_dir.join("settings.json");
    let index_db_path = data_dir.join("index.db");
    Ok(AppPaths {
        data_dir,
        config_dir,
        settings_path,
        index_db_path,
    })
}

impl Settings {
    pub fn default_for_current_machine() -> anyhow::Result<Self> {
        let paths = app_paths()?;
        let performance_mode = PerformanceMode::Balanced;
        let profile = performance_mode.profile();
        let mut settings = Self {
            index: IndexConfig {
                roots: list_fixed_drive_roots(),
                exclusions: vec![
                    "$Recycle.Bin".to_owned(),
                    "System Volume Information".to_owned(),
                    "pagefile.sys".to_owned(),
                    "hiberfil.sys".to_owned(),
                ],
                include_hidden: false,
                include_system: false,
                scan_concurrency: 4,
                db_path: paths.index_db_path,
            },
            performance_mode,
            process_refresh_ms: profile.process_refresh_ms,
            monitor_refresh_ms: profile.monitor_refresh_ms,
            alert_rules: AlertRule::default_rules(),
            thermal: ThermalSettings::default(),
            saved_at_utc: Utc::now().timestamp(),
        };
        settings.sanitize();
        Ok(settings)
    }

    pub fn apply_performance_profile(&mut self) {
        let profile = self.performance_mode.profile();
        self.process_refresh_ms = profile.process_refresh_ms;
        self.monitor_refresh_ms = profile.monitor_refresh_ms;
    }

    pub fn sanitize(&mut self) {
        self.index.scan_concurrency = self.index.scan_concurrency.clamp(1, MAX_SCAN_CONCURRENCY);
        self.process_refresh_ms = sanitize_refresh_interval_ms(self.process_refresh_ms);
        self.monitor_refresh_ms = sanitize_refresh_interval_ms(self.monitor_refresh_ms);
        self.thermal.sanitize();
        for rule in &mut self.alert_rules {
            sanitize_alert_rule(rule);
        }
    }
}

pub fn load_or_create_settings() -> anyhow::Result<Settings> {
    let paths = app_paths()?;
    fs::create_dir_all(&paths.data_dir).context("échec de la création du dossier de données")?;
    fs::create_dir_all(&paths.config_dir)
        .context("échec de la création du dossier de configuration")?;

    if paths.settings_path.exists() {
        let content = fs::read_to_string(&paths.settings_path)
            .with_context(|| format!("échec de la lecture de {}", paths.settings_path.display()))?;
        let parsed: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("échec de l'analyse de {}", paths.settings_path.display()))?;
        let missing_performance_mode = parsed.get("performance_mode").is_none();
        let mut settings: Settings = serde_json::from_value(parsed)
            .with_context(|| format!("échec de l'analyse de {}", paths.settings_path.display()))?;
        settings.index.db_path = paths.index_db_path;
        if missing_performance_mode {
            settings.performance_mode = PerformanceMode::Balanced;
            settings.apply_performance_profile();
        }
        refresh_alert_rule_labels(&mut settings.alert_rules);
        settings.sanitize();
        return Ok(settings);
    }

    let settings = Settings::default_for_current_machine()?;
    save_settings(&settings)?;
    Ok(settings)
}

pub fn save_settings(settings: &Settings) -> anyhow::Result<()> {
    let paths = app_paths()?;
    fs::create_dir_all(&paths.data_dir).context("échec de la création du dossier de données")?;
    fs::create_dir_all(&paths.config_dir)
        .context("échec de la création du dossier de configuration")?;
    let mut settings = settings.clone();
    settings.sanitize();
    let content = serde_json::to_string_pretty(&settings)
        .context("échec de la sérialisation des paramètres")?;
    let mut temp_file = tempfile::Builder::new()
        .prefix("settings.")
        .suffix(".tmp")
        .tempfile_in(&paths.config_dir)
        .with_context(|| {
            format!(
                "échec de la création du fichier temporaire dans {}",
                paths.config_dir.display()
            )
        })?;
    temp_file
        .write_all(content.as_bytes())
        .with_context(|| format!("échec de l'écriture de {}", paths.settings_path.display()))?;
    temp_file.as_file_mut().sync_all().with_context(|| {
        format!(
            "échec de la synchronisation de {}",
            paths.settings_path.display()
        )
    })?;
    temp_file
        .persist(&paths.settings_path)
        .map_err(|error| error.error)
        .with_context(|| format!("échec du remplacement de {}", paths.settings_path.display()))?;
    Ok(())
}

fn refresh_alert_rule_labels(rules: &mut [AlertRule]) {
    for rule in rules {
        rule.refresh_label();
    }
}

fn sanitize_refresh_interval_ms(value: u64) -> u64 {
    value.clamp(MIN_REFRESH_INTERVAL_MS, MAX_REFRESH_INTERVAL_MS)
}

fn sanitize_alert_rule(rule: &mut AlertRule) {
    rule.threshold_percent = sanitize_alert_threshold(rule.threshold_percent);
    rule.sustain_seconds = rule
        .sustain_seconds
        .clamp(MIN_ALERT_SUSTAIN_SECONDS, MAX_ALERT_SUSTAIN_SECONDS);
    rule.refresh_label();
}

fn sanitize_alert_threshold(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(MIN_ALERT_THRESHOLD_PERCENT, MAX_ALERT_THRESHOLD_PERCENT)
    } else {
        MAX_ALERT_THRESHOLD_PERCENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_json_loads_with_default_thermal_settings() {
        let json = r#"
        {
          "index": {
            "roots": ["C:\\\\"],
            "exclusions": [],
            "include_hidden": false,
            "include_system": false,
            "scan_concurrency": 4,
            "db_path": "C:\\\\Temp\\\\index.db"
          },
          "process_refresh_ms": 1000,
          "monitor_refresh_ms": 1000,
          "alert_rules": [],
          "saved_at_utc": 0
        }
        "#;

        let settings: Settings = serde_json::from_str(json).expect("legacy settings should load");
        assert!(settings.thermal.enabled);
        assert!(settings.thermal.notifications_enabled);
        assert!(!settings.thermal.auto_cooling_enabled);
    }

    #[test]
    fn old_settings_json_defaults_to_balanced_performance_mode() {
        let json = r#"
        {
          "index": {
            "roots": ["C:\\\\"],
            "exclusions": [],
            "include_hidden": false,
            "include_system": false,
            "scan_concurrency": 4,
            "db_path": "C:\\\\Temp\\\\index.db"
          },
          "process_refresh_ms": 1000,
          "monitor_refresh_ms": 1000,
          "alert_rules": [],
          "saved_at_utc": 0
        }
        "#;

        let mut settings: Settings =
            serde_json::from_str(json).expect("legacy settings should load");
        assert_eq!(settings.performance_mode, PerformanceMode::Balanced);

        settings.apply_performance_profile();
        let profile = PerformanceMode::Balanced.profile();
        assert_eq!(settings.process_refresh_ms, profile.process_refresh_ms);
        assert_eq!(settings.monitor_refresh_ms, profile.monitor_refresh_ms);
    }

    #[test]
    fn performance_profiles_match_expected_cadences() {
        let economy = PerformanceMode::Economy.profile();
        assert_eq!(economy.process_refresh_ms, 3_000);
        assert_eq!(economy.monitor_refresh_ms, 3_000);
        assert_eq!(economy.thermal_refresh_ms, 5_000);
        assert_eq!(economy.ui_idle_ms, 1_000);
        assert_eq!(economy.history_capacity, 120);

        let balanced = PerformanceMode::Balanced.profile();
        assert_eq!(balanced.process_refresh_ms, 1_500);
        assert_eq!(balanced.monitor_refresh_ms, 2_000);
        assert_eq!(balanced.thermal_refresh_ms, 3_000);
        assert_eq!(balanced.ui_idle_ms, 500);
        assert_eq!(balanced.history_capacity, 180);

        let real_time = PerformanceMode::RealTime.profile();
        assert_eq!(real_time.process_refresh_ms, 750);
        assert_eq!(real_time.monitor_refresh_ms, 1_000);
        assert_eq!(real_time.thermal_refresh_ms, 1_500);
        assert_eq!(real_time.ui_idle_ms, 250);
        assert_eq!(real_time.history_capacity, 300);
    }

    #[test]
    fn settings_sanitize_clamps_runtime_and_alert_values() -> anyhow::Result<()> {
        let mut settings = Settings::default_for_current_machine()?;
        settings.index.scan_concurrency = usize::MAX;
        settings.process_refresh_ms = 1;
        settings.monitor_refresh_ms = u64::MAX;
        settings.alert_rules[0].threshold_percent = -25.0;
        settings.alert_rules[0].sustain_seconds = 0;
        settings.alert_rules[1].threshold_percent = f32::NAN;
        settings.alert_rules[1].sustain_seconds = u64::MAX;

        settings.sanitize();

        assert_eq!(settings.index.scan_concurrency, MAX_SCAN_CONCURRENCY);
        assert_eq!(settings.process_refresh_ms, MIN_REFRESH_INTERVAL_MS);
        assert_eq!(settings.monitor_refresh_ms, MAX_REFRESH_INTERVAL_MS);
        assert_eq!(
            settings.alert_rules[0].threshold_percent,
            MIN_ALERT_THRESHOLD_PERCENT
        );
        assert_eq!(
            settings.alert_rules[0].sustain_seconds,
            MIN_ALERT_SUSTAIN_SECONDS
        );
        assert_eq!(
            settings.alert_rules[1].threshold_percent,
            MAX_ALERT_THRESHOLD_PERCENT
        );
        assert_eq!(
            settings.alert_rules[1].sustain_seconds,
            MAX_ALERT_SUSTAIN_SECONDS
        );
        Ok(())
    }
}
