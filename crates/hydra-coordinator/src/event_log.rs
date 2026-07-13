//! The SSE **event log** — a *pure function of the durable ledger prefix*, never a parallel-tracked
//! structure. An event exists only after its tokens are durable and the incremental detokenizer has
//! yielded complete UTF-8 for them; ids are **dense and stable** (1, 2, 3 …), so a client that
//! reconnects with `Last-Event-ID: k` replays exactly the events after `k` and sees a byte-identical
//! suffix (at-least-once with stable ids; the client's dedup is the exactly-once half, spec §8).

/// One streamed event: a dense stable id, its UTF-8 text (SSE `data:`), and the durable output
/// position its tokens reached (for diagnostics / the resume gate).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub id: u64,
    pub data: String,
    pub last_output_pos: i64,
}

/// Append-only log of emitted events. Built by [`Session`](crate::session::Session) from durable
/// commits only.
#[derive(Default, Debug)]
pub struct EventLog {
    events: Vec<Event>,
}

impl EventLog {
    pub fn new() -> Self {
        EventLog { events: Vec::new() }
    }

    /// Append a new event with the next dense id; returns it. Only non-empty text should be
    /// appended (an event carries visible bytes).
    pub fn append(&mut self, data: String, last_output_pos: i64) -> Event {
        let id = self.events.len() as u64 + 1;
        let ev = Event { id, data, last_output_pos };
        self.events.push(ev.clone());
        ev
    }

    /// Events strictly after `last_event_id` (the resume replay). `0` yields the whole stream.
    pub fn since(&self, last_event_id: u64) -> &[Event] {
        let from = (last_event_id as usize).min(self.events.len());
        &self.events[from..]
    }

    pub fn all(&self) -> &[Event] {
        &self.events
    }

    pub fn last_id(&self) -> u64 {
        self.events.len() as u64
    }

    /// The full emitted text so far (concatenation of all event data).
    pub fn full_text(&self) -> String {
        self.events.iter().map(|e| e.data.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_dense_and_since_is_a_suffix() {
        let mut log = EventLog::new();
        log.append("Hel".into(), 0);
        log.append("lo, ".into(), 1);
        log.append("world".into(), 2);
        assert_eq!(log.last_id(), 3);
        assert_eq!(log.all().iter().map(|e| e.id).collect::<Vec<_>>(), vec![1, 2, 3]);

        let full = log.full_text();
        // For EVERY cut point, prefix + since(cut) reconstructs the whole stream byte-for-byte.
        for cut in 0..=log.last_id() {
            let prefix: String = log.since(0)[..cut as usize].iter().map(|e| e.data.as_str()).collect();
            let suffix: String = log.since(cut).iter().map(|e| e.data.as_str()).collect();
            assert_eq!(format!("{prefix}{suffix}"), full, "resume at {cut} must be byte-identical");
        }
        // A cut past the end yields nothing.
        assert!(log.since(99).is_empty());
    }
}
