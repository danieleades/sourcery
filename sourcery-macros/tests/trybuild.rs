#[test]
fn ui() {
    let t = trybuild::TestCases::new();

    // Pass tests - valid usage
    t.pass("tests/ui/aggregate_generics.rs");
    t.pass("tests/ui/projection_generics.rs");
    t.pass("tests/ui/aggregate_event_name_collision.rs");

    // Compile-fail tests - missing required attributes
    t.compile_fail("tests/ui/fail/aggregate_missing_id.rs");
    t.compile_fail("tests/ui/fail/aggregate_missing_error.rs");
    t.compile_fail("tests/ui/fail/aggregate_missing_events.rs");
    t.compile_fail("tests/ui/fail/aggregate_empty_events.rs");
    t.compile_fail("tests/ui/fail/projection_missing_id.rs");

    // Compile-fail tests - malformed attribute values
    t.compile_fail("tests/ui/fail/aggregate_id_string_literal.rs");
    t.compile_fail("tests/ui/fail/aggregate_error_string_literal.rs");
    t.compile_fail("tests/ui/fail/aggregate_kind_non_string.rs");
    t.compile_fail("tests/ui/fail/aggregate_event_enum_non_string.rs");
    t.compile_fail("tests/ui/fail/projection_id_string_literal.rs");
    t.compile_fail("tests/ui/fail/projection_metadata_string_literal.rs");
    t.compile_fail("tests/ui/fail/projection_kind_non_string.rs");

    // Compile-fail tests - invalid usage patterns
    t.compile_fail("tests/ui/fail/aggregate_on_enum.rs");
    t.compile_fail("tests/ui/fail/projection_on_enum.rs");
    t.compile_fail("tests/ui/fail/aggregate_duplicate_attribute.rs");
    t.compile_fail("tests/ui/fail/projection_duplicate_attribute.rs");
}
