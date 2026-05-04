use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sysinfo::Components;

const MAX_ACER_PAYLOAD_BYTES: usize = 16 * 1024;
const MAX_ACER_RESPONSE_BYTES: usize = 256 * 1024;
const MIN_TEMPERATURE_CELSIUS: f32 = 0.0;
const MAX_TEMPERATURE_CELSIUS: f32 = 150.0;
const MAX_FAN_SPEED_RPM: u32 = 50_000;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalState {
    #[default]
    Normal,
    Warning,
    Critical,
}

impl ThermalState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Warning => "Attention",
            Self::Critical => "Critique",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum TemperatureSensorKind {
    Cpu,
    Gpu,
    System,
    Other,
}

impl TemperatureSensorKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Gpu => "GPU",
            Self::System => "Systeme",
            Self::Other => "Autre",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum TemperatureSource {
    AcerNitro,
    WindowsGeneric,
    #[default]
    Unavailable,
}

impl TemperatureSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::AcerNitro => "Acer Nitro",
            Self::WindowsGeneric => "Windows generique",
            Self::Unavailable => "Indisponible",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TemperatureReading {
    pub sensor_id: String,
    pub name: String,
    pub kind: TemperatureSensorKind,
    pub temperature_celsius: Option<f32>,
    pub max_temperature_celsius: Option<f32>,
    pub critical_temperature_celsius: Option<f32>,
    pub warning_limit_celsius: Option<f32>,
    pub critical_limit_celsius: Option<f32>,
    pub fan_speed_rpm: Option<u32>,
    pub source: TemperatureSource,
    pub available: bool,
    pub state: ThermalState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThermalCapabilities {
    pub source: TemperatureSource,
    pub read_supported: bool,
    pub control_supported: bool,
    pub fan_control_supported: bool,
    pub operating_mode_supported: bool,
}

impl Default for ThermalCapabilities {
    fn default() -> Self {
        Self {
            source: TemperatureSource::Unavailable,
            read_supported: false,
            control_supported: false,
            fan_control_supported: false,
            operating_mode_supported: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CoolingAction {
    FanMax,
    TurboMode,
}

impl CoolingAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::FanMax => "Ventilateurs au maximum",
            Self::TurboMode => "Mode turbo",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoolingActionRecord {
    pub action: CoolingAction,
    pub detail: String,
    pub applied_at_utc: i64,
    pub restored_at_utc: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThermalStatusSnapshot {
    pub monitoring_enabled: bool,
    pub auto_cooling_enabled: bool,
    pub control_available: bool,
    pub state: ThermalState,
    pub source: TemperatureSource,
    pub last_action: Option<CoolingActionRecord>,
    pub last_error: Option<String>,
    pub capabilities: ThermalCapabilities,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ThermalThresholdMode {
    #[default]
    Auto,
    Custom,
}

impl ThermalThresholdMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Custom => "Personnalise",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThermalThresholdPair {
    pub warning_celsius: f32,
    pub critical_celsius: f32,
}

impl ThermalThresholdPair {
    pub fn is_valid(&self) -> bool {
        self.warning_celsius.is_finite()
            && self.critical_celsius.is_finite()
            && self.warning_celsius >= MIN_TEMPERATURE_CELSIUS
            && self.critical_celsius <= MAX_TEMPERATURE_CELSIUS
            && self.critical_celsius > self.warning_celsius
    }

    fn sanitized_or(&self, fallback: Self) -> Self {
        if self.is_valid() {
            Self {
                warning_celsius: self
                    .warning_celsius
                    .clamp(MIN_TEMPERATURE_CELSIUS, MAX_TEMPERATURE_CELSIUS),
                critical_celsius: self
                    .critical_celsius
                    .clamp(MIN_TEMPERATURE_CELSIUS, MAX_TEMPERATURE_CELSIUS),
            }
        } else {
            fallback
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThermalSettings {
    pub enabled: bool,
    pub notifications_enabled: bool,
    pub auto_cooling_enabled: bool,
    pub threshold_mode: ThermalThresholdMode,
    pub cpu_thresholds: ThermalThresholdPair,
    pub gpu_thresholds: ThermalThresholdPair,
}

impl Default for ThermalSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            notifications_enabled: true,
            auto_cooling_enabled: false,
            threshold_mode: ThermalThresholdMode::Auto,
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
}

impl ThermalSettings {
    pub fn sanitize(&mut self) {
        let fallback = Self::default();
        self.cpu_thresholds = self.cpu_thresholds.sanitized_or(fallback.cpu_thresholds);
        self.gpu_thresholds = self.gpu_thresholds.sanitized_or(fallback.gpu_thresholds);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThermalThresholds {
    pub warning_celsius: f32,
    pub critical_celsius: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CapturedControlState {
    AcerNitro {
        fan_control: Option<Value>,
        operating_mode: Option<i32>,
    },
}

pub trait CoolingController {
    fn set_fan_max(&mut self) -> anyhow::Result<()>;
    fn set_turbo_mode(&mut self) -> anyhow::Result<()>;
}

pub trait ThermalAutomationController: Send {
    fn control_available(&self) -> bool;
    fn capture_control_state(&mut self) -> Option<CapturedControlState>;
    fn apply_max_cooling(&mut self) -> anyhow::Result<CoolingAction>;
    fn restore_previous_state(
        &mut self,
        state: &CapturedControlState,
    ) -> anyhow::Result<CoolingAction>;
}

pub fn apply_recommended_cooling(
    controller: &mut impl CoolingController,
) -> anyhow::Result<CoolingAction> {
    match controller.set_fan_max() {
        Ok(()) => Ok(CoolingAction::FanMax),
        Err(fan_error) => controller
            .set_turbo_mode()
            .map(|_| CoolingAction::TurboMode)
            .with_context(|| format!("fan max indisponible: {fan_error}")),
    }
}

pub fn thresholds_for_reading(
    reading: &TemperatureReading,
    settings: &ThermalSettings,
) -> ThermalThresholds {
    match settings.threshold_mode {
        ThermalThresholdMode::Auto => auto_thresholds_for_reading(reading),
        ThermalThresholdMode::Custom => match reading.kind {
            TemperatureSensorKind::Cpu => ThermalThresholds {
                warning_celsius: settings.cpu_thresholds.warning_celsius,
                critical_celsius: settings.cpu_thresholds.critical_celsius,
            },
            TemperatureSensorKind::Gpu => ThermalThresholds {
                warning_celsius: settings.gpu_thresholds.warning_celsius,
                critical_celsius: settings.gpu_thresholds.critical_celsius,
            },
            TemperatureSensorKind::System | TemperatureSensorKind::Other => {
                auto_thresholds_for_reading(reading)
            }
        },
    }
}

pub fn auto_thresholds_for_reading(reading: &TemperatureReading) -> ThermalThresholds {
    if let Some(hardware_critical) = reading
        .critical_temperature_celsius
        .filter(|value| value.is_finite() && *value >= 20.0)
    {
        let warning_celsius = (hardware_critical - 10.0).max(0.0);
        return ThermalThresholds {
            warning_celsius,
            critical_celsius: (hardware_critical - 5.0).max(warning_celsius + 1.0),
        };
    }
    ThermalThresholds {
        warning_celsius: 85.0,
        critical_celsius: 95.0,
    }
}

pub fn next_thermal_state(
    previous: ThermalState,
    current_celsius: f32,
    thresholds: ThermalThresholds,
) -> ThermalState {
    match previous {
        ThermalState::Normal => {
            if current_celsius >= thresholds.critical_celsius {
                ThermalState::Critical
            } else if current_celsius >= thresholds.warning_celsius {
                ThermalState::Warning
            } else {
                ThermalState::Normal
            }
        }
        ThermalState::Warning => {
            if current_celsius >= thresholds.critical_celsius {
                ThermalState::Critical
            } else if current_celsius < thresholds.warning_celsius - 3.0 {
                ThermalState::Normal
            } else {
                ThermalState::Warning
            }
        }
        ThermalState::Critical => {
            if current_celsius < thresholds.critical_celsius - 5.0 {
                if current_celsius >= thresholds.warning_celsius {
                    ThermalState::Warning
                } else {
                    ThermalState::Normal
                }
            } else {
                ThermalState::Critical
            }
        }
    }
}

#[derive(Clone)]
pub struct ThermalCollection {
    pub readings: Vec<TemperatureReading>,
    pub capabilities: ThermalCapabilities,
}

pub struct ThermalManager {
    acer_backend: Option<AcerNitroBackend>,
    generic_backend: GenericWindowsBackend,
    last_capabilities: ThermalCapabilities,
}

impl ThermalManager {
    pub fn new() -> Self {
        let acer_backend = AcerNitroBackend::probe().ok();
        let last_capabilities = acer_backend
            .as_ref()
            .map(|backend| backend.capabilities())
            .unwrap_or_default();
        Self {
            acer_backend,
            generic_backend: GenericWindowsBackend::new(),
            last_capabilities,
        }
    }

    pub fn collect(&mut self) -> anyhow::Result<ThermalCollection> {
        if let Some(backend) = self.acer_backend.as_mut() {
            match backend.collect() {
                Ok(readings) if !readings.is_empty() => {
                    let capabilities = backend.capabilities();
                    self.last_capabilities = capabilities.clone();
                    return Ok(ThermalCollection {
                        readings,
                        capabilities,
                    });
                }
                Ok(_) | Err(_) => {}
            }
        }

        let readings = self.generic_backend.collect()?;
        let capabilities = self.generic_backend.capabilities();
        self.last_capabilities = capabilities.clone();
        Ok(ThermalCollection {
            readings,
            capabilities,
        })
    }

    pub fn capabilities(&self) -> ThermalCapabilities {
        self.last_capabilities.clone()
    }
}

impl Default for ThermalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CoolingController for ThermalManager {
    fn set_fan_max(&mut self) -> anyhow::Result<()> {
        let backend = self
            .acer_backend
            .as_mut()
            .ok_or_else(|| anyhow!("controle ventilateur Acer indisponible"))?;
        backend.set_fan_max()
    }

    fn set_turbo_mode(&mut self) -> anyhow::Result<()> {
        let backend = self
            .acer_backend
            .as_mut()
            .ok_or_else(|| anyhow!("mode turbo Acer indisponible"))?;
        backend.set_turbo_mode()
    }
}

impl ThermalAutomationController for ThermalManager {
    fn control_available(&self) -> bool {
        self.last_capabilities.control_supported
    }

    fn capture_control_state(&mut self) -> Option<CapturedControlState> {
        self.acer_backend
            .as_mut()
            .and_then(|backend| backend.capture_control_state())
    }

    fn apply_max_cooling(&mut self) -> anyhow::Result<CoolingAction> {
        apply_recommended_cooling(self)
    }

    fn restore_previous_state(
        &mut self,
        state: &CapturedControlState,
    ) -> anyhow::Result<CoolingAction> {
        let backend = self
            .acer_backend
            .as_mut()
            .ok_or_else(|| anyhow!("backend Acer indisponible pour la restauration"))?;
        backend.restore_previous_state(state)
    }
}

struct GenericWindowsBackend {
    components: Components,
}

impl GenericWindowsBackend {
    fn new() -> Self {
        Self {
            components: Components::new_with_refreshed_list(),
        }
    }

    fn capabilities(&self) -> ThermalCapabilities {
        ThermalCapabilities {
            source: TemperatureSource::WindowsGeneric,
            read_supported: true,
            control_supported: false,
            fan_control_supported: false,
            operating_mode_supported: false,
        }
    }

    fn collect(&mut self) -> anyhow::Result<Vec<TemperatureReading>> {
        self.components.refresh(false);
        let readings = self
            .components
            .iter()
            .filter_map(|component| {
                let temperature = component.temperature()?;
                let label = component.label().to_owned();
                Some(TemperatureReading {
                    sensor_id: component.id().unwrap_or(component.label()).to_owned(),
                    name: label.clone(),
                    kind: sensor_kind_from_label(&label),
                    temperature_celsius: Some(temperature),
                    max_temperature_celsius: component.max(),
                    critical_temperature_celsius: component.critical(),
                    warning_limit_celsius: None,
                    critical_limit_celsius: None,
                    fan_speed_rpm: None,
                    source: TemperatureSource::WindowsGeneric,
                    available: true,
                    state: ThermalState::Normal,
                })
            })
            .collect::<Vec<_>>();

        if readings.is_empty() {
            return Err(anyhow!("aucun capteur thermique Windows disponible"));
        }
        Ok(readings)
    }
}

struct AcerNitroBackend {
    address: SocketAddr,
}

impl AcerNitroBackend {
    fn probe() -> anyhow::Result<Self> {
        let backend = Self {
            address: "127.0.0.1:46933"
                .parse()
                .expect("Acer Nitro loopback address must be valid"),
        };
        let readings = backend.collect()?;
        if readings.is_empty() {
            return Err(anyhow!("aucune temperature Acer Nitro disponible"));
        }
        Ok(backend)
    }

    fn capabilities(&self) -> ThermalCapabilities {
        ThermalCapabilities {
            source: TemperatureSource::AcerNitro,
            read_supported: true,
            control_supported: true,
            fan_control_supported: true,
            operating_mode_supported: true,
        }
    }

    fn collect(&self) -> anyhow::Result<Vec<TemperatureReading>> {
        let response = self
            .send_packet(10, &json!({}))
            .context("lecture GET_MONITOR_DATA Acer impossible")?
            .ok_or_else(|| anyhow!("reponse Acer vide pour GET_MONITOR_DATA"))?;
        let data = response
            .get("data")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("reponse Acer sans bloc data"))?;

        let mut readings = Vec::new();
        if let Some(temperature) = value_as_f32(data.get("CPU_TEMPERATURE")) {
            readings.push(TemperatureReading {
                sensor_id: "acer-cpu".into(),
                name: "CPU".into(),
                kind: TemperatureSensorKind::Cpu,
                temperature_celsius: Some(temperature),
                max_temperature_celsius: None,
                critical_temperature_celsius: None,
                warning_limit_celsius: None,
                critical_limit_celsius: None,
                fan_speed_rpm: value_as_u32(data.get("CPU_FANSPEED")),
                source: TemperatureSource::AcerNitro,
                available: true,
                state: ThermalState::Normal,
            });
        }

        for (key, value) in data {
            if !key.starts_with("GPU") || !key.ends_with("_TEMPERATURE") {
                continue;
            }
            if let Some(temperature) = value_as_f32(Some(value)) {
                let name = key.trim_end_matches("_TEMPERATURE").to_owned();
                let fan_key = format!("{}_FANSPEED", name);
                readings.push(TemperatureReading {
                    sensor_id: format!("acer-{}", name.to_ascii_lowercase()),
                    name,
                    kind: TemperatureSensorKind::Gpu,
                    temperature_celsius: Some(temperature),
                    max_temperature_celsius: None,
                    critical_temperature_celsius: None,
                    warning_limit_celsius: None,
                    critical_limit_celsius: None,
                    fan_speed_rpm: value_as_u32(data.get(&fan_key)),
                    source: TemperatureSource::AcerNitro,
                    available: true,
                    state: ThermalState::Normal,
                });
            }
        }

        for (key, value) in data {
            if !key.starts_with("SYS") || !key.ends_with("_TEMPERATURE") {
                continue;
            }
            if let Some(temperature) = value_as_f32(Some(value)) {
                let name = key.trim_end_matches("_TEMPERATURE").to_owned();
                let fan_key = format!("{}_FANSPEED", name);
                readings.push(TemperatureReading {
                    sensor_id: format!("acer-{}", name.to_ascii_lowercase()),
                    name,
                    kind: TemperatureSensorKind::System,
                    temperature_celsius: Some(temperature),
                    max_temperature_celsius: None,
                    critical_temperature_celsius: None,
                    warning_limit_celsius: None,
                    critical_limit_celsius: None,
                    fan_speed_rpm: value_as_u32(data.get(&fan_key)),
                    source: TemperatureSource::AcerNitro,
                    available: true,
                    state: ThermalState::Normal,
                });
            }
        }

        readings.sort_by_key(|reading| match reading.kind {
            TemperatureSensorKind::Cpu => (0, reading.name.clone()),
            TemperatureSensorKind::Gpu => (1, reading.name.clone()),
            TemperatureSensorKind::System => (2, reading.name.clone()),
            TemperatureSensorKind::Other => (3, reading.name.clone()),
        });
        Ok(readings)
    }

    fn capture_control_state(&mut self) -> Option<CapturedControlState> {
        let fan_control = self.get_updated_data("FAN_CONTROL").ok().flatten();
        let operating_mode = self
            .get_updated_data("OPERATING_MODE")
            .ok()
            .flatten()
            .and_then(|value| value.get("mode").and_then(Value::as_i64))
            .map(|value| value as i32);

        if fan_control.is_none() && operating_mode.is_none() {
            return None;
        }

        Some(CapturedControlState::AcerNitro {
            fan_control,
            operating_mode,
        })
    }

    fn restore_previous_state(
        &mut self,
        state: &CapturedControlState,
    ) -> anyhow::Result<CoolingAction> {
        match state {
            CapturedControlState::AcerNitro {
                fan_control: Some(fan_control),
                ..
            } => {
                self.set_device_data("FAN_CONTROL", fan_control.clone())?;
                Ok(CoolingAction::FanMax)
            }
            CapturedControlState::AcerNitro {
                operating_mode: Some(mode),
                ..
            } => {
                self.set_device_data("OPERATING_MODE", json!({ "mode": mode }))?;
                Ok(CoolingAction::TurboMode)
            }
            CapturedControlState::AcerNitro {
                fan_control: None,
                operating_mode: None,
            } => Err(anyhow!("etat precedent Acer non lisible")),
        }
    }

    fn set_fan_max(&mut self) -> anyhow::Result<()> {
        let payload = json!({
            "mode": 1
        });
        self.set_device_data("FAN_CONTROL", payload)?;
        Ok(())
    }

    fn set_turbo_mode(&mut self) -> anyhow::Result<()> {
        self.set_device_data("OPERATING_MODE", json!({ "mode": 5 }))?;
        Ok(())
    }

    fn get_updated_data(&self, function: &str) -> anyhow::Result<Option<Value>> {
        let response = self.send_packet(20, &json!({ "Function": function }))?;
        Ok(response.and_then(|value| value.get("data").cloned()))
    }

    fn set_device_data(&self, function: &str, parameter: Value) -> anyhow::Result<()> {
        let response = self.send_packet(
            100,
            &json!({
                "Function": function,
                "Parameter": parameter,
            }),
        )?;
        validate_acer_command_response(response.as_ref(), function)
    }

    fn send_packet(&self, packet_id: u32, payload: &Value) -> anyhow::Result<Option<Value>> {
        let payload = payload.to_string();
        if payload.len() > MAX_ACER_PAYLOAD_BYTES {
            anyhow::bail!("requete Acer trop volumineuse: {} octets", payload.len());
        }

        let mut stream = TcpStream::connect_timeout(&self.address, Duration::from_millis(700))
            .with_context(|| format!("connexion impossible a {}", self.address))?;
        stream
            .set_read_timeout(Some(Duration::from_millis(700)))
            .context("impossible de fixer le timeout de lecture Acer")?;
        stream
            .set_write_timeout(Some(Duration::from_millis(700)))
            .context("impossible de fixer le timeout d'ecriture Acer")?;
        stream
            .write_all(b"ACER")
            .context("impossible d'ecrire le marqueur Acer")?;
        stream
            .write_all(&packet_id.to_le_bytes())
            .context("impossible d'ecrire le packet id Acer")?;
        stream
            .write_all(payload.as_bytes())
            .with_context(|| format!("impossible d'envoyer la requete Acer {payload}"))?;

        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(size) => {
                    if buffer.len().saturating_add(size) > MAX_ACER_RESPONSE_BYTES {
                        anyhow::bail!(
                            "reponse Acer trop volumineuse: plus de {} octets",
                            MAX_ACER_RESPONSE_BYTES
                        );
                    }
                    buffer.extend_from_slice(&chunk[..size]);
                    if serde_json::from_slice::<Value>(&buffer).is_ok() {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => {
                    return Err(error).context("echec de lecture de la reponse Acer");
                }
            }
        }

        if buffer.is_empty() {
            return Ok(None);
        }
        let response = serde_json::from_slice::<Value>(&buffer).with_context(|| {
            format!(
                "reponse Acer invalide: {}",
                String::from_utf8_lossy(&buffer)
            )
        })?;
        Ok(Some(response))
    }
}

fn validate_acer_command_response(response: Option<&Value>, function: &str) -> anyhow::Result<()> {
    let response = response.ok_or_else(|| anyhow!("reponse Acer vide pour {function}"))?;
    let result = response
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("reponse Acer sans resultat pour {function}"))?;
    if result != "0" {
        anyhow::bail!("le service Acer a retourne result={result} pour {function}");
    }
    Ok(())
}

fn sensor_kind_from_label(label: &str) -> TemperatureSensorKind {
    let upper = label.to_ascii_uppercase();
    if upper.contains("CPU") {
        TemperatureSensorKind::Cpu
    } else if upper.contains("GPU") {
        TemperatureSensorKind::Gpu
    } else if upper.contains("SYS") || upper.contains("SYSTEM") || upper == "COMPUTER" {
        TemperatureSensorKind::System
    } else {
        TemperatureSensorKind::Other
    }
}

fn value_as_f32(value: Option<&Value>) -> Option<f32> {
    let parsed = match value {
        Some(Value::Number(number)) => number.as_f64().and_then(finite_f32),
        Some(Value::String(value)) => value.parse::<f32>().ok(),
        _ => None,
    }?;
    valid_temperature_celsius(parsed)
}

fn value_as_u32(value: Option<&Value>) -> Option<u32> {
    let parsed = match value {
        Some(Value::Number(number)) => number.as_u64().and_then(|value| u32::try_from(value).ok()),
        Some(Value::String(value)) => value.parse::<u32>().ok(),
        _ => None,
    }?;
    (parsed <= MAX_FAN_SPEED_RPM).then_some(parsed)
}

fn finite_f32(value: f64) -> Option<f32> {
    let value = value as f32;
    value.is_finite().then_some(value)
}

fn valid_temperature_celsius(value: f32) -> Option<f32> {
    (value.is_finite() && (MIN_TEMPERATURE_CELSIUS..=MAX_TEMPERATURE_CELSIUS).contains(&value))
        .then_some(value)
}

pub fn group_temperature_series_by_kind(
    readings: &[TemperatureReading],
) -> BTreeMap<TemperatureSensorKind, Vec<TemperatureReading>> {
    let mut grouped = BTreeMap::new();
    for reading in readings {
        grouped
            .entry(reading.kind)
            .or_insert_with(Vec::new)
            .push(reading.clone());
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockCoolingController {
        fan_max_calls: usize,
        turbo_calls: usize,
        fan_max_result: anyhow::Result<()>,
        turbo_result: anyhow::Result<()>,
    }

    impl CoolingController for MockCoolingController {
        fn set_fan_max(&mut self) -> anyhow::Result<()> {
            self.fan_max_calls += 1;
            self.fan_max_result
                .as_ref()
                .map(|_| ())
                .map_err(|error| anyhow!(error.to_string()))
        }

        fn set_turbo_mode(&mut self) -> anyhow::Result<()> {
            self.turbo_calls += 1;
            self.turbo_result
                .as_ref()
                .map(|_| ())
                .map_err(|error| anyhow!(error.to_string()))
        }
    }

    fn cpu_reading_with_critical(critical_temperature_celsius: Option<f32>) -> TemperatureReading {
        TemperatureReading {
            sensor_id: "cpu".into(),
            name: "CPU".into(),
            kind: TemperatureSensorKind::Cpu,
            temperature_celsius: Some(80.0),
            max_temperature_celsius: None,
            critical_temperature_celsius,
            warning_limit_celsius: None,
            critical_limit_celsius: None,
            fan_speed_rpm: None,
            source: TemperatureSource::AcerNitro,
            available: true,
            state: ThermalState::Normal,
        }
    }

    #[test]
    fn auto_thresholds_use_hardware_critical_when_available() {
        let thresholds = auto_thresholds_for_reading(&cpu_reading_with_critical(Some(105.0)));
        assert_eq!(
            thresholds,
            ThermalThresholds {
                warning_celsius: 95.0,
                critical_celsius: 100.0,
            }
        );
    }

    #[test]
    fn auto_thresholds_fall_back_to_defaults_when_hardware_critical_is_missing() {
        let thresholds = auto_thresholds_for_reading(&cpu_reading_with_critical(None));
        assert_eq!(
            thresholds,
            ThermalThresholds {
                warning_celsius: 85.0,
                critical_celsius: 95.0,
            }
        );
    }

    #[test]
    fn auto_thresholds_ignore_invalid_hardware_critical_values() {
        for critical in [Some(f32::NAN), Some(5.0)] {
            let thresholds = auto_thresholds_for_reading(&cpu_reading_with_critical(critical));
            assert_eq!(
                thresholds,
                ThermalThresholds {
                    warning_celsius: 85.0,
                    critical_celsius: 95.0,
                }
            );
        }
    }

    #[test]
    fn next_state_uses_hysteresis_when_leaving_warning_and_critical() {
        let thresholds = ThermalThresholds {
            warning_celsius: 85.0,
            critical_celsius: 95.0,
        };

        assert_eq!(
            next_thermal_state(ThermalState::Normal, 86.0, thresholds),
            ThermalState::Warning
        );
        assert_eq!(
            next_thermal_state(ThermalState::Warning, 83.0, thresholds),
            ThermalState::Warning
        );
        assert_eq!(
            next_thermal_state(ThermalState::Warning, 81.9, thresholds),
            ThermalState::Normal
        );
        assert_eq!(
            next_thermal_state(ThermalState::Warning, 96.0, thresholds),
            ThermalState::Critical
        );
        assert_eq!(
            next_thermal_state(ThermalState::Critical, 91.0, thresholds),
            ThermalState::Critical
        );
        assert_eq!(
            next_thermal_state(ThermalState::Critical, 89.0, thresholds),
            ThermalState::Warning
        );
    }

    #[test]
    fn recommended_cooling_prefers_fan_max() {
        let mut controller = MockCoolingController {
            fan_max_calls: 0,
            turbo_calls: 0,
            fan_max_result: Ok(()),
            turbo_result: Ok(()),
        };
        let action = apply_recommended_cooling(&mut controller).expect("fan max should work");
        assert_eq!(action, CoolingAction::FanMax);
        assert_eq!(controller.fan_max_calls, 1);
        assert_eq!(controller.turbo_calls, 0);
    }

    #[test]
    fn recommended_cooling_falls_back_to_turbo_when_fan_max_fails() {
        let mut controller = MockCoolingController {
            fan_max_calls: 0,
            turbo_calls: 0,
            fan_max_result: Err(anyhow!("fan control unavailable")),
            turbo_result: Ok(()),
        };
        let action = apply_recommended_cooling(&mut controller).expect("turbo should be used");
        assert_eq!(action, CoolingAction::TurboMode);
        assert_eq!(controller.fan_max_calls, 1);
        assert_eq!(controller.turbo_calls, 1);
    }

    #[test]
    fn acer_value_parsing_rejects_non_finite_and_out_of_range_values() {
        assert_eq!(value_as_f32(Some(&json!(72.5))), Some(72.5));
        assert_eq!(value_as_f32(Some(&json!("NaN"))), None);
        assert_eq!(value_as_f32(Some(&json!(-1.0))), None);
        assert_eq!(value_as_f32(Some(&json!(151.0))), None);
        assert_eq!(value_as_u32(Some(&json!(1200))), Some(1200));
        assert_eq!(value_as_u32(Some(&json!(50_001))), None);
        assert_eq!(value_as_u32(Some(&json!(u64::from(u32::MAX) + 1))), None);
    }

    #[test]
    fn acer_command_response_requires_explicit_success() {
        let success = json!({ "result": "0" });
        validate_acer_command_response(Some(&success), "FAN_CONTROL")
            .expect("result=0 should pass");

        let missing_result = json!({});
        let failed = json!({ "result": "1" });
        assert!(validate_acer_command_response(None, "FAN_CONTROL").is_err());
        assert!(validate_acer_command_response(Some(&missing_result), "FAN_CONTROL").is_err());
        assert!(validate_acer_command_response(Some(&failed), "FAN_CONTROL").is_err());
    }

    #[test]
    fn thermal_settings_default_requires_explicit_auto_cooling_opt_in() {
        assert!(!ThermalSettings::default().auto_cooling_enabled);
    }

    #[test]
    fn thermal_settings_sanitize_restores_invalid_thresholds() {
        let mut settings = ThermalSettings {
            cpu_thresholds: ThermalThresholdPair {
                warning_celsius: 200.0,
                critical_celsius: 201.0,
            },
            gpu_thresholds: ThermalThresholdPair {
                warning_celsius: f32::NAN,
                critical_celsius: 95.0,
            },
            ..Default::default()
        };

        settings.sanitize();

        assert_eq!(settings.cpu_thresholds.warning_celsius, 85.0);
        assert_eq!(settings.cpu_thresholds.critical_celsius, 95.0);
        assert_eq!(settings.gpu_thresholds.warning_celsius, 85.0);
        assert_eq!(settings.gpu_thresholds.critical_celsius, 95.0);
    }
}
