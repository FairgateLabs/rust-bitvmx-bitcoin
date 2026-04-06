use rust_bitvmx_bitcoin::{
    config::config::Config, coordinator::BitcoinCoordinator, helper::init_trace,
};
use std::rc::Rc;
use storage_backend::storage::Storage;
use tracing::info;

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
