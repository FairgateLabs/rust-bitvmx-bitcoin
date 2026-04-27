use key_manager::key_manager::KeyManager;
use rust_bitvmx_bitcoin::{config::config::Config, coordinator::BitcoinCoordinator};
use std::rc::Rc;
use storage_backend::storage::Storage;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn init_trace() {
    let filter = EnvFilter::builder()
        .parse("info,libp2p=off,p2p_protocol=off,p2p_handler=off,tarpc=off")
        .expect("Invalid filter");
    let _ = tracing_subscriber::fmt()
        .with_target(true)
        .with_env_filter(filter)
        .try_init();
}

fn main() -> Result<(), anyhow::Error> {
    init_trace();
    let config = Config::load_config("config/coordinator_config.yaml")?;
    info!("Loaded configuration: {:?}", config);
    let storage = Rc::new(Storage::new(&config.storage)?);
    // KeyManager shares coordinator storage (different key namespaces).
    let key_manager = Rc::new(KeyManager::new(
        config.rpc.network,
        None,
        None,
        &config.storage,
    )?);
    let _coordinator =
        BitcoinCoordinator::new_with_paths(&config.rpc, storage, key_manager, Some(config.settings))?;
    info!("Initialized Bitcoin coordinator");
    Ok(())
}
