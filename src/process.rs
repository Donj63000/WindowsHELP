use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sysinfo::{ProcessStatus, ProcessesToUpdate, System};
use tokio::runtime::Handle;

use crate::platform_windows::{
    PriorityClass, collect_thread_counts, get_process_priority, kill_process, set_process_priority,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub name: String,
    pub path: Option<PathBuf>,
    pub cpu: f32,
    pub memory_bytes: u64,
    pub threads: u32,
    pub status: String,
    pub priority: PriorityClass,
}

#[derive(Clone, Debug)]
pub enum ProcessAction {
    Kill,
    SetPriority(PriorityClass),
}

pub struct ProcessManager {
    snapshots: Arc<RwLock<Vec<ProcessSnapshot>>>,
    last_error: Arc<Mutex<Option<String>>>,
    refresh_interval: Arc<RwLock<Duration>>,
}

impl ProcessManager {
    pub fn new(runtime: Handle, refresh_interval: Duration) -> Self {
        let manager = Self {
            snapshots: Arc::new(RwLock::new(Vec::new())),
            last_error: Arc::new(Mutex::new(None)),
            refresh_interval: Arc::new(RwLock::new(refresh_interval)),
        };
        manager.spawn_refresh_loop(runtime);
        manager
    }

    pub fn snapshots(&self) -> Vec<ProcessSnapshot> {
        self.snapshots
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|guard| guard.clone())
    }

    pub fn update_refresh_interval(&self, refresh_interval: Duration) {
        if let Ok(mut guard) = self.refresh_interval.write() {
            *guard = refresh_interval;
        }
    }

    pub fn perform_action(&self, pid: u32, action: ProcessAction) -> anyhow::Result<()> {
        match action {
            ProcessAction::Kill => kill_process(pid),
            ProcessAction::SetPriority(priority) => set_process_priority(pid, priority),
        }
    }

    fn spawn_refresh_loop(&self, runtime: Handle) {
        let snapshots = Arc::clone(&self.snapshots);
        let last_error = Arc::clone(&self.last_error);
        let refresh_interval = Arc::clone(&self.refresh_interval);

        runtime.spawn(async move {
            let mut system = System::new_all();
            system.refresh_all();
            loop {
                match refresh_process_snapshots_with_system(&mut system) {
                    Ok(processes) => {
                        if let Ok(mut guard) = snapshots.write() {
                            *guard = processes;
                        }
                        if let Ok(mut error_guard) = last_error.lock() {
                            *error_guard = None;
                        }
                    }
                    Err(error) => {
                        if let Ok(mut guard) = last_error.lock() {
                            *guard = Some(error.to_string());
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

pub fn refresh_process_snapshots() -> anyhow::Result<Vec<ProcessSnapshot>> {
    let mut system = System::new_all();
    system.refresh_all();
    refresh_process_snapshots_with_system(&mut system)
}

fn refresh_process_snapshots_with_system(
    system: &mut System,
) -> anyhow::Result<Vec<ProcessSnapshot>> {
    system.refresh_cpu_usage();
    system.refresh_memory();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let thread_counts = collect_thread_counts().unwrap_or_default();

    let mut processes: Vec<ProcessSnapshot> = system
        .processes()
        .iter()
        .map(|(pid, process)| ProcessSnapshot {
            pid: pid.as_u32(),
            name: process.name().to_string_lossy().to_string(),
            path: process.exe().map(|path| path.to_path_buf()),
            cpu: process.cpu_usage(),
            memory_bytes: process.memory(),
            threads: thread_counts
                .get(&pid.as_u32())
                .copied()
                .unwrap_or_default(),
            status: translate_process_status(process.status()).to_owned(),
            priority: get_process_priority(pid.as_u32()).unwrap_or(PriorityClass::Normal),
        })
        .collect();

    processes.sort_by(|left, right| {
        right
            .cpu
            .partial_cmp(&left.cpu)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.memory_bytes.cmp(&left.memory_bytes))
    });
    Ok(processes)
}

fn translate_process_status(status: ProcessStatus) -> &'static str {
    match status {
        ProcessStatus::Idle => "Inactif",
        ProcessStatus::Run => "En cours",
        ProcessStatus::Sleep => "En veille",
        ProcessStatus::Stop => "Arrêté",
        ProcessStatus::Zombie => "Zombie",
        ProcessStatus::Tracing => "Traçage",
        ProcessStatus::Dead => "Bloqué",
        ProcessStatus::Wakekill => "Réveil forcé",
        ProcessStatus::Waking => "Réveil",
        ProcessStatus::Parked => "En pause",
        ProcessStatus::LockBlocked => "Bloqué par verrou",
        ProcessStatus::UninterruptibleDiskSleep => "Attente disque non interrompable",
        ProcessStatus::Suspended => "Suspendu",
        ProcessStatus::Unknown(_) => "Inconnu",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn process_listing_sees_spawned_process_and_can_manage_it() -> anyhow::Result<()> {
        let mut child = Command::new("cmd")
            .args(["/C", "ping", "127.0.0.1", "-n", "30"])
            .spawn()?;
        let pid = child.id();

        let snapshots = refresh_process_snapshots()?;
        assert!(snapshots.iter().any(|snapshot| snapshot.pid == pid));

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
