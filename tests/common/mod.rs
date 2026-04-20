include!("../../src/test_utils/mod.rs");

use bitcoin::{Address, Amount, CompressedPublicKey};
use bitcoincore_rpc::RpcApi as _;
use bitvmx_bitcoin_rpc::bitcoin_client::{BitcoinClient, BitcoinClientApi};
use rust_bitvmx_bitcoin::{
    config::config::{BitcoinSettings, CoordinatorStorageSettings},
    coordinator::BitcoinCoordinator,
    core::storage::CoordinatorStorage,
    errors::BitcoinCoordinatorError,
    types::TransactionState,
};

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
            "Mine {} blocks to address {:?}",
            blocks_mined, regtest_wallet
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

// =============================================================================
// Coordinator construction helpers
// =============================================================================

/// Creates a `BitcoinCoordinator` from a `TestSetup` using default settings.
pub fn create_coordinator(setup: &TestSetup) -> BitcoinCoordinator {
    BitcoinCoordinator::new_with_paths(
        &setup.bitcoind.rpc_config,
        setup.storage.get_raw_storage(),
        None,
    )
    .expect("Failed to create BitcoinCoordinator")
}

/// Creates a `BitcoinCoordinator` from a `TestSetup` with custom `BitcoinSettings`.
pub fn create_coordinator_with_settings(
    setup: &TestSetup,
    settings: BitcoinSettings,
) -> BitcoinCoordinator {
    BitcoinCoordinator::new_with_paths(
        &setup.bitcoind.rpc_config,
        setup.storage.get_raw_storage(),
        Some(settings),
    )
    .expect("Failed to create BitcoinCoordinator with settings")
}

/// Returns a `CoordinatorStorage` view over the test setup's shared storage.
pub fn get_coord_storage(setup: &TestSetup) -> CoordinatorStorage {
    CoordinatorStorage::new(
        setup.storage.get_raw_storage(),
        CoordinatorStorageSettings::default(),
    )
}

// =============================================================================
// Transaction helpers
// =============================================================================

/// Creates a funded, signed Bitcoin transaction that is **not yet broadcast**.
///
/// Internally this:
/// 1. Sends 1 000 000 sats from the test wallet to itself (via `fund_address`),
///    mining one confirming block in the process.
/// 2. Builds a raw transaction that spends that output (900 000 sats to a new
///    wallet address, leaving 100 000 sats for fees).
/// 3. Signs the transaction with the test wallet.
/// 4. Returns the signed `Transaction` object without broadcasting it.
///
/// The returned transaction is immediately valid for broadcast and can be
/// handed to the coordinator for dispatch.
pub fn create_signed_tx_to_dispatch(bitcoin_client: &BitcoinClient) -> anyhow::Result<Transaction> {
    // Ensure the wallet is loaded and get a wallet address.
    let wallet_address = bitcoin_client
        .init_wallet("test_wallet")
        .map_err(|e| anyhow::anyhow!("init_wallet failed: {:?}", e))?;

    // Fund the wallet address (broadcasts + mines 1 block so the output is confirmed).
    let (funding_tx, funding_vout) = bitcoin_client
        .fund_address(&wallet_address, Amount::from_sat(1_000_000))
        .map_err(|e| anyhow::anyhow!("fund_address failed: {:?}", e))?;
    let funding_txid = funding_tx.compute_txid();

    // Pick a fresh recipient address.
    let recipient = bitcoin_client
        .client
        .get_new_address(None, Some(bitcoincore_rpc::json::AddressType::Bech32))
        .map_err(|e| anyhow::anyhow!("get_new_address failed: {:?}", e))?;

    // Build a raw transaction spending the funded UTXO.
    let inputs = vec![bitcoincore_rpc::json::CreateRawTransactionInput {
        txid: funding_txid,
        vout: funding_vout,
        sequence: None,
    }];
    let mut outputs = std::collections::HashMap::new();
    outputs.insert(
        format!("{}", recipient.assume_checked()),
        Amount::from_sat(900_000),
    );
    let raw_tx = bitcoin_client
        .client
        .create_raw_transaction(&inputs, &outputs, None, None)
        .map_err(|e| anyhow::anyhow!("create_raw_transaction failed: {:?}", e))?;

    // Sign with the wallet key.
    let signed = bitcoin_client
        .client
        .sign_raw_transaction_with_wallet(&raw_tx, None, None)
        .map_err(|e| anyhow::anyhow!("sign_raw_transaction_with_wallet failed: {:?}", e))?;
    anyhow::ensure!(
        signed.complete,
        "Transaction signing incomplete: {:?}",
        signed.errors
    );

    //TODO: check if nedded:
    // Lock the funding UTXO so that subsequent wallet operations (e.g. a
    // second call to `fund_address`) do not select it as an input while this
    // transaction is still unbroadcast.  Without this, the wallet sees the
    // UTXO as available and may spend it in the next `send_to_address` call,
    // causing this signed-but-unbroadcast transaction to fail with
    // "bad-txns-inputs-missingorspent" when it is later submitted.
    bitcoin_client
        .client
        .lock_unspent(&[bitcoin::OutPoint::new(funding_txid, funding_vout)])
        .map_err(|e| anyhow::anyhow!("lock_unspent failed: {:?}", e))?;

    // Decode to a `Transaction` (still unbroadcast).
    let tx = bitcoin::consensus::Decodable::consensus_decode(&mut &signed.hex[..])
        .map_err(|e| anyhow::anyhow!("consensus_decode failed: {:?}", e))?;
    Ok(tx)
}

