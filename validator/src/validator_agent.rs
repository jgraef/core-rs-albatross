use std::sync::Arc;
use std::cmp::Ordering;
use std::fmt;
use std::time::Duration;

use network_primitives::validator_info::{ValidatorInfo, SignedValidatorInfo};
use network_primitives::address::PeerId;
use network::Peer;
use utils::observer::{PassThroughNotifier, weak_passthru_listener};
use parking_lot::RwLock;
use bls::bls12_381::CompressedPublicKey;
use block_albatross::{SignedPbftProposal, ForkProof, ViewChange, PbftPrepareMessage,
                      PbftCommitMessage};
use primitives::policy;
use blockchain_albatross::Blockchain;
use hash::{Hash, Blake2bHash};
use handel::update::LevelUpdateMessage;
use utils::rate_limit::RateLimit;
use messages::ViewChangeProofMessage;


pub enum ValidatorAgentEvent {
    ValidatorInfos(Vec<SignedValidatorInfo>),
    ForkProof(Box<ForkProof>),
    ViewChange(Box<LevelUpdateMessage<ViewChange>>),
    ViewChangeProof(Box<ViewChangeProofMessage>),
    PbftProposal(Box<SignedPbftProposal>),
    PbftPrepare(Box<LevelUpdateMessage<PbftPrepareMessage>>),
    PbftCommit(Box<LevelUpdateMessage<PbftCommitMessage>>),
}

pub struct ValidatorAgentState {
    pub(crate) validator_info: Option<SignedValidatorInfo>,
    pbft_proposal_limit: RateLimit,
}

pub struct ValidatorAgent {
    pub(crate) peer: Arc<Peer>,
    pub(crate) blockchain: Arc<Blockchain<'static>>,
    pub(crate) state: RwLock<ValidatorAgentState>,
    pub notifier: RwLock<PassThroughNotifier<'static, ValidatorAgentEvent>>,
}

impl ValidatorAgent {
    pub fn new(peer: Arc<Peer>, blockchain: Arc<Blockchain<'static>>) -> Arc<Self> {
        let agent = Arc::new(Self {
            peer,
            blockchain,
            state: RwLock::new(ValidatorAgentState {
                validator_info: None,
                pbft_proposal_limit: RateLimit::new(5, Duration::from_secs(10)),
            }),
            notifier: RwLock::new(PassThroughNotifier::new()),
        });

        Self::init_listeners(&agent);

        agent
    }

    fn init_listeners(this: &Arc<Self>) {
        this.peer.channel.msg_notifier.validator_info.write()
            .register(weak_passthru_listener(Arc::downgrade(this), |this, signed_infos: Vec<SignedValidatorInfo>| {
                this.on_validator_infos(signed_infos);
            }));
        this.peer.channel.msg_notifier.fork_proof.write()
            .register(weak_passthru_listener(Arc::downgrade(this), |this, fork_proof| {
                this.on_fork_proof_message(fork_proof);
            }));
        this.peer.channel.msg_notifier.pbft_proposal.write()
            .register(weak_passthru_listener(Arc::downgrade(this),
                |this, proposal| this.on_pbft_proposal_message(proposal)));

        this.peer.channel.msg_notifier.pbft_prepare.write()
            .register(weak_passthru_listener(Arc::downgrade(this),
                |this, prepare| this.on_pbft_prepare_message(prepare)));
        this.peer.channel.msg_notifier.pbft_commit.write()
            .register(weak_passthru_listener(Arc::downgrade(this),
                |this, commit| this.on_pbft_commit_message(commit)));
        this.peer.channel.msg_notifier.view_change.write()
            .register(weak_passthru_listener( Arc::downgrade(this), |this, view_change| {
                this.on_view_change_message(view_change);
            }));
        this.peer.channel.msg_notifier.view_change_proof.write()
            .register(weak_passthru_listener( Arc::downgrade(this), |this, view_change_proof| {
                this.on_view_change_proof(view_change_proof);
            }));
    }

    /// When a list of validator infos is received, verify the signatures and notify
    fn on_validator_infos(&self, signed_infos: Vec<SignedValidatorInfo>) {
        debug!("[VALIDATOR-INFO] contains {} validator infos", signed_infos.len());

        let mut valid_infos = Vec::new();
        for signed_info in signed_infos {
            // TODO: first check if we already know this validator. If so, we don't need to check
            // the signature of this info.
            if let Ok(public_key) = signed_info.message.public_key.uncompress() {
                let signature_okay = signed_info.verify(&public_key);
                trace!("[VALIDATOR-INFO] {:#?}, signature_okay={}", signed_info.message, signature_okay);
                if signature_okay {
                    valid_infos.push(signed_info);
                }
            }
            else {
                error!("Uncompressing public key failed: {}", signed_info.message.peer_address);
            }
        }

        self.notifier.read().notify(ValidatorAgentEvent::ValidatorInfos(valid_infos));
    }

