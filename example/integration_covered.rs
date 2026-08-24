//! A crate whose only coverage lives in an integration suite.
//!
//! `scale` has no `#[cfg(test)]` test at all: with unit tests alone every
//! mutant of it survives, which is exactly the gap `integration_tests` closes.

pub fn scale(x: i32) -> i32 {
    x * 7
}
