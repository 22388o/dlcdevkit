//! p2pderivatives/rust-dlc https://github.com/p2pderivatives/rust-dlc/blob/master/dlc-sled-storage-provider/src/lib.rs (2024)
//! # dlc-sled-storage-provider
//! Storage provider for dlc-manager using sled as underlying storage.

use dlc_manager::chain_monitor::ChainMonitor;
use dlc_manager::channel::accepted_channel::AcceptedChannel;
use dlc_manager::channel::offered_channel::OfferedChannel;
use dlc_manager::channel::signed_channel::{SignedChannel, SignedChannelStateType};
use dlc_manager::channel::{Channel, ClosedChannel, ClosedPunishedChannel, ClosingChannel, FailedAccept, FailedSign};
use dlc_manager::contract::accepted_contract::AcceptedContract;
use dlc_manager::contract::offered_contract::OfferedContract;
use dlc_manager::contract::ser::Serializable;
use dlc_manager::contract::signed_contract::SignedContract;
use dlc_manager::contract::{
    ClosedContract, Contract, FailedAcceptContract, FailedSignContract, PreClosedContract,
};
use dlc_manager::{error::Error, ContractId, Storage};
use sled::transaction::{ConflictableTransactionResult, UnabortableTransactionError};
use sled::{Db, Transactional, Tree};
use std::convert::TryInto;
use std::io::{Cursor, Read};

use crate::transport::PeerInformation;
use crate::DdkStorage;

const CONTRACT_TREE: u8 = 1;
const CHANNEL_TREE: u8 = 2;
const CHAIN_MONITOR_TREE: u8 = 3;
const CHAIN_MONITOR_KEY: u8 = 4;
const PEER_KEY: u8 = 5;
// const UTXO_TREE: u8 = 6;
// const KEY_PAIR_TREE: u8 = 7;
// const ADDRESS_TREE: u8 = 8;

/// Implementation of Storage interface using the sled DB backend.
#[derive(Debug, Clone)]
pub struct SledStorageProvider {
    db: Db,
}

macro_rules! convertible_enum {
    (enum $name:ident {
        $($vname:ident $(= $val:expr)?,)*;
        $($tname:ident $(= $tval:expr)?,)*
    }, $input:ident) => {
        #[derive(Debug)]
        enum $name {
            $($vname $(= $val)?,)*
            $($tname $(= $tval)?,)*
        }

        impl From<$name> for u8 {
            fn from(prefix: $name) -> u8 {
                prefix as u8
            }
        }

        impl std::convert::TryFrom<u8> for $name {
            type Error = Error;

            fn try_from(v: u8) -> Result<Self, Self::Error> {
                match v {
                    $(x if x == u8::from($name::$vname) => Ok($name::$vname),)*
                    $(x if x == u8::from($name::$tname) => Ok($name::$tname),)*
                    _ => Err(Error::StorageError("Unknown prefix".to_string())),
                }
            }
        }

        impl $name {
            fn get_prefix(input: &$input) -> u8 {
                let prefix = match input {
                    $($input::$vname(_) => $name::$vname,)*
                    $($input::$tname{..} => $name::$tname,)*
                };
                prefix.into()
            }
        }
    }
}

convertible_enum!(
    enum ContractPrefix {
        Offered = 1,
        Accepted,
        Signed,
        Confirmed,
        PreClosed,
        Closed,
        FailedAccept,
        FailedSign,
        Refunded,
        Rejected,;
    },
    Contract
);

convertible_enum!(
    enum ChannelPrefix {
        Offered = 100,
        Accepted,
        Signed,
        FailedAccept,
        FailedSign,
        Closing,
        Closed,
        CounterClosed,
        ClosedPunished,
        CollaborativelyClosed,
        Cancelled,;
    },
    Channel
);

convertible_enum!(
    enum SignedChannelPrefix {;
        Established = 1,
        SettledOffered,
        SettledReceived,
        SettledAccepted,
        SettledConfirmed,
        Settled,
        Closing,
        CollaborativeCloseOffered,
        RenewAccepted,
        RenewOffered,
        RenewFinalized,
        RenewConfirmed,
    },
    SignedChannelStateType
);

fn to_storage_error<T>(e: T) -> Error
where
    T: std::fmt::Display,
{
    Error::StorageError(e.to_string())
}

impl SledStorageProvider {
    /// Creates a new instance of a SledStorageProvider.
    pub fn new(path: &str) -> Result<Self, sled::Error> {
        Ok(SledStorageProvider {
            db: sled::open(path)?,
        })
    }

