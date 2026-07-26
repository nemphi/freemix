use std::{collections::BTreeMap, str::FromStr, time::Duration};

use crate::{
    BoundedCache, CsvRowsAdapter, DataAdapter, DataPath, DataSource, DataValue, Decimal,
    FakePollingSource, FakePushSource, FieldBinding, JsonObjectAdapter, ManualClock, Mapper,
    MappingStatus, PollingSource, PushSource, RetryError, RetryOutcome, RetryPolicy, RetryState,
    Schema, SchemaType, SecretRefId, SourceError, Transform, ValueSelector, ValueType,
};

fn object(fields: impl IntoIterator<Item = (&'static str, DataValue)>) -> DataValue {
    DataValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

#[test]
fn values_and_schema_report_exact_type_errors() {
    let schema = Schema::new(SchemaType::Object(BTreeMap::from([
        ("active".to_owned(), SchemaType::Bool),
        (
            "scores".to_owned(),
            SchemaType::List(Box::new(SchemaType::Integer)),
        ),
    ])));
    let data = object([
        ("active", DataValue::Bool(true)),
        (
            "scores",
            DataValue::List(vec![DataValue::Integer(4), DataValue::String("bad".into())]),
        ),
    ]);
    let error = schema.validate(&data).unwrap_err();
    assert_eq!(error.path, DataPath::from_str("scores[1]").unwrap());
    assert_eq!(error.expected, ValueType::Integer);
    assert_eq!(error.actual, ValueType::String);
}

#[test]
fn paths_extract_objects_and_lists() {
    let data = object([(
        "players",
        DataValue::List(vec![object([("name", DataValue::from("Ada"))])]),
    )]);
    assert_eq!(
        DataPath::from_str("players[0].name")
            .unwrap()
            .extract(&data)
            .unwrap(),
        &DataValue::from("Ada")
    );
}

#[test]
fn transforms_are_typed_and_mapping_reports_every_field() {
    let root = object([
        ("first", DataValue::from("Ada")),
        ("last", DataValue::from("Lovelace")),
        ("score", DataValue::Integer(120)),
    ]);
    let mut name = FieldBinding::new("display_name", DataPath::from_str("first").unwrap());
    name.expected = Some(ValueType::String);
    name.transforms.push(Transform::Concatenate {
        parts: vec![
            ValueSelector::Current,
            ValueSelector::Path(DataPath::from_str("last").unwrap()),
        ],
        separator: " ".to_owned(),
    });
    let mut score = FieldBinding::new("score", DataPath::from_str("score").unwrap());
    score.transforms.push(Transform::Clamp {
        min: DataValue::Integer(0),
        max: DataValue::Integer(100),
    });
    score
        .transforms
        .push(Transform::Format("{value}%".to_owned()));
    let mut city = FieldBinding::new("city", DataPath::from_str("city").unwrap());
    city.transforms
        .push(Transform::Fallback(DataValue::from("Unknown")));
    let mut invalid = FieldBinding::new("invalid", DataPath::from_str("score").unwrap());
    invalid.expected = Some(ValueType::Bool);

    let report = Mapper::new(vec![name, score, city, invalid]).map(&root);
    assert_eq!(
        report.output["display_name"],
        DataValue::from("Ada Lovelace")
    );
    assert_eq!(report.output["score"], DataValue::from("100%"));
    assert_eq!(report.output["city"], DataValue::from("Unknown"));
    assert_eq!(
        report.fields[3].status,
        MappingStatus::TypeError {
            expected: ValueType::Bool,
            actual: ValueType::Integer,
        }
    );
    assert!(!report.is_success());
}

#[test]
fn decimal_clamp_is_exact() {
    let value = DataValue::Decimal(Decimal::from_str("12.50").unwrap());
    let result = Transform::Clamp {
        min: DataValue::Decimal(Decimal::from_str("0.1").unwrap()),
        max: DataValue::Decimal(Decimal::from_str("10.25").unwrap()),
    }
    .apply(value, &DataValue::Null)
    .unwrap();
    assert_eq!(
        result,
        DataValue::Decimal(Decimal::from_str("10.25").unwrap())
    );
}

#[test]
fn polling_lifecycle_and_script_order_are_deterministic() {
    let mut source = FakePollingSource::new("poll", Duration::from_secs(2));
    source.enqueue(DataValue::Integer(1));
    source.enqueue_error("temporary");
    source.enqueue(DataValue::Integer(2));
    assert_eq!(source.poll(), Err(SourceError::NotRunning));
    source.start().unwrap();
    assert_eq!(source.poll().unwrap().unwrap().sequence, 0);
    assert_eq!(source.poll(), Err(SourceError::Adapter("temporary".into())));
    let event = source.poll().unwrap().unwrap();
    assert_eq!((event.sequence, event.value), (1, DataValue::Integer(2)));
    assert_eq!(source.poll().unwrap(), None);
    source.stop().unwrap();
}

#[test]
fn push_lifecycle_preserves_emission_order() {
    let mut source = FakePushSource::new("push");
    source.start().unwrap();
    assert_eq!(source.push(DataValue::from("a")).unwrap(), 0);
    assert_eq!(source.push(DataValue::from("b")).unwrap(), 1);
    let first = source.next().unwrap().unwrap();
    let second = source.next().unwrap().unwrap();
    assert_eq!((first.sequence, first.value), (0, DataValue::from("a")));
    assert_eq!((second.sequence, second.value), (1, DataValue::from("b")));
    source.stop().unwrap();
    assert_eq!(source.next(), Err(SourceError::NotRunning));
}

#[test]
fn cache_expires_and_evicts_lru_using_manual_clock() {
    let clock = ManualClock::new(10);
    let mut cache = BoundedCache::new(2, Duration::from_millis(100)).unwrap();
    cache.insert("a", 1, &clock);
    cache.insert("b", 2, &clock);
    assert_eq!(cache.get(&"a", &clock), Some(&1));
    cache.insert("c", 3, &clock);
    assert_eq!(cache.get(&"b", &clock), None);
    assert_eq!(cache.get(&"a", &clock), Some(&1));
    clock.advance(Duration::from_millis(100));
    assert!(cache.is_empty(&clock));
}

#[test]
fn retry_records_backoff_and_terminal_result() {
    let policy = RetryPolicy {
        max_attempts: 4,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(25),
        multiplier: 2,
    };
    let mut retry = RetryState::new(policy).unwrap();
    retry.record_failure("one").unwrap();
    retry.record_failure("two").unwrap();
    retry.record_failure("three").unwrap();
    retry.record_success().unwrap();
    assert_eq!(
        retry
            .records()
            .iter()
            .map(|record| record.delay_before)
            .collect::<Vec<_>>(),
        vec![
            Duration::ZERO,
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(25),
        ]
    );
    assert_eq!(retry.records()[3].outcome, RetryOutcome::Succeeded);
    assert_eq!(retry.record_failure("late"), Err(RetryError::Completed));
}

#[test]
fn fake_adapter_representations_are_typed_and_validated() {
    let csv = CsvRowsAdapter::new(
        vec!["name".into(), "score".into()],
        vec![vec!["Ada".into(), "10".into()]],
    );
    let value = csv.representation().unwrap();
    assert_eq!(
        DataPath::from_str("[0].name")
            .unwrap()
            .extract(&value)
            .unwrap(),
        &DataValue::from("Ada")
    );
    let DataValue::List(rows) = value else {
        panic!("CSV representation must be a list");
    };
    assert_eq!(
        rows[0],
        object([
            ("name", DataValue::from("Ada")),
            ("score", DataValue::from("10"))
        ])
    );

    let json = JsonObjectAdapter::new(BTreeMap::from([(
        "enabled".to_owned(),
        DataValue::Bool(true),
    )]));
    assert_eq!(
        json.representation().unwrap(),
        object([("enabled", DataValue::Bool(true))])
    );
}

#[test]
fn secret_reference_debug_output_is_redacted() {
    let secret = SecretRefId::new("production-api-key");
    assert_eq!(secret.as_str(), "production-api-key");
    let debug = format!("{secret:?}");
    assert_eq!(debug, "SecretRefId([REDACTED])");
    assert!(!debug.contains("production-api-key"));
}
