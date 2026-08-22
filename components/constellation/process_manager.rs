/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::process::Child;

use crossbeam_channel::{Receiver, Select};
use log::{debug, warn};
use profile_traits::mem::{ProfilerChan, ProfilerMsg};

pub struct Process {
    kind: ProcessKind,
    system_memory_reporter_name: Option<String>,
}

enum ProcessKind {
    Unsandboxed(Child),
    Sandboxed(u32),
}

impl Process {
    pub(crate) fn unsandboxed(child: Child) -> Self {
        Self {
            kind: ProcessKind::Unsandboxed(child),
            system_memory_reporter_name: None,
        }
    }

    pub(crate) fn sandboxed(pid: u32) -> Self {
        Self {
            kind: ProcessKind::Sandboxed(pid),
            system_memory_reporter_name: None,
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        match &self.kind {
            ProcessKind::Unsandboxed(child) => child.id(),
            ProcessKind::Sandboxed(pid) => *pid,
        }
    }

    pub(crate) fn set_system_memory_reporter_name(&mut self, name: Option<String>) {
        self.system_memory_reporter_name = name;
    }

    fn system_memory_reporter_name(&self) -> Option<&str> {
        self.system_memory_reporter_name.as_deref()
    }

    fn wait(&mut self) {
        match &mut self.kind {
            ProcessKind::Unsandboxed(child) => {
                let _ = child.wait();
            },
            ProcessKind::Sandboxed(_pid) => {
                // TODO: use nix::waitpid() on supported platforms.
                warn!("wait() is not yet implemented for sandboxed processes.");
            },
        }
    }
}

type ProcessReceiver = Receiver<Result<(), ipc_channel::IpcError>>;

pub(crate) struct ProcessManager {
    processes: Vec<(Process, ProcessReceiver)>,
    mem_profiler_chan: ProfilerChan,
}

impl ProcessManager {
    pub fn new(mem_profiler_chan: ProfilerChan) -> Self {
        Self {
            processes: vec![],
            mem_profiler_chan,
        }
    }

    pub fn add(&mut self, receiver: ProcessReceiver, process: Process) {
        debug!("Adding process pid={}", process.pid());
        self.processes.push((process, receiver));
    }

    pub fn register<'a>(&'a self, select: &mut Select<'a>) {
        for (_, receiver) in &self.processes {
            select.recv(receiver);
        }
    }

    pub fn receiver_at(&self, index: usize) -> &ProcessReceiver {
        let (_, receiver) = &self.processes[index];
        receiver
    }

    #[servo_tracing::instrument(skip_all)]
    pub fn remove(&mut self, index: usize) {
        let (mut process, _) = self.processes.swap_remove(index);
        debug!("Removing process pid={}", process.pid());
        if let Some(reporter_name) = process.system_memory_reporter_name() {
            self.mem_profiler_chan
                .send(ProfilerMsg::UnregisterReporter(reporter_name.to_owned()));
        }
        process.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::Process;

    #[test]
    fn sandboxed_process_keeps_parent_reporter_name() {
        let mut process = Process::sandboxed(42);
        assert_eq!(process.pid(), 42);
        assert_eq!(process.system_memory_reporter_name(), None);

        process.set_system_memory_reporter_name(Some("system-content-42".to_owned()));

        assert_eq!(
            process.system_memory_reporter_name(),
            Some("system-content-42")
        );
    }
}
