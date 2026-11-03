//! Per-run "steer" channel: lets the runtime hand a message to a backend run
//! that is already in progress, so the model sees it as soon as its current
//! tool call finishes instead of after the whole run.
//!
//! The channel is a small mutex-guarded inbox rather than a plain
//! `mpsc` pair so that closing it is atomic with respect to sending. Once a
//! run closes its inbox (it is ending, or its backend refused further input),
//! every later [`SteerSender::send`] fails and gives the message back, so the
//! sender can queue it for a later run instead. Nothing can be pushed into an
//! inbox that nobody will read again.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

struct Inbox {
    state: Mutex<InboxState>,
    ready: Condvar,
}

struct InboxState {
    messages: VecDeque<String>,
    closed: bool,
}

impl Inbox {
    fn lock(&self) -> MutexGuard<'_, InboxState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The runtime's end of a run's steer channel.
pub struct SteerSender {
    inbox: Arc<Inbox>,
}

/// The run's end of its steer channel. Cheap to clone; all clones share one
/// inbox.
#[derive(Clone)]
pub struct SteerReceiver {
    inbox: Arc<Inbox>,
}

/// What [`SteerReceiver::recv_timeout`] found.
#[derive(Debug, Eq, PartialEq)]
pub enum SteerRecv {
    Message(String),
    Timeout,
    /// The inbox is closed and empty; no message will ever arrive.
    Closed,
}

/// Create a connected sender and receiver.
pub fn steer_channel() -> (SteerSender, SteerReceiver) {
    let inbox = Arc::new(Inbox {
        state: Mutex::new(InboxState {
            messages: VecDeque::new(),
            closed: false,
        }),
        ready: Condvar::new(),
    });
    (
        SteerSender {
            inbox: Arc::clone(&inbox),
        },
        SteerReceiver { inbox },
    )
}

impl SteerSender {
    /// Hand `prompt` to the run. When the run has closed its inbox the prompt
    /// comes back unchanged so the caller can queue it instead.
    pub fn send(&self, prompt: String) -> Result<(), String> {
        let mut state = self.inbox.lock();
        if state.closed {
            return Err(prompt);
        }
        state.messages.push_back(prompt);
        drop(state);
        self.inbox.ready.notify_all();
        Ok(())
    }
}

impl SteerReceiver {
    /// The next message, if one is waiting.
    pub fn try_recv(&self) -> Option<String> {
        self.inbox.lock().messages.pop_front()
    }

    /// Wait up to `timeout` for the next message.
    pub fn recv_timeout(&self, timeout: Duration) -> SteerRecv {
        let deadline = Instant::now() + timeout;
        let mut state = self.inbox.lock();
        loop {
            if let Some(message) = state.messages.pop_front() {
                return SteerRecv::Message(message);
            }
            if state.closed {
                return SteerRecv::Closed;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return SteerRecv::Timeout;
            }
            state = self
                .inbox
                .ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// Refuse all further messages and return those still waiting, in order.
    /// Safe to call more than once.
    pub fn close(&self) -> Vec<String> {
        let mut state = self.inbox.lock();
        state.closed = true;
        let leftover = state.messages.drain(..).collect();
        drop(state);
        self.inbox.ready.notify_all();
        leftover
    }

    pub fn is_closed(&self) -> bool {
        self.inbox.lock().closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_arrive_in_order() {
        let (tx, rx) = steer_channel();
        tx.send("one".to_string()).unwrap();
        tx.send("two".to_string()).unwrap();
        assert_eq!(rx.try_recv().as_deref(), Some("one"));
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(10)),
            SteerRecv::Message("two".to_string())
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(10)),
            SteerRecv::Timeout
        );
    }

    #[test]
    fn closing_returns_waiting_messages_and_refuses_new_ones() {
        let (tx, rx) = steer_channel();
        tx.send("kept".to_string()).unwrap();
        assert_eq!(rx.close(), vec!["kept".to_string()]);
        assert_eq!(tx.send("late".to_string()), Err("late".to_string()));
        assert!(rx.is_closed());
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(10)),
            SteerRecv::Closed
        );
        assert!(rx.close().is_empty());
    }

    #[test]
    fn waiting_receiver_wakes_on_send() {
        let (tx, rx) = steer_channel();
        let waiter = std::thread::spawn(move || rx.recv_timeout(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(50));
        tx.send("hello".to_string()).unwrap();
        assert_eq!(
            waiter.join().unwrap(),
            SteerRecv::Message("hello".to_string())
        );
    }
}
