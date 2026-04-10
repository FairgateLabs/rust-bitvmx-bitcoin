mod common;
use common::*;

#[test]
fn test_coordinator_construction() {
    init_trace();
    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    setup.end_all().unwrap();
}
