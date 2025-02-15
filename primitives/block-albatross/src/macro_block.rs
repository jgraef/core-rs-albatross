use std::convert::TryInto;
use std::fmt;
use std::io;

use failure::Fail;

use beserial::{Deserialize, Serialize};
use bls::bls12_381::CompressedSignature;
use bls::bls12_381::lazy::LazyPublicKey;
use collections::bitset::BitSet;
use collections::compressed_list::CompressedList;
use hash::{Blake2bHash, Hash, SerializeContent};
use keys::Address;
use primitives::coin::Coin;
use primitives::policy;
use primitives::validators::{Slot, Slots};

use crate::BlockError;
use crate::pbft::PbftProof;
use crate::signed;

#[derive(Clone, Debug, Fail)]
pub enum IntoSlotsError {
    #[fail(display = "Extrinsics missing in macro block")]
    MissingExtrinsics,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MacroBlock {
    pub header: MacroHeader,
    pub justification: Option<PbftProof>,
    pub extrinsics: Option<MacroExtrinsics>
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MacroHeader {
    pub version: u16,

    /// The list of validator public keys for the next epoch.
    pub validators: CompressedList<LazyPublicKey>,

    pub block_number: u32,
    pub view_number: u32,
    pub parent_macro_hash: Blake2bHash,

    pub seed: CompressedSignature,
    pub parent_hash: Blake2bHash,
    pub state_root: Blake2bHash,
    pub extrinsics_root: Blake2bHash,

    /// A merkle root over all transactions from the previous epoch.
    pub transactions_root: Blake2bHash,

    pub timestamp: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MacroExtrinsics {
    /// Staker and reward addresses for the next epoch's validators.
    pub slot_addresses: CompressedList<SlotAddresses>,
    /// The slash fine for the next epoch.
    pub slash_fine: Coin,
    /// The final list of slashes from the previous epoch.
    pub slashed_set: BitSet,
}

impl TryInto<Slots> for MacroBlock {
    type Error = IntoSlotsError;

    fn try_into(self) -> Result<Slots, Self::Error> {
        if self.extrinsics.is_none() {
            return Err(IntoSlotsError::MissingExtrinsics);
        }
        let extrinsics = self.extrinsics.unwrap();

        let public_keys = self.header.validators.into_iter();
        let addresses = extrinsics.slot_addresses.into_iter();

        let slots: Vec<Slot> = public_keys.zip(addresses)
            .map(|(p, a)| Slot {
                public_key: p.clone(),
                staker_address: a.staker_address.clone(),
                reward_address_opt: if a.reward_address == a.staker_address {
                    None
                } else {
                    Some(a.reward_address.clone())
                }
            })
            .collect();
        assert_eq!(slots.len(), policy::SLOTS as usize);

        let slash_fine = extrinsics.slash_fine;
        Ok(Slots::new(slots, slash_fine))
    }
}

// CHECKME: Check for performance
impl MacroExtrinsics {
    pub fn from(slots: Slots, slashed_set: BitSet) -> Self {
        let addresses = slots.iter().map(|slot| SlotAddresses {
            staker_address: slot.staker_address.clone(),
            reward_address: slot.reward_address_opt.as_ref().unwrap_or(&slot.staker_address).clone(),
        });
        let slash_fine = slots.slash_fine();
        MacroExtrinsics {
            slot_addresses: addresses.collect(),
            slash_fine,
            slashed_set,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SlotAddresses {
    pub staker_address: Address,
    pub reward_address: Address,
}

impl signed::Message for MacroHeader {
    const PREFIX: u8 = signed::PREFIX_PBFT_PROPOSAL;
}

impl MacroBlock {
    pub fn verify(&self) -> Result<(), BlockError> {
        if self.header.block_number >= 1 && self.justification.is_none() {
            return Err(BlockError::NoJustification);
        }
        if !self.header.validators.verify() || self.header.validators.len() != policy::SLOTS as usize {
            return Err(BlockError::InvalidValidators);
        }
        if let Some(ref extrinsics) = self.extrinsics {
            let addr = &extrinsics.slot_addresses;
            if !addr.verify() || addr.len() != policy::SLOTS as usize {
                return Err(BlockError::InvalidValidators);
            }
        }
        Ok(())
    }

    pub fn hash(&self) -> Blake2bHash {
        self.header.hash()
    }
}

impl SerializeContent for MacroHeader {
    fn serialize_content<W: io::Write>(&self, writer: &mut W) -> io::Result<usize> { Ok(self.serialize(writer)?) }
}

#[allow(clippy::derive_hash_xor_eq)] // TODO: Shouldn't be necessary
impl Hash for MacroHeader { }

impl SerializeContent for MacroExtrinsics {
    fn serialize_content<W: io::Write>(&self, writer: &mut W) -> io::Result<usize> { Ok(self.serialize(writer)?) }
}

// TODO Do we need merkle here?
#[allow(clippy::derive_hash_xor_eq)] // TODO: Shouldn't be necessary
impl Hash for MacroExtrinsics { }

impl fmt::Display for MacroBlock {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "[#{}, view {}, type Macro]",
               self.header.block_number,
               self.header.view_number)
    }
}
