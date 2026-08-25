//! The one thread that owns the composition.
//!
//! Every mutation of the fiber graph happens here, one at a time, because the
//! ordering the model rests on assumes transitions do not interleave. Reads do
//! not come here: a component resolves its services from a committed view and
//! a caller reads the roster from a published [`Snapshot`], neither of which
//! needs the writer.
//!
//! It is also the thread anything else that must not interleave runs on. A
//! script interpreter is the motivating case: several callers used to reach the
//! same engine at once, each reading its state, changing it and writing it
//! back, and one of the writes was lost. Handing that work to the reactor
//! removes the interleaving rather than guarding it.
//!
//! # Why a thread and not a task
//!
//! A spawned task only progresses while somebody polls the runtime it was
//! spawned on, and between agent runs nobody does — reloads and timers would
//! stall until the next prompt. A plain thread with a `current_thread` runtime
//! keeps its own time. It is the same shape, and for the same reason, as the
//! worker thread that blocking capabilities already run on.
//!
//! # What must not happen here
//!
//! Nothing on this thread may block waiting for something that is itself
//! waiting on this thread. In practice: a disposer must not call a blocking
//! capability, and no caller outside blocks on a reply it has not been promised
//! one for. Submitting work is fire-and-forget; the answer, when there is one,
//! comes back on a channel.

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::component::Component;
use super::fiber::Status;
use super::runtime::Runtime;
use super::uid::Uid;
use crate::tool::BoxFuture;

/// What the reactor was asked to do.
pub enum Job {
    Mount {
        component: Arc<dyn Component>,
        config: Value,
        reply: Option<oneshot::Sender<Result<Uid, String>>>,
    },
    Unmount(Uid),
    Enable(Uid),
    /// Run pending transitions. Harmless to ask for twice.
    Settle,
    /// Arbitrary work that has to run where the composition runs.
    ///
    /// `coalesce` names a class of job that a repeat adds nothing to: a redraw
    /// always draws the state it finds, and a timer tick that has not run yet
    /// is not made more true by asking again. Two jobs sharing a name collapse
    /// to the later one. `None` means every one of these matters.
    Task {
        coalesce: Option<&'static str>,
        work: Box<dyn FnOnce(Runtime) -> BoxFuture<'static, ()> + Send>,
    },
}

impl Job {
    fn coalesce_key(&self) -> Option<&'static str> {
        match self {
            Job::Settle => Some("settle"),
            Job::Task { coalesce, .. } => *coalesce,
            _ => None,
        }
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Job::Mount { .. } => f.write_str("Mount"),
            Job::Unmount(uid) => write!(f, "Unmount({uid})"),
            Job::Enable(uid) => write!(f, "Enable({uid})"),
            Job::Settle => f.write_str("Settle"),
            Job::Task { coalesce, .. } => write!(f, "Task({coalesce:?})"),
        }
    }
}

/// What the composition looks like right now, frozen.
///
/// Published by the reactor whenever the graph changes and read by everyone
/// else. Holding one costs a pointer and never blocks the writer, which is what
/// keeps reading off the coordination path entirely.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub roster: Vec<Status>,
}

impl Snapshot {
    pub fn active(&self) -> impl Iterator<Item = &Status> {
        self.roster.iter().filter(|status| status.state.is_loaded())
    }

    /// The fibers that are waiting on a service nobody provides.
    ///
    /// The first thing to look at when a component reports nothing and does
    /// nothing, because waiting is a legitimate state and therefore a silent
    /// one.
    pub fn waiting(&self) -> impl Iterator<Item = &Status> {
        self.roster
            .iter()
            .filter(|status| !status.missing.is_empty())
    }
}

/// A handle to the reactor thread.
pub struct Reactor {
    jobs: Option<mpsc::UnboundedSender<Job>>,
    snapshot: Arc<RwLock<Arc<Snapshot>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Reactor {
    /// Start the thread.
    ///
    /// `published` is called on the reactor thread each time the composition
    /// changes, so a front end can refresh what it caches without asking.
    #[must_use]
    pub fn spawn(published: impl Fn(&Arc<Snapshot>) + Send + 'static) -> Reactor {
        let (jobs, inbox) = mpsc::unbounded_channel::<Job>();
        let snapshot = Arc::new(RwLock::new(Arc::new(Snapshot::default())));
        let shared = Arc::clone(&snapshot);

        let thread = std::thread::Builder::new()
            .name("aphid-reactor".to_owned())
            .spawn(move || serve(inbox, &shared, &published))
            // A harness that cannot start its reactor still runs; the
            // composition simply stays empty, which is a quieter failure than
            // refusing to open a session.
            .ok();

        Reactor {
            jobs: Some(jobs),
            snapshot,
            thread,
        }
    }

    /// Queue a job. Nothing waits for it.
    pub fn send(&self, job: Job) {
        if let Some(jobs) = self.jobs.as_ref() {
            let _ = jobs.send(job);
        }
    }

    /// Mount a component and wait for the reactor to say whether it took.
    ///
    /// # Errors
    ///
    /// The component's own refusal — bad configuration, or a dependency cycle
    /// — or a note that the reactor is gone.
    pub async fn mount(&self, component: Arc<dyn Component>, config: Value) -> Result<Uid, String> {
        let (reply, answer) = oneshot::channel();
        self.send(Job::Mount {
            component,
            config,
            reply: Some(reply),
        });
        answer
            .await
            .map_err(|_| "the reactor stopped before it answered".to_owned())?
    }

    /// The composition as of the last change.
    #[must_use]
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot
            .read()
            .map(|current| Arc::clone(&current))
            .unwrap_or_default()
    }

