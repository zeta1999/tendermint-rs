use crossbeam_channel as channel;

use crate::bail;
use crate::errors::{Error, ErrorKind};
use crate::evidence::EvidenceReporter;
use crate::fork_detector::{Fork, ForkDetection, ForkDetector};
use crate::light_client::LightClient;
use crate::peer_list::PeerList;
use crate::state::State;
use crate::types::{Height, LightBlock, PeerId, Status, TMLightBlock};

pub trait Handle<LB> {
    /// Get latest trusted block from the [`Supervisor`].
    fn latest_trusted(&self) -> Result<Option<LB>, Error> {
        todo!()
    }

    /// Verify to the highest block.
    fn verify_to_highest(&self) -> Result<LB, Error> {
        todo!()
    }

    /// Verify to the block at the given height.
    fn verify_to_target(&self, _height: Height) -> Result<LB, Error> {
        todo!()
    }

    /// Terminate the underlying [`Supervisor`].
    fn terminate(&self) -> Result<(), Error> {
        todo!()
    }
}

/// Input events sent by the [`Handle`]s to the [`Supervisor`]. They carry a [`Callback`] which is
/// used to communicate back the responses of the requests.
#[derive(Debug)]
enum HandleInput<LB> {
    /// Terminate the supervisor process
    Terminate(channel::Sender<()>),
    /// Verify to the highest height, call the provided callback with result
    VerifyToHighest(channel::Sender<Result<LB, Error>>),
    /// Verify to the given height, call the provided callback with result
    VerifyToTarget(Height, channel::Sender<Result<LB, Error>>),
    /// Get the latest trusted block.
    LatestTrusted(channel::Sender<Option<LB>>),
}

/// A light client `Instance` packages a `LightClient` together with its `State`.
#[derive(Debug)]
pub struct Instance<LB>
where
    LB: LightBlock,
{
    /// The light client for this instance
    pub light_client: LightClient<LB>,
    /// The state of the light client for this instance
    pub state: State<LB>,
}

impl<LB> Instance<LB>
where
    LB: LightBlock,
{
    /// Constructs a new instance from the given light client and its state.
    pub fn new(light_client: LightClient<LB>, state: State<LB>) -> Self {
        Self {
            light_client,
            state,
        }
    }

    pub fn latest_trusted(&self) -> Option<LB> {
        self.state.light_store.latest(Status::Trusted)
    }

    pub fn trust_block(&mut self, lb: &LB) {
        self.state.light_store.update(lb, Status::Trusted);
    }
}

/// The supervisor manages multiple light client instances, of which one
/// is deemed to be the primary instance through which blocks are retrieved
/// and verified. The other instances are considered as witnesses
/// which are consulted to perform fork detection.
///
/// If primary verification fails, the primary client is removed and a witness
/// is promoted to primary. If a witness is deemed faulty, then the witness is
/// removed.
///
/// The supervisor is intended to be ran in its own thread, and queried
/// via a `Handle`.
///
/// ## Example
///
/// ```rust,ignore
/// let mut supervisor: Supervisor = todo!();
/// let mut handle = supervisor.handle();
///
/// // Spawn the supervisor in its own thread.
/// std::thread::spawn(|| supervisor.run());
///
/// loop {
///     // Asynchronously query the supervisor via a handle
///     let maybe_block = handle.verify_to_highest();
///     match maybe_block {
///         Ok(light_block) => {
///             println!("[info] synced to block {}", light_block.height());
///         }
///         Err(e) => {
///             println!("[error] sync failed: {}", e);
///         }
///     });
///
///     std::thread::sleep(Duration::from_millis(800));
/// }
/// ```
pub struct Supervisor<LB>
where
    LB: LightBlock,
{
    /// List of peers and their instances (primary, witnesses, full and faulty nodes)
    peers: PeerList<Instance<LB>>,

    /// An instance of the fork detector
    fork_detector: Box<dyn ForkDetector<LB>>,

    /// Reporter of fork evidence
    evidence_reporter: Box<dyn EvidenceReporter<LB>>,

    /// Channel through which to reply to `Handle`s
    sender: channel::Sender<HandleInput<LB>>,

    /// Channel through which to receive events from the `Handle`s
    receiver: channel::Receiver<HandleInput<LB>>,
}

