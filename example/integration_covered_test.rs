//! The suite that covers `scale`, as a separate integration crate.

use integration_covered::scale;

#[test]
fn scales_by_seven() {
    assert_eq!(scale(3), 21);
    assert_eq!(scale(0), 0);
    assert_eq!(scale(-2), -14);
}
