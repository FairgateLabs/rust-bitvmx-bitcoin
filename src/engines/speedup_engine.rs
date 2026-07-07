use std::rc::Rc;

use bitcoin::{OutPoint, Transaction, Txid};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::types::TypesToMonitor;
use key_manager::key_manager::KeyManager;
use protocol_builder::{
    builder::ProtocolBuilder,
    types::{
        output::{SpeedupData, MAX_DUST_LIMIT},
        Utxo,
    },
};
use tracing::{debug, info, warn};

use crate::{
    config::{
        config::SpeedupSettings,
        settings::{CPFP_TRANSACTION_CONTEXT, RBF_TRANSACTION_CONTEXT},
    },
    core::fee::FeeManager,
    engines::common::EngineContext,
    errors::BitcoinCoordinatorError,
    types::{
        CoordinatedTx, CoordinatorNews, FeeInfo, SpeedupContext, SpeedupKind, TransactionState,
        TxKind,
    },
};

struct SpeedupBuildResult {
    tx: Transaction,
    fee: u64,
    capped: bool,
    funding_inputs: Vec<Utxo>,
}

/// SpeedupEngine implements the four speedup-related phases of `tick()`:
/// 1. `dispatch_pending_speedups`: broadcast `ToDispatch` speedups built in a prior tick.
/// 2. `review_speedups`: update state from chain (no dispatch).
/// 3. `boost_if_stale`: build a boost CPFP or RBF, save as `ToDispatch`.
/// 4. `create_cpfp_batch`: build one CPFP for the next PendingSpeedupParents batch.
pub struct SpeedupEngine {
    ctx: Rc<EngineContext>,
    key_manager: Rc<KeyManager>,
    settings: SpeedupSettings,
}

impl SpeedupEngine {
    pub fn new(
        ctx: Rc<EngineContext>,
        key_manager: Rc<KeyManager>,
        settings: SpeedupSettings,
    ) -> Self {
        Self {
            ctx,
            key_manager,
            settings,
        }
    }

    /// Step 2 of `tick`: update each speedup's state from chain. Never dispatches.
    /// Reorg of a Confirmed speedup moves it back to InMempool.
    pub fn review_speedups(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;
        if all_speedups.is_empty() {
            return Ok(());
        }

        let max_confs = self.ctx.monitor.settings.max_monitoring_confirmations;

        for tx in &all_speedups {
            if !matches!(
                tx.state,
                TransactionState::InMempool | TransactionState::Confirmed
            ) {
                continue;
            }

            let status = self.ctx.monitor.get_tx_status(&tx.txid, true)?;

            if status.is_in_mempool() {
                if tx.state == TransactionState::Confirmed {
                    self.ctx.mark_reorg(tx, current_height)?;
                }
                continue;
            }

            // Tx is neither in mempool nor on chain.
            if status.is_not_found() {
                // A speedup we deliberately replaced (RBF) must not be re-queued or guarded; its
                // disappearance is the intended local swap, not a reorg flap. See
                // `CoordinatedTx::has_live_replacement` for why both signals are needed.
                if tx.has_live_replacement(&all_speedups) {
                    continue;
                }
                // Otherwise re-queue the same tx for dispatch this tick. Step 4 sends the exact same tx. Possible outcomes:
                //   - AlreadyKnown / AlreadyConfirmed → false positive; revert.
                //   - Success                         → re-broadcast accepted.
                info!(
                    txid = %tx.txid,
                    state = ?tx.state,
                    "speedup not found in mempool / chain; re-queueing the same tx for dispatch",
                );
                // Arm the reorg-flap fail guard.
                self.ctx
                    .storage
                    .requeue_not_found(tx.txid, current_height + max_confs)?;
                continue;
            }

            if status.is_finalized(max_confs) {
                self.ctx.mark_finalized(tx.txid, current_height)?;
                // If this is an RBF predecessor, remove any replaced-by link.
                self.remove_replaced_rbf(tx, current_height)?;
                // Add the finalized tx's funding inputs back to the funding manager's queue, replacing its funding parents.
                self.ctx.funding_manager.replace_on_finalize(tx.txid)?;
                continue;
            }

            if status.is_confirmed() {
                self.ctx.mark_confirmed(tx.txid)?;
                continue;
            }

            if status.is_orphan() {
                self.ctx.mark_orphan(tx.txid)?;
                continue;
            }
        }

        Ok(())
    }

