use std::sync::{Arc, Weak};
use std::time::Duration;
use std::collections::HashMap;

use parking_lot::RwLock;

use account::Account;
use block_albatross::{
    Block,
    BlockType,
    ForkProof,
    MacroBlock,
    MacroExtrinsics,
    MicroBlock,
    MicroExtrinsics,
    MicroHeader,
    PbftCommitMessage,
    PbftPrepareMessage,
    PbftProof,
    PbftProposal,
    SignedPbftCommitMessage,
    SignedPbftPrepareMessage,
    SignedPbftProposal,
    SignedViewChange,
    ViewChange,
    ViewChangeProof,
};
use block_production_albatross::BlockProducer;
use blockchain_albatross::Blockchain;
use blockchain_base::BlockchainEvent;
use bls::bls12_381::KeyPair;
use collections::grouped_list::Group;
use consensus::{AlbatrossConsensusProtocol, Consensus, ConsensusEvent};
use hash::{Blake2bHash, Hash};
use network_primitives::networks::NetworkInfo;
use network_primitives::validator_info::{SignedValidatorInfo, ValidatorInfo};
use primitives::validators::IndexedSlot;
use utils::mutable_once::MutableOnce;
use utils::timers::Timers;
use utils::observer::ListenerHandle;

use crate::error::Error;
use crate::slash::ForkProofPool;
use crate::validator_network::{ValidatorNetwork, ValidatorNetworkEvent};


#[derive(Clone, Debug)]
pub enum SlotChange  {
    NextBlock,
    ViewChange(ViewChange, ViewChangeProof),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ValidatorStatus {
    None,
    Synced, // Already reached consensus with peers but we're not still a validator
    Potential,
    Active,
}

struct ValidatorListeners {
    consensus: ListenerHandle,
    blockchain: ListenerHandle,
}

pub struct Validator {
    blockchain: Arc<Blockchain<'static>>,
    block_producer: BlockProducer<'static>,
    consensus: Arc<Consensus<AlbatrossConsensusProtocol>>,
    validator_network: Arc<ValidatorNetwork>,
    validator_key: KeyPair,

    timers: Timers<ValidatorTimer>,

    state: RwLock<ValidatorState>,

    self_weak: MutableOnce<Weak<Validator>>,
    listeners: MutableOnce<Option<ValidatorListeners>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ValidatorTimer {
    ViewChange,
}

pub struct ValidatorState {
    pk_idx: Option<u16>,
    slots: Option<u16>,
    status: ValidatorStatus,
    fork_proof_pool: ForkProofPool,
    view_number: u32,
    active_view_change: Option<ViewChange>,
    proposed_extrinsics: HashMap<Blake2bHash, MacroExtrinsics>,
}

impl Validator {
    const BLOCK_TIMEOUT: Duration = Duration::from_secs(10);
    //const PBFT_TIMEOUT: Duration = Duration::from_secs(60);

    pub fn new(consensus: Arc<Consensus<AlbatrossConsensusProtocol>>, validator_key: KeyPair) -> Result<Arc<Self>, Error> {
        let compressed_public_key = validator_key.public.compress();
        let info = ValidatorInfo {
            public_key: compressed_public_key,
            peer_address: consensus.network.network_config.peer_address().clone(),
            udp_address: None,
            valid_from: consensus.blockchain.block_number(),
        };
        let validator_network = ValidatorNetwork::new(consensus.network.clone(), consensus.blockchain.clone(), SignedValidatorInfo::from_message(info, &validator_key.secret, 0));
        let block_producer = BlockProducer::new(consensus.blockchain.clone(), consensus.mempool.clone(), validator_key.clone());
        let view_number = consensus.blockchain.next_view_number();

        debug!("Initializing validator");

        let this = Arc::new(Validator {
            blockchain: consensus.blockchain.clone(),
            block_producer,
            consensus,
            validator_network,

            validator_key,
            timers: Timers::new(),

            state: RwLock::new(ValidatorState {
                pk_idx: None,
                slots: None,
                status: ValidatorStatus::None,
                fork_proof_pool: ForkProofPool::new(),
                view_number,
                active_view_change: None,
                proposed_extrinsics: HashMap::new(),
            }),

            self_weak: MutableOnce::new(Weak::new()),
            listeners: MutableOnce::new(None),
        });
        Validator::init_listeners(&this);

        Ok(this)
    }

