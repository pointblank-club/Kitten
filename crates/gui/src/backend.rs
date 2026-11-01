//! Talking to `bootshiftd`. The GUI never touches a disk: it asks the root helper over the
//! system D-Bus and renders whatever comes back. Calls run on a worker thread, because the
//! helper reads disks and polkit may sit on a password prompt.

#[cfg(test)]
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use bootshift_core::layout::Target;
use serde::de::DeserializeOwned;

use crate::protocol::{Applied, CheckResult, DiskSummary, FinishedRun, Planned, Probed, Reply};

const BUS_NAME: &str = "org.bootshift.Helper1";
const OBJECT_PATH: &str = "/org/bootshift/Helper1";

/// The helper couldn't be reached, or answered with something that isn't a reply.
#[derive(Clone, Debug, PartialEq)]
pub struct BackendError(pub String);

pub trait Backend: Send {
    /// True when showing made-up data instead of a real disk.
    fn is_demo(&self) -> bool;
    fn list_disks(&mut self) -> Result<Reply<Vec<DiskSummary>>, BackendError>;
    fn probe(&mut self, disk: &str) -> Result<Reply<Probed>, BackendError>;
    fn plan(&mut self, disk: &str, target: &Target) -> Result<Reply<Planned>, BackendError>;
    /// `None` rows mean the helper ran its checks but didn't list them one by one.
    fn check(
        &mut self,
        disk: &str,
        target: &Target,
    ) -> Result<Reply<Option<Vec<CheckResult>>>, BackendError>;
    fn apply(&mut self, disk: &str, target: &Target) -> Result<Reply<Applied>, BackendError>;
    /// The outcome of the last applied run, if the recovery system finished one since the last
    /// time the layout was changed. `None` means there is nothing to reopen onto, so the app
    /// starts on the goal launcher as usual.
    fn last_run(&mut self) -> Result<Reply<Option<FinishedRun>>, BackendError>;
}

/// Each call blocks its (worker) thread until the helper answers.
pub struct HelperBackend {
    connection: zbus::Connection,
}

impl HelperBackend {
    /// Connects to the system bus and checks the helper answers, so a missing helper is found
    /// before the first screen rather than in the middle of a flow.
    pub fn connect() -> Result<Self, BackendError> {
        let connection = zbus::block_on(zbus::Connection::system()).map_err(failure)?;
        let helper = Self { connection };
        helper.call("Version", &())?;
        Ok(helper)
    }

    /// Every helper method answers with one string: a version, or a reply as JSON.
    fn call<B>(&self, method: &str, body: &B) -> Result<String, BackendError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let call =
            self.connection
                .call_method(Some(BUS_NAME), OBJECT_PATH, Some(BUS_NAME), method, body);
        let message = zbus::block_on(call).map_err(failure)?;
        message.body().deserialize::<String>().map_err(failure)
    }

    fn reply<T: DeserializeOwned, B>(&self, method: &str, body: &B) -> Result<T, BackendError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let json = self.call(method, body)?;
        serde_json::from_str(&json).map_err(|error| {
            BackendError(format!(
                "the helper's answer to {method} can't be read: {error}"
            ))
        })
    }
}

impl Backend for HelperBackend {
    fn is_demo(&self) -> bool {
        false
    }

    fn list_disks(&mut self) -> Result<Reply<Vec<DiskSummary>>, BackendError> {
        self.reply("ListDisks", &())
    }

    fn probe(&mut self, disk: &str) -> Result<Reply<Probed>, BackendError> {
        self.reply("Probe", &(disk,))
    }

    fn plan(&mut self, disk: &str, target: &Target) -> Result<Reply<Planned>, BackendError> {
        self.reply("Plan", &(disk, target_json(target)?))
    }

    fn check(
        &mut self,
        disk: &str,
        target: &Target,
    ) -> Result<Reply<Option<Vec<CheckResult>>>, BackendError> {
        self.reply("Check", &(disk, target_json(target)?))
    }

    fn apply(&mut self, disk: &str, target: &Target) -> Result<Reply<Applied>, BackendError> {
        self.reply("Apply", &(disk, target_json(target)?))
    }

