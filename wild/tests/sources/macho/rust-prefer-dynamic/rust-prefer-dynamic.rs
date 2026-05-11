//#CompArgs:--test -C prefer-dynamic
//#RunEnabled:false

#[test]
fn rust_test_harness_links() {
    assert_eq!(std::env::args().count(), 1);
}