    fn get_data_with_prefix<T: Serializable>(
        &self,
        tree: &Tree,
        prefix: &[u8],
        consume: Option<u64>,
    ) -> Result<Vec<T>, Error> {
        let iter = tree.iter();
        iter.values()
            .filter_map(|res| {
                let value = res.unwrap();
                let mut cursor = Cursor::new(&value);
                let mut pref = vec![0u8; prefix.len()];
                cursor.read_exact(&mut pref).expect("Error reading prefix");
                if pref == prefix {
                    if let Some(c) = consume {
                        cursor.set_position(cursor.position() + c);
                    }
                    Some(Ok(T::deserialize(&mut cursor).ok()?))
                } else {
                    None
                }
            })
            .collect()
    }

    fn open_tree(&self, tree_id: &[u8; 1]) -> Result<Tree, Error> {
        self.db
            .open_tree(tree_id)
            .map_err(|e| Error::StorageError(format!("Error opening contract tree: {}", e)))
    }

    fn contract_tree(&self) -> Result<Tree, Error> {
        self.open_tree(&[CONTRACT_TREE])
    }

    fn channel_tree(&self) -> Result<Tree, Error> {
        self.open_tree(&[CHANNEL_TREE])
    }

}
impl DdkStorage for SledStorageProvider {
    fn list_peers(&self) -> anyhow::Result<Vec<PeerInformation>> {
        if let Some(bytes) = self.db.get("peers")? {
            let peers: Vec<PeerInformation> = serde_json::from_slice(&bytes)?;
            Ok(peers)
        } else {
            Ok(vec![])
        }
    }

    fn save_peer(&self, peer: PeerInformation) -> anyhow::Result<()> {
        let mut known_peers = self.list_peers()?;

        if known_peers.contains(&peer) {
            return Ok(());
        }

        known_peers.push(peer);
        let peer_vec = serde_json::to_vec(&known_peers)?;

        self.db.insert("peers", peer_vec)?;

        Ok(())
    }
}

impl Storage for SledStorageProvider {
    fn get_contract(&self, contract_id: &ContractId) -> Result<Option<Contract>, Error> {
        match self
            .contract_tree()?
            .get(contract_id)
            .map_err(to_storage_error)?
        {
            Some(res) => Ok(Some(deserialize_contract(&res)?)),
            None => Ok(None),
        }
    }

    fn get_contracts(&self) -> Result<Vec<Contract>, Error> {
        self.contract_tree()?
            .iter()
            .values()
            .map(|x| deserialize_contract(&x.unwrap()))
            .collect::<Result<Vec<Contract>, Error>>()
    }

