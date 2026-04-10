use bitcoin::{Address, CompressedPublicKey, Network, PublicKey};
use bitcoind::bitcoind::BitcoindFlags;
use bitvmx_bitcoin_rpc::bitcoin_client::{BitcoinClient, BitcoinClientApi};
use console::style;
use rust_bitvmx_bitcoin::{
    coordinator::BitcoinCoordinator,
    errors::BitcoinCoordinatorError,
    test_utils::{
        dummy_pubkey, init_trace as internal_init_trace, StorageTestConfig, TestBitcoind,
    },
};
use std::rc::Rc;
use tracing::info;

/// Configuration for creating a test setup
pub struct TestSetupConfig {
    pub blocks_mined: u32,
    pub bitcoind_flags: Option<BitcoindFlags>,
}

impl Default for TestSetupConfig {
    fn default() -> Self {
        Self {
            blocks_mined: 102,
            bitcoind_flags: None,
        }
    }
}

/// Test setup components that are commonly used across tests
pub struct TestSetup {
    pub storage: StorageTestConfig,
    pub bitcoind: TestBitcoind,
    pub bitcoin_client: Rc<BitcoinClient>,
    pub public_key: PublicKey,
    pub funding_wallet: Address,
    pub regtest_wallet: Address,
}

impl TestSetup {
    /// Creates a complete test setup with all common components
    pub fn new(config: TestSetupConfig) -> Result<Self, anyhow::Error> {
        let bitcoind = TestBitcoind::new(None, config.bitcoind_flags)?;

        let storage = StorageTestConfig::new();
        let bitcoin_client = Rc::new(BitcoinClient::new_from_config(&bitcoind.rpc_config)?);
        let (public_key, funding_wallet, regtest_wallet) = Self::setup_wallet_and_mine_blocks(
            &bitcoin_client,
            bitcoind.rpc_config.network,
            config.blocks_mined,
        )?;

        Ok(TestSetup {
            bitcoind,
            storage,
            bitcoin_client,
            public_key,
            funding_wallet,
            regtest_wallet,
        })
    }

    /// Sets up wallet and mines initial blocks
    fn setup_wallet_and_mine_blocks(
        bitcoin_client: &Rc<BitcoinClient>,
        network: Network,
        blocks_mined: u32,
    ) -> Result<(PublicKey, Address, Address), anyhow::Error> {
        let public_key = dummy_pubkey();
        let compressed = CompressedPublicKey::try_from(public_key)
            .map_err(|e| anyhow::anyhow!("Failed to compress public key: {:?}", e))?;
        let funding_wallet = Address::p2wpkh(&compressed, network);
        let regtest_wallet = bitcoin_client
            .init_wallet("test_wallet")
            .map_err(|e| anyhow::anyhow!("Failed to init wallet: {:?}", e))?;

        info!(
            "{} Mine {} blocks to address {:?}",
            style("Test").green(),
            blocks_mined,
            regtest_wallet
        );

        bitcoin_client
            .mine_blocks_to_address(blocks_mined as u64, &regtest_wallet)
            .map_err(|e| anyhow::anyhow!("Failed to mine blocks: {:?}", e))?;

        Ok((public_key, funding_wallet, regtest_wallet))
    }

    pub fn end_all(self) -> Result<(), anyhow::Error> {
        self.bitcoind.stop()?;
        self.storage.remove()?;
        Ok(())
    }
}

/// Tick the coordinator until `is_ready()` returns `true`.
pub fn tick_until_ready(coordinator: &BitcoinCoordinator) -> Result<(), BitcoinCoordinatorError> {
    loop {
        coordinator.tick()?;
        if coordinator.is_ready()? {
            break;
        }
    }
    Ok(())
}

pub fn init_trace() {
    internal_init_trace();
}
