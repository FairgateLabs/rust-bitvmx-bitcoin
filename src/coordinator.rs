use std::rc::Rc;

use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::{
    bitcoin_client::{BitcoinClient, BitcoinClientApi},
    rpc_config::RpcConfig,
    types::BlockHeight,
};
use bitvmx_transaction_monitor::{
    monitor::Monitor,
    types::{MonitorNews, TypesToMonitor},
    TransactionStatus,
};
use key_manager::key_manager::KeyManager;
use protocol_builder::types::{output::SpeedupData, Utxo};
use storage_backend::storage::Storage;
use tracing::{debug, info, warn};

use crate::{
    config::{
        configs::BitcoinSettings,
        settings::{CPFP_TRANSACTION_CONTEXT, RBF_TRANSACTION_CONTEXT},
    },
    core::{
        dispatcher::Dispatcher, fee::FeeManager, funding::FundingManager,
        storage::CoordinatorStorage,
    },
    engines::{
        common::EngineContext, speedup_engine::SpeedupEngine, transaction_engine::TransactionEngine,
    },
    errors::BitcoinCoordinatorError,
    types::{AckNews, CoordinatedTx, CoordinatorNews, News, TransactionState, TxKind},
};

pub struct BitcoinCoordinator {
    speedup_engine: SpeedupEngine,
    tx_engine: TransactionEngine,
}

impl BitcoinCoordinator {
    /// Builds a coordinator wired to the given RPC node and shared storage.
    ///
    /// * `rpc_config` - Bitcoin RPC endpoint and network.
    /// * `storage` - Shared persistent backend used by every internal component.
    /// * `key_manager` - Signer used to build CPFP/RBF speedup transactions.
    /// * `settings` - Optional override for tuning constants. Defaults are used
    ///   when `None`.
    pub fn new_with_paths(
        rpc_config: &RpcConfig,
        storage: Rc<Storage>,
        key_manager: Rc<KeyManager>,
        settings: Option<BitcoinSettings>,
    ) -> Result<Self, BitcoinCoordinatorError> {
        let settings = settings.unwrap_or_default();
        settings.validate()?;

        let bitcoin_client = Rc::new(BitcoinClient::new_from_config(rpc_config)?);

        // Fail fast if it is off.
        if !bitcoin_client.is_txindex_enabled()? {
            return Err(BitcoinCoordinatorError::InvalidConfiguration(
                "the Bitcoin node must run with -txindex=1".to_string(),
            ));
        }

        let monitor = Monitor::new_with_paths(rpc_config, storage.clone(), Some(settings.monitor))?;

        // Share a single Rc<CoordinatorStorage> between EngineContext and FundingManager.
        let coordinator_storage = Rc::new(CoordinatorStorage::new(storage, settings.storage));
        let cs_for_funding: Rc<CoordinatorStorage> = Rc::clone(&coordinator_storage);
        let funding_storage: Rc<dyn crate::core::funding::FundingStorage> = cs_for_funding;
        let funding_manager = FundingManager::new(settings.funding, funding_storage);
        let cs_for_dispatcher: Rc<CoordinatorStorage> = Rc::clone(&coordinator_storage);
        let dispatcher_storage: Rc<dyn crate::core::dispatcher::DispatcherStorage> =
            cs_for_dispatcher;
        let dispatcher = Dispatcher::new(settings.dispatcher, bitcoin_client, dispatcher_storage);
        let fee_manager = FeeManager::new(settings.fee);
        let coordinator_config = settings.coordinator;

        let ctx = Rc::new(EngineContext::new(
            monitor,
            fee_manager,
            funding_manager,
            dispatcher,
            coordinator_storage,
            coordinator_config,
        ));

        let speedup_engine = SpeedupEngine::new(Rc::clone(&ctx), key_manager, settings.speedup);
        let tx_engine = TransactionEngine::new(ctx);

        Ok(Self {
            speedup_engine,
            tx_engine,
        })
    }

    // =========================================================================
    // Public API
    // =========================================================================

    /// Returns `true` when the monitor is fully synced with the chain.
    pub fn is_ready(&self) -> Result<bool, BitcoinCoordinatorError> {
        Ok(self.tx_engine.ctx.monitor.is_ready()?)
    }

