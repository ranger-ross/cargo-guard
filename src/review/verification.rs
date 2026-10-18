use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

use cargo_metadata::PackageId;
use eyre::Context;

use super::{
    ReviewPackage,
    fingerprint::{ArtifactIndex, Verification},
};
use crate::config::ApprovedVersion;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    Selected,
    Global,
}

pub(super) struct Request {
    pub index: usize,
    pub scope: Scope,
    pub generation: u64,
    pub package: ReviewPackage,
    pub approvals: ApprovedVersion,
}

pub(super) struct Result {
    pub index: usize,
    pub scope: Scope,
    pub generation: u64,
    pub package_id: PackageId,
    pub verification: Verification,
}

enum Command {
    Verify(Box<Request>),
    Prioritize(usize),
    Stop,
}

pub(super) struct Verifier {
    sender: mpsc::Sender<Command>,
    receiver: mpsc::Receiver<Result>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Verifier {
    pub fn start(root: PathBuf, directory: PathBuf) -> eyre::Result<Self> {
        let (sender, commands) = mpsc::channel();
        let (results, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&cancelled);
        let ui = thread::current();
        let worker = thread::Builder::new()
            .name("review-fingerprints".into())
            .spawn(move || {
                let mut artifacts = ArtifactIndex::new(directory);
                let mut pending = VecDeque::new();
                loop {
                    if pending.is_empty() {
                        let Ok(command) = commands.recv() else { break };
                        if !enqueue(command, &mut pending) {
                            break;
                        }
                    }
                    for command in commands.try_iter() {
                        if !enqueue(command, &mut pending) {
                            return;
                        }
                    }
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(request) = pending.pop_front() else {
                        continue;
                    };
                    let verification =
                        artifacts.verify(&request.package, &request.approvals, &root, &stopped);
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    if results
                        .send(Result {
                            index: request.index,
                            scope: request.scope,
                            generation: request.generation,
                            package_id: request.package.id,
                            verification,
                        })
                        .is_err()
                    {
                        break;
                    }
                    ui.unpark();
                }
            })
            .context("could not start fingerprint verification worker")?;
        Ok(Self {
            sender,
            receiver,
            cancelled,
            worker: Some(worker),
        })
    }

    pub fn verify(&self, request: Request) -> eyre::Result<()> {
        self.sender
            .send(Command::Verify(Box::new(request)))
            .map_err(|_| eyre::eyre!("fingerprint verification worker stopped"))
    }

    pub fn prioritize(&self, index: usize) {
        let _ = self.sender.send(Command::Prioritize(index));
    }

    pub fn poll(&self) -> Option<Result> {
        self.receiver.try_recv().ok()
    }
}

fn enqueue(command: Command, pending: &mut VecDeque<Box<Request>>) -> bool {
    match command {
        Command::Verify(request) => {
            if let Some(index) = pending
                .iter()
                .position(|old| old.index == request.index && old.scope == request.scope)
            {
                pending[index] = request;
            } else {
                pending.push_back(request);
            }
        }
        Command::Prioritize(index) => {
            if let Some(position) = pending.iter().position(|request| request.index == index) {
                let request = pending.remove(position).expect("pending request exists");
                pending.push_front(request);
            }
        }
        Command::Stop => return false,
    }
    true
}

impl Drop for Verifier {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        let _ = self.sender.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
