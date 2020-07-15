//! State maintained by the light client.

use crate::{
    store::LightStore,
    types::{Height, LightBlock, Status},
};

use contracts::*;
use std::collections::{HashMap, HashSet};

/// Records which blocks were needed to verify a target block, eg. during bisection.
pub type VerificationTrace = HashMap<Height, HashSet<Height>>;

/// The state managed by the light client.
#[derive(Debug)]
pub struct State<LB> {
    /// Store for light blocks.
    pub light_store: Box<dyn LightStore<LB>>,

    /// Records which blocks were needed to verify a target block, eg. during bisection.
    pub verification_trace: VerificationTrace,
}

impl<LB> State<LB>
where
    LB: LightBlock,
{
    pub fn new(light_store: impl LightStore<LB> + 'static) -> Self {
        Self {
            light_store: Box::new(light_store),
            verification_trace: VerificationTrace::new(),
        }
    }

    /// Record that the block at `height` was needed to verify the block at `target_height`.
    ///
    /// ## Preconditions
    /// - `height` < `target_height`
    #[pre(height <= target_height)]
    pub fn trace_block(&mut self, target_height: Height, height: Height) {
        self.verification_trace
            .entry(target_height)
            .or_insert_with(HashSet::new)
            .insert(height);
    }

    /// Get the verification trace for the block at `target_height`.
    pub fn get_trace(&self, target_height: Height) -> Vec<LB> {
        let mut trace = self
            .verification_trace
            .get(&target_height)
            .unwrap_or(&HashSet::new())
            .iter()
            .flat_map(|h| self.light_store.get(*h, Status::Verified))
            .collect::<Vec<_>>();

        trace.sort_by_key(|lb| lb.height());
        trace.reverse();
        trace
    }
}