/// Creates a funded, signed Bitcoin transaction with zero fee.
/// Identical to [`create_signed_tx_to_dispatch`] except that the full 1 000 000 sat
/// input is forwarded as output (leaving 0 sats for miners).  
pub fn create_zero_fee_tx(bitcoin_client: &BitcoinClient) -> anyhow::Result<Transaction> {
    let wallet_address = bitcoin_client
        .init_wallet("test_wallet")
        .map_err(|e| anyhow::anyhow!("init_wallet failed: {:?}", e))?;

    let (funding_tx, funding_vout) = bitcoin_client
        .fund_address(&wallet_address, Amount::from_sat(1_000_000))
        .map_err(|e| anyhow::anyhow!("fund_address failed: {:?}", e))?;
    let funding_txid = funding_tx.compute_txid();

    let recipient = bitcoin_client
        .client
        .get_new_address(None, Some(bitcoincore_rpc::json::AddressType::Bech32))
        .map_err(|e| anyhow::anyhow!("get_new_address failed: {:?}", e))?;

    let inputs = vec![bitcoincore_rpc::json::CreateRawTransactionInput {
        txid: funding_txid,
        vout: funding_vout,
        sequence: None,
    }];
    let mut outputs = std::collections::HashMap::new();
    // Output equals input — leaves exactly 0 sats as fee.
    outputs.insert(
        format!("{}", recipient.assume_checked()),
        Amount::from_sat(1_000_000),
    );

    let raw_tx = bitcoin_client
        .client
        .create_raw_transaction(&inputs, &outputs, None, None)
        .map_err(|e| anyhow::anyhow!("create_raw_transaction failed: {:?}", e))?;

    let signed = bitcoin_client
        .client
        .sign_raw_transaction_with_wallet(&raw_tx, None, None)
        .map_err(|e| anyhow::anyhow!("sign_raw_transaction_with_wallet failed: {:?}", e))?;
    anyhow::ensure!(signed.complete, "signing incomplete: {:?}", signed.errors);

    bitcoin_client
        .client
        .lock_unspent(&[bitcoin::OutPoint::new(funding_txid, funding_vout)])
        .map_err(|e| anyhow::anyhow!("lock_unspent failed: {:?}", e))?;

    let tx = bitcoin::consensus::Decodable::consensus_decode(&mut &signed.hex[..])
        .map_err(|e| anyhow::anyhow!("consensus_decode failed: {:?}", e))?;
    Ok(tx)
}

/// Mines `n` blocks to `address` using `bitcoin_client`.
pub fn mine_blocks(
    bitcoin_client: &BitcoinClient,
    n: u64,
    address: &Address,
) -> anyhow::Result<()> {
    bitcoin_client
        .mine_blocks_to_address(n, address)
        .map_err(|e| anyhow::anyhow!("mine_blocks_to_address failed: {:?}", e))
}

// =============================================================================
// Monitor / coordinator sync helpers
// =============================================================================

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

/// Poll the coordinator storage until `txid` reaches `expected_state`, or
/// until `max_ticks` ticks have been performed.  Returns `true` if the state
/// was reached.
pub fn tick_until_state(
    coordinator: &BitcoinCoordinator,
    storage: &CoordinatorStorage,
    txid: bitcoin::Txid,
    expected_state: TransactionState,
    max_ticks: u32,
) -> Result<bool, BitcoinCoordinatorError> {
    for i in 0..max_ticks {
        coordinator.tick()?;
        if let Some(tx) = storage.get_tx_by_id(txid)? {
            if tx.state == expected_state {
                info!(
                    "After {} ticks, reached expected state {:?} for txid {}",
                    i + 1,
                    expected_state,
                    txid
                );
                return Ok(true);
            }
        }
    }
    Ok(false)
}