impl<LB> std::fmt::Debug for Supervisor<LB>
where
    LB: LightBlock,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("peers", &self.peers)
            .finish()
    }
}

// Ensure the `Supervisor` can be sent across thread boundaries.
static_assertions::assert_impl_all!(Supervisor<TMLightBlock>: Send);

impl<LB> Supervisor<LB>
where
    LB: LightBlock,
{
    /// Constructs a new supevisor from the given list of peers and fork detector instance.
    pub fn new(
        peers: PeerList<Instance<LB>>,
        fork_detector: impl ForkDetector<LB> + 'static,
        evidence_reporter: impl EvidenceReporter<LB> + 'static,
    ) -> Self {
        let (sender, receiver) = channel::unbounded();

        Self {
            peers,
            sender,
            receiver,
            fork_detector: Box::new(fork_detector),
            evidence_reporter: Box::new(evidence_reporter),
        }
    }

    /// Create a new handle to this supervisor.
    pub fn handle(&mut self) -> impl Handle<LB> {
        SupervisorHandle::new(self.sender.clone())
    }

    fn latest_trusted(&self) -> Option<LB> {
        self.peers.primary().latest_trusted()
    }

    /// Verify to the highest block.
    pub fn verify_to_highest(&mut self) -> Result<LB, Error> {
        self.verify(None)
    }

    /// Verify to the block at the given height.
    pub fn verify_to_target(&mut self, height: Height) -> Result<LB, Error> {
        self.verify(Some(height))
    }

    /// Verify either to the latest block (if `height == None`) or to a given block (if `height == Some(height)`).
    fn verify(&mut self, height: Option<Height>) -> Result<LB, Error> {
        let primary = self.peers.primary_mut();

        // Perform light client core verification for the given height (or highest).
        let verdict = match height {
            None => primary.light_client.verify_to_highest(&mut primary.state),
            Some(height) => primary
                .light_client
                .verify_to_target(height, &mut primary.state),
        };

        match verdict {
            // Verification succeeded, let's perform fork detection
            Ok(verified_block) => {
                let trusted_block = primary
                    .latest_trusted()
                    .ok_or_else(|| ErrorKind::NoTrustedState(Status::Trusted))?;

                // Perform fork detection with the highest verified block and the trusted block.
                let outcome = self.detect_forks(&verified_block, &trusted_block)?;

                match outcome {
                    // There was a fork or a faulty peer
                    ForkDetection::Detected(forks) => {
                        let forked = self.process_forks(forks)?;
                        if !forked.is_empty() {
                            // Fork detected, exiting
                            bail!(ErrorKind::ForkDetected(forked))
                        }

                        // If there were no hard forks, perform verification again
                        self.verify(height)
                    }
                    ForkDetection::NotDetected => {
                        // We need to re-ask for the primary here as the compiler
                        // is not smart enough to realize that we do not mutate
                        // the `primary` field of `PeerList` between the initial
                        // borrow of the primary and here (can't blame it, it's
                        // not that obvious).
                        self.peers.primary_mut().trust_block(&verified_block);

                        // No fork detected, exiting
                        Ok(verified_block)
                    }
                }
            }
            // Verification failed
            Err(_err) => {
                // TODO: Log/record error

                // Swap primary, and continue with new primary, if there is any witness left.
                self.peers.replace_faulty_primary()?;
                self.verify(height)
            }
        }
    }

    fn process_forks(&mut self, forks: Vec<Fork<LB>>) -> Result<Vec<PeerId>, Error> {
        let mut forked = Vec::with_capacity(forks.len());

        for fork in forks {
            match fork {
                // An actual fork was detected, report evidence and record forked peer.
                // TODO: also report to primary
                Fork::Forked { primary, witness } => {
                    let provider = witness.provider();
                    self.report_evidence(provider, &primary, &witness)?;

                    forked.push(provider);
                }
                // A witness has timed out, remove it from the peer list.
                Fork::Timeout(provider, _error) => {
                    self.peers.replace_faulty_witness(provider);
                    // TODO: Log/record the error
                }
                // A witness has been deemed faulty, remove it from the peer list.
                Fork::Faulty(block, _error) => {
                    self.peers.replace_faulty_witness(block.provider());
                    // TODO: Log/record the error
                }
            }
        }

        Ok(forked)
    }

    /// Report the given evidence of a fork.
    fn report_evidence(
        &mut self,
        provider: PeerId,
        primary: &LB,
        witness: &LB,
    ) -> Result<(), Error> {
        let evidence = self.evidence_reporter.build_conflicting_headers_evidence(
            primary.signed_header().clone(),
            witness.signed_header().clone(),
        );

        self.evidence_reporter
            .report(evidence, provider)
            .map_err(ErrorKind::Io)?;

        Ok(())
    }

    /// Perform fork detection with the given verified block and trusted block.
    fn detect_forks(
        &self,
        verified_block: &LB,
        trusted_block: &LB,
    ) -> Result<ForkDetection<LB>, Error> {
        if self.peers.witnesses_ids().is_empty() {
            bail!(ErrorKind::NoWitnesses);
        }

        let witnesses = self
            .peers
            .witnesses_ids()
            .iter()
            .filter_map(|id| self.peers.get(id))
            .collect();

        self.fork_detector
            .detect_forks(verified_block, &trusted_block, witnesses)
    }

    /// Run the supervisor event loop in the same thread.
    ///
    /// This method should typically be called within a new thread with `std::thread::spawn`.
    pub fn run(mut self) -> Result<(), Error> {
        loop {
            let event = self.receiver.recv().map_err(ErrorKind::from)?;

            match event {
                HandleInput::LatestTrusted(sender) => {
                    let outcome = self.latest_trusted();
                    sender.send(outcome).map_err(ErrorKind::from)?;
                }
                HandleInput::Terminate(sender) => {
                    sender.send(()).map_err(ErrorKind::from)?;
                    return Ok(());
                }
                HandleInput::VerifyToTarget(height, sender) => {
                    let outcome = self.verify_to_target(height);
                    sender.send(outcome).map_err(ErrorKind::from)?;
                }
                HandleInput::VerifyToHighest(sender) => {
                    let outcome = self.verify_to_highest();
                    sender.send(outcome).map_err(ErrorKind::from)?;
                }
            }
        }
    }
}

