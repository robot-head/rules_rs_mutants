//! A second suite over the same library.
//!
//! Its existence is the regression test for the launcher's ten-argument limit:
//! with a flag pair per suite, two suites overflowed the stub and the sweep
//! failed to build with a message that named neither.

use integration_covered::scale;

#[test]
fn scales_larger_inputs() {
    assert_eq!(scale(10), 70);
    assert_eq!(scale(100), 700);
}