    fn create_contract(&self, contract: &OfferedContract) -> Result<(), Error> {
        let serialized = serialize_contract(&Contract::Offered(contract.clone()))?;
        self.contract_tree()?
            .insert(contract.id, serialized)
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn delete_contract(&self, contract_id: &ContractId) -> Result<(), Error> {
        self.contract_tree()?
            .remove(contract_id)
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn update_contract(&self, contract: &Contract) -> Result<(), Error> {
        let serialized = serialize_contract(contract)?;
        self.contract_tree()?
            .transaction::<_, _, UnabortableTransactionError>(|db| {
                match contract {
                    a @ Contract::Accepted(_) | a @ Contract::Signed(_) => {
                        db.remove(&a.get_temporary_id())?;
                    }
                    _ => {}
                };

                db.insert(&contract.get_id(), serialized.clone())?;
                Ok(())
            })
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn get_contract_offers(&self) -> Result<Vec<OfferedContract>, Error> {
        self.get_data_with_prefix(
            &self.contract_tree()?,
            &[ContractPrefix::Offered.into()],
            None,
        )
    }

    fn get_signed_contracts(&self) -> Result<Vec<SignedContract>, Error> {
        self.get_data_with_prefix(
            &self.contract_tree()?,
            &[ContractPrefix::Signed.into()],
            None,
        )
    }

    fn get_confirmed_contracts(&self) -> Result<Vec<SignedContract>, Error> {
        self.get_data_with_prefix(
            &self.contract_tree()?,
            &[ContractPrefix::Confirmed.into()],
            None,
        )
    }

    fn get_preclosed_contracts(&self) -> Result<Vec<PreClosedContract>, Error> {
        self.get_data_with_prefix(
            &self.contract_tree()?,
            &[ContractPrefix::PreClosed.into()],
            None,
        )
    }

    fn upsert_channel(&self, channel: Channel, contract: Option<Contract>) -> Result<(), Error> {
        let serialized = serialize_channel(&channel)?;
        let serialized_contract = match contract.as_ref() {
            Some(c) => Some(serialize_contract(c)?),
            None => None,
        };
        let channel_tree = self.channel_tree()?;
        let contract_tree = self.contract_tree()?;
        (&channel_tree, &contract_tree)
            .transaction::<_, ()>(
                |(channel_db, contract_db)| -> ConflictableTransactionResult<(), UnabortableTransactionError> {
                    match &channel {
                        a @ Channel::Accepted(_) | a @ Channel::Signed(_) => {
                            channel_db.remove(&a.get_temporary_id())?;
                        }
                        _ => {}
                    };

                    channel_db.insert(&channel.get_id(), serialized.clone())?;

                    if let Some(c) = contract.as_ref() {
                        insert_contract(
                            contract_db,
                            serialized_contract
                                .clone()
                                .expect("to have the serialized version"),
                            c,
                        )?;
                    }
                    Ok(())
                },
            )
        .map_err(to_storage_error)?;
        Ok(())
    }

    fn delete_channel(&self, channel_id: &dlc_manager::ChannelId) -> Result<(), Error> {
        self.channel_tree()?
            .remove(channel_id)
            .map_err(to_storage_error)?;
        Ok(())
    }

    fn get_channel(&self, channel_id: &dlc_manager::ChannelId) -> Result<Option<Channel>, Error> {
        match self
            .channel_tree()?
            .get(channel_id)
            .map_err(to_storage_error)?
        {
            Some(res) => Ok(Some(deserialize_channel(&res)?)),
            None => Ok(None),
        }
    }

    fn get_signed_channels(
        &self,
        channel_state: Option<SignedChannelStateType>,
    ) -> Result<Vec<SignedChannel>, Error> {
        let (prefix, consume) = if let Some(state) = &channel_state {
            (
                vec![
                    ChannelPrefix::Signed.into(),
                    SignedChannelPrefix::get_prefix(state),
                ],
                None,
            )
        } else {
            (vec![ChannelPrefix::Signed.into()], Some(1))
        };

        self.get_data_with_prefix(&self.channel_tree()?, &prefix, consume)
    }

    fn get_offered_channels(&self) -> Result<Vec<OfferedChannel>, Error> {
        self.get_data_with_prefix(
            &self.channel_tree()?,
            &[ChannelPrefix::Offered.into()],
            None,
        )
    }

    fn persist_chain_monitor(&self, monitor: &ChainMonitor) -> Result<(), Error> {
        self.open_tree(&[CHAIN_MONITOR_TREE])?
            .insert([CHAIN_MONITOR_KEY], monitor.serialize()?)
            .map_err(|e| Error::StorageError(format!("Error writing chain monitor: {}", e)))?;
        Ok(())
    }
    fn get_chain_monitor(&self) -> Result<Option<ChainMonitor>, dlc_manager::error::Error> {
        let serialized = self
            .open_tree(&[CHAIN_MONITOR_TREE])?
            .get([CHAIN_MONITOR_KEY])
            .map_err(|e| Error::StorageError(format!("Error reading chain monitor: {}", e)))?;
        let deserialized = match serialized {
            Some(s) => Some(
                ChainMonitor::deserialize(&mut ::std::io::Cursor::new(s))
                    .map_err(to_storage_error)?,
            ),
            None => None,
        };
        Ok(deserialized)
    }
}

fn insert_contract(
    db: &sled::transaction::TransactionalTree,
    serialized: Vec<u8>,
    contract: &Contract,
) -> Result<Option<sled::IVec>, UnabortableTransactionError> {
    match contract {
        a @ Contract::Accepted(_) | a @ Contract::Signed(_) => {
            db.remove(&a.get_temporary_id())?;
        }
        _ => {}
    };

    db.insert(&contract.get_id(), serialized)
}

fn serialize_contract(contract: &Contract) -> Result<Vec<u8>, ::std::io::Error> {
    let serialized = match contract {
        Contract::Offered(o) | Contract::Rejected(o) => o.serialize(),
        Contract::Accepted(o) => o.serialize(),
        Contract::Signed(o) | Contract::Confirmed(o) | Contract::Refunded(o) => o.serialize(),
        Contract::FailedAccept(c) => c.serialize(),
        Contract::FailedSign(c) => c.serialize(),
        Contract::PreClosed(c) => c.serialize(),
        Contract::Closed(c) => c.serialize(),
    };
    let mut serialized = serialized?;
    let mut res = Vec::with_capacity(serialized.len() + 1);
    res.push(ContractPrefix::get_prefix(contract));
    res.append(&mut serialized);
    Ok(res)
}

fn deserialize_contract(buff: &sled::IVec) -> Result<Contract, Error> {
    let mut cursor = ::std::io::Cursor::new(buff);
    let mut prefix = [0u8; 1];
    cursor.read_exact(&mut prefix)?;
    let contract_prefix: ContractPrefix = prefix[0].try_into()?;
    let contract = match contract_prefix {
        ContractPrefix::Offered => {
            Contract::Offered(OfferedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::Accepted => Contract::Accepted(
            AcceptedContract::deserialize(&mut cursor).map_err(to_storage_error)?,
        ),
        ContractPrefix::Signed => {
            Contract::Signed(SignedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::Confirmed => {
            Contract::Confirmed(SignedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::PreClosed => Contract::PreClosed(
            PreClosedContract::deserialize(&mut cursor).map_err(to_storage_error)?,
        ),
        ContractPrefix::Closed => {
            Contract::Closed(ClosedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::FailedAccept => Contract::FailedAccept(
            FailedAcceptContract::deserialize(&mut cursor).map_err(to_storage_error)?,
        ),
        ContractPrefix::FailedSign => Contract::FailedSign(
            FailedSignContract::deserialize(&mut cursor).map_err(to_storage_error)?,
        ),
        ContractPrefix::Refunded => {
            Contract::Refunded(SignedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ContractPrefix::Rejected => {
            Contract::Rejected(OfferedContract::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
    };
    Ok(contract)
}

fn serialize_channel(channel: &Channel) -> Result<Vec<u8>, ::std::io::Error> {
    let serialized = match channel {
        Channel::Offered(o) => o.serialize(),
        Channel::Accepted(a) => a.serialize(),
        Channel::Signed(s) => s.serialize(),
        Channel::FailedAccept(f) => f.serialize(),
        Channel::FailedSign(f) => f.serialize(),
        Channel::Cancelled(o) => o.serialize(),
        Channel::Closing(c) => c.serialize(),
        Channel::Closed(c) => c.serialize(),
        Channel::CollaborativelyClosed(c) => c.serialize(),
        Channel::CounterClosed(c) => c.serialize(),
        Channel::ClosedPunished(c) => c.serialize(),
    };
    let mut serialized = serialized?;
    let mut res = Vec::with_capacity(serialized.len() + 1);
    res.push(ChannelPrefix::get_prefix(channel));
    if let Channel::Signed(s) = channel {
        res.push(SignedChannelPrefix::get_prefix(&s.state.get_type()))
    }
    res.append(&mut serialized);
    Ok(res)
}

fn deserialize_channel(buff: &sled::IVec) -> Result<Channel, Error> {
    let mut cursor = ::std::io::Cursor::new(buff);
    let mut prefix = [0u8; 1];
    cursor.read_exact(&mut prefix)?;
    let channel_prefix: ChannelPrefix = prefix[0].try_into()?;
    let channel = match channel_prefix {
        ChannelPrefix::Offered => {
            Channel::Offered(OfferedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::Accepted => {
            Channel::Accepted(AcceptedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::Signed => {
            // Skip the channel state prefix.
            cursor.set_position(cursor.position() + 1);
            Channel::Signed(SignedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::FailedAccept => {
            Channel::FailedAccept(FailedAccept::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::FailedSign => {
            Channel::FailedSign(FailedSign::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::Cancelled => {
            Channel::Cancelled(OfferedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::Closed => {
            Channel::Closed(ClosedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::Closing => {
            Channel::Closing(ClosingChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::CounterClosed => {
            Channel::CounterClosed(ClosedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::ClosedPunished => {
            Channel::ClosedPunished(ClosedPunishedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
        ChannelPrefix::CollaborativelyClosed => {
            Channel::CollaborativelyClosed(ClosedChannel::deserialize(&mut cursor).map_err(to_storage_error)?)
        }
    };
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! sled_test {
        ($name: ident, $body: expr) => {
            #[test]
            fn $name() {
                let path = format!("{}{}", "tests/data/dlc_storage/sleddb/", std::stringify!($name));
                {
                    let storage = SledStorageProvider::new(&path).expect("Error opening sled DB");
                    #[allow(clippy::redundant_closure_call)]
                    $body(storage);
                }
                std::fs::remove_dir_all(path).unwrap();
            }
        };
    }

    fn deserialize_object<T>(serialized: &[u8]) -> T
    where
        T: Serializable,
    {
        let mut cursor = std::io::Cursor::new(&serialized);
        T::deserialize(&mut cursor).unwrap()
    }

    sled_test!(
        create_contract_can_be_retrieved,
        |storage: SledStorageProvider| {
            let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Offered");
            let contract = deserialize_object(serialized);

            storage
                .create_contract(&contract)
                .expect("Error creating contract");

            let retrieved = storage
                .get_contract(&contract.id)
                .expect("Error retrieving contract.");

            if let Some(Contract::Offered(retrieved_offer)) = retrieved {
                assert_eq!(serialized[..], retrieved_offer.serialize().unwrap()[..]);
            } else {
                unreachable!();
            }
        }
    );

    sled_test!(
        update_contract_is_updated,
        |storage: SledStorageProvider| {
            let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Offered");
            let offered_contract = deserialize_object(serialized);
            let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Accepted");
            let accepted_contract = deserialize_object(serialized);
            let accepted_contract = Contract::Accepted(accepted_contract);

            storage
                .create_contract(&offered_contract)
                .expect("Error creating contract");

            storage
                .update_contract(&accepted_contract)
                .expect("Error updating contract.");
            let retrieved = storage
                .get_contract(&accepted_contract.get_id())
                .expect("Error retrieving contract.");

            if let Some(Contract::Accepted(_)) = retrieved {
            } else {
                unreachable!();
            }
        }
    );

    sled_test!(
        delete_contract_is_deleted,
        |storage: SledStorageProvider| {
            let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Offered");
            let contract = deserialize_object(serialized);
            storage
                .create_contract(&contract)
                .expect("Error creating contract");

            storage
                .delete_contract(&contract.id)
                .expect("Error deleting contract");

            assert!(storage
                .get_contract(&contract.id)
                .expect("Error querying contract")
                .is_none());
        }
    );

    fn insert_offered_signed_and_confirmed(storage: &mut SledStorageProvider) {
        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Offered");
        let offered_contract = deserialize_object(serialized);
        storage
            .create_contract(&offered_contract)
            .expect("Error creating contract");

        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Signed");
        let signed_contract = Contract::Signed(deserialize_object(serialized));
        storage
            .update_contract(&signed_contract)
            .expect("Error creating contract");
        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Signed1");
        let signed_contract = Contract::Signed(deserialize_object(serialized));
        storage
            .update_contract(&signed_contract)
            .expect("Error creating contract");

        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Confirmed");
        let confirmed_contract = Contract::Confirmed(deserialize_object(serialized));
        storage
            .update_contract(&confirmed_contract)
            .expect("Error creating contract");
        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Confirmed1");
        let confirmed_contract = Contract::Confirmed(deserialize_object(serialized));
        storage
            .update_contract(&confirmed_contract)
            .expect("Error creating contract");

        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/PreClosed");
        let preclosed_contract = Contract::PreClosed(deserialize_object(serialized));
        storage
            .update_contract(&preclosed_contract)
            .expect("Error creating contract");
    }

    fn insert_offered_and_signed_channels(storage: &mut SledStorageProvider) {
        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/Offered");
        let offered_contract = deserialize_object(serialized);
        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/OfferedChannel");
        let offered_channel = deserialize_object(serialized);
        storage
            .upsert_channel(
                Channel::Offered(offered_channel),
                Some(Contract::Offered(offered_contract)),
            )
            .expect("Error creating contract");

        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/SignedChannelEstablished");
        let signed_channel = Channel::Signed(deserialize_object(serialized));
        storage
            .upsert_channel(signed_channel, None)
            .expect("Error creating contract");

        let serialized = include_bytes!("../../tests/data/dlc_storage/sled/SignedChannelSettled");
        let signed_channel = Channel::Signed(deserialize_object(serialized));
        storage
            .upsert_channel(signed_channel, None)
            .expect("Error creating contract");
    }

    sled_test!(
        get_signed_contracts_only_signed,
        |mut storage: SledStorageProvider| {
            insert_offered_signed_and_confirmed(&mut storage);

            let signed_contracts = storage
                .get_signed_contracts()
                .expect("Error retrieving signed contracts");

            assert_eq!(2, signed_contracts.len());
        }
    );

    sled_test!(
        get_confirmed_contracts_only_confirmed,
        |mut storage: SledStorageProvider| {
            insert_offered_signed_and_confirmed(&mut storage);

            let confirmed_contracts = storage
                .get_confirmed_contracts()
                .expect("Error retrieving signed contracts");

            assert_eq!(2, confirmed_contracts.len());
        }
    );

    sled_test!(
        get_offered_contracts_only_offered,
        |mut storage: SledStorageProvider| {
            insert_offered_signed_and_confirmed(&mut storage);

            let offered_contracts = storage
                .get_contract_offers()
                .expect("Error retrieving signed contracts");

            assert_eq!(1, offered_contracts.len());
        }
    );

    sled_test!(
        get_preclosed_contracts_only_preclosed,
        |mut storage: SledStorageProvider| {
            insert_offered_signed_and_confirmed(&mut storage);

            let preclosed_contracts = storage
                .get_preclosed_contracts()
                .expect("Error retrieving preclosed contracts");

            assert_eq!(1, preclosed_contracts.len());
        }
    );
    sled_test!(
        get_contracts_all_returned,
        |mut storage: SledStorageProvider| {
            insert_offered_signed_and_confirmed(&mut storage);

            let contracts = storage.get_contracts().expect("Error retrieving contracts");

            assert_eq!(6, contracts.len());
        }
    );

    sled_test!(
        get_offered_channels_only_offered,
        |mut storage: SledStorageProvider| {
            insert_offered_and_signed_channels(&mut storage);

            let offered_channels = storage
                .get_offered_channels()
                .expect("Error retrieving offered channels");
            assert_eq!(1, offered_channels.len());
        }
    );

    sled_test!(
        get_signed_established_channel_only_established,
        |mut storage: SledStorageProvider| {
            insert_offered_and_signed_channels(&mut storage);

            let signed_channels = storage
                .get_signed_channels(Some(
                    dlc_manager::channel::signed_channel::SignedChannelStateType::Established,
                ))
                .expect("Error retrieving offered channels");
            assert_eq!(1, signed_channels.len());
            if let dlc_manager::channel::signed_channel::SignedChannelState::Established {
                ..
            } = &signed_channels[0].state
            {
            } else {
                panic!(
                    "Expected established state got {:?}",
                    &signed_channels[0].state
                );
            }
        }
    );

    sled_test!(
        get_channel_by_id_returns_correct_channel,
        |mut storage: SledStorageProvider| {
            insert_offered_and_signed_channels(&mut storage);

            let serialized = include_bytes!("../../tests/data/dlc_storage/sled/AcceptedChannel");
            let accepted_channel: AcceptedChannel = deserialize_object(serialized);
            let channel_id = accepted_channel.channel_id;
            storage
                .upsert_channel(Channel::Accepted(accepted_channel), None)
                .expect("Error creating contract");

            storage
                .get_channel(&channel_id)
                .expect("error retrieving previously inserted channel.")
                .expect("to have found the previously inserted channel.");
        }
    );

    sled_test!(
        delete_channel_is_not_returned,
        |mut storage: SledStorageProvider| {
            insert_offered_and_signed_channels(&mut storage);

            let serialized = include_bytes!("../../tests/data/dlc_storage/sled/AcceptedChannel");
            let accepted_channel: AcceptedChannel = deserialize_object(serialized);
            let channel_id = accepted_channel.channel_id;
            storage
                .upsert_channel(Channel::Accepted(accepted_channel), None)
                .expect("Error creating contract");

            storage
                .get_channel(&channel_id)
                .expect("could not retrieve previously inserted channel.");

            storage
                .delete_channel(&channel_id)
                .expect("to be able to delete the channel");

            assert!(storage
                .get_channel(&channel_id)
                .expect("error getting channel.")
                .is_none());
        }
    );

    sled_test!(
        persist_chain_monitor_test,
        |storage: SledStorageProvider| {
            let chain_monitor = ChainMonitor::new(123);

            storage
                .persist_chain_monitor(&chain_monitor)
                .expect("to be able to persist the chain monistor.");

            let retrieved = storage
                .get_chain_monitor()
                .expect("to be able to retrieve the chain monitor.")
                .expect("to have a persisted chain monitor.");

            assert_eq!(chain_monitor, retrieved);
        }
    );
}
