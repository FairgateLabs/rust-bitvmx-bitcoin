use crate::config::config::SpeedupSettings;

pub struct SpeedupEngine {
    settings: SpeedupSettings,
}

impl SpeedupEngine {
    pub fn new(settings: SpeedupSettings) -> Self {
        Self { settings }
    }
}
