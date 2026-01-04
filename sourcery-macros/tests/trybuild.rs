#[test]
fn ui() {
    let t = trybuild::TestCases::new();

    // Pass tests - valid usage patterns
    t.pass("tests/ui/pass/*.rs");

    // Compile-fail tests - invalid usage that should produce helpful errors
    t.compile_fail("tests/ui/fail/*.rs");
}
