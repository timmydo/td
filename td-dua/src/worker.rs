//! The thread that touches the file system for the window: it scans and
//! deletes in the order asked, so the window never waits on a slow disk.
//! The thread ends when the window drops its end; a walk in progress is
//! cancelled, and a deletion in progress is left to the process's exit.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use crate::delete::{self, Target};
use crate::scan::{self, Progress};
use crate::tree::{NodeId, Tree};

#[derive(Debug)]
pub enum Job {
    /// Scan `path`; `at` is the node it refreshes, none for a new tree.
    Scan { path: PathBuf, at: Option<NodeId> },
    /// Delete each target in order; each answers on its own.
    Delete { targets: Vec<(NodeId, Target)> },
}

#[derive(Debug)]
pub enum Reply {
    Scanned {
        at: Option<NodeId>,
        path: PathBuf,
        tree: Result<Tree, String>,
    },
    Deleted {
        results: Vec<(NodeId, Target, Result<(), String>)>,
    },
}

pub struct Worker {
    jobs: Option<Sender<Job>>,
    replies: Receiver<Reply>,
    progress: Arc<Progress>,
}

impl Worker {
    pub fn start() -> Result<Self, String> {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (reply_tx, replies) = mpsc::channel();
        let progress = Arc::new(Progress::default());
        let shared = Arc::clone(&progress);
        std::thread::Builder::new()
            .name("dua-worker".to_owned())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let reply = run(job, &shared);
                    if reply_tx.send(reply).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("cannot start the scanning thread: {error}"))?;
        Ok(Self {
            jobs: Some(jobs),
            replies,
            progress,
        })
    }

    pub fn send(&self, job: Job) -> Result<(), String> {
        self.jobs
            .as_ref()
            .ok_or("the scanning thread has ended")?
            .send(job)
            .map_err(|_| "the scanning thread has ended".to_owned())
    }

    pub fn try_recv(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
    }

    pub fn progress(&self) -> &Progress {
        &self.progress
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.progress.cancel.store(true, Ordering::Relaxed);
        self.jobs = None;
    }
}

/// One job, on whatever thread runs it.
pub fn run(job: Job, progress: &Progress) -> Reply {
    match job {
        Job::Scan { path, at } => {
            progress.reset();
            let tree = scan::scan(&path, progress).map_err(|error| error.to_string());
            Reply::Scanned { at, path, tree }
        }
        Job::Delete { targets } => Reply::Deleted {
            results: targets
                .into_iter()
                .map(|(id, target)| {
                    let result = delete::delete(&target).map_err(|error| error.to_string());
                    (id, target, result)
                })
                .collect(),
        },
    }
}
