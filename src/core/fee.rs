use crate::config::config::FeeSettings;

pub struct FeeEngine {
    settings: FeeSettings,
}

impl FeeEngine {
    pub fn new(settings: FeeSettings) -> Self {
        Self { settings }
    }
}