    pub fn init_listeners(this: &Arc<Validator>) {
        unsafe { this.self_weak.replace(Arc::downgrade(this)); };

        // Setup event handlers for blockchain events
        let weak = Arc::downgrade(this);
        let consensus = this.consensus.notifier.write().register(move |e: &ConsensusEvent| {
            let this = upgrade_weak!(weak);
            match e {
                ConsensusEvent::Established => this.on_consensus_established(),
                ConsensusEvent::Lost => this.on_consensus_lost(),
                _ => {},
            }
        });

        // Set up event handlers for blockchain events
        let weak = Arc::downgrade(this);
        let blockchain = this.blockchain.notifier.write().register(move |e: &BlockchainEvent<Block>| {
            // We're spawning this handler in a thread, since it does quite a lot of work.
            // Specifically this might lock the validator state, but in this handler the Blockchain
            // also still holds the push_lock. This can cause a dead-lock with another thread that
            // produces a block, because this will first lock the validator state and then
            // Blockchain's push_lock.
            let this = upgrade_weak!(weak);
            // We need to clone to move this into the thread. Alternatively we could Arc events.
            // But except for rebranching, this is only the type of the event and a hash, so not
            // very expensive to clone anyway.
            let e = e.clone();
            tokio::spawn(futures::future::lazy(move|| {
                this.on_blockchain_event(&e);
                Ok(())
            }));
        });

        // Set up event handlers for validator network events
        let weak = Arc::downgrade(this);
        this.validator_network.notifier.write().register(move |e: ValidatorNetworkEvent| {
            let this = upgrade_weak!(weak);
            this.on_validator_network_event(e);
        });

        // Set up the view change timer in case there's a block timeout
        // Note: In start_view_change() we check so that it's only executed if we are an active validator
        let weak = Arc::downgrade(this);
        this.timers.set_interval(ValidatorTimer::ViewChange, move || {
            let this = upgrade_weak!(weak);
            this.on_block_timeout();
        }, Self::BLOCK_TIMEOUT);

        // remember listeners for when we drop this validator
        let listeners = ValidatorListeners {
            consensus,
            blockchain,
        };
        unsafe { this.listeners.replace(Some(listeners)); }
    }

    fn on_block_timeout(&self) {
        self.start_view_change();
    }

    pub fn on_consensus_established(&self) {
        trace!("Consensus established");
        self.init_epoch();

        // trigger slot change, if we're active
        let state = self.state.read();
        if state.status == ValidatorStatus::Active {
            drop(state);
            self.on_slot_change(SlotChange::NextBlock);
        }
    }

    pub fn on_consensus_lost(&self) {
        trace!("Consensus lost");
        let mut state = self.state.write();
        state.status = ValidatorStatus::None;
    }

    fn reset_view_change_interval(&self, timeout: Duration) {
        let weak = self.self_weak.clone();
        self.timers.reset_interval(ValidatorTimer::ViewChange, move || {
            let this = upgrade_weak!(weak);
            this.on_block_timeout();
        }, timeout);
    }

    fn on_blockchain_event(&self, event: &BlockchainEvent<Block>) {
        // Handle each block type (which is directly related to each event type).
        match event {
            BlockchainEvent::Finalized(hash) => {
                // Init new validator epoch
                self.init_epoch();
                self.validator_network.on_blockchain_changed(hash);
            },

            BlockchainEvent::Extended(hash) => {
                self.on_blockchain_extended(hash);
                self.validator_network.on_blockchain_changed(hash);
            },

            BlockchainEvent::Rebranched(old_chain, new_chain) => {
                self.on_blockchain_rebranched(old_chain, new_chain);
                let (hash, _) = new_chain.last().expect("Expected non-empty new_chain after rebranch");
                self.validator_network.on_blockchain_changed(hash);
            }
        }

        let mut state = self.state.write();

        // The new block might increase the view number before we actually finish the view change
        // Therefore we always update here.
        state.view_number = self.blockchain.next_view_number();

        // clear out proposed extrinsics
        state.proposed_extrinsics.clear();

        if state.status == ValidatorStatus::Potential || state.status == ValidatorStatus::Active {
            // Reset the view change timeout because we received a valid block.
            // NOTE: This doesn't take the state lock, so we don't need to drop it
            self.reset_view_change_interval(Self::BLOCK_TIMEOUT);
            state.active_view_change = None;

        }

        // If we're an active validator, we need to check if we're the next block producer.
        if state.status == ValidatorStatus::Active {
            // NOTE: This might take the state lock, so we drop it here
            drop(state);
            self.on_slot_change(SlotChange::NextBlock);
        }
    }

