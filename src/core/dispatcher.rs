use crate::config::config::DispatcherSettings;

pub struct Dispatcher {
    settings: DispatcherSettings,
}

impl Dispatcher {
    pub fn new(settings: DispatcherSettings) -> Self {
        Self { settings }
    }
}
