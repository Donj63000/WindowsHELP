use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HWND, LPARAM, RECT, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetPriorityClass,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess,
    PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_TERMINATE, SetPriorityClass, TerminateProcess,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW, WM_CLOSE,
};
use windows::core::{BOOL, PCWSTR};

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const DRIVE_TYPE_FIXED: u32 = 3;
const MAX_TOAST_TITLE_CHARS: usize = 120;
const MAX_TOAST_BODY_CHARS: usize = 420;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkArea {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

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

    pub fn recommended_choices() -> [Self; 3] {
        [Self::Idle, Self::BelowNormal, Self::Normal]
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

pub fn primary_work_area() -> Option<WorkArea> {
    let mut rect = RECT::default();
    // SAFETY: SPI_GETWORKAREA writes a RECT to the provided pointer and does not retain it.
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&mut rect as *mut RECT).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }

    let width = rect.right.saturating_sub(rect.left);
    let height = rect.bottom.saturating_sub(rect.top);
    (width > 0 && height > 0).then_some(WorkArea {
        left: rect.left as f32,
        top: rect.top as f32,
        width: width as f32,
        height: height as f32,
    })
}

pub fn primary_work_area_size() -> Option<(f32, f32)> {
    primary_work_area().map(|area| (area.width, area.height))
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
    ensure_existing_path(path)?;
    Command::new(explorer_executable())
        .arg(path)
        .spawn()
        .with_context(|| format!("échec de l'ouverture de {}", path.display()))?;
    Ok(())
}

pub fn reveal_in_explorer(path: &Path) -> anyhow::Result<()> {
    ensure_existing_path(path)?;
    let argument = format!("/select,{}", path.display());
    Command::new(explorer_executable())
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
    let title = sanitize_xml_text(title, MAX_TOAST_TITLE_CHARS);
    let body = sanitize_xml_text(body, MAX_TOAST_BODY_CHARS);
    let xml = format!(
        "<toast><visual><binding template='ToastGeneric'><text>{}</text><text>{}</text></binding></visual></toast>",
        escape_xml(&title),
        escape_xml(&body)
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

    Command::new(powershell_executable())
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

fn ensure_existing_path(path: &Path) -> anyhow::Result<()> {
    match path.try_exists() {
        Ok(true) => Ok(()),
        Ok(false) => anyhow::bail!("chemin introuvable: {}", path.display()),
        Err(error) => {
            Err(error).with_context(|| format!("impossible de verifier {}", path.display()))
        }
    }
}

fn explorer_executable() -> PathBuf {
    windows_root().join("explorer.exe")
}

fn powershell_executable() -> PathBuf {
    windows_root()
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

fn windows_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn with_process_handle<T, F>(
    pid: u32,
    desired_access: PROCESS_ACCESS_RIGHTS,
    callback: F,
) -> anyhow::Result<T>
where
    F: FnOnce(HANDLE) -> anyhow::Result<T>,
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

pub fn has_visible_window(pid: u32) -> anyhow::Result<bool> {
    Ok(collect_visible_window_pids()?.contains(&pid))
}

pub fn collect_visible_window_pids() -> anyhow::Result<HashSet<u32>> {
    let mut context = WindowPidCollector {
        target_pid: None,
        pids: HashSet::new(),
        closed_windows: 0,
        error: None,
        send_close: false,
    };
    enum_windows(&mut context)?;
    Ok(context.pids)
}

pub fn close_process_gracefully(pid: u32) -> anyhow::Result<()> {
    let mut context = WindowPidCollector {
        target_pid: Some(pid),
        pids: HashSet::new(),
        closed_windows: 0,
        error: None,
        send_close: true,
    };
    enum_windows(&mut context)?;
    if let Some(error) = context.error {
        return Err(anyhow!(error));
    }
    if context.closed_windows == 0 {
        anyhow::bail!("aucune fenetre visible n'a ete trouvee pour le processus {pid}");
    }
    Ok(())
}

pub fn wait_for_process_exit(pid: u32, timeout_ms: u32) -> anyhow::Result<bool> {
    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    with_process_handle(
        pid,
        PROCESS_ACCESS_RIGHTS(PROCESS_QUERY_LIMITED_INFORMATION.0 | SYNCHRONIZE_ACCESS),
        |handle| {
            let status = unsafe { WaitForSingleObject(handle, timeout_ms) };
            if status == WAIT_OBJECT_0 {
                Ok(true)
            } else if status == WAIT_TIMEOUT {
                Ok(false)
            } else {
                Err(anyhow!(
                    "echec de l'attente de fermeture du processus {pid} (code {:?})",
                    status
                ))
            }
        },
    )
}

struct WindowPidCollector {
    target_pid: Option<u32>,
    pids: HashSet<u32>,
    closed_windows: usize,
    error: Option<String>,
    send_close: bool,
}

fn enum_windows(context: &mut WindowPidCollector) -> anyhow::Result<()> {
    let context_ptr = context as *mut WindowPidCollector;
    // SAFETY: context_ptr remains valid for the duration of EnumWindows.
    unsafe { EnumWindows(Some(enum_windows_callback), LPARAM(context_ptr as isize)) }
        .context("echec de l'enumeration des fenetres Windows")
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let context = unsafe { &mut *(lparam.0 as *mut WindowPidCollector) };
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return true.into();
    }

    let is_top_level = unsafe { GetWindow(hwnd, GW_OWNER) }.is_err();
    if !is_top_level {
        return true.into();
    }

    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0 {
        return true.into();
    }

    context.pids.insert(pid);

    if context.send_close && context.target_pid == Some(pid) {
        if let Err(error) = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
            if context.error.is_none() {
                context.error = Some(format!(
                    "impossible d'envoyer WM_CLOSE au processus {pid}: {error}"
                ));
            }
        } else {
            context.closed_windows += 1;
        }
    }

    true.into()
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn sanitize_xml_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|character| is_valid_xml_text_char(*character))
        .take(max_chars)
        .collect()
}

fn is_valid_xml_text_char(character: char) -> bool {
    matches!(
        character,
        '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}'
    )
}

fn escape_powershell_single_quoted(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_text_is_bounded_and_xml_safe() {
        let value = format!("ok{}\u{0}<", "x".repeat(MAX_TOAST_TITLE_CHARS + 10));
        let sanitized = sanitize_xml_text(&value, MAX_TOAST_TITLE_CHARS);

        assert_eq!(sanitized.chars().count(), MAX_TOAST_TITLE_CHARS);
        assert!(!sanitized.contains('\u{0}'));
        assert!(escape_xml("<tag>&value").contains("&lt;tag&gt;&amp;value"));
    }

    #[test]
    fn powershell_arguments_escape_single_quotes() {
        assert_eq!(escape_powershell_single_quoted("a'b"), "a''b");
    }
}
