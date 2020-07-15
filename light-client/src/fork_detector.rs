use serde::{Deserialize, Serialize};

use crate::{
    errors::{Error, ErrorExt, ErrorKind},
    operations::{Hasher, ProdHasher},
    state::State,
    store::memory::MemoryStore,
    supervisor::Instance,
    types::{LightBlock, PeerId, Status, TMLightBlock},
};

/// Result of fork detection
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ForkDetection<LB> {
    /// One or more forks have been detected
    Detected(Vec<Fork<LB>>),
    /// No fork has been detected
    NotDetected,
}

/// Types of fork
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Fork<LB> {
    /// An actual fork was found for this `LightBlock`
    Forked { primary: LB, witness: LB },
    /// The node has been deemed faulty for this `LightBlock`
    Faulty(LB, ErrorKind),
    /// The node has timed out
    Timeout(PeerId, ErrorKind),
}

/// Interface for a fork detector
pub trait ForkDetector<LB: LightBlock>: Send {
    /// Detect forks using the given verified block, trusted block,
    /// and list of witnesses to verify the given light block against.
    fn detect_forks(
        &self,
        verified_block: &LB,
        trusted_block: &LB,
        witnesses: Vec<&Instance<LB>>,
    ) -> Result<ForkDetection<LB>, Error>;
}

/// A production-ready fork detector which compares
/// light blocks fetched from the witnesses by hash.
/// If the hashes don't match, this fork detector
/// then attempts to verify the light block pulled from
/// the witness against a light block containing only
/// the given trusted state, and then:
///
/// - If the verification succeeds, we have a real fork
/// - If verification fails because of lack of trust,
///   we have a potential fork.
/// - If verification fails for any other reason, the
///   witness is deemed faulty.
pub struct ProdForkDetector<LB>
where
    LB: LightBlock,
{
    hasher: Box<dyn Hasher<LB>>,
}

impl<LB> ProdForkDetector<LB>
where
    LB: LightBlock,
{
    /// Construct a new fork detector that will use the given header hasher.
    pub fn new(hasher: impl Hasher<LB> + 'static) -> Self {
        Self {
            hasher: Box::new(hasher),
        }
    }
}

impl Default for ProdForkDetector<TMLightBlock> {
    fn default() -> Self {
        Self::new(ProdHasher::default())
    }
}

impl<LB> ForkDetector<LB> for ProdForkDetector<LB>
where
    LB: LightBlock,
{
    /// Perform fork detection. See the documentation `ProdForkDetector` for details.
    fn detect_forks(
        &self,
        verified_block: &LB,
        trusted_block: &LB,
        witnesses: Vec<&Instance<LB>>,
    ) -> Result<ForkDetection<LB>, Error> {
        let primary_hash = self.hasher.hash_header(&verified_block.header());

        let mut forks = Vec::with_capacity(witnesses.len());

        for witness in witnesses {
            let mut state: State<LB> = State::new(MemoryStore::new());

            let (witness_block, _) = witness
                .light_client
                .get_or_fetch_block(verified_block.height(), &mut state)?;

            let witness_hash = self.hasher.hash_header(&witness_block.header());

            if primary_hash == witness_hash {
                // Hashes match, continue with next witness, if any.
                continue;
            }

            state
                .light_store
                .insert(trusted_block.clone(), Status::Verified);

            state
                .light_store
                .insert(witness_block.clone(), Status::Unverified);

            let result = witness
                .light_client
                .verify_to_target(verified_block.height(), &mut state);

            match result {
                Ok(_) => forks.push(Fork::Forked {
                    primary: verified_block.clone(),
                    witness: witness_block,
                }),
                Err(e) if e.kind().has_expired() => {
                    forks.push(Fork::Forked {
                        primary: verified_block.clone(),
                        witness: witness_block,
                    });
                }
                Err(e) if e.kind().is_timeout() => {
                    forks.push(Fork::Timeout(witness_block.provider(), e.kind().clone()))
                }
                Err(e) => forks.push(Fork::Faulty(witness_block, e.kind().clone())),
            }
        }

        if forks.is_empty() {
            Ok(ForkDetection::NotDetected)
        } else {
            Ok(ForkDetection::Detected(forks))
        }
    }
}
