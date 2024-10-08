use super::SledStorageProvider;
use crate::error::WalletError;
use crate::signer::{DeriveSigner, SignerInformation};
use bdk_wallet::ChangeSet;
use bdk_wallet::WalletPersister;
use bitcoin::{
    key::rand::{thread_rng, Rng},
    secp256k1::{PublicKey, SecretKey},
};

impl WalletPersister for SledStorageProvider {
    type Error = WalletError;

    fn persist(persister: &mut Self, changeset: &ChangeSet) -> Result<(), Self::Error> {
        let wallet_tree = persister.wallet_tree()?;
        let rand_key: [u8; 32] = thread_rng().gen();
        let new_changeset = bincode::serialize(changeset).map_err(|_| {
            WalletError::StorageError(sled::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Serialization error",
            )))
        })?;
        wallet_tree.insert(rand_key, new_changeset)?;
        Ok(())
    }

    fn initialize(_persister: &mut Self) -> Result<ChangeSet, Self::Error> {
        Ok(ChangeSet::default())
    }
}

impl DeriveSigner for SledStorageProvider {
    type Error = WalletError;

    fn get_key_information(&self, key_id: [u8; 32]) -> Result<SignerInformation, Self::Error> {
        let key = hex::encode(key_id);
        let info = self.signer_tree()?.get(key)?.unwrap();
        Ok(bincode::deserialize::<SignerInformation>(&info)?)
    }

    /// Store the secret and public with the givem key id
    fn store_derived_key_id(
        &self,
        key_id: [u8; 32],
        signer_information: SignerInformation,
    ) -> Result<(), WalletError> {
        let serialized_signer_info = bincode::serialize(&signer_information).map_err(|_| {
            WalletError::StorageError(sled::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Deserialization error aggregating changset.",
            )))
        })?;

        // Store the key id string instead of bytes.
        let key_id = hex::encode(key_id);

        self.signer_tree()?.insert(key_id, serialized_signer_info)?;
        Ok(())
    }

    /// Retrieve the secrety key for a given public key.
    fn get_secret_key(&self, public_key: &PublicKey) -> Result<SecretKey, WalletError> {
        let tree = self.signer_tree()?;
        for result in tree.iter() {
            if let Ok(value) = result {
                let info: SignerInformation = bincode::deserialize(&value.1).map_err(|_| {
                    WalletError::StorageError(sled::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Deserialization error aggregating changset.",
                    )))
                })?;
                if info.public_key == *public_key {
                    return Ok(info.secret_key);
                }
            }
        }

        Err(WalletError::SignerError(
            "Could not find secret key.".into(),
        ))
    }

    fn import_address_to_storage(&self, _address: &bitcoin::Address) -> Result<(), WalletError> {
        Ok(())
    }
}
