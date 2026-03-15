 use std::fs;
use std::path::PathBuf;

use anyhow::{Context, anyhow};
use chrono::Utc;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::monitor::AlertRule;
use crate::platform_windows::list_fixed_drive_roots;
use crate::thermal::ThermalSettings;

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
        Ok(Self {
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
            process_refresh_ms: 1_000,
            monitor_refresh_ms: 1_000,
            alert_rules: AlertRule::default_rules(),
            thermal: ThermalSettings::default(),
            saved_at_utc: Utc::now().timestamp(),
        })
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
        let mut settings: Settings = serde_json::from_str(&content)
            .with_context(|| format!("échec de l'analyse de {}", paths.settings_path.display()))?;
        settings.index.db_path = paths.index_db_path;
        refresh_alert_rule_labels(&mut settings.alert_rules);
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
    let content = serde_json::to_string_pretty(settings)
        .context("échec de la sérialisation des paramètres")?;
    fs::write(&paths.settings_path, content)
        .with_context(|| format!("échec de l'écriture de {}", paths.settings_path.display()))?;
    Ok(())
}

fn refresh_alert_rule_labels(rules: &mut [AlertRule]) {
    for rule in rules {
        rule.refresh_label();
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
        assert!(settings.thermal.auto_cooling_enabled);
    }
}