    fn last_run(&mut self) -> Result<Reply<Option<FinishedRun>>, BackendError> {
        // TODO(helper): the recovery system records the outcome (chunks, bytes, power cuts,
        // seconds) in the journal on the EFI partition. A `LastRun` D-Bus method should read
        // that journal and return it so the app reopens onto the finished screen after the
        // restart. Until that method exists on the helper, report nothing rather than invent a
        // result, so the real app opens on the launcher as before. Needs the hardware box to
        // implement and validate.
        Ok(Reply::Ok { value: None })
    }
}

fn target_json(target: &Target) -> Result<String, BackendError> {
    serde_json::to_string(target).map_err(failure)
}

fn failure(error: impl std::fmt::Display) -> BackendError {
    BackendError(error.to_string())
}

pub enum Request {
    ListDisks,
    LastRun,
    Probe { disk: String },
    Plan { disk: String, target: Target },
    Check { disk: String, target: Target },
    Apply { disk: String, target: Target },
}

pub enum Response {
    Disks(Result<Reply<Vec<DiskSummary>>, BackendError>),
    LastRun(Result<Reply<Option<FinishedRun>>, BackendError>),
    Probed(Result<Reply<Probed>, BackendError>),
    Planned(Result<Reply<Planned>, BackendError>),
    Checked(Result<Reply<Option<Vec<CheckResult>>>, BackendError>),
    Applied(Result<Reply<Applied>, BackendError>),
}

/// Runs backend calls one at a time on their own thread. Each answer carries the number of the
/// request it answers, so the app can drop answers it no longer wants.
pub struct Worker {
    mode: Mode,
    next_id: u64,
    is_demo: bool,
}

enum Mode {
    Thread {
        requests: Sender<(u64, Request)>,
        responses: Receiver<(u64, Response)>,
    },
    /// Answers each request as it is sent, for tests that need every answer in a known frame.
    #[cfg(test)]
    Inline {
        backend: Box<dyn Backend>,
        answers: VecDeque<(u64, Response)>,
    },
}

impl Worker {
    pub fn spawn(mut backend: Box<dyn Backend>, wake: impl Fn() + Send + 'static) -> Self {
        let is_demo = backend.is_demo();
        let (requests, inbox) = mpsc::channel::<(u64, Request)>();
        let (outbox, responses) = mpsc::channel();
        thread::spawn(move || {
            for (id, request) in inbox {
                let response = serve(backend.as_mut(), request);
                if outbox.send((id, response)).is_err() {
                    break;
                }
                wake();
            }
        });
        Self {
            mode: Mode::Thread {
                requests,
                responses,
            },
            next_id: 0,
            is_demo,
        }
    }

    #[cfg(test)]
    pub fn inline(backend: Box<dyn Backend>) -> Self {
        Self {
            is_demo: backend.is_demo(),
            mode: Mode::Inline {
                backend,
                answers: VecDeque::new(),
            },
            next_id: 0,
        }
    }

    pub fn is_demo(&self) -> bool {
        self.is_demo
    }

    pub fn send(&mut self, request: Request) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        match &mut self.mode {
            // A closed channel means the worker thread is gone; the request then never
            // answers, which leaves every gated button disabled.
            Mode::Thread { requests, .. } => {
                let _ = requests.send((id, request));
            }
            #[cfg(test)]
            Mode::Inline { backend, answers } => {
                answers.push_back((id, serve(backend.as_mut(), request)));
            }
        }
        id
    }

    pub fn try_recv(&mut self) -> Option<(u64, Response)> {
        match &mut self.mode {
            Mode::Thread { responses, .. } => responses.try_recv().ok(),
            #[cfg(test)]
            Mode::Inline { answers, .. } => answers.pop_front(),
        }
    }
}

fn serve(backend: &mut dyn Backend, request: Request) -> Response {
    match request {
        Request::ListDisks => Response::Disks(backend.list_disks()),
        Request::LastRun => Response::LastRun(backend.last_run()),
        Request::Probe { disk } => Response::Probed(backend.probe(&disk)),
        Request::Plan { disk, target } => Response::Planned(backend.plan(&disk, &target)),
        Request::Check { disk, target } => Response::Checked(backend.check(&disk, &target)),
        Request::Apply { disk, target } => Response::Applied(backend.apply(&disk, &target)),
    }
}
