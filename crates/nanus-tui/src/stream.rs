//! Where this interface stands in the session's stream: the epoch, the watermark, and what the
//! store holds.
//!
//! An attachment tells the interface three things about the session it joined: which stream it is
//! (the epoch), the last frame id the attachment accounts for (the watermark), and how much of the
//! log the store held at that instant (the durable frontier). The transcript was built from the
//! store through that frontier and the backlog after it, so a context frame at or below the
//! watermark is one the attachment already accounted for, a frame from another epoch belongs to a
//! stream this interface never joined, and a checkpoint moves only the watermark and the frontier.
//!
//! The rules are here rather than in the runtime because they are a pure function of numbers and
//! strings: the view-only build tests them without an agent, a link or a store.

use std::collections::VecDeque;

/// How many recent frame ids are remembered, to drop one that arrives twice.
///
/// Ids increase, so anything at or below the watermark is already dropped; this catches only a
/// repeat of a recent one above it. Context frames are one per step and per decision, so a short
/// window is a long stretch of conversation.
const RECENT_IDS: usize = 64;

/// What to do with a frame that carries a stream position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Apply it: it is new to this interface.
    Apply,
    /// Ignore it: the attachment already accounted for it, or it already arrived.
    Duplicate,
    /// Refuse it: it belongs to a stream this interface did not attach to.
    ForeignEpoch,
}

/// This interface's position in one session's stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamCursor {
    /// The epoch the attachment named; `None` before an attachment, or for a recording.
    epoch: Option<String>,
    /// The attachment's watermark: every id at or below it is already accounted for.
    watermark: u64,
    /// The newest id applied since.
    newest: u64,
    /// Recent ids above the watermark, to drop a repeat.
    recent: VecDeque<u64>,
    /// How many events the store holds, as far as this interface has been told.
    durable_events: u64,
}

impl StreamCursor {
    /// The position an attachment establishes.
    #[must_use]
    pub fn attached(epoch: impl Into<String>, watermark: u64, durable_events: u64) -> Self {
        Self {
            epoch: Some(epoch.into()),
            watermark,
            newest: watermark,
            recent: VecDeque::new(),
            durable_events,
        }
    }

    /// The epoch this interface attached to, if it has attached.
    #[must_use]
    pub fn epoch(&self) -> Option<&str> {
        self.epoch.as_deref()
    }

    /// The newest frame id this interface has accounted for.
    #[must_use]
    pub const fn watermark(&self) -> u64 {
        if self.newest > self.watermark {
            self.newest
        } else {
            self.watermark
        }
    }

    /// How many events the store holds, as far as this interface has been told.
    #[must_use]
    pub const fn durable_events(&self) -> u64 {
        self.durable_events
    }

    /// Whether a batch identified by `epoch` belongs to this stream.
    ///
    /// A cursor that never attached — a recording — accepts nothing as its own.
    #[must_use]
    pub fn is_own(&self, epoch: &str) -> bool {
        self.epoch.as_deref() == Some(epoch)
    }

    /// Decides what to do with a live frame, and records it when it is to be applied.
    pub fn admit(&mut self, epoch: &str, frame_id: u64) -> Admission {
        if !self.is_own(epoch) {
            return Admission::ForeignEpoch;
        }
        if frame_id <= self.watermark || self.recent.contains(&frame_id) {
            return Admission::Duplicate;
        }
        if self.recent.len() == RECENT_IDS {
            self.recent.pop_front();
        }
        self.recent.push_back(frame_id);
        self.newest = self.newest.max(frame_id);
        assert!(self.recent.len() <= RECENT_IDS, "the window stays bounded");
        Admission::Apply
    }

    /// Notes that the store now holds `events` events; an older report moves nothing back.
    pub fn durable(&mut self, events: u64) {
        self.durable_events = self.durable_events.max(events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame the attachment accounted for is dropped, a new one is applied once, and a repeat
    /// of it is dropped — which is what "exactly one copy" means for a frame with an id.
    #[test]
    fn an_id_at_or_below_the_watermark_or_seen_before_is_dropped() {
        let mut cursor = StreamCursor::attached("e1", 10, 4);
        assert_eq!(cursor.admit("e1", 9), Admission::Duplicate);
        assert_eq!(cursor.admit("e1", 10), Admission::Duplicate);
        assert_eq!(cursor.admit("e1", 12), Admission::Apply);
        assert_eq!(cursor.admit("e1", 12), Admission::Duplicate);
        // A frame between the watermark and the newest, not yet seen, is still new: the agent
        // assigns ids as it produces frames, and two queues can deliver them out of step.
        assert_eq!(cursor.admit("e1", 11), Admission::Apply);
        assert_eq!(cursor.watermark(), 12);
    }

    /// A frame from another stream is refused rather than compared: ids start again in a new
    /// epoch, so comparing them would drop a new frame or apply a stale one.
    #[test]
    fn a_frame_from_another_epoch_is_refused() {
        let mut cursor = StreamCursor::attached("e1", 10, 4);
        assert_eq!(cursor.admit("e2", 11), Admission::ForeignEpoch);
        assert_eq!(cursor.admit("e2", 1), Admission::ForeignEpoch);
        assert!(cursor.is_own("e1"));
        assert!(!cursor.is_own("e2"));
        // And an interface that never attached has no stream at all.
        let mut unattached = StreamCursor::default();
        assert_eq!(unattached.admit("e1", 1), Admission::ForeignEpoch);
        assert_eq!(unattached.epoch(), None);
    }

    /// The durable count only moves forward.
    #[test]
    fn the_durable_count_never_moves_back() {
        let mut cursor = StreamCursor::attached("e1", 0, 4);
        cursor.durable(9);
        assert_eq!(cursor.durable_events(), 9);
        cursor.durable(6);
        assert_eq!(cursor.durable_events(), 9);
    }

    /// The window of recent ids is bounded however long a session runs.
    #[test]
    fn the_window_of_recent_ids_stays_bounded() {
        let mut cursor = StreamCursor::attached("e1", 0, 0);
        for id in 1..=500 {
            assert_eq!(cursor.admit("e1", id), Admission::Apply);
        }
        assert_eq!(cursor.recent.len(), RECENT_IDS);
        assert_eq!(cursor.admit("e1", 500), Admission::Duplicate);
        assert_eq!(cursor.watermark(), 500);
    }
}