    /// Advances the monitor and runs one tick of the coordinator. No-op while
    /// [`Self::is_ready`] is `false`.
    ///
    /// The tick is a strict 6-step pipeline. Build/save and dispatch are split
    /// across consecutive ticks so a crash between RPC send and storage commit
    /// cannot leave an on-chain transaction without a local record.
    ///
    /// 1. Review in-flight non-speedups; no dispatch here.
    /// 2. Review in-flight speedups; no dispatch here.
    /// 3. Dispatch `ToDispatch` non-speedups (parents and plain txs).
    /// 4. Dispatch `ToDispatch` speedups built in a previous tick (or
    ///    re-queued by step 2's not_found path).
    /// 5. Boost the latest live speedup if stale; save `ToDispatch` for next tick.
    /// 6. Build one CPFP batch for pending parents; save `ToDispatch` for next tick.
    pub fn tick(&self) -> Result<(), BitcoinCoordinatorError> {
        self.tx_engine.ctx.monitor.tick()?;

        if !self.is_ready()? {
            debug!("Coordinator not ready, skipping tick");
            return Ok(());
        }

        let current_height = self.tx_engine.ctx.monitor.get_monitor_height()?;
        self.tx_engine.ctx.storage.cleanup_news(current_height)?; // Cleanup acknowledged news

        self.tx_engine.review_active()?;
        self.speedup_engine.review_speedups()?;
        self.tx_engine.dispatch_pending()?;
        let boost_failed = self.speedup_engine.dispatch_pending_speedups()?;
        self.speedup_engine.boost_if_stale(boost_failed)?;
        self.speedup_engine.create_cpfp_batch()?;

        Ok(())
    }

    /// Registers a transaction for dispatch without speedup support.
    ///
    /// * `tx` - Signed transaction to broadcast.
    /// * `context` - Opaque client-defined tag echoed back in news for this tx.
    /// * `target_block_height` - Earliest block at which to broadcast. `None`
    ///   means dispatch as soon as possible.
    /// * `confirmation_trigger` - Emit a confirmation news at this confirmation
    ///   count. `None` disables it.
    /// * `stuck_in_mempool_blocks` - Emit a `TransactionStuckInMempool` news
    ///   after this many blocks in the mempool. `None` disables the check.
    pub fn dispatch_without_speedup(
        &self,
        tx: Transaction,
        context: String,
        target_block_height: Option<u32>,
        confirmation_trigger: Option<u32>,
        stuck_in_mempool_blocks: Option<u32>,
    ) -> Result<(), BitcoinCoordinatorError> {
        let txid = tx.compute_txid();
        self.register_tx(
            tx,
            crate::types::TxKind::Normal,
            context,
            target_block_height,
            confirmation_trigger,
            stuck_in_mempool_blocks,
        )?;
        info!("Transaction({}) registered for dispatch", txid);
        Ok(())
    }

    /// Registers a transaction for dispatch with CPFP speedup support enabled.
    ///
    /// Stuck-in-mempool detection is always disabled: the coordinator boosts
    /// the parent automatically when it persists in the mempool.
    ///
    /// * `tx` - Signed parent transaction.
    /// * `speedup_data` - Signing/UTXO metadata used to build a CPFP for `tx`.
    /// * `context` - Opaque client-defined tag echoed back in news for this tx.
    /// * `target_block_height` - Earliest block at which to broadcast. `None`
    ///   means dispatch as soon as possible.
    /// * `confirmation_trigger` - Emit a confirmation news at this confirmation
    ///   count. `None` disables it.
    pub fn dispatch_with_speedup(
        &self,
        tx: Transaction,
        speedup_data: SpeedupData,
        context: String,
        target_block_height: Option<u32>,
        confirmation_trigger: Option<u32>,
    ) -> Result<(), BitcoinCoordinatorError> {
        let txid = tx.compute_txid();
        // Stuck-in-mempool detection is not needed: the coordinator will create
        // a boost (CPFP/RBF) when a speedup-enabled parent is stuck.
        self.register_tx(
            tx,
            TxKind::NeedsSpeedup(speedup_data),
            context,
            target_block_height,
            confirmation_trigger,
            None,
        )?;
        info!("Transaction({}) registered for dispatch with speedup", txid);
        Ok(())
    }

    /// Dispatches a transaction with or without speedup support, chosen by the
    /// presence of `speedup_data`. Stuck-in-mempool detection is always
    /// disabled through this entrypoint.
    ///
    /// * `tx` - Signed transaction to broadcast.
    /// * `speedup_data` - Enables CPFP support when `Some`. Plain dispatch when
    ///   `None`.
    /// * `context` - Opaque client-defined tag echoed back in news for this tx.
    /// * `target_block_height` - Earliest block at which to broadcast. `None`
    ///   means dispatch as soon as possible.
    /// * `confirmation_trigger` - Emit a confirmation news at this confirmation
    ///   count. `None` disables it.
    pub fn dispatch(
        &self,
        tx: Transaction,
        speedup_data: Option<SpeedupData>,
        context: String,
        target_block_height: Option<u32>,
        confirmation_trigger: Option<u32>,
    ) -> Result<(), BitcoinCoordinatorError> {
        match speedup_data {
            Some(data) => self.dispatch_with_speedup(
                tx,
                data,
                context,
                target_block_height,
                confirmation_trigger,
            ),
            None => self.dispatch_without_speedup(
                tx,
                context,
                target_block_height,
                confirmation_trigger,
                None,
            ),
        }
    }