    fn init_epoch(&self) {
        let mut state = self.state.write();
        state.view_number = 0;

        match self.get_pk_idx_and_slots() {
            Some((pk_idx, slots)) => {
                debug!("Setting validator to active: pk_idx={}", pk_idx);
                state.pk_idx = Some(pk_idx);
                state.slots = Some(slots);
                state.status = ValidatorStatus::Active;

                // Notify validator network that we have finality and update epoch-related state
                // (i.e. set the validator ID)
                self.validator_network.reset_epoch(Some(pk_idx as usize));
            },
            None => {
                debug!("Setting validator to inactive");
                state.pk_idx = None;
                state.slots = None;
                state.status = if self.is_potential_validator() { ValidatorStatus::Potential } else { ValidatorStatus::Synced };

                // Notify validator network that we have finality and update epoch-related state
                // (i.e. remove the validator ID)
                self.validator_network.reset_epoch(None);
            },
        }
    }

    // Sets the state according to the information on the block
    pub fn on_blockchain_extended(&self, hash: &Blake2bHash) {
        let block = self.blockchain.get_block(hash, false, false).unwrap_or_else(|| panic!("We got the block hash ({}) from an event from the blockchain itself", &hash));

        let mut state = self.state.write();
        state.fork_proof_pool.apply_block(&block);
    }

    // Sets the state according to the rebranch
    pub fn on_blockchain_rebranched(&self, old_chain: &[(Blake2bHash, Block)], new_chain: &[(Blake2bHash, Block)]) {
        let mut state = self.state.write();
        for (_hash, block) in old_chain.iter() {
            state.fork_proof_pool.revert_block(block);
        }
        for (_hash, block) in new_chain.iter() {
            state.fork_proof_pool.apply_block(&block);
        }
    }

    fn on_validator_network_event(&self, event: ValidatorNetworkEvent) {
        {
            let state = self.state.write();

            // Validator network events are only intersting to active validators
            if state.status != ValidatorStatus::Active {
                return;
            }
        }

        match event {
            ValidatorNetworkEvent::ViewChangeComplete(event) => {
                let (view_change, view_change_proof) = *event;
                debug!("Completed view change to {}", view_change);
                self.on_slot_change(SlotChange::ViewChange(view_change, view_change_proof));
            },
            ValidatorNetworkEvent::PbftProposal(event) => {
                let (hash, proposal) = *event;
                self.on_pbft_proposal(hash, proposal)
            },
            ValidatorNetworkEvent::PbftPrepareComplete(event) => {
                let (hash, _) = *event;
                self.on_pbft_prepare_complete(hash)
            },
            ValidatorNetworkEvent::PbftComplete(event) => {
                let (hash, proposal, proof) = *event;
                self.on_pbft_commit_complete(hash, proposal, proof)
            },
            ValidatorNetworkEvent::ForkProof(event) => self.on_fork_proof(*event),
        }
    }

    fn on_fork_proof(&self, fork_proof: ForkProof) {
        self.state.write().fork_proof_pool.insert(fork_proof);
    }

