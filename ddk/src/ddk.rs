use crate::chain::EsploraClient;
use crate::wallet::DlcDevKitWallet;
use crate::{DdkOracle, DdkStorage, DdkTransport};
use anyhow::anyhow;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use dlc_manager::{
    contract::contract_input::ContractInput, CachedContractSignerProvider, ContractId,
    SimpleSigner, SystemTimeProvider,
};
use dlc_messages::oracle_msgs::OracleAnnouncement;
use dlc_messages::{AcceptDlc, Message, OfferDlc};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::runtime::Runtime;
use crossbeam::channel::{unbounded, Sender, Receiver};

/// DlcDevKit type alias for the [dlc_manager::manager::Manager]
pub type DlcDevKitDlcManager<S, O> = dlc_manager::manager::Manager<
    Arc<DlcDevKitWallet<S>>,
    Arc<CachedContractSignerProvider<Arc<DlcDevKitWallet<S>>, SimpleSigner>>,
    Arc<EsploraClient>,
    Arc<S>,
    Arc<O>,
    Arc<SystemTimeProvider>,
    Arc<DlcDevKitWallet<S>>,
    SimpleSigner,
>;

#[derive(Debug)]
pub enum DlcManagerMessage {
    AcceptDlc {
        contract: ContractId,
        responder: Sender<(ContractId, PublicKey, AcceptDlc)>
    },
    OfferDlc {
        contract_input: ContractInput,
        counter_party: PublicKey,
        oracle_announcements: Vec<OracleAnnouncement>,
        responder: Sender<OfferDlc>,
    },
    ProcessMessages,
}

pub struct DlcDevKit<T: DdkTransport, S: DdkStorage, O: DdkOracle> {
    pub runtime: Arc<RwLock<Option<Runtime>>>,
    pub wallet: Arc<DlcDevKitWallet<S>>,
    pub manager: Arc<DlcDevKitDlcManager<S, O>>,
    pub sender: Arc<Sender<DlcManagerMessage>>,
    pub receiver: Arc<Receiver<DlcManagerMessage>>,
    pub transport: Arc<T>,
    pub storage: Arc<S>,
    pub oracle: Arc<O>,
    pub network: Network,
}

impl<T, S, O> DlcDevKit<T, S, O>
where 
    T: DdkTransport, S: DdkStorage, O: DdkOracle
{
    pub fn start(&self) -> anyhow::Result<()> {
        let mut runtime_lock = self.runtime.write().unwrap();

        if runtime_lock.is_some() {
            return Err(anyhow!("DDK is still running."));
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;

        
        let manager_transport = self.transport.clone();
        let manager_clone = self.manager.clone();
        let receiver_clone = self.receiver.clone();
        std::thread::spawn(move || Self::run_manager(manager_clone, manager_transport, receiver_clone));

        let transport_clone = self.transport.clone();
        runtime.spawn(async move {
            transport_clone.listen().await;
        });

        let wallet_clone = self.wallet.clone();
        runtime.spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(10));
            loop {
                timer.tick().await;
                wallet_clone.sync().unwrap();
            }
        });

        let processor = self.sender.clone();
        runtime.spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(5));
            loop {
                timer.tick().await;
                processor.send(DlcManagerMessage::ProcessMessages).expect("couldn't send message");
            }
        });

        // TODO: connect stored peers.

        *runtime_lock = Some(runtime);

        Ok(())
    }

    fn run_manager(manager: Arc<DlcDevKitDlcManager<S, O>>, transport: Arc<T>, receiver: Arc<Receiver<DlcManagerMessage>>) {
        while let Ok(msg) = receiver.recv() {
            match msg {
                DlcManagerMessage::OfferDlc { contract_input, counter_party, oracle_announcements, responder } => {
                    let offer = manager.send_offer_with_announcements(&contract_input, counter_party, vec![oracle_announcements]).expect("can't create offerdlc");
                    responder.send(offer).expect("send offer error")
                },
                DlcManagerMessage::AcceptDlc { contract, responder } => {
                    let accept = manager.accept_contract_offer(&contract).expect("can't accept offer");
                    responder.send(accept).expect("can't send")
                }
                DlcManagerMessage::ProcessMessages => {
                    let messages = transport.get_and_clear_received_messages();

                    for (counter_party, message) in messages {
                        tracing::info!(
                            counter_party = counter_party.to_string(),
                            "Processing DLC message"
                        );

                        let message_response = manager.on_dlc_message(&message, counter_party).expect("no on dlc message");
                        if let Some(msg) = message_response {
                            tracing::info!("Responding to message received.");
                            tracing::debug!(message=?msg);
                            transport.send_message(counter_party, msg);
                        }
                    }

                    if transport.has_pending_messages() {
                        transport.process_messages()
                    }
                }
            }
        }

    }

    pub fn connect_if_necessary(&self) -> anyhow::Result<()> {
        let _known_peers = self.storage.list_peers()?;

        // check from already connected

        Ok(())
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn send_dlc_offer(
        &self,
        contract_input: &ContractInput,
        counter_party: PublicKey,
        oracle_announcements: Vec<OracleAnnouncement>,
    ) -> anyhow::Result<OfferDlc> {
        let (responder, receiver) = unbounded();
        self.sender.send(DlcManagerMessage::OfferDlc { contract_input: contract_input.to_owned(), counter_party, oracle_announcements, responder }).expect("sending offer message");
        let offer = receiver.recv().expect("no offer dlc");

        let contract_id = hex::encode(&offer.temporary_contract_id);
        self.transport
            .send_message(counter_party, Message::Offer(offer.clone()));
        tracing::info!(
            counterparty = counter_party.to_string(),
            contract_id,
            "Sent DLC offer to counterparty."
        );

        Ok(offer)
    }

    pub fn accept_dlc_offer(
        &self,
        contract: [u8; 32],
    ) -> anyhow::Result<(String, String, AcceptDlc)> {
        let (responder, receiver) = unbounded();
        self.sender.send(DlcManagerMessage::AcceptDlc { contract, responder }).expect("couldnt send accept");
        let (contract_id, public_key, accept_dlc) = receiver.recv().expect("coudlnt accept dlc");

        self.transport
            .send_message(public_key, Message::Accept(accept_dlc.clone()));

        let contract_id = hex::encode(&contract_id);
        let counter_party = public_key.to_string();
        tracing::info!(counter_party, contract_id, "Accepted DLC contract.");

        Ok((contract_id, counter_party, accept_dlc))
    }
}

