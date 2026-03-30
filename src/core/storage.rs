use std::rc::Rc;
use storage_backend::storage::Storage;

pub struct CoordinatorStorage {
    pub storage: Rc<Storage>,
}

impl CoordinatorStorage {
    pub fn new(storage: Rc<Storage>) -> Self {
        Self { storage }
    }
}
