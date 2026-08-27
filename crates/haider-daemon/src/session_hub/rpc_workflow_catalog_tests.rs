#![allow(clippy::expect_used)]

use super::*;

#[test]
fn publication_preserves_authority_records_and_eligibility_classes() {
    let user = haider_protocol::loom::compile_pipe(
        &haider_protocol::loom::parse_pipe("review: Patch -> Patch\ncheck \"review\""),
        |_| None,
    )
    .expect("control-only user workflow compiles");
    let authoritative_builtins = haider_protocol::graph::built_in_workflow_catalog();
    let catalog = published_workflow_catalog(std::slice::from_ref(&user));

    assert_eq!(catalog.len(), authoritative_builtins.len() + 1);
    for (entry, authoritative) in catalog.iter().zip(&authoritative_builtins) {
        assert_eq!(
            entry,
            &WorkflowCatalogEntryV1::BuiltIn {
                id: authoritative.template.name.clone(),
                main_session_eligible: authoritative.main_session_eligible,
                template: authoritative.template.clone(),
            }
        );
    }
    assert_eq!(
        catalog.last(),
        Some(&WorkflowCatalogEntryV1::User {
            id: user.id.clone(),
            main_session_eligible: true,
            workflow: user,
        })
    );
}
