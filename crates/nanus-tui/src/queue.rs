//! Prompts typed while a turn is already running.
//!
//! The agent serves one turn per session and refuses a second prompt rather than
//! interleaving it with the first, because a turn owns the session's log. A reader who
//! types ahead therefore has nowhere to put their words unless the interface holds them,
//! and that is what this is: a plain FIFO that the runtime appends to when a prompt cannot
//! be sent, and drains one entry per turn end.
//!
//! The queue belongs to the interface rather than to the session, and that is deliberate.
//! A prompt nobody has sent yet is not part of the conversation and has no place in the
//! log; it is a thing one terminal is about to say, and it should die with that terminal
//! rather than being replayed to the next client that attaches. The agent is still the
//! authority on what runs: the interface only waits for the ending frame it is already
//! watching for, and sends the next prompt when the agent is ready for one.

/// Prompts waiting for the running turn to end, oldest first.
///
/// The oldest is the one that runs next, so the front is what [`Queue::pop_front`]
/// returns and what the runtime sends. Positions are stable while a reader edits an
/// entry: editing *removes* the prompt from the queue and holds it in the composer, so a
/// turn that ends mid-edit cannot send the half-read text — the editor puts it back at
/// the position it came from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    prompts: Vec<String>,
}

impl Queue {
    /// Creates an empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            prompts: Vec::new(),
        }
    }

    /// Returns `true` when there is nothing waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }

    /// Returns how many prompts are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.prompts.len()
    }

    /// The index of the last prompt, or `None` when the queue is empty.
    ///
    /// A selection is clamped through this rather than by subtracting one from the
    /// length, because the length is zero exactly when there is nothing to select.
    #[must_use]
    pub fn last_index(&self) -> Option<usize> {
        self.prompts.len().checked_sub(1)
    }

    /// Adds a prompt to the end.
    pub fn push(&mut self, prompt: String) {
        self.prompts.push(prompt);
    }

    /// Takes the oldest prompt, if there is one.
    pub fn pop_front(&mut self) -> Option<String> {
        if self.prompts.is_empty() {
            return None;
        }
        Some(self.prompts.remove(0))
    }

    /// The prompt at `index`, if there is one.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        self.prompts.get(index).map(String::as_str)
    }

    /// The prompts, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.prompts.iter().map(String::as_str)
    }

    /// Removes and returns the prompt at `index`, if there is one.
    pub fn remove(&mut self, index: usize) -> Option<String> {
        if index < self.prompts.len() {
            Some(self.prompts.remove(index))
        } else {
            None
        }
    }

    /// Puts `prompt` back at `index`, clamped to the end of the queue.
    ///
    /// Clamped rather than refused because the position is a hint: an edit that began
    /// when the entry was fifth should come back near the front if the four before it
    /// were sent while it was being edited, but there is no reason for it to be lost.
    pub fn insert(&mut self, index: usize, prompt: String) {
        let index = index.min(self.prompts.len());
        self.prompts.insert(index, prompt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_queue_is_empty() {
        let queue = Queue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.last_index(), None);
        assert_eq!(queue.get(0), None);
        assert_eq!(queue.iter().count(), 0);
    }

    #[test]
    fn prompts_leave_in_the_order_they_arrived() {
        let mut queue = Queue::new();
        queue.push("first".to_owned());
        queue.push("second".to_owned());
        queue.push("third".to_owned());
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.last_index(), Some(2));
        assert_eq!(
            queue.iter().collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(queue.pop_front().as_deref(), Some("first"));
        assert_eq!(queue.pop_front().as_deref(), Some("second"));
        assert_eq!(queue.pop_front().as_deref(), Some("third"));
        assert!(queue.is_empty());
        assert_eq!(queue.pop_front(), None);
    }

    /// Removing an entry a reader no longer wants must not disturb the order of the ones
    /// around it, because that order is the order they will run in.
    #[test]
    fn removing_an_entry_closes_the_gap() {
        let mut queue = Queue::new();
        queue.push("first".to_owned());
        queue.push("second".to_owned());
        queue.push("third".to_owned());
        assert_eq!(queue.remove(1).as_deref(), Some("second"));
        assert_eq!(queue.iter().collect::<Vec<_>>(), ["first", "third"]);
        assert_eq!(
            queue.remove(2),
            None,
            "an index that is not there is not one"
        );
    }

    /// An edit puts the prompt back where it came from, and a position that no longer
    /// exists clamps to the end rather than being refused.
    #[test]
    fn an_edited_prompt_returns_to_its_place() {
        let mut queue = Queue::new();
        queue.push("first".to_owned());
        queue.push("second".to_owned());
        let edited = queue.remove(0).expect("the entry is there to edit");
        assert_eq!(edited, "first");
        assert_eq!(queue.iter().collect::<Vec<_>>(), ["second"]);
        queue.insert(0, "first, revised".to_owned());
        assert_eq!(
            queue.iter().collect::<Vec<_>>(),
            ["first, revised", "second"]
        );

        // The entries before it were sent while it was being edited, so the old position
        // is past the end; it goes to the back rather than nowhere.
        let mut shrunk = Queue::new();
        shrunk.push("only".to_owned());
        shrunk.insert(7, "later".to_owned());
        assert_eq!(shrunk.iter().collect::<Vec<_>>(), ["only", "later"]);
    }
}
