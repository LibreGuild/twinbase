//! Live log of what clients sent to a mock server (shown in the UI's traffic panel).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::broadcast;

const CAPACITY: usize = 2000;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrafficKind {
    Connect { peer: String, user: Option<String>, database: Option<String> },
    Disconnect,
    Query { text: String, duration_us: u64, rows: u64, error: Option<String> },
}

#[derive(Debug, Clone, Serialize)]
pub struct TrafficEvent {
    pub id: u64,
    /// Milliseconds since the Unix epoch.
    pub at: i64,
    pub connection: u64,
    #[serde(flatten)]
    pub kind: TrafficKind,
}

struct Inner {
    events: Mutex<VecDeque<TrafficEvent>>,
    next_id: AtomicU64,
    next_connection: AtomicU64,
    tx: broadcast::Sender<TrafficEvent>,
}

/// Bounded in-memory traffic log with live subscribers.
#[derive(Clone)]
pub struct TrafficLog {
    inner: Arc<Inner>,
}

impl Default for TrafficLog {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self {
            inner: Arc::new(Inner {
                events: Mutex::new(VecDeque::with_capacity(256)),
                next_id: AtomicU64::new(1),
                next_connection: AtomicU64::new(1),
                tx,
            }),
        }
    }
}

impl TrafficLog {
    pub fn new_connection_id(&self) -> u64 {
        self.inner.next_connection.fetch_add(1, Ordering::Relaxed)
    }

    pub fn record(&self, connection: u64, kind: TrafficKind) {
        let event = TrafficEvent {
            id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
            at: chrono::Utc::now().timestamp_millis(),
            connection,
            kind,
        };
        {
            let mut ev = self.inner.events.lock();
            if ev.len() >= CAPACITY {
                ev.pop_front();
            }
            ev.push_back(event.clone());
        }
        let _ = self.inner.tx.send(event);
    }

    pub fn query(&self, connection: u64, text: &str, started: std::time::Instant, rows: u64, error: Option<String>) {
        let text: String = text.chars().take(4000).collect();
        self.record(connection, TrafficKind::Query { text, duration_us: started.elapsed().as_micros() as u64, rows, error });
    }

    /// Record a fault injected into a connection (shown as a failed statement).
    pub fn fault(&self, connection: u64, text: &str, what: &str) {
        self.query(connection, text, std::time::Instant::now(), 0, Some(format!("[fault] {what}")));
    }

    /// Events with id greater than `after`, oldest first (at most `limit`).
    pub fn since(&self, after: u64, limit: usize) -> Vec<TrafficEvent> {
        let ev = self.inner.events.lock();
        let mut out: Vec<TrafficEvent> = ev.iter().filter(|e| e.id > after).cloned().collect();
        if out.len() > limit {
            out.drain(..out.len() - limit);
        }
        out
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TrafficEvent> {
        self.inner.tx.subscribe()
    }

    pub fn clear(&self) {
        self.inner.events.lock().clear();
    }
}
