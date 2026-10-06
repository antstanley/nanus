//! Hold the exact model selection across an optionally admitted request/tool step.
use core::cell::{Cell, RefCell};

use nanus_ports::{LlmHandle, ReasoningEffort};

use super::AgentRunner;
use crate::BundleError;

#[derive(Default)]
pub(super) struct Selection {
    epoch: Cell<u64>,
    holds: Cell<u32>,
    pending: RefCell<Pending>,
}

#[derive(Default)]
struct Pending {
    model: Option<String>,
    effort: PendingEffort,
    llm: Option<LlmHandle>,
}

#[derive(Default)]
enum PendingEffort {
    #[default]
    Unchanged,
    Set(Option<ReasoningEffort>),
}

pub(super) struct Hold<'a> {
    runner: &'a AgentRunner,
}

impl Selection {
    pub(super) fn epoch(&self) -> Result<u64, BundleError> {
        let epoch = self.epoch.get();
        if epoch == u64::MAX {
            return Err(BundleError::context("model selection epoch exhausted"));
        }
        Ok(epoch)
    }

    fn changed(&self) {
        self.epoch.set(self.epoch.get().saturating_add(1));
    }

    pub(super) fn model(&self, model: &str) -> bool {
        if self.holds.get() == 0 {
            self.changed();
            return false;
        }
        self.pending.borrow_mut().model = Some(model.to_owned());
        true
    }

    pub(super) fn effort(&self, effort: Option<ReasoningEffort>) -> bool {
        if self.holds.get() == 0 {
            self.changed();
            return false;
        }
        self.pending.borrow_mut().effort = PendingEffort::Set(effort);
        true
    }

    pub(super) fn llm(&self, llm: &LlmHandle) -> bool {
        if self.holds.get() == 0 {
            self.changed();
            return false;
        }
        self.pending.borrow_mut().llm = Some(llm.clone());
        true
    }
}

impl AgentRunner {
    /// Holds the exact selection for a step when managed context or either admission port is
    /// active, so the adapter, model and effort a request was prepared under are the ones it is
    /// dispatched, validated and checkpointed under. Setters queue until the hold is released.
    pub(super) fn hold_selection(&self, managed: bool) -> Result<Option<Hold<'_>>, BundleError> {
        if !managed && self.admission.is_none() && self.records.is_none() {
            return Ok(None);
        }
        self.selection.epoch()?;
        let count = self
            .selection
            .holds
            .get()
            .checked_add(1)
            .ok_or_else(|| BundleError::context("model selection hold count exhausted"))?;
        self.selection.holds.set(count);
        assert!(count > 0);
        Ok(Some(Hold { runner: self }))
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let selection = &self.runner.selection;
        let count = selection.holds.get();
        assert!(count > 0);
        selection.holds.set(count.saturating_sub(1));
        if count != 1 {
            return;
        }
        let pending = core::mem::take(&mut *selection.pending.borrow_mut());
        let changed = pending.model.is_some()
            || !matches!(pending.effort, PendingEffort::Unchanged)
            || pending.llm.is_some();
        if let Some(model) = pending.model {
            *self.runner.model.borrow_mut() = model;
        }
        if let PendingEffort::Set(effort) = pending.effort {
            self.runner.effort.set(effort);
        }
        if let Some(llm) = pending.llm {
            *self.runner.llm.borrow_mut() = llm;
        }
        if changed {
            selection.changed();
        }
        assert_eq!(selection.holds.get(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exhausted_epochs_never_wrap_back_into_an_admissible_selection() {
        let selection = Selection::default();
        selection.epoch.set(u64::MAX - 1);
        assert_eq!(selection.epoch().ok(), Some(u64::MAX - 1));
        selection.changed();
        assert!(selection.epoch().is_err());
        selection.changed();
        assert_eq!(selection.epoch.get(), u64::MAX);
        assert!(selection.epoch().is_err());
    }
}