    pub fn on_slot_change(&self, slot_change: SlotChange) {
        let (view_number, view_change_proof) = match slot_change {
            SlotChange::NextBlock => {
                // a new block will just use the current view number and no view change proof
                (self.blockchain.next_view_number(), None)
            },
            SlotChange::ViewChange(view_change, view_change_proof) => {
                let mut state = self.state.write();

                // If we have an active view change with this view number, clear it.
                if let Some(vc) = &state.active_view_change {
                    if vc == &view_change {
                        state.active_view_change = None;
                    }
                }

                // check if this view change is still relevant
                if state.view_number < view_change.new_view_number {
                    // Reset view change interval again.
                    self.reset_view_change_interval(Self::BLOCK_TIMEOUT);

                    // update our view number
                    state.view_number = view_change.new_view_number;

                    // we're at the new view number and need a view change proof for it
                    (view_change.new_view_number, Some(view_change_proof))
                }
                else {
                    // we're already at a better view number
                    (state.view_number, None)
                }
            },
        };

        // Check if we are the next block producer and act accordingly
        let IndexedSlot { slot, .. } = self.blockchain.get_next_block_producer(view_number, None);
        let public_key = self.validator_key.public.compress();
        trace!("Next block producer: {:?}", slot.public_key.compressed());

        if slot.public_key.compressed() == &public_key {
            let weak = self.self_weak.clone();
            trace!("Spawning thread to produce next block");
            tokio::spawn(futures::lazy(move || {
                if let Some(this) = Weak::upgrade(&weak) {
                    match this.blockchain.get_next_block_type(None) {
                        BlockType::Macro => { this.produce_macro_block(view_change_proof) },
                        BlockType::Micro => { this.produce_micro_block(view_change_proof) },
                    }
                }
                Ok(())
            }));
        }
    }

    pub fn on_pbft_proposal(&self, hash: Blake2bHash, _proposal: PbftProposal) {
        let state = self.state.write();
        trace!("Received proposal: {}", hash);
        // View change messages should only be sent by active validators.
        if state.status != ValidatorStatus::Active {
            return;
        }

        // Note: we don't verify this hash as the network validator already did.
        let pk_idx = state.pk_idx.expect("Already checked that we are an active validator before calling this function");

        drop(state);

        trace!("Signing prepare: pk_idx={}", pk_idx);
        let prepare_message = SignedPbftPrepareMessage::from_message(
            PbftPrepareMessage { block_hash: hash.clone() },
            &self.validator_key.secret,
            pk_idx
        );

        self.validator_network.push_prepare(prepare_message)
            .unwrap_or_else(|e| debug!("Failed to push pBFT prepare: {}", e));
    }

    pub fn on_pbft_prepare_complete(&self, hash: Blake2bHash) {
        trace!("Complete prepare for: {}", hash);
        let state = self.state.read();
        // View change messages should only be sent by active validators.
        if state.status != ValidatorStatus::Active {
            return;
        }

        // Note: we don't verify this hash as the network validator already did
        let pk_idx = state.pk_idx.expect("Already checked that we are an active validator before calling this function");

        drop(state);

        trace!("Signing commit message: pk_idx={}", pk_idx);
        let commit_message = SignedPbftCommitMessage::from_message(
            PbftCommitMessage { block_hash: hash },
            &self.validator_key.secret,
            pk_idx
        );

        self.validator_network.push_commit(commit_message)
            .unwrap_or_else(|e| debug!("Failed to push pBFT commit: {}", e));
    }

    pub fn on_pbft_commit_complete(&self, hash: Blake2bHash, proposal: PbftProposal, proof: PbftProof) {
        let mut state = self.state.write();

        if let Some(extrinsics) = state.proposed_extrinsics.remove(&hash) {
            assert_eq!(proposal.header.extrinsics_root, extrinsics.hash());

            // Note: we're not verifying the justification as the validator network already did that
            let block = Block::Macro(MacroBlock {
                header: proposal.header,
                justification: Some(proof),
                extrinsics: Some(extrinsics)
            });

            //trace!("Relaying finished macro block: {:#?}", block);
            drop(state);

            // Automatically relays block.
            self.blockchain.push_block(block, false)
                .unwrap_or_else(|e| panic!("Pushing macro block to blockchain failed: {:?}", e));
        }
    }