    /// When a fork proof message is received
    fn on_fork_proof_message(&self, fork_proof: ForkProof) {
        debug!("[FORK-PROOF] Fork proof:");

        if !fork_proof.is_valid_at(self.blockchain.block_number() + 1) {
            debug!("[FORK-PROOF] Not valid");
            return;
        }

        debug!("[FORK-PROOF] Header 1: {:?}", fork_proof.header1);
        debug!("[FORK_PROOF] Header 2: {:?}", fork_proof.header2);
        let block_number = fork_proof.block_number();
        let view_number = fork_proof.view_number();

        let producer = self.blockchain.get_block_producer_at(block_number, view_number, None);
        if producer.is_none() {
            debug!("[FORK-PROOF] Unknown block producer for #{}.{}", block_number, view_number);
            return;
        }

        let slot = producer.unwrap().slot;
        if let Err(e) = fork_proof.verify(&slot.public_key.uncompress_unchecked()) {
            debug!("[FORK-PROOF] Invalid signature in fork proof: {:?}", e);
            return;
        }

        self.notifier.read().notify(ValidatorAgentEvent::ForkProof(Box::new(fork_proof)));
    }

    /// When a view change message is received, verify the signature and pass it to ValidatorNetwork
    fn on_view_change_message(&self, update_message: LevelUpdateMessage<ViewChange>) {
        trace!("[VIEW-CHANGE] Received: number={} update={:?} peer={}",
               update_message.tag,
               update_message.update,
               self.peer.peer_address());

        let block_number = self.blockchain.block_number();
        let blockchain_epoch = policy::epoch_at(block_number + 1);
        let view_change_epoch = policy::epoch_at(update_message.tag.block_number);
        match view_change_epoch.cmp(&blockchain_epoch) {
            Ordering::Greater => {
                debug!("[VIEW-CHANGE] Ignoring view change message for a future epoch: current=#{}/{}, change_to=#{}/{}", block_number, blockchain_epoch, update_message.tag.block_number, view_change_epoch);
                return;
            },
            Ordering::Less => {
                debug!("[VIEW-CHANGE] Ignoring view change message for an old epoch: current=#{}/{}, change_to=#{}/{}", block_number, blockchain_epoch, update_message.tag.block_number, view_change_epoch);
                return;
            },
            Ordering::Equal => (),
        };

        self.notifier.read().notify(ValidatorAgentEvent::ViewChange(Box::new(update_message)))
    }

    fn on_view_change_proof(&self, proof: ViewChangeProofMessage) {
        trace!("[VIEW-CHANGE] Received proof: {:?}", proof);

        self.notifier.read().notify(ValidatorAgentEvent::ViewChangeProof(Box::new(proof)))
    }

    /// When a pbft block proposal is received
    fn on_pbft_proposal_message(&self, proposal: SignedPbftProposal) {
        if !self.state.write().pbft_proposal_limit.note_single() {
            warn!("Ignoring proposal - rate limit exceeded");
            return;
        }

        trace!("Received macro block proposal: {:?}", &proposal);

        let proposal_block = proposal.message.header.block_number;
        let current_block = self.blockchain.head().block_number();

        // Reject proposal if the blockchain is already longer
        if proposal_block <= current_block {
            trace!("[PBFT-PROPOSAL] Ignoring old proposal: {:?}", proposal.message.header);
            return;
        }

        // Reject proposal if it lies in another epoch
        if policy::epoch_at(proposal_block) != policy::epoch_at(current_block) {
            warn!("[PBFT-PROPOSAL] Ignoring proposal in another epoch: {}",
                  proposal.message.header.hash::<Blake2bHash>());
            return;
        }

        self.notifier.read().notify(ValidatorAgentEvent::PbftProposal(Box::new(proposal)));
    }

    /// When a pbft prepare message is received, verify the signature and pass it to ValidatorNetwork
    /// TODO: The validator network could just register this it-self
    fn on_pbft_prepare_message(&self, level_update: LevelUpdateMessage<PbftPrepareMessage>) {
        trace!("[PBFT-PREPARE] Received: block_hash={} update={:?} peer={}",
               level_update.tag.block_hash,
               level_update.update,
               self.peer.peer_address());

        self.notifier.read().notify(ValidatorAgentEvent::PbftPrepare(Box::new(level_update)));
    }

    /// When a pbft commit message is received, verify the signature and pass it to ValidatorNetwork
    /// FIXME This will verify a commit message with the current validator set, not with the one for
    /// which this commit is for.
    fn on_pbft_commit_message(&self, level_update: LevelUpdateMessage<PbftCommitMessage>) {
        trace!("[PBFT-COMMIT] Received: block_hash={} update={:?} peer={}",
               level_update.tag.block_hash,
               level_update.update,
               self.peer.peer_address());

        self.notifier.read().notify(ValidatorAgentEvent::PbftCommit(Box::new(level_update)));
    }

    pub fn validator_info(&self) -> Option<ValidatorInfo> {
        self.state.read().validator_info.as_ref().map(|signed| signed.message.clone())
    }

    pub fn public_key(&self) -> Option<CompressedPublicKey> {
        self.state.read().validator_info.as_ref().map(|info| info.message.public_key.clone())
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer.peer_address().peer_id.clone()
    }
}

impl fmt::Debug for ValidatorAgent {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "ValidatorAgent {{ peer_id: {}, public_key: {:?} }}", self.peer_id(), self.public_key())
    }
}