    /// Finish what is queued, unload everything, then stop.
    pub fn stop(mut self) {
        // Dropping the sender is what ends the loop, once it has drained.
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl std::fmt::Debug for Reactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reactor")
    }
}

fn serve(
    inbox: mpsc::UnboundedReceiver<Job>,
    snapshot: &Arc<RwLock<Arc<Snapshot>>>,
    published: &(impl Fn(&Arc<Snapshot>) + Send + 'static),
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    else {
        return;
    };
    runtime.block_on(reactor(inbox, snapshot, published));
}

async fn reactor(
    mut inbox: mpsc::UnboundedReceiver<Job>,
    snapshot: &Arc<RwLock<Arc<Snapshot>>>,
    published: &(impl Fn(&Arc<Snapshot>) + Send + 'static),
) {
    let rt = Runtime::new();

    loop {
        let Some(first) = inbox.recv().await else {
            break;
        };
        let mut replies = Vec::new();
        for job in drain(first, &mut inbox) {
            run(job, &rt, &mut replies).await;
        }
        rt.settle().await;
        // Published before anyone is answered, and answered only after the
        // batch settles. A caller that awaits a mount means "and it is
        // loaded"; waking it any earlier hands it a snapshot that does not yet
        // contain what it just asked for, which is a race it cannot wait out.
        publish(&rt, snapshot, published);
        for (reply, outcome) in replies {
            let _ = reply.send(outcome);
        }
    }

    rt.shutdown().await;
    publish(&rt, snapshot, published);
}

/// Everything waiting right now, with the repeats that add nothing removed.
///
/// The **last** of each coalescing kind is kept, not the first: a batch can
/// hold a redraw, then work that changes what would be drawn, then another
/// redraw, and keeping the first would draw the state as it was and throw away
/// the ask that would have drawn it as it is.
fn drain(first: Job, inbox: &mut mpsc::UnboundedReceiver<Job>) -> Vec<Job> {
    let mut queue = VecDeque::from([first]);
    while let Ok(job) = inbox.try_recv() {
        queue.push_back(job);
    }

    let mut kept: Vec<Job> = Vec::with_capacity(queue.len());
    let mut seen: Vec<&'static str> = Vec::new();
    while let Some(job) = queue.pop_back() {
        if let Some(key) = job.coalesce_key() {
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
        }
        kept.push(job);
    }
    kept.reverse();
    kept
}

type Reply = (oneshot::Sender<Result<Uid, String>>, Result<Uid, String>);

async fn run(job: Job, rt: &Runtime, replies: &mut Vec<Reply>) {
    match job {
        Job::Mount {
            component,
            config,
            reply,
        } => {
            let outcome = rt.mount(component, config);
            if let Some(reply) = reply {
                replies.push((reply, outcome));
            }
        }
        Job::Unmount(uid) => rt.unmount(uid).await,
        Job::Enable(uid) => rt.enable(uid).await,
        Job::Settle => rt.settle().await,
        Job::Task { work, .. } => work(rt.clone()).await,
    }
}

fn publish(
    rt: &Runtime,
    snapshot: &Arc<RwLock<Arc<Snapshot>>>,
    published: &impl Fn(&Arc<Snapshot>),
) {
    let next = Arc::new(Snapshot {
        roster: rt.roster(),
    });
    if let Ok(mut current) = snapshot.write() {
        *current = Arc::clone(&next);
    }
    published(&next);
}

#[cfg(test)]
mod tests {
    use super::{Job, drain};
    use tokio::sync::mpsc;

    fn task(coalesce: Option<&'static str>) -> Job {
        Job::Task {
            coalesce,
            work: Box::new(|_| Box::pin(async {})),
        }
    }

    fn kinds(jobs: &[Job]) -> Vec<String> {
        jobs.iter().map(|job| format!("{job:?}")).collect()
    }

    fn batch(jobs: Vec<Job>) -> Vec<Job> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut jobs = jobs.into_iter();
        let first = jobs.next().expect("at least one");
        for job in jobs {
            tx.send(job).expect("receiver is alive");
        }
        drop(tx);
        drain(first, &mut rx)
    }

    #[test]
    fn a_tick_asked_for_twice_runs_once() {
        let kept = batch(vec![
            task(Some("tick")),
            task(Some("tick")),
            task(Some("tick")),
        ]);
        assert_eq!(kinds(&kept), [r#"Task(Some("tick"))"#]);
    }

    #[test]
    fn settling_twice_settles_once() {
        let kept = batch(vec![Job::Settle, Job::Settle]);
        assert_eq!(kinds(&kept), ["Settle"]);
    }

    #[test]
    fn work_that_does_not_coalesce_is_kept_in_order() {
        // Two notices are two things the user was shown. Neither stands in for
        // the other.
        let kept = batch(vec![
            task(None),
            task(Some("tick")),
            task(None),
            task(Some("tick")),
        ]);
        assert_eq!(
            kinds(&kept),
            ["Task(None)", "Task(None)", r#"Task(Some("tick"))"#]
        );
    }

    #[test]
    fn a_redraw_keeps_the_place_of_the_last_one_asked_for() {
        // The one that matters is the one after the work that changed what
        // would be drawn.
        let kept = batch(vec![task(Some("draw")), task(None), task(Some("draw"))]);
        assert_eq!(kinds(&kept), ["Task(None)", r#"Task(Some("draw"))"#]);
    }
}