    fn start_view_change(&self) {
        let mut state = self.state.write();

        // View change messages should only be sent by active validators.
        if state.status != ValidatorStatus::Active {
            return;
        }

        // If we already started a view change (i.e. added our contribution), we don't do anything
        if state.active_view_change.is_some() {
            debug!("View change already started");
            return;
        }

        // The number of the block that timed out.
        let block_number = self.blockchain.height() + 1;
        let new_view_number = state.view_number + 1;
        let message = ViewChange { block_number, new_view_number };

        info!("Starting view change to {}", message);

        let pk_idx = state.pk_idx.expect("Checked above that we are an active validator");
        let view_change_message = SignedViewChange::from_message(message.clone(), &self.validator_key.secret, pk_idx);
        state.active_view_change = Some(message);

        drop(state);

        // Broadcast our view change number message to the other validators.
        self.validator_network.start_view_change(view_change_message);
     }

    fn get_pk_idx_and_slots(&self) -> Option<(u16, u16)> {
        let compressed = self.validator_key.public.compress();
        let validator_list = self.blockchain.current_validators();
        let item = validator_list.groups().iter().enumerate()
            .find(|(_, Group(_, public_key))| public_key.compressed() == &compressed);
        item.map(|(i, Group(num_slots, _))| (i as u16, *num_slots))
    }

    fn produce_macro_block(&self, view_change: Option<ViewChangeProof>) {
        let mut state = self.state.write();

        // FIXME: Don't use network time
        let timestamp = self.consensus.network.network_time.now();
        let (pbft_proposal, proposed_extrinsics) = self.block_producer.next_macro_block_proposal(timestamp, state.view_number, view_change);
        state.proposed_extrinsics.insert(pbft_proposal.header.hash(), proposed_extrinsics);
        let pk_idx = state.pk_idx.expect("Checked that we are an active validator before entering this function");

        drop(state);

        let signed_proposal = SignedPbftProposal::from_message(pbft_proposal, &self.validator_key.secret, pk_idx);
        self.validator_network.start_pbft(signed_proposal)
            .unwrap_or_else(|e| error!("Failed to start pBFT proposal: {}", e));

    }

    fn produce_micro_block(&self, view_change_proof: Option<ViewChangeProof>) {
        let max_size = MicroBlock::MAX_SIZE
            - MicroHeader::SIZE
            - MicroExtrinsics::get_metadata_size(0, 0);

        let state = self.state.read();
        let fork_proofs = state.fork_proof_pool.get_fork_proofs_for_block(max_size);
        let timestamp = self.consensus.network.network_time.now();
        let view_number = state.view_number;

        // Drop lock before push, otherwise two concurrent threads can dead-lock because the
        // validator and blockchain lock are circular dependent.
        drop(state);

        let block = self.block_producer.next_micro_block(fork_proofs, timestamp, view_number, vec![], view_change_proof);
        info!("Produced block #{}.{}: {}",
              block.header.block_number,
              block.header.view_number,
              block.header.hash::<Blake2bHash>());

        // Automatically relays block.
        match self.blockchain.push(Block::Micro(block)) {
            Ok(r) => trace!("Push result: {:?}", r),
            Err(e) => error!("Failed to push produced micro block to blockchain: {:?}", e),
        }
    }

    fn is_potential_validator(&self) -> bool {
        let validator_registry = NetworkInfo::from_network_id(self.blockchain.network_id).validator_registry_address().expect("Albatross consensus always has the address set.");
        let contract = self.blockchain.state().accounts().get(validator_registry, None);
        if let Account::Staking(contract) = contract {
            let public_key = self.validator_key.public.compress();

            // FIXME: Inefficient linear scan.
            contract.active_stake_sorted.iter().any(|stake| stake.validator_key() == &public_key)
        } else {
            panic!("Validator registry has a wrong account type.");
        }
    }
}

impl Drop for Validator {
    fn drop(&mut self) {
        if let Some(listeners) = self.listeners.as_ref() {
            self.consensus.notifier.write().deregister(listeners.consensus);
            self.blockchain.notifier.write().deregister(listeners.blockchain);
            self.validator_network.notifier.write().deregister();
        }
    }
}