    /// Cancels monitoring and removes the targeted transactions from coordinator storage.
    ///
    /// Only client-registered txs (`Normal` / `NeedsSpeedup`) still in `ToDispatch` state are cancellable.
    /// Already-dispatched, Speedup-kind, fundings or missing txids are refused and a news item is
    /// emitted per rejected txid. Non-`Transactions` variants (`OutputPattern`, `SpendingUTXOTransaction`,
    /// `NewBlock`) pass through to monitor cancellation as-is.
    ///
    /// * `data` - Monitoring entry to cancel.
    pub fn cancel(&self, data: TypesToMonitor) -> Result<(), BitcoinCoordinatorError> {
        match data {
            TypesToMonitor::Transactions(txids, context, confirmation_trigger) => {
                self.cancel_transactions(txids, context, confirmation_trigger)
            }
            other => {
                self.tx_engine.ctx.monitor.cancel(other.clone())?;
                info!("Cancelled monitoring for {:?}", other);
                Ok(())
            }
        }
    }

    /// Registers a funding UTXO available to pay future speedup fees. Emits an
    /// `InvalidFundingUtxo` news item when the UTXO is below
    /// `min_funding_amount_sats` instead of storing it.
    ///
    /// * `utxo` - Spendable UTXO to register as funding.
    pub fn add_funding(&self, utxo: Utxo) -> Result<(), BitcoinCoordinatorError> {
        info!(
            "Funding added | Txid({}) | Vout({}) | Amount({})",
            utxo.txid, utxo.vout, utxo.amount
        );
        if let Some(news) = self.tx_engine.ctx.funding_manager.set_funding(utxo)? {
            self.tx_engine.ctx.storage.add_news(news)?;
        }
        Ok(())
    }

    /// Queries the current blockchain status of a transaction via the monitor.
    /// The mempool is also searched.
    ///
    /// * `txid` - Transaction to look up.
    pub fn get_transaction(
        &self,
        txid: Txid,
    ) -> Result<TransactionStatus, BitcoinCoordinatorError> {
        Ok(self.tx_engine.ctx.monitor.get_tx_status(&txid, true)?)
    }

    /// Returns all unacknowledged monitor and coordinator news. Internal
    /// CPFP/RBF speedup news entries are filtered out.
    pub fn get_news(&self) -> Result<News, BitcoinCoordinatorError> {
        let current_height = self.tx_engine.ctx.monitor.get_monitor_height()?;
        let monitor_news = self.tx_engine.ctx.monitor.get_news()?;
        let coordinator_news = self
            .tx_engine
            .ctx
            .storage
            .get_and_mark_news(current_height)?;

        // Filter out internal coordinator transactions (CPFP/RBF speedups),
        // since the client's Context does not distinguish speedup variants.
        let monitor_news: Vec<MonitorNews> = monitor_news
            .into_iter()
            .filter(|news| {
                if let MonitorNews::Transaction(n) = news {
                    !n.context.contains(CPFP_TRANSACTION_CONTEXT)
                        && !n.context.contains(RBF_TRANSACTION_CONTEXT)
                } else {
                    true
                }
            })
            .collect();

        Ok(News {
            monitor_news,
            coordinator_news,
        })
    }

    /// Acknowledges a news item so it is not returned again.
    ///
    /// * `news` - Monitor or coordinator news entry to mark as consumed.
    pub fn ack_news(&self, news: AckNews) -> Result<(), BitcoinCoordinatorError> {
        match news {
            AckNews::Monitor(n) => self.tx_engine.ctx.monitor.ack_news(n)?,
            AckNews::Coordinator(n) => {
                let current_height = self.tx_engine.ctx.monitor.get_monitor_height()?;
                self.tx_engine.ctx.storage.ack_news(n, current_height)?;
            }
        }
        Ok(())
    }