    /// Step 4 of `tick`: broadcast `ToDispatch` speedups built in a prior tick (or re-queued this tick).
    pub fn dispatch_pending_speedups(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;

        let pending: Vec<CoordinatedTx> = all_speedups
            .into_iter()
            .filter(|tx| tx.state == TransactionState::ToDispatch)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }

        let dispatchable = self.ctx.apply_retry_rate_limit(pending);
        if dispatchable.is_empty() {
            return Ok(());
        }

        let results = self
            .ctx
            .dispatcher
            .dispatch(dispatchable.clone(), &self.ctx.monitor)?;

        for (txid, outcome) in results {
            if let Some(tx) = dispatchable.iter().find(|t| t.txid == txid) {
                self.ctx.handle_dispatch_result(
                    tx,
                    txid,
                    outcome,
                    current_height,
                    tx.fee_info.clone(),
                )?;
            }
        }
        Ok(())
    }

    /// Step 5 of `tick`: if the latest live speedup is stale, build a boost (new CPFP when slots are
    /// available, otherwise RBF) and save it as `ToDispatch`. Short-circuits if any speedup is already
    /// `ToDispatch` or if the live tip is already at cap.
    pub fn boost_if_stale(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;

        let (
            last_txid,
            last_rate,
            next_bump,
            use_rbf,
            parent_entries,
            rbf_initial_inputs,
            rbf_inherited_count,
        ) = {
            // Short-circuit if any speedup is already `ToDispatch`.
            if all_speedups
                .iter()
                .any(|tx| tx.state == TransactionState::ToDispatch)
            {
                return Ok(());
            }

            // Find the latest speedup with state InMempool and not already being replaced by an RBF.
            let last = match all_speedups.iter().rev().find(|tx| {
                tx.state == TransactionState::InMempool && !tx.has_live_replacement(&all_speedups)
            }) {
                Some(t) => t,
                None => return Ok(()),
            };

            // If the latest live speedup was broadcast less than `min_blocks_before_resend_speedup` blocks ago,
            // it's not stale enough to boost.
            let broadcast_height = match last.broadcast_block_height {
                Some(h) => h,
                None => return Ok(()),
            };
            if current_height.saturating_sub(broadcast_height)
                < self.settings.min_blocks_before_resend_speedup
            {
                return Ok(());
            }

            // If the live tip's package is already paying at the max-fee-rate cap, do not boost it further.
            if last.fee_info.package_fee_rate >= self.ctx.fee_manager.settings.max_feerate_sat_vb {
                return Ok(());
            }

            // Get the context from the last boost (RBF or CPFP).
            let last_context = match &last.kind {
                TxKind::Speedup(k) => k.context(),
                _ => {
                    warn!(txid = %last.txid, "expected Speedup kind in boost_if_stale; skipping");
                    return Ok(());
                }
            };

            // Decide whether to boost via RBF or CPFP, depending on how many unconfirmed speedups are currently in the mempool.
            let inmempool_count = all_speedups
                .iter()
                .filter(|tx| is_live_in_mempool(tx, &all_speedups))
                .count() as u32;
            let use_rbf = inmempool_count >= self.settings.max_unconfirmed_speedups;
            let parent_entries: Vec<(SpeedupData, usize)> = if use_rbf {
                last_context
                    .parent_data
                    .iter()
                    .map(|(sd, _, vs)| (sd.clone(), *vs))
                    .collect()
            } else {
                vec![]
            };
            let rbf_inherited_count = if use_rbf {
                last_context.funding_inputs.len()
            } else {
                0
            };
            let rbf_initial_inputs = if use_rbf {
                Some(last_context.funding_inputs.clone())
            } else {
                None
            };

            (
                last.txid,
                last.fee_info.package_fee_rate,
                last_context.bump_fee_used * self.settings.bump_fee_percentage,
                use_rbf,
                parent_entries,
                rbf_initial_inputs,
                rbf_inherited_count,
            )
        };

        // Fetch the current fee rate and available funding for the boost
        let (network_fee_rate, fee_news) = self
            .ctx
            .fee_manager
            .get_network_fee_rate(&self.ctx.monitor)?;
        if let Some(news) = fee_news {
            self.ctx.storage.add_news(news)?;
        }
        // Every boost must out-pay its predecessor: floor the network rate at minimum `predecessor_rate + 1`
        let fee_rate = FeeManager::boost_fee_rate(network_fee_rate, last_rate);
        if fee_rate != network_fee_rate {
            debug!(
                network = network_fee_rate,
                predecessor = last_rate,
                clamped = fee_rate,
                use_rbf = use_rbf,
                "boost_if_stale: network rate below predecessor; CLAMPED boost rate to predecessor + 1",
            );
        }

        let unconfirmed: Vec<CoordinatedTx> = all_speedups
            .iter()
            .filter(|tx| is_live_in_mempool(tx, &all_speedups))
            .cloned()
            .collect();
        let (chain_diff_fee, _chain_vsize) =
            self.ctx.fee_manager.chain_fee_diff(fee_rate, &unconfirmed);

        // Build the boost. RBF reuses the replaced tx's funding inputs.
        let Some(SpeedupBuildResult {
            tx: new_tx,
            fee: fee_paid,
            capped,
            funding_inputs: build_funding_inputs,
        }) = self.build_speedup(
            &parent_entries,
            rbf_initial_inputs,
            next_bump,
            fee_rate,
            chain_diff_fee,
        )?
        else {
            return Ok(());
        };

        // Build succeeded: persist the boost as `ToDispatch` and register with the monitor.
        let context = Self::make_speedup_context(&build_funding_inputs, next_bump, &parent_entries);
        let new_txid = new_tx.compute_txid();
        // Parents this boost credits (empty for a CPFP-of-CPFP boost, inherited protocol parents for RBF).
        let parent_vbytes: usize = parent_entries.iter().map(|(_, vs)| *vs).sum();
        let fee_info =
            self.ctx
                .fee_manager
                .fee_info_for_paid_speedup(&new_tx, fee_paid, parent_vbytes);
        let kind = if use_rbf {
            SpeedupKind::RBF {
                replaces: last_txid,
                new_funding_inputs: build_funding_inputs[rbf_inherited_count..].to_vec(),
                context,
            }
        } else {
            SpeedupKind::CPFP {
                parents: vec![last_txid],
                context,
            }
        };
        let ctx_str = Self::ctx_for_kind(&kind).to_string();
        // News reports the package rate: that is the rate the cap bounds and the operator cares about.
        let effective_rate = fee_info.package_fee_rate;

        self.save_speedup(new_tx, fee_info, kind, current_height)?;

        if capped {
            // Notify the operator that this boost is at the configured cap.
            self.ctx
                .storage
                .add_news(CoordinatorNews::MaxFeeRateReached {
                    txid: new_txid,
                    effective_fee_rate: effective_rate,
                    context: ctx_str,
                })?;
            // For a capped RBF, the broadcast may fail BIP-125 rule 4 against a predecessor priced below
            // the network floor. Mark the predecessor at-cap so `boost_if_stale`'s tip-at-cap check skips
            // it next tick, preventing a busy-loop of doomed RBF attempts.
            if use_rbf {
                let max = self.ctx.fee_manager.settings.max_feerate_sat_vb;
                if let Some(mut predecessor) = self.ctx.storage.get_tx_by_id(last_txid)? {
                    // Mark the predecessor's package rate at the cap so the tip-at-cap check skips it.
                    predecessor.fee_info.package_fee_rate = max;
                    self.ctx.storage.update_tx(&predecessor)?;
                }
            }
        }

        Ok(())
    }

    /// Step 6 of `tick`: build one CPFP covering the next PendingSpeedupParents
    /// batch and save it as `ToDispatch`. Short-circuits when a `ToDispatch`
    /// speedup already exists or when no slot is available.
    pub fn create_cpfp_batch(&self) -> Result<(), BitcoinCoordinatorError> {
        let parents = self.ctx.storage.get_pending_speedup_parents()?;
        if parents.is_empty() {
            return Ok(());
        }

        let all_speedups = self.ctx.storage.get_speedups_ordered()?;
        if all_speedups
            .iter()
            .any(|tx| tx.state == TransactionState::ToDispatch)
        {
            return Ok(());
        }

        let unconfirmed: Vec<CoordinatedTx> = all_speedups
            .iter()
            .filter(|tx| is_live_in_mempool(tx, &all_speedups))
            .cloned()
            .collect();
        let available_slots = self
            .settings
            .max_unconfirmed_speedups
            .saturating_sub(unconfirmed.len() as u32);
        if available_slots == 0 {
            return Ok(());
        }

        // Fetch the current fee rate and calculate the chain fee difference for the new CPFP.
        let (fee_rate, fee_news) = self
            .ctx
            .fee_manager
            .get_network_fee_rate(&self.ctx.monitor)?;
        if let Some(news) = fee_news {
            self.ctx.storage.add_news(news)?;
        }
        let (chain_diff_fee, _chain_vsize) =
            self.ctx.fee_manager.chain_fee_diff(fee_rate, &unconfirmed);
        let bump_fee = self.ctx.fee_manager.base_fee_multiplier();

        // Take only the first batch: one CPFP per tick.
        let mut batches = self.ctx.dispatcher.batch_by_weight(&parents, 1);
        let batch = match batches.pop() {
            Some(b) => b,
            None => return Ok(()),
        };

        // Fetch the SpeedupData and vsize for each parent in the batch.
        let parent_entries: Vec<(SpeedupData, usize)> = batch
            .iter()
            .filter_map(|p| {
                if let TxKind::NeedsSpeedup(ref sd) = p.kind {
                    Some((sd.clone(), p.tx.vsize()))
                } else {
                    None
                }
            })
            .collect();
        if parent_entries.is_empty() {
            return Ok(());
        }
        // Build the CPFP. Short-circuit if the fee would exceed available funding.

        let Some(SpeedupBuildResult {
            tx: cpfp_tx,
            fee: fee_paid,
            capped,
            funding_inputs: build_funding_inputs,
        }) = self.build_speedup(
            &parent_entries,
            None, // CPFP: build_speedup calls get_funding internally.
            bump_fee,
            fee_rate,
            chain_diff_fee,
        )?
        else {
            return Ok(());
        };

        // Build succeeded: remove parents from the pending set and persist the CPFP.
        let parent_txids: Vec<Txid> = batch.iter().map(|p| p.txid).collect();
        for parent_txid in &parent_txids {
            self.ctx
                .storage
                .remove_pending_speedup_parent(*parent_txid)?;
        }
        let context = Self::make_speedup_context(&build_funding_inputs, bump_fee, &parent_entries);
        // The protocol parents this CPFP speeds up; their vsize sets the package rate denominator.
        let parent_vbytes: usize = parent_entries.iter().map(|(_, vs)| *vs).sum();
        let fee_info =
            self.ctx
                .fee_manager
                .fee_info_for_paid_speedup(&cpfp_tx, fee_paid, parent_vbytes);
        let kind = SpeedupKind::CPFP {
            parents: parent_txids,
            context,
        };
        if capped {
            // If the CPFP package is at the maximum fee cap, notify via news with the package rate.
            self.ctx
                .storage
                .add_news(CoordinatorNews::MaxFeeRateReached {
                    txid: cpfp_tx.compute_txid(),
                    effective_fee_rate: fee_info.package_fee_rate,
                    context: Self::ctx_for_kind(&kind).to_string(),
                })?;
        }
        let current_height = self.ctx.monitor.get_monitor_height()?;
        self.save_speedup(cpfp_tx, fee_info, kind, current_height)?;

        Ok(())
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Unified CPFP / RBF builder.
    /// - `rbf_initial_inputs == None` → CPFP path. Calls `get_funding` to acquire the primary.
    /// - `rbf_initial_inputs == Some` → RBF path. Reuses the predecessor's funding inputs. On failure,
    ///    only the funding added by this call (if any) is released; the inherited inputs stay marked.
    fn build_speedup(
        &self,
        parent_entries: &[(SpeedupData, usize)],
        rbf_initial_inputs: Option<Vec<Utxo>>,
        bump_fee: f64,
        fee_rate: u64,
        chain_diff_fee: u64,
    ) -> Result<Option<SpeedupBuildResult>, BitcoinCoordinatorError> {
        let speedups_data: Vec<SpeedupData> =
            parent_entries.iter().map(|(d, _)| d.clone()).collect();
        let parent_vsizes: Vec<usize> = parent_entries.iter().map(|(_, vs)| *vs).collect();

        let is_rbf = rbf_initial_inputs.is_some();
        let (mut funding_inputs, inherited_count, primary_is_speedup) =
            if let Some(inputs) = rbf_initial_inputs {
                let inherited = inputs.len();
                // Determine if the predecessor's primary is speedup-derived (only then may combine sweep a second input).
                let primary_speedup = match inputs.first() {
                    Some(fi) => self
                        .ctx
                        .storage
                        .is_speedup_derived(&OutPoint::new(fi.txid, fi.vout))?,
                    None => false,
                };
                (inputs, inherited, primary_speedup)
            } else {
                let all_speedups = self.ctx.storage.get_speedups_ordered()?;
                let (first_utxo, is_speedup) =
                    match self.ctx.funding_manager.get_funding(&all_speedups)? {
                        Some(t) => t,
                        None => {
                            self.emit_funding_not_available()?;
                            return Ok(None);
                        }
                    };
                (vec![first_utxo], 0, is_speedup)
            };

        let mut child_vsize = 0usize;

        loop {
            let total_available: u64 = funding_inputs.iter().map(|u| u.amount).sum();
            let dummy_vsize = ProtocolBuilder {}
                .speedup_transactions(
                    &speedups_data,
                    funding_inputs.clone(),
                    &funding_inputs[0].pub_key,
                    1,
                    &self.key_manager,
                )?
                .vsize();
            if child_vsize == 0 {
                child_vsize = dummy_vsize;
            }

            let (fee, capped) = self.ctx.fee_manager.compute_speedup_fee(
                &parent_vsizes,
                child_vsize,
                bump_fee,
                fee_rate,
                is_rbf,
                chain_diff_fee,
            );

            if total_available.saturating_sub(fee) < MAX_DUST_LIMIT {
                // Combine: only possible with a single, Speedup-derived as primary.
                if funding_inputs.len() == 1 && primary_is_speedup {
                    if let Some(extra) = self.ctx.funding_manager.get_combine_funding()? {
                        funding_inputs.push(extra);
                        child_vsize = 0;
                        continue;
                    }
                }
                // No combine (or combine returned None). Release only what this call marked as spent.
                self.ctx
                    .funding_manager
                    .release_marks(&funding_inputs[inherited_count..])?; // Do not release inherited RBF inputs.
                self.emit_insufficient_funds(total_available, fee + MAX_DUST_LIMIT)?;
                return Ok(None);
            }

            let final_tx = ProtocolBuilder {}.speedup_transactions(
                &speedups_data,
                funding_inputs.clone(),
                &funding_inputs[0].pub_key,
                fee,
                &self.key_manager,
            )?;

            let final_vsize = final_tx.vsize();
            if child_vsize >= final_vsize {
                return Ok(Some(SpeedupBuildResult {
                    tx: final_tx,
                    fee,
                    capped,
                    funding_inputs,
                }));
            }
            child_vsize = final_vsize;
        }
    }

    fn emit_insufficient_funds(
        &self,
        available: u64,
        required: u64,
    ) -> Result<(), BitcoinCoordinatorError> {
        self.ctx
            .storage
            .add_news(CoordinatorNews::InsufficientFunds {
                available,
                required,
            })?;
        Ok(())
    }

    fn emit_funding_not_available(&self) -> Result<(), BitcoinCoordinatorError> {
        self.ctx
            .storage
            .add_news(CoordinatorNews::FundingNotAvailable)?;
        Ok(())
    }

    fn make_speedup_context(
        funding_inputs: &[Utxo],
        bump_fee: f64,
        parent_entries: &[(SpeedupData, usize)],
    ) -> SpeedupContext {
        SpeedupContext {
            funding_inputs: funding_inputs.to_vec(),
            replaced_by: None,
            bump_fee_used: bump_fee,
            parent_data: parent_entries
                .iter()
                .map(|(sd, vs)| (sd.clone(), amount_from_speedup_data(sd), *vs))
                .collect(),
            spent: false,
        }
    }

    fn ctx_for_kind(kind: &SpeedupKind) -> &'static str {
        match kind {
            SpeedupKind::RBF { .. } => RBF_TRANSACTION_CONTEXT,
            SpeedupKind::CPFP { .. } => CPFP_TRANSACTION_CONTEXT,
        }
    }

    /// Persist a freshly-built CPFP/RBF as `ToDispatch` and register it with the monitor.
    fn save_speedup(
        &self,
        tx: Transaction,
        fee_info: FeeInfo,
        kind: SpeedupKind,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let ctx_str = Self::ctx_for_kind(&kind);
        let txid = tx.compute_txid();
        let record = CoordinatedTx {
            txid,
            tx,
            kind: TxKind::Speedup(kind),
            state: TransactionState::ToDispatch,
            broadcast_block_height: None,
            target_block_height: current_height,
            stuck_in_mempool_blocks: None,
            confirmation_trigger: None,
            settled_block_height: None,
            fail_guard_until: None,
            retry_count: 0,
            fee_info,
            context: ctx_str.to_string(),
        };

        self.ctx.storage.insert_speedup(record.clone())?;
        match record.speedup_kind()? {
            SpeedupKind::RBF { replaces, .. } => {
                info!("Built RBF | Txid({}) | Replaces({})", record.txid, replaces)
            }
            SpeedupKind::CPFP { parents, .. } => info!(
                "Built CPFP | Txid({}) | Parents({:?})",
                record.txid, parents
            ),
        }
        self.ctx.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], ctx_str.to_string(), None),
            false,
        )?;
        Ok(())
    }

    /// Remove any replaced by RBF transactions from monitoring and mark them as failed.
    fn remove_replaced_rbf(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
            let mut current_id = *replaces;
            loop {
                let next_id =
                    self.ctx
                        .storage
                        .get_tx_by_id(current_id)?
                        .and_then(|prev| match &prev.kind {
                            TxKind::Speedup(SpeedupKind::RBF {
                                replaces: older, ..
                            }) => Some(*older),
                            _ => None,
                        });
                self.ctx.monitor.cancel(TypesToMonitor::Transactions(
                    vec![current_id],
                    tx.context.clone(),
                    None,
                ))?;
                self.ctx
                    .storage
                    .settle_tx(current_id, TransactionState::Failed, current_height)?;
                match next_id {
                    Some(older) => current_id = older,
                    None => break,
                }
            }
        }
        Ok(())
    }
}

/// Returns true if the tx is a speedup that is currently live in the mempool. A speedup is considered
/// not live if it has been or is being replaced by an RBF, see `CoordinatedTx::has_live_replacement`.
fn is_live_in_mempool(tx: &CoordinatedTx, all_speedups: &[CoordinatedTx]) -> bool {
    tx.state == TransactionState::InMempool && !tx.has_live_replacement(all_speedups)
}

fn amount_from_speedup_data(data: &SpeedupData) -> u64 {
    data.utxo
        .as_ref()
        .map(|u| u.amount)
        .or_else(|| data.partial_utxo.map(|(_, _, a)| a))
        .unwrap_or(0)
}
