use unit_first::guarded;

const _: [(); 1] = [(); guarded()];

#[test]
fn integration_guard() {
    assert_eq!(guarded(), 1);
}
