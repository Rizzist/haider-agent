#![allow(clippy::expect_used)]

use haider_protocol::credential::{AccountIdentity, AccountPrincipal};

fn identity(id: Option<&str>, verified: bool) -> AccountIdentity {
    AccountIdentity {
        email: Some("same@example.test".into()),
        display_name: None,
        account_id: id.map(str::to_owned),
        plan: None,
        issuer: None,
        captured_at: 1,
        verified,
    }
}

#[test]
fn a25_anthropic_placeholder_is_never_an_identity_or_email_bridge() {
    for verified in [false, true] {
        let placeholder = identity(Some("anthropic:shared"), verified);
        let equal = identity(Some("anthropic:shared"), verified);
        let other = identity(Some("anthropic:other"), verified);
        let legacy = identity(None, verified);
        let stable = identity(Some("account=a;organization=b"), verified);
        let principal = AccountPrincipal::new("anthropic-oauth", Some(&placeholder))
            .expect("fixture principal");
        assert!(principal.identity_unverified());
        for neighbor in [&equal, &other, &legacy, &stable] {
            let neighbor = AccountPrincipal::new("anthropic-oauth", Some(neighbor))
                .expect("fixture principal");
            assert!(!principal.same_account(neighbor));
            assert!(!neighbor.same_account(principal));
        }
    }
}

#[test]
fn a25_placeholder_policy_preserves_verified_false_and_email_migration() {
    for provider in ["anthropic-oauth", "other-oauth", "openai-oauth"] {
        let id = if provider == "openai-oauth" {
            "user:1:a;workspace:1:b"
        } else {
            "account=a;organization=b"
        };
        let a = identity(Some(id), false);
        let mut equal = identity(Some(id), true);
        equal.email = Some("different@example.test".into());
        let left = AccountPrincipal::new(provider, Some(&a)).expect("fixture principal");
        assert!(left.same_account(
            AccountPrincipal::new(provider, Some(&equal)).expect("fixture principal")
        ));
        assert!(!left.same_account(
            AccountPrincipal::new("mismatched-provider", Some(&a)).expect("fixture principal")
        ));
        let legacy = identity(None, false);
        assert_eq!(
            left.same_account(
                AccountPrincipal::new(provider, Some(&legacy)).expect("fixture principal")
            ),
            provider != "openai-oauth"
        );
        let different = identity(Some("different-stable-id"), false);
        assert!(!left.same_account(
            AccountPrincipal::new(provider, Some(&different)).expect("fixture principal")
        ));
    }
    let unrelated = identity(Some("anthropic:shared"), false);
    let unrelated_principal =
        AccountPrincipal::new("other-oauth", Some(&unrelated)).expect("fixture principal");
    assert!(unrelated_principal.same_account(unrelated_principal));
    let workspace = identity(Some("workspace-only"), false);
    let principal =
        AccountPrincipal::new("openai-oauth", Some(&workspace)).expect("fixture principal");
    assert!(!principal.same_account(principal));
}
