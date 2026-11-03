//! Transport: dispatch a member run on a worker thread and forward its
//! [`AgentEvent`]s back to the runtime loop, tagged with the member.

use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use crate::adapter::{MemberRunner, RunRequest};
use crate::domain::event::AgentEvent;
use crate::runtime::RuntimeInput;
use crate::runtime::team_runtime::RunAction;

const AGENT_EVENT_QUEUE_CAPACITY: usize = 256;

/// Start `action` on a detached worker thread. The runner streams `AgentEvent`s
/// into a per-run channel; a forwarder relays them to the runtime loop as
/// [`RuntimeInput::Agent`].
pub fn dispatch(
    runner: Arc<dyn MemberRunner>,
    action: RunAction,
    input_tx: SyncSender<RuntimeInput>,
) -> thread::JoinHandle<()> {
    let RunAction {
        member,
        prompt,
        session,
        cancel,
        effort,
        steer,
    } = action;
    // A runner without a mid-turn input path must refuse steers up front, so
    // the runtime queues such messages instead of waiting on a run that will
    // never read them.
    let steer = steer.filter(|steer| {
        let supported = runner.supports_steer();
        if !supported {
            steer.close();
        }
        supported
    });

    thread::spawn(move || {
        // Bound the per-run queue so a backend that floods many individually
        // valid small events is backpressured through to its stdout pipe.
        let (ev_tx, ev_rx) = mpsc::sync_channel::<AgentEvent>(AGENT_EVENT_QUEUE_CAPACITY);
        let forward_member = member.clone();
        let forward_cancel = Arc::clone(&cancel);
        let forwarder = thread::spawn(move || {
            let mut saw_exit = false;
            let mut saw_fatal = false;
            while let Ok(event) = ev_rx.recv() {
                saw_exit |= matches!(event, AgentEvent::Exited { .. });
                saw_fatal |= matches!(event, AgentEvent::Fatal(_));
                if input_tx
                    .send(RuntimeInput::Agent(forward_member.clone(), event))
                    .is_err()
                {
                    return;
                }
            }
            if !saw_exit {
                if !saw_fatal && !forward_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = input_tx.send(RuntimeInput::Agent(
                        forward_member.clone(),
                        AgentEvent::Fatal(
                            "backend runner stopped without an exit event".to_string(),
                        ),
                    ));
                }
                let _ = input_tx.send(RuntimeInput::Agent(
                    forward_member,
                    AgentEvent::Exited {
                        code: None,
                        ok: false,
                    },
                ));
            }
        });

        runner.run(
            RunRequest {
                prompt,
                session,
                cancel,
                effort,
                steer: steer.clone(),
            },
            ev_tx,
        );
        // Whatever the runner did, nothing may be steered into a run that has
        // returned.
        if let Some(steer) = steer {
            steer.close();
        }
        let _ = forwarder.join();
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use crate::domain::team::BackendKind;

    struct SilentRunner;

    impl MemberRunner for SilentRunner {
        fn backend(&self) -> BackendKind {
            BackendKind::Codex
        }

        fn run(&self, _req: RunRequest, _events: SyncSender<AgentEvent>) {}
    }

    #[test]
    fn runner_return_without_exit_is_closed_as_failure() {
        let (input_tx, input_rx) = mpsc::sync_channel(8);
        let worker = dispatch(
            Arc::new(SilentRunner),
            RunAction {
                member: "builder".into(),
                prompt: "test".to_string(),
                session: None,
                cancel: Arc::new(AtomicBool::new(false)),
                effort: None,
                steer: None,
            },
            input_tx,
        );

        let first = input_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            first,
            RuntimeInput::Agent(_, AgentEvent::Fatal(message))
                if message.contains("without an exit event")
        ));
        let second = input_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            second,
            RuntimeInput::Agent(_, AgentEvent::Exited { ok: false, .. })
        ));
        worker.join().unwrap();
    }

    #[test]
    fn dispatch_forwards_the_members_effort_to_the_runner() {
        use crate::adapter::fake::FakeRunner;
        use crate::domain::team::{BackendKind, Effort};
        use std::sync::Mutex;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let runner = FakeRunner::new(BackendKind::Codex, move |req| {
            recorder.lock().unwrap().push(req.effort);
            Vec::new()
        });
        let (input_tx, input_rx) = mpsc::sync_channel(8);
        let worker = dispatch(
            Arc::new(runner),
            RunAction {
                member: "builder".into(),
                prompt: "test".to_string(),
                session: None,
                cancel: Arc::new(AtomicBool::new(false)),
                effort: Some(Effort::High),
                steer: None,
            },
            input_tx,
        );
        worker.join().unwrap();
        drop(input_rx);

        assert_eq!(*seen.lock().unwrap(), vec![Some(Effort::High)]);
    }

    #[test]
    fn runner_without_steering_gets_its_steer_channel_closed_at_dispatch() {
        use crate::adapter::{FakeRunner, steer_channel};
        let (tx, rx) = steer_channel();
        let runner = FakeRunner::new(BackendKind::Agy, |_| Vec::new());
        assert!(!runner.supports_steer());
        let (input_tx, input_rx) = mpsc::sync_channel(8);
        let worker = dispatch(
            Arc::new(runner),
            RunAction {
                member: "helper".into(),
                prompt: "test".to_string(),
                session: None,
                cancel: Arc::new(AtomicBool::new(false)),
                effort: None,
                steer: Some(rx),
            },
            input_tx,
        );
        worker.join().unwrap();
        drop(input_rx);
        assert_eq!(tx.send("late".to_string()), Err("late".to_string()));
    }

    #[test]
    fn steer_channel_is_closed_once_the_run_returns() {
        use crate::adapter::steer_channel;
        struct SteerableRunner(Arc<std::sync::Mutex<Option<String>>>);
        impl MemberRunner for SteerableRunner {
            fn backend(&self) -> BackendKind {
                BackendKind::Codex
            }
            fn supports_steer(&self) -> bool {
                true
            }
            fn run(&self, req: RunRequest, events: SyncSender<AgentEvent>) {
                let steer = req.steer.expect("steerable runs get a channel");
                *self.0.lock().unwrap() = steer.try_recv();
                let _ = events.send(AgentEvent::Exited {
                    code: None,
                    ok: true,
                });
            }
        }
        let (tx, rx) = steer_channel();
        tx.send("while running".to_string()).unwrap();
        let seen = Arc::new(std::sync::Mutex::new(None));
        let (input_tx, input_rx) = mpsc::sync_channel(8);
        let worker = dispatch(
            Arc::new(SteerableRunner(Arc::clone(&seen))),
            RunAction {
                member: "helper".into(),
                prompt: "test".to_string(),
                session: None,
                cancel: Arc::new(AtomicBool::new(false)),
                effort: None,
                steer: Some(rx),
            },
            input_tx,
        );
        worker.join().unwrap();
        drop(input_rx);
        assert_eq!(seen.lock().unwrap().as_deref(), Some("while running"));
        assert!(tx.send("after".to_string()).is_err());
    }
}
