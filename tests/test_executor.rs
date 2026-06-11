//! Integration smoke test for the public `test-util` re-export.
//! Run with `cargo test --features test-util`.

#[cfg(feature = "test-util")]
#[test]
fn test_framework_is_publicly_reexported() {
    use sourcery::{Aggregate, test::TestFramework};

    struct Empty;

    impl Aggregate for Empty {
        type Error = String;
        type Event = ();
        type Id = String;

        const KIND: &'static str = "empty";

        fn create(_event: &Self::Event) -> Self {
            Self
        }

        fn apply(&mut self, _event: &Self::Event) {}
    }

    let _ = TestFramework::<Empty>::new();
}

#[cfg(not(feature = "test-util"))]
#[test]
fn test_util_feature_is_required() {
    panic!(
        "Integration tests require the `test-util` feature. Run `cargo test --features test-util` \
         to execute them."
    );
}