    /// Registers data to be monitored without scheduling a dispatch. Mempool
    /// search is disabled here; it is enabled internally once the related
    /// transaction is broadcast.
    ///
    /// * `data` - Monitoring target (txids, output pattern, spending UTXO, or
    ///   new block).
    pub fn monitor(&self, data: TypesToMonitor) -> Result<(), BitcoinCoordinatorError> {
        self.tx_engine.ctx.monitor.monitor(data, false)?;
        Ok(())
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Persist a transaction and register it with the monitor.
    fn register_tx(
        &self,
        tx: Transaction,
        kind: TxKind,
        context: String,
        target_block_height: Option<BlockHeight>,
        confirmation_trigger: Option<u32>,
        stuck_in_mempool_blocks: Option<u32>,
    ) -> Result<(), BitcoinCoordinatorError> {
        let txid = tx.compute_txid();

        // Check if the txid is already registered before doing any work.
        if let Some(tx) = self.tx_engine.ctx.storage.get_tx_by_id(txid)? {
            warn!(
                "Transaction({}) is already registered, skipping. Transaction info: {:?}",
                txid, tx
            );
            return Ok(());
        }

        let current_height = self.tx_engine.ctx.monitor.get_monitor_height()?;
        let target_height = target_block_height.unwrap_or(current_height);
        let fee_manager = &self.tx_engine.ctx.fee_manager;

        let (fee_rate, _) = fee_manager.get_network_fee_rate(&self.tx_engine.ctx.monitor)?;
        let fee_info = fee_manager.compute_fee_for_tx(&tx, fee_rate);

        // Register for confirmation tracking (mempool search disabled until after
        // the tx is actually broadcast).
        self.tx_engine.ctx.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], context.clone(), confirmation_trigger),
            false,
        )?;

        self.tx_engine.ctx.storage.insert_tx(CoordinatedTx {
            txid,
            tx,
            kind: kind.clone(),
            state: TransactionState::ToDispatch,
            broadcast_block_height: None,
            target_block_height: target_height,
            stuck_in_mempool_blocks,
            confirmation_trigger,
            settled_block_height: None,
            fail_guard_until: None,
            retry_count: 0,
            fee_info,
            context,
        })?;

        if matches!(kind, TxKind::NeedsSpeedup(_)) {
            self.tx_engine
                .ctx
                .storage
                .add_pending_speedup_parent(txid)?;
        }

        Ok(())
    }

    /// Classify each txid, emit `InvalidCancel` news for rejected ones, then drop
    /// PSP entries + cancel monitoring + remove storage for the eligible set.
    fn cancel_transactions(
        &self,
        txids: Vec<Txid>,
        context: String,
        confirmation_trigger: Option<u32>,
    ) -> Result<(), BitcoinCoordinatorError> {
        let mut eligible: Vec<Txid> = Vec::new();
        for txid in &txids {
            match self.classify_cancel_request(*txid)? {
                Ok(()) => eligible.push(*txid),
                Err(reason) => {
                    warn!(%txid, %reason, "cancel refused");
                    self.tx_engine
                        .ctx
                        .storage
                        .add_news(CoordinatorNews::InvalidCancel {
                            txid: *txid,
                            reason,
                        })?;
                }
            }
        }

        if eligible.is_empty() {
            return Ok(());
        }

        // Drop NeedsSpeedup parents from PSP explicitly.
        for txid in &eligible {
            self.tx_engine
                .ctx
                .storage
                .remove_pending_speedup_parent(*txid)?;
        }
        self.tx_engine
            .ctx
            .monitor
            .cancel(TypesToMonitor::Transactions(
                eligible.clone(),
                context,
                confirmation_trigger,
            ))?;
        for txid in &eligible {
            self.tx_engine.ctx.storage.remove_tx(*txid)?;
        }
        info!("Cancelled {} tx(s): {:?}", eligible.len(), eligible);
        Ok(())
    }

    /// `Ok(Ok(()))` when the txid is eligible for cancel; `Ok(Err(reason))` when
    /// rejected, with the reason string the operator will see in the news.
    fn classify_cancel_request(
        &self,
        txid: Txid,
    ) -> Result<Result<(), String>, BitcoinCoordinatorError> {
        let Some(tx) = self.tx_engine.ctx.storage.get_tx_by_id(txid)? else {
            return Ok(Err("tx not found in coordinator storage".to_string()));
        };
        if !matches!(tx.kind, TxKind::Normal | TxKind::NeedsSpeedup(_)) {
            return Ok(Err(format!(
                "tx kind {:?} is not cancellable (only Normal / NeedsSpeedup)",
                tx.kind
            )));
        }
        if tx.state != TransactionState::ToDispatch {
            return Ok(Err(format!(
                "tx already dispatched (state={:?}); cancel is only valid in ToDispatch",
                tx.state
            )));
        }
        Ok(Ok(()))
    }
}
