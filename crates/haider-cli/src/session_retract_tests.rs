use super::*;

#[test]
fn retract_requires_an_explicit_unique_session_argument() {
    let args = |items: &[&str]| {
        items
            .iter()
            .map(|item| (*item).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        parse(&args(&["--session", "session", "--json"])),
        Ok(SessionId::new("session"))
    );
    for invalid in [
        vec![],
        args(&["--session"]),
        args(&["--session", ""]),
        args(&["--session", "a", "--session", "b"]),
        args(&["a"]),
    ] {
        assert!(parse(&invalid).is_err());
    }
}