/// A [`Handle`] to the [`Supervisor`] which allows to communicate with
/// the supervisor across thread boundaries via message passing.
struct SupervisorHandle<LB> {
    sender: channel::Sender<HandleInput<LB>>,
}

impl<LB> SupervisorHandle<LB>
where
    LB: LightBlock,
{
    /// Crate a new handle that sends events to the supervisor via
    /// the given channel. For internal use only.
    fn new(sender: channel::Sender<HandleInput<LB>>) -> Self {
        Self { sender }
    }

    fn verify(
        &self,
        make_event: impl FnOnce(channel::Sender<Result<LB, Error>>) -> HandleInput<LB>,
    ) -> Result<LB, Error> {
        let (sender, receiver) = channel::bounded::<Result<LB, Error>>(1);

        let event = make_event(sender);
        self.sender.send(event).map_err(ErrorKind::from)?;

        receiver.recv().map_err(ErrorKind::from)?
    }
}

impl<LB> Handle<LB> for SupervisorHandle<LB>
where
    LB: LightBlock,
{
    fn latest_trusted(&self) -> Result<Option<LB>, Error> {
        let (sender, receiver) = channel::bounded::<Option<LB>>(1);

        self.sender
            .send(HandleInput::LatestTrusted(sender))
            .map_err(ErrorKind::from)?;

        Ok(receiver.recv().map_err(ErrorKind::from)?)
    }

    fn verify_to_highest(&self) -> Result<LB, Error> {
        self.verify(HandleInput::VerifyToHighest)
    }

    fn verify_to_target(&self, height: Height) -> Result<LB, Error> {
        self.verify(|sender| HandleInput::VerifyToTarget(height, sender))
    }

    fn terminate(&self) -> Result<(), Error> {
        let (sender, receiver) = channel::bounded::<()>(1);

        self.sender
            .send(HandleInput::Terminate(sender))
            .map_err(ErrorKind::from)?;

        Ok(receiver.recv().map_err(ErrorKind::from)?)
    }
}
