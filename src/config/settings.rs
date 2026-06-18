// Default Bitcoin Coordinator constants

// Context string for CPFP transactions.
pub const CPFP_TRANSACTION_CONTEXT: &str = "CPFP_TRANSACTION";
pub const RBF_TRANSACTION_CONTEXT: &str = "RBF_TRANSACTION";
pub const FUNDING_TRANSACTION_CONTEXT: &str = "FUNDING_TRANSACTION";

// Bitcoin Core has a mempool policy called the "chain limit":
// You can’t have more than 25 unconfirmed transactions chained together (i.e. one spending the other).
pub const DEFAULT_MAX_UNCONFIRMED_SPEEDUPS: u32 = 10;
pub const MAX_LIMIT_UNCONFIRMED_PARENTS: u32 = 25;

// Minimum number of unconfirmed transactions required to dispatch a CPFP (Child Pays For Parent) transaction.
// This is due to Bitcoin's mempool chain limit policy, which restricts the number of unconfirmed transactions that can be chained together (default is 25).
// To create a valid CPFP, there must be at least one unconfirmed parent transaction and at least one unconfirmed output available to spend for the CPFP.
// This ensures that the CPFP transaction can be constructed and accepted by the mempool under Bitcoin's standardness rules.
pub const MIN_UNCONFIRMED_TXS_FOR_CPFP: u32 = 2;

// Maximum transaction weight in bytes.
pub const DEFAULT_MAX_TX_WEIGHT: u64 = 400_000;
pub const MAX_MAX_TX_WEIGHT: u64 = 400_000;

// Maximum number of RBF attempts for a single transaction
pub const DEFAULT_MAX_RBF_ATTEMPTS: u32 = 10;
pub const MAX_MAX_RBF_ATTEMPTS: u32 = 20;

// Minimum funding amount in sats to ensure sufficient funds for speedups
pub const DEFAULT_MIN_FUNDING_AMOUNT_SATS: u64 = 10_000;
pub const MIN_MIN_FUNDING_AMOUNT_SATS: u64 = 10_000;

// Minimum blocks to wait before attempting to resend a speedup transaction (CPFP or RBF)
pub const DEFAULT_MIN_BLOCKS_BEFORE_RESEND_SPEEDUP: u32 = 1;
pub const MAX_MIN_BLOCKS_BEFORE_RESEND_SPEEDUP: u32 = 3;

// Maximum feerate sat/vbyte allowed for speedups
pub const DEFAULT_MAX_FEERATE_SAT_VB: u64 = 1000;
pub const MAX_MAX_FEERATE_SAT_VB: u64 = 1000;

// Fee multiplier for base fee multiplier
pub const DEFAULT_BASE_FEE_MULTIPLIER: f64 = 1.0;
pub const MAX_BASE_FEE_MULTIPLIER: f64 = 100.0;

// Bump fee percentage
pub const DEFAULT_BUMP_FEE_PERCENTAGE: f64 = 1.5;
pub const MAX_BUMP_FEE_PERCENTAGE: f64 = 100.0;

// Retry interval seconds
pub const DEFAULT_RETRY_INTERVAL_SECONDS: u64 = 5;
pub const MAX_RETRY_INTERVAL_SECONDS: u64 = 300; // 5 minutes

// Retry attempts sending tx after an error
pub const DEFAULT_RETRY_ATTEMPTS_SENDING_TX: u32 = 3;

// User-set safety floor applied to every speedup's effective fee rate.
// Also used as the fallback when bitcoind's fee estimate is unavailable.
pub const DEFAULT_MIN_SAFE_FEE_RATE: u64 = 1;

// Fee percentage increase for RBF (% of original fee)
pub const DEFAULT_RBF_FEE_MULTIPLIER: f64 = 1.5;
pub const MAX_RBF_FEE_MULTIPLIER: f64 = 3.0;

// Maximum block confirmations to track after reaching finalized or failed state
pub const DEFAULT_MAX_TRACKING_CONFIRMATIONS: u32 = 10;
