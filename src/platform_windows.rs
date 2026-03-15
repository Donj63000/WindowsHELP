use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess,
    PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_TERMINATE, SetPriorityClass, TerminateProcess,
};
use windows::core::PCWSTR;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const DRIVE_TYPE_FIXED: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PriorityClass {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

impl PriorityClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Inactif",
            Self::BelowNormal => "Inférieure à la normale",
            Self::Normal => "Normale",
            Self::AboveNormal => "Supérieure à la normale",
            Self::High => "Haute",
        }
    }

    pub fn all() -> [Self; 5] {
        [
            Self::Idle,
            Self::BelowNormal,
            Self::Normal,
            Self::AboveNormal,
            Self::High,
        ]
    }

    fn to_windows(self) -> u32 {
        match self {
            Self::Idle => IDLE_PRIORITY_CLASS.0,
            Self::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS.0,
            Self::Normal => NORMAL_PRIORITY_CLASS.0,
            Self::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS.0,
            Self::High => HIGH_PRIORITY_CLASS.0,
        }
    }

    fn from_windows(raw: u32) -> Self {
        match raw {
            x if x == IDLE_PRIORITY_CLASS.0 => Self::Idle,
            x if x == BELOW_NORMAL_PRIORITY_CLASS.0 => Self::BelowNormal,
            x if x == ABOVE_NORMAL_PRIORITY_CLASS.0 => Self::AboveNormal,
            x if x == HIGH_PRIORITY_CLASS.0 => Self::High,
            _ => Self::Normal,
        }
    }
}

fn to_pcwstr(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

pub fn list_fixed_drive_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for letter in b'A'..=b'Z' {
        let root = format!("{}:\\", letter as char);
        let wide = to_pcwstr(&root);
        // SAFETY: the wide string is NUL terminated and lives for the duration of the call.
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
        if drive_type == DRIVE_TYPE_FIXED {
            roots.push(PathBuf::from(root));
        }
    }
    roots
}

pub fn metadata_attributes(metadata: &std::fs::Metadata) -> u32 {
    metadata.file_attributes()
}

pub fn is_hidden(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_HIDDEN != 0
}

pub fn is_system(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_SYSTEM != 0
}

pub fn open_path(path: &Path) -> anyhow::Result<()> {
    Command::new("explorer")
        .arg(path)
        .spawn()
        .with_context(|| format!("échec de l'ouverture de {}", path.display()))?;
    Ok(())
}

pub fn reveal_in_explorer(path: &Path) -> anyhow::Result<()> {
    let argument = format!("/select,{}", path.display());
    Command::new("explorer")
        .arg(argument)
        .spawn()
        .with_context(|| {
            format!(
                "échec de l'affichage de {} dans l'Explorateur",
                path.display()
            )
        })?;
    Ok(())
}

pub fn show_toast_notification(title: &str, body: &str) -> anyhow::Result<()> {
    let xml = format!(
        "<toast><visual><binding template='ToastGeneric'><text>{}</text><text>{}</text></binding></visual></toast>",
        escape_xml(title),
        escape_xml(body)
    );
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
         [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null; \
         $xml = New-Object Windows.Data.Xml.Dom.XmlDocument; \
         $xml.LoadXml('{}'); \
         $toast = [Windows.UI.Notifications.ToastNotification]::new($xml); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Windows PowerShell').Show($toast);",
        escape_powershell_single_quoted(&xml)
    );

    Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
        ])
        .arg(script)
        .spawn()
        .context("echec de l'envoi de la notification toast Windows")?;
    Ok(())
}

fn with_process_handle<F>(
    pid: u32,
    desired_access: PROCESS_ACCESS_RIGHTS,
    callback: F,
) -> anyhow::Result<()>
where
    F: FnOnce(HANDLE) -> anyhow::Result<()>,
{
    // SAFETY: OpenProcess is called with a valid pid value and the resulting handle is closed.
    let handle = unsafe { OpenProcess(desired_access, false, pid) }
        .with_context(|| format!("échec de l'ouverture du processus {pid}"))?;
    let result = callback(handle);
    // SAFETY: handle was returned by OpenProcess and must be closed once.
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}

pub fn kill_process(pid: u32) -> anyhow::Result<()> {
    with_process_handle(pid, PROCESS_TERMINATE, |handle| {
        // SAFETY: handle is a valid process handle with terminate rights.
        unsafe { TerminateProcess(handle, 1) }
            .with_context(|| format!("échec de la terminaison du processus {pid}"))?;
        Ok(())
    })
}

pub fn set_process_priority(pid: u32, priority: PriorityClass) -> anyhow::Result<()> {
    with_process_handle(
        pid,
        PROCESS_ACCESS_RIGHTS(PROCESS_SET_INFORMATION.0 | PROCESS_QUERY_LIMITED_INFORMATION.0),
        |handle| {
            // SAFETY: handle is valid and SetPriorityClass only reads the class value.
            unsafe { SetPriorityClass(handle, PROCESS_CREATION_FLAGS(priority.to_windows())) }
                .with_context(|| {
                    format!("échec de la modification de la priorité du processus {pid}")
                })?;
            Ok(())
        },
    )
}

pub fn get_process_priority(pid: u32) -> anyhow::Result<PriorityClass> {
    let mut priority = PriorityClass::Normal;
    with_process_handle(pid, PROCESS_QUERY_LIMITED_INFORMATION, |handle| {
        // SAFETY: handle is valid and queried only for metadata.
        let raw = unsafe { GetPriorityClass(handle) };
        if raw == 0 {
            return Err(anyhow!(
                "échec de la lecture de la priorité du processus {pid}"
            ));
        }
        priority = PriorityClass::from_windows(raw);
        Ok(())
    })?;
    Ok(priority)
}

pub fn collect_thread_counts() -> anyhow::Result<HashMap<u32, u32>> {
    let mut counts = HashMap::new();
    // SAFETY: the snapshot handle is closed after use.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }
        .context("échec de la création de l'instantané des threads")?;
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };

    // SAFETY: entry.dwSize is initialized as required by the API.
    let mut has_thread = unsafe { Thread32First(snapshot, &mut entry) }.is_ok();
    while has_thread {
        *counts.entry(entry.th32OwnerProcessID).or_insert(0) += 1;
        // SAFETY: entry remains initialized for subsequent calls.
        has_thread = unsafe { Thread32Next(snapshot, &mut entry) }.is_ok();
    }

    // SAFETY: snapshot is a valid handle returned by CreateToolhelp32Snapshot.
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    Ok(counts)
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}
