use std::{
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Result};
use async_trait::async_trait;
use bdk::{
    bitcoin::util::bip32::ExtendedPrivKey,
    bitcoincore_rpc::{bitcoincore_rpc_json::ScanningDetails, RpcApi},
    blockchain::{
        rpc::{Auth, RpcSyncParams},
        ConfigurableBlockchain, RpcBlockchain, RpcConfig,
    },
    wallet::AddressInfo,
    Balance, FeeRate, SignOptions, SyncOptions,
};
use bitcoin::{
    util::bip32::{ChildNumber, DerivationPath},
    Address, OutPoint, Script, Transaction,
};
use database::wallet_database::WalletDatabase;
use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator};
use log::{error, info};
use settings::Network;
use settings::Settings;

use crate::{api::WalletInterface, bitcoind::BitcoindClient};

pub struct Wallet {
    // bdk::Wallet uses a RefCell to hold the database which is not thread safe so we use a mutex here.
    wallet: Arc<Mutex<bdk::Wallet<WalletDatabase>>>,
    bitcoind_client: Arc<BitcoindClient>,
}

#[async_trait]
impl WalletInterface for Wallet {
    fn balance(&self) -> Result<Balance> {
        match self.wallet.try_lock() {
            Ok(wallet) => Ok(wallet.get_balance()?),
            Err(_) => Ok(Balance::default()),
        }
    }

    async fn transfer(
        &self,
        address: Address,
        amount: u64,
        fee_rate: Option<FeeRate>,
        min_conf: Option<u8>,
        utxos: Vec<OutPoint>,
    ) -> Result<Transaction> {
        let height = self.bitcoind_client.get_blockchain_info().await?.blocks as u32;

        match self.wallet.try_lock() {
            Ok(wallet) => {
                let mut tx_builder = wallet.build_tx();
                if amount == u64::MAX {
                    tx_builder.drain_wallet().drain_to(address.script_pubkey());
                } else {
                    tx_builder
                        .add_recipient(address.script_pubkey(), amount)
                        .drain_wallet()
                        .add_utxos(&utxos)?;
                }
                tx_builder.current_height(
                    min_conf.map_or_else(|| height, |min_conf| height - min_conf as u32),
                );
                if let Some(fee_rate) = fee_rate {
                    tx_builder.fee_rate(fee_rate);
                }
                let tx = tx_builder.finish()?.0.extract_tx();
                Ok(tx)
            }
            Err(_) => bail!("Wallet is still syncing with chain"),
        }
    }

    fn new_address(&self) -> Result<AddressInfo> {
        let address = self
            .wallet
            .try_lock()
            .unwrap()
            .get_address(bdk::wallet::AddressIndex::LastUnused)?;
        Ok(address)
    }
}

impl Wallet {
    pub fn new(
        seed: &[u8; 32],
        settings: &Settings,
        bitcoind_client: Arc<BitcoindClient>,
        database: WalletDatabase,
    ) -> Result<Wallet> {
        let xprivkey = ExtendedPrivKey::new_master(settings.bitcoin_network.into(), seed)?;
        let native_segwit_base_path = "m/84";

        let coin_type = match settings.bitcoin_network {
            Network::Main => 0,
            _ => 1,
        };

        let base_path = DerivationPath::from_str(native_segwit_base_path)?;
        let derivation_path = base_path.extend([ChildNumber::from_hardened_idx(coin_type)?]);
        let receive_descriptor_template = bdk::descriptor!(wpkh((
            xprivkey,
            derivation_path.extend([ChildNumber::Normal { index: 0 }])
        )))?;
        let change_descriptor_template = bdk::descriptor!(wpkh((
            xprivkey,
            derivation_path.extend([ChildNumber::Normal { index: 1 }])
        )))?;

        let bdk_wallet = Arc::new(Mutex::new(bdk::Wallet::new(
            receive_descriptor_template,
            Some(change_descriptor_template),
            settings.bitcoin_network.into(),
            database,
        )?));

        let url = format!(
            "http://{}:{}",
            settings.bitcoind_rpc_host, settings.bitcoind_rpc_port
        );

        // Sometimes we get wallet sync failure - https://github.com/bitcoindevkit/bdk/issues/859
        // It prevents a historical sync. So only add funds while kld is running.
        let rpc_sync_params = RpcSyncParams {
            start_script_count: 100,
            start_time: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            force_start_time: false,
            poll_rate_sec: 10,
        };

        let wallet_config = RpcConfig {
            url: url.clone(),
            auth: Auth::Cookie {
                file: settings.bitcoin_cookie_path.clone().into(),
            },
            network: settings.bitcoin_network.into(),
            wallet_name: "kld-wallet".to_string(),
            sync_params: Some(rpc_sync_params),
        };
        let blockchain = RpcBlockchain::from_config(&wallet_config)?;

        let wallet_clone = bdk_wallet.clone();
        tokio::task::spawn_blocking(move || {
            loop {
                match blockchain.get_wallet_info() {
                    Ok(wallet_info) => {
                        match wallet_info.scanning {
                            Some(ScanningDetails::Scanning { duration, progress }) => {
                                info!(
                                    "Wallet is synchronising with the blockchain. {}% progress after {} seconds.",
                                    (progress * 100_f32).round(),
                                    duration
                                );
                            }
                            _ => {
                                // Don't want to block for a long time while the wallet is syncing so use try_lock everywhere else.
                                if let Err(e) = wallet_clone
                                    .lock()
                                    .expect("Cannot obtain mutex for wallet")
                                    .sync(&blockchain, SyncOptions::default())
                                {
                                    error!("Walled sync failed with bitcoind rpc endpoint {url:}. Check the logs of your bitcoind for more context: {e:}");
                                } else {
                                    info!("Wallet is synchronised to blockchain");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Could not get wallet info: {e}");
                    }
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        });

        Ok(Wallet {
            wallet: bdk_wallet,
            bitcoind_client,
        })
    }

    pub fn fund_tx(
        &self,
        output_script: &Script,
        channel_value_satoshis: &u64,
    ) -> Result<Transaction> {
        let wallet = self.wallet.try_lock().unwrap();

        let mut tx_builder = wallet.build_tx();
        let fee_sats_per_1000_wu = self
            .bitcoind_client
            .get_est_sat_per_1000_weight(ConfirmationTarget::Normal);

        // TODO: is this the correct conversion??
        let sat_per_vb = match fee_sats_per_1000_wu {
            253 => 1.0,
            _ => fee_sats_per_1000_wu as f32 / 250.0,
        };

        let fee_rate = FeeRate::from_sat_per_vb(sat_per_vb);

        tx_builder
            .add_recipient(output_script.clone(), *channel_value_satoshis)
            .fee_rate(fee_rate)
            .enable_rbf();

        let (mut psbt, _tx_details) = tx_builder.finish()?;

        let _finalized = wallet.sign(&mut psbt, SignOptions::default())?;

        let funding_tx = psbt.extract_tx();
        Ok(funding_tx)
    }
}
