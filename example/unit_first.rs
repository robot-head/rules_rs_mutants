pub const fn guarded() -> usize {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn checks_guarded_value() {
        assert_eq!(super::guarded(), 1);
    }
}
