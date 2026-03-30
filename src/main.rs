use rust_bitvmx_bitcoin::{config::config::Config, coordinator::BitcoinCoordinator};
use std::rc::Rc;
use storage_backend::storage::Storage;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() -> Result<(), anyhow::Error> {
    init_trace();
    let config = Config::load_config("config/coordinator_config.yaml")?;
    info!("Loaded configuration: {:?}", config);
    let storage = Rc::new(Storage::new(&config.storage)?);
    let _coordinator =
        BitcoinCoordinator::new_with_paths(&config.rpc, storage, Some(config.settings))?;
    info!("Initialized Bitcoin coordinator");
    Ok(())
}

pub fn init_trace() {
    let default_modules = [
        "info",
        "libp2p=off",
        "bitvmx_transaction_monitor=debug",
        "bitcoin_indexer=debug",
        "bitcoin_coordinator=debug",
        "bitcoin_rpc=debug",
        "bitcoin_client=debug",
        "p2p_protocol=off",
        "p2p_handler=off",
        "tarpc=off",
        "key_manager=off",
        "memory=off",
    ];

    let filter = EnvFilter::builder()
        .parse(default_modules.join(","))
        .expect("Invalid filter");

    // Try to set the global default, but ignore if it's already set
    // This allows multiple tests to call this function without panicking
    let _ = tracing_subscriber::fmt()
        .with_target(true)
        .with_env_filter(filter)
        .try_init();
}
