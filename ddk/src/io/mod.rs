use bitcoin::bip32::Xpriv;
use bitcoin::Network;
use bitcoin::key::rand;
use rand::Fill;
use std::{fs::File, io::Write, path::Path};
use crate::config::SeedConfig;

pub fn xprv_from_config(
    seed_config: &SeedConfig,
    network: Network,
) -> anyhow::Result<Xpriv> {
    let seed = match seed_config {
        SeedConfig::Bytes(bytes) => Xpriv::new_master(network, bytes)?,
        SeedConfig::File(file) => {
            if Path::new(&format!("{file}/seed.ddk")).exists() {
                let seed = std::fs::read(format!("{file}/seed.ddk"))?;
                let mut key = [0; 64];
                key.copy_from_slice(&seed);
                let xprv = Xpriv::new_master(network, &seed)?;
                xprv
            } else {
                let mut file = File::create(format!("{file}/seed.ddk"))?;
                let mut entropy = [0u8; 64];
                entropy.try_fill(&mut rand::thread_rng())?;
                // let _mnemonic = Mnemonic::from_entropy(&entropy)?;
                let xprv = Xpriv::new_master(network, &entropy)?;
                file.write_all(&entropy)?;
                xprv
            }
        }
    };

    Ok(seed)
}
