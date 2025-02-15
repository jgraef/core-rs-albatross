use std::collections::btree_map::BTreeMap;
use std::convert::TryFrom;
use std::fs::{OpenOptions, read_to_string};
use std::io::Error as IoError;
use std::path::Path;

use chrono::{DateTime, Utc};
use failure::Fail;
use toml::de::Error as TomlError;

use account::{Account, AccountError, AccountsList, BasicAccount, StakingContract};
use accounts::Accounts;
use beserial::{Serialize, SerializingError};
use block_albatross::{Block, MacroBlock, MacroExtrinsics, MacroHeader};
use bls::bls12_381::{
    CompressedPublicKey as CompressedBlsPublicKey,
    PublicKey as BlsPublicKey,
    SecretKey as BlsSecretKey,
    Signature as BlsSignature,
};
use bls::bls12_381::lazy::LazyPublicKey;
use collections::bitset::BitSet;
use collections::grouped_list::{Group, GroupedList};
use database::volatile::{VolatileDatabaseError, VolatileEnvironment};
use database::WriteTransaction;
use hash::{Blake2bHash, Blake2bHasher, Hash, Hasher};
use keys::Address;
use primitives::coin::Coin;
use primitives::policy;
use primitives::validators::Slots;

mod config;

#[derive(Debug, Fail)]
pub enum GenesisBuilderError {
    #[fail(display = "No signing key to generate genesis seed.")]
    NoSigningKey,
    #[fail(display = "No staking contract address.")]
    NoStakingContractAddress,
    #[fail(display = "Invalid timestamp: {}", _0)]
    InvalidTimestamp(DateTime<Utc>),
    #[fail(display = "Serialization failed")]
    SerializingError(#[cause] SerializingError),
    #[fail(display = "I/O error")]
    IoError(#[cause] IoError),
    #[fail(display = "Failed to parse TOML file")]
    TomlError(#[cause] TomlError),
    #[fail(display = "Failed to stake")]
    StakingError(#[cause] AccountError),
    #[fail(display = "Database error")]
    DatabaseError(#[cause] VolatileDatabaseError),
}

impl From<SerializingError> for GenesisBuilderError {
    fn from(e: SerializingError) -> Self {
        GenesisBuilderError::SerializingError(e)
    }
}

impl From<IoError> for GenesisBuilderError {
    fn from(e: IoError) -> Self {
        GenesisBuilderError::IoError(e)
    }
}

impl From<TomlError> for GenesisBuilderError {
    fn from(e: TomlError) -> Self {
        GenesisBuilderError::TomlError(e)
    }
}

impl From<AccountError> for GenesisBuilderError {
    fn from(e: AccountError) -> Self {
        GenesisBuilderError::StakingError(e)
    }
}

impl From<VolatileDatabaseError> for GenesisBuilderError {
    fn from(e: VolatileDatabaseError) -> Self {
        GenesisBuilderError::DatabaseError(e)
    }
}


pub struct GenesisInfo {
    pub block: Block,
    pub hash: Blake2bHash,
    pub accounts: Vec<(Address, Account)>,
}


#[derive(Default)]
pub struct GenesisBuilder {
    pub signing_key: Option<BlsSecretKey>,
    pub seed_message: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub stakes: Vec<config::GenesisStake>,
    pub accounts: Vec<config::GenesisAccount>,
    pub staking_contract_address: Option<Address>,
}

impl GenesisBuilder {
    pub fn with_signing_key(&mut self, secret_key: BlsSecretKey) -> &mut Self {
        self.signing_key = Some(secret_key);
        self
    }

    pub fn with_seed_message<S: AsRef<str>>(&mut self, seed_message: S) -> &mut Self {
        self.seed_message = Some(seed_message.as_ref().to_string());
        self
    }

    pub fn with_timestamp(&mut self, timestamp: DateTime<Utc>) -> &mut Self {
        self.timestamp = Some(timestamp);
        self
    }

    pub fn with_staking_contract_address(&mut self, address: Address) -> &mut Self {
        self.staking_contract_address = Some(address);
        self
    }

    pub fn with_genesis_stake(&mut self, staker_address: Address, reward_address: Option<Address>, validator_key: BlsPublicKey, balance: Coin) -> &mut Self {
        self.stakes.push(config::GenesisStake {
            staker_address,
            reward_address,
            validator_key,
            balance
        });
        self
    }

    pub fn with_basic_account(&mut self, address: Address, balance: Coin) -> &mut Self {
        self.accounts.push(config::GenesisAccount {
            address,
            balance
        });
        self
    }

    pub fn with_config_file<P: AsRef<Path>>(&mut self, path: P) -> Result<&mut Self, GenesisBuilderError> {
        let config::GenesisConfig {
            signing_key,
            seed_message,
            timestamp,
            mut stakes,
            mut accounts,
            staking_contract,
        } = toml::from_str(&read_to_string(path)?)?;

        signing_key.map(|skey| self.with_signing_key(skey));
        seed_message.map(|msg| self.with_seed_message(msg));
        timestamp.map(|t| self.with_timestamp(t));
        staking_contract.map(|address| self.with_staking_contract_address(address));
        self.stakes.append(&mut stakes);
        self.accounts.append(&mut accounts);

        Ok(self)
    }

    fn select_validators(&self, pre_genesis_hash: &BlsSignature, staking_contract: &StakingContract) -> Result<(Slots, GroupedList<LazyPublicKey>), GenesisBuilderError> {
        let slot_allocation = staking_contract
            .select_validators(&pre_genesis_hash.compress(), policy::SLOTS, policy::MAX_CONSIDERED as usize);

        let validators = { // construct validator slot list with slot counts
            // count slots per public key
            let mut slot_counts: BTreeMap<&CompressedBlsPublicKey, u16> = BTreeMap::new();
            for validator in slot_allocation.iter() {
                slot_counts.entry(validator.public_key.compressed())
                    .and_modify(|x| *x += 1) // if has value, increment
                    .or_insert(1); // if no value, insert 1
            }

            // map to Validators
            GroupedList(
                slot_counts.iter()
                    .map(|(key, num)| Group(*num, (*key).clone().into()))
                    .collect()
            )
        };

        Ok((slot_allocation, validators))
    }

    pub fn generate(&self) -> Result<GenesisInfo, GenesisBuilderError> {
        let timestamp = self.timestamp.unwrap_or_else(Utc::now);

        // generate seeds
        let signing_key = self.signing_key.as_ref().ok_or(GenesisBuilderError::NoSigningKey)?;
        // random message used as seed for VRF that generates pre-genesis seed
        let seed_message = self.seed_message.clone()
            .unwrap_or_else(|| "love ai amor mohabbat hubun cinta lyubov bhalabasa amour kauna pi'ara liebe eshq upendo prema amore katresnan sarang anpu prema yeu".to_string());
        // pre-genesis seed (used for slot selection)
        let pre_genesis_seed = signing_key
            .sign_hash(Blake2bHasher::new().digest(seed_message.as_bytes()));
        // seed of genesis block = VRF(seed_0)
        let seed = signing_key.sign(&pre_genesis_seed);

        // generate staking contract
        let staking_contract = self.generate_staking_contract()?;

        // generate slot allocation from staking contract
        let (slot_allocation, validators) = self.select_validators(&pre_genesis_seed, &staking_contract)?;

        // extrinsics
        let extrinsics = MacroExtrinsics::from(slot_allocation, BitSet::new());
        let extrinsics_root = extrinsics.hash::<Blake2bHash>();
        debug!("Extrinsics root: {}", &extrinsics_root);

        // accounts
        let mut genesis_accounts: Vec<(Address, Account)> = Vec::new();
        genesis_accounts.push((Address::clone(self.staking_contract_address.as_ref().ok_or(GenesisBuilderError::NoStakingContractAddress)?), Account::Staking(staking_contract)));
        for account in &self.accounts {
            genesis_accounts.push((account.address.clone(), Account::Basic(BasicAccount { balance: account.balance })));
        }

        // state root
        let state_root = {
            let db = VolatileEnvironment::new(10)?;
            let accounts = Accounts::new(&db);
            let mut txn = WriteTransaction::new(&db);
            // XXX need to clone, since init needs the actual data
            accounts.init(&mut txn, genesis_accounts.clone());
            accounts.hash(Some(&txn))
        };
        debug!("State root: {}", &state_root);

        // the header
        let header = MacroHeader {
            version: 1,
            validators: validators.iter().collect(),
            block_number: 0,
            view_number: 0,
            parent_macro_hash: [0u8; 32].into(),
            seed: seed.compress(),
            parent_hash: [0u8; 32].into(),
            state_root,
            extrinsics_root,
            transactions_root: [0u8; 32].into(),
            timestamp: u64::try_from(timestamp.timestamp_millis())
                .map_err(|_| GenesisBuilderError::InvalidTimestamp(timestamp))?,
        };

        // genesis hash
        let genesis_hash = header.hash::<Blake2bHash>();

        Ok(GenesisInfo {
            block: Block::Macro(MacroBlock {
                header,
                justification: None,
                extrinsics: Some(extrinsics),
            }),
            hash: genesis_hash,
            accounts: genesis_accounts,
        })
    }

    fn generate_staking_contract(&self) -> Result<StakingContract, GenesisBuilderError> {
        let mut contract = StakingContract::default();

        for stake in self.stakes.iter() {
            contract.stake(&stake.staker_address, stake.balance, stake.validator_key.compress(), stake.reward_address.clone())?;
        }

        Ok(contract)
    }

    pub fn write_to_files<P: AsRef<Path>>(&self, directory: P) -> Result<Blake2bHash, GenesisBuilderError> {
        let GenesisInfo { block, hash, accounts } = self.generate()?;

        debug!("Genesis block: {}", &hash);
        debug!("{:#?}", &block);
        debug!("Accounts:");
        debug!("{:#?}", &accounts);

        let block_path = directory.as_ref().join("block.dat");
        info!("Writing block to {}", block_path.display());
        let mut file = OpenOptions::new().create(true).write(true).open(&block_path)?;
        block.serialize(&mut file)?;

        let accounts_path = directory.as_ref().join("accounts.dat");
        info!("Writing accounts to {}", accounts_path.display());
        let mut file = OpenOptions::new().create(true).write(true).open(&accounts_path)?;
        AccountsList(accounts).serialize(&mut file)?;

        Ok(hash)
    }
}
