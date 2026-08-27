#![allow(clippy::expect_used)]

use super::*;

#[test]
fn tool_catalog_and_provider_schemas_have_process_wide_identity() {
    let first_catalog = registered_tool_catalog();
    let second_catalog = registered_tool_catalog();
    assert!(std::ptr::eq(first_catalog, second_catalog));
    let first_tools = registered_tools();
    let second_tools = registered_tools();
    assert_eq!(first_tools.as_ptr(), second_tools.as_ptr());

    let first_definitions = TurnToolFactory::shared_definitions(&BrokerToolFactory);
    let second_definitions = TurnToolFactory::shared_definitions(&BrokerToolFactory);
    assert!(Arc::ptr_eq(&first_definitions, &second_definitions));
    assert_eq!(first_definitions.as_ptr(), second_definitions.as_ptr());
}

#[test]
fn request_input_provider_schema_matches_pinned_wire_bytes() {
    const GOLDEN: &[u8] = br#"{"name":"request_input","description":"","input_schema":{"properties":{"body":{"items":{"type":"string"},"type":"array"},"default":{"type":"string"},"kind":{"enum":["question","choice"],"type":"string"},"options":{"items":{"properties":{"detail":{"type":"string"},"key":{"type":"string"},"label":{"type":"string"}},"required":["key","label"],"type":"object"},"type":"array"},"title":{"type":"string"}},"required":["kind","title"],"type":"object"}}"#;
    let definition = registered_provider_definitions()
        .first()
        .cloned()
        .expect("request_input provider definition");
    assert_eq!(definition.name, "request_input");
    assert_eq!(serde_json::to_vec(&definition).expect("wire bytes"), GOLDEN);
}

#[test]
fn shared_filtered_tool_view_preserves_legacy_wire_bytes() {
    let factory: Arc<dyn TurnToolFactory> = Arc::new(BrokerToolFactory);
    let grant = default_child_grant();
    let mut expected = build_registered_tools()
        .iter()
        .map(|entry| provider_definition(&entry.manifest))
        .collect::<Vec<_>>();
    expected.retain(|definition| definition.name != "mobile");
    expected.retain(|definition| {
        grant.tools.contains(&definition.name)
            && registered_tool_by_name(&definition.name).is_some_and(|entry| {
                grant_admits_tool_manifest(&grant, &entry.manifest.name, &entry.manifest.effects)
            })
    });
    let (local_web_tool_names, _) =
        provider_web_tool_names("fake", WebCapabilityDegrade::default());
    expected.retain(|definition| {
        !is_local_web_tool(&definition.name) || local_web_tool_names.contains(&definition.name)
    });

    let actual = advertised_tool_definitions(
        &factory,
        Some(&grant),
        "fake",
        WebCapabilityDegrade::default(),
    );
    let first_pack = advertised_tool_pack_for_mobile_state(
        &factory,
        Some(&grant),
        "fake",
        WebCapabilityDegrade::default(),
        false,
    );
    let second_pack = advertised_tool_pack_for_mobile_state(
        &factory,
        Some(&grant),
        "fake",
        WebCapabilityDegrade::default(),
        false,
    );
    assert!(Arc::ptr_eq(&first_pack, &second_pack));
    assert!(Arc::ptr_eq(
        &first_pack.definitions,
        &second_pack.definitions
    ));
    assert_eq!(
        first_pack.digest,
        canonical_tool_definitions_digest(&expected)
    );
    assert_eq!(
        serde_json::to_vec(&actual).expect("actual wire bytes"),
        serde_json::to_vec(&expected).expect("legacy wire bytes")
    );
}

#[test]
fn approval_retry_cache_reuses_typed_operation_and_fences_full_call_identity() {
    let parses = std::cell::Cell::new(0_u32);
    let mut operations = HashMap::new();
    let key = ParsedToolOperationKey {
        run_id: RunId::new("run-shared"),
        item_id: ItemId::new("item-shared"),
        call_id: "call-shared".into(),
        route: RegisteredToolRoute::FsWrite,
    };
    let first = cache_parsed_operation(&mut operations, key.clone(), || {
        parses.set(parses.get() + 1);
        BrokerToolDispatcher::parse_tool_operation(
            key.route,
            &key.call_id,
            &serde_json::json!({"path": "one.txt", "content": "first"}),
        )
    })
    .expect("first typed parse");
    let second = cache_parsed_operation(&mut operations, key.clone(), || {
        parses.set(parses.get() + 1);
        BrokerToolDispatcher::parse_tool_operation(
            key.route,
            &key.call_id,
            &serde_json::json!({"path": "two.txt", "content": "must-not-reparse"}),
        )
    })
    .expect("approval retry cache");

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(parses.get(), 1);
    let ParsedToolOperation::FsWrite(operation) = first.as_ref() else {
        panic!("cached route must retain its typed operation")
    };
    assert_eq!(operation.path, PathBuf::from("one.txt"));

    let distinct = ParsedToolOperationKey {
        item_id: ItemId::new("item-distinct"),
        ..key.clone()
    };
    let third = cache_parsed_operation(&mut operations, distinct, || {
        parses.set(parses.get() + 1);
        BrokerToolDispatcher::parse_tool_operation(
            key.route,
            &key.call_id,
            &serde_json::json!({"path": "three.txt", "content": "new-call-identity"}),
        )
    })
    .expect("distinct item identity parses independently");
    assert!(!Arc::ptr_eq(&first, &third));
    assert_eq!(parses.get(), 2);

    operations.remove(&key);
    assert!(!operations.contains_key(&key));
}

#[test]
fn parsed_operation_lease_retains_only_an_approval_retry() {
    let key = ParsedToolOperationKey {
        run_id: RunId::new("run-lease"),
        item_id: ItemId::new("item-lease"),
        call_id: "call-lease".into(),
        route: RegisteredToolRoute::FsWrite,
    };
    let operation = Arc::new(ParsedToolOperation::FsWrite(FsWrite::new(
        "lease.txt",
        "content",
    )));
    let operations = StdMutex::new(HashMap::from([(key.clone(), Arc::clone(&operation))]));

    {
        let _terminal = ParsedToolOperationLease::new(&operations, key.clone());
    }
    assert!(
        !operations
            .lock()
            .expect("operation cache")
            .contains_key(&key)
    );

    operations
        .lock()
        .expect("operation cache")
        .insert(key.clone(), operation);
    {
        let mut approval = ParsedToolOperationLease::new(&operations, key.clone());
        approval.retain_for_approval();
    }
    assert!(
        operations
            .lock()
            .expect("operation cache")
            .contains_key(&key)
    );
}

#[test]
fn filtered_tool_pack_cache_is_process_bounded() {
    let mut cache = FilteredToolPackCache {
        packs: HashMap::new(),
        insertion_order: VecDeque::new(),
    };
    for index in 0..=FILTERED_TOOL_PACK_CACHE_CAPACITY {
        cache.insert(
            vec![index],
            Arc::new(ToolDefinitionPack {
                definitions: Arc::<[ToolDefinition]>::from(Vec::new()),
                digest: index.to_string(),
            }),
        );
    }
    assert_eq!(cache.packs.len(), FILTERED_TOOL_PACK_CACHE_CAPACITY);
    assert!(cache.get(&[0]).is_none());
    assert!(cache.get(&[FILTERED_TOOL_PACK_CACHE_CAPACITY]).is_some());
}
