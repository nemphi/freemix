#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

const EXPECTED_FEATURES: usize = 103;
const EXPECTED_PRIORITIES: [(&str, usize); 3] = [("P0", 24), ("P1", 70), ("P2", 9)];
const NAMED_INTEGRATIONS: &[&str] = &[
    "IN-002", "IN-017", "IN-018", "IN-019", "IN-029", "IN-030", "GX-004", "GX-010", "AU-006",
    "AU-011", "OR-008", "OR-009", "OR-010", "OR-016", "RC-009",
];
const EXACT_INTEGRATION: &str = "exact-authorized-integration-required";
const GENERIC_EQUIVALENT: &str = "equivalent-behavior-allowed";
const TEST_PLANNED: &str = "planned";
const TEST_PRESENT: &str = "present";
const TEST_VERIFIED: &str = "verified";

#[derive(Clone, Debug)]
struct Registry {
    profiles: Vec<String>,
    workstreams: Vec<String>,
}

#[derive(Clone, Debug)]
struct TestRecord {
    id: String,
    state: String,
    paths: Vec<String>,
    commands: Vec<String>,
}

#[derive(Clone, Debug)]
struct Waiver {
    id: String,
    feature: String,
    category: String,
    reason: String,
    issue: String,
    approver: String,
    expiry: String,
}

#[derive(Clone, Debug)]
struct Feature {
    id: String,
    priority: String,
    phase: u8,
    owner: String,
    dependencies: Vec<String>,
    capability_profiles: Vec<String>,
    tests: Vec<String>,
    status: String,
    substitution_policy: String,
}

#[derive(Clone, Debug)]
struct Ledger {
    registry: Registry,
    tests: Vec<TestRecord>,
    waivers: Vec<Waiver>,
    features: Vec<Feature>,
}

#[derive(Debug)]
enum Value {
    String(String),
    Integer(u8),
    Array(Vec<String>),
}

#[derive(Default)]
struct RecordBuffer {
    current: Option<BTreeMap<String, Value>>,
    values: Vec<BTreeMap<String, Value>>,
}

impl RecordBuffer {
    fn finish(&mut self) {
        if let Some(values) = self.current.take() {
            self.values.push(values);
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    match run(&args) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<String, String> {
    let root = workspace_root();
    match args {
        [group, command] if group == "parity" && command == "check" => check_parity(&root, None),
        [group, command, flag, phase]
            if group == "parity" && command == "check" && flag == "--phase" =>
        {
            let phase = phase
                .parse::<u8>()
                .map_err(|_| format!("invalid phase {phase}; expected 1 through 9"))?;
            if !(1..=9).contains(&phase) {
                return Err(format!("invalid phase {phase}; expected 1 through 9"));
            }
            check_parity(&root, Some(phase))
        }
        [group, command] if group == "deps" && command == "check" => {
            let dependency_count = check_dependencies(&root)?;
            Ok(format!(
                "deps check passed: {dependency_count} local path dependencies"
            ))
        }
        _ => Err("usage: cargo run -p xtask -- <parity check [--phase N]|deps check>".to_owned()),
    }
}

fn check_parity(root: &Path, phase: Option<u8>) -> Result<String, String> {
    let ledger_source = read(&root.join("parity.toml"))?;
    let markdown = read(&root.join("docs/spec/feature-parity.md"))?;
    let ledger = parse_ledger(&ledger_source)?;
    validate_ledger(&ledger, &markdown, root, phase)?;
    let phase_claim = phase.map_or_else(String::new, |phase| format!(", phase {phase} verified"));
    Ok(format!(
        "parity check passed: {} features (P0=24, P1=70, P2=9){phase_claim}",
        ledger.features.len()
    ))
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask must live at tools/xtask")
        .to_path_buf()
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("failed to read {}: {error}", path.display()))
}

fn parse_ledger(source: &str) -> Result<Ledger, String> {
    #[derive(Clone, Copy)]
    enum Section {
        Root,
        Registry,
        Test,
        Waiver,
        Feature,
    }

    let mut section = Section::Root;
    let mut schema_version = None;
    let mut registry_values = BTreeMap::new();
    let mut tests = RecordBuffer::default();
    let mut waivers = RecordBuffer::default();
    let mut features = RecordBuffer::default();

    for (index, source_line) in source.lines().enumerate() {
        let line_number = index + 1;
        let line = source_line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        match line {
            "[registry]" => {
                finish_open_records([&mut tests, &mut waivers, &mut features]);
                section = Section::Registry;
            }
            "[[tests]]" => {
                finish_open_records([&mut tests, &mut waivers, &mut features]);
                tests.current = Some(BTreeMap::new());
                section = Section::Test;
            }
            "[[waivers]]" => {
                finish_open_records([&mut tests, &mut waivers, &mut features]);
                waivers.current = Some(BTreeMap::new());
                section = Section::Waiver;
            }
            "[[features]]" => {
                finish_open_records([&mut tests, &mut waivers, &mut features]);
                features.current = Some(BTreeMap::new());
                section = Section::Feature;
            }
            _ if line.starts_with('[') => {
                return Err(format!("line {line_number}: unknown section {line}"));
            }
            _ => {
                let (key, raw_value) = line
                    .split_once('=')
                    .ok_or_else(|| format!("line {line_number}: expected key = value"))?;
                let key = key.trim().to_owned();
                let value = parse_value(raw_value.trim(), line_number)?;
                match section {
                    Section::Root => {
                        if key != "schema_version" {
                            return Err(format!("line {line_number}: unknown root key {key}"));
                        }
                        if schema_version
                            .replace(take_integer(&value, &key)?)
                            .is_some()
                        {
                            return Err("duplicate schema_version".to_owned());
                        }
                    }
                    Section::Registry => {
                        insert_unique(&mut registry_values, &key, value, line_number)?;
                    }
                    Section::Test => {
                        let values = tests.current.as_mut().ok_or_else(|| {
                            format!("line {line_number}: test field outside [[tests]]")
                        })?;
                        insert_unique(values, &key, value, line_number)?;
                    }
                    Section::Waiver => {
                        let values = waivers.current.as_mut().ok_or_else(|| {
                            format!("line {line_number}: waiver field outside [[waivers]]")
                        })?;
                        insert_unique(values, &key, value, line_number)?;
                    }
                    Section::Feature => {
                        let values = features.current.as_mut().ok_or_else(|| {
                            format!("line {line_number}: feature field outside [[features]]")
                        })?;
                        insert_unique(values, &key, value, line_number)?;
                    }
                }
            }
        }
    }
    finish_open_records([&mut tests, &mut waivers, &mut features]);

    build_ledger(
        schema_version,
        registry_values,
        tests.values,
        waivers.values,
        features.values,
    )
}

fn build_ledger(
    schema_version: Option<u8>,
    registry_values: BTreeMap<String, Value>,
    test_values: Vec<BTreeMap<String, Value>>,
    waiver_values: Vec<BTreeMap<String, Value>>,
    feature_values: Vec<BTreeMap<String, Value>>,
) -> Result<Ledger, String> {
    if schema_version != Some(3) {
        return Err("schema_version must be 3".to_owned());
    }
    let registry = build_registry(registry_values)?;
    let tests = test_values
        .into_iter()
        .enumerate()
        .map(|(index, values)| build_test(values, index + 1))
        .collect::<Result<Vec<_>, _>>()?;
    let waivers = waiver_values
        .into_iter()
        .enumerate()
        .map(|(index, values)| build_waiver(values, index + 1))
        .collect::<Result<Vec<_>, _>>()?;
    let features = feature_values
        .into_iter()
        .enumerate()
        .map(|(index, values)| build_feature(values, index + 1))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Ledger {
        registry,
        tests,
        waivers,
        features,
    })
}

fn finish_open_records(records: [&mut RecordBuffer; 3]) {
    for record in records {
        record.finish();
    }
}

fn insert_unique(
    values: &mut BTreeMap<String, Value>,
    key: &str,
    value: Value,
    line: usize,
) -> Result<(), String> {
    if values.insert(key.to_owned(), value).is_some() {
        return Err(format!("line {line}: duplicate key {key}"));
    }
    Ok(())
}

fn parse_value(raw: &str, line: usize) -> Result<Value, String> {
    if raw.starts_with('"') {
        return parse_string(raw, line).map(Value::String);
    }
    if raw.starts_with('[') {
        if !raw.ends_with(']') {
            return Err(format!("line {line}: arrays must be on one line"));
        }
        let inner = &raw[1..raw.len() - 1];
        if inner.trim().is_empty() {
            return Ok(Value::Array(Vec::new()));
        }
        let mut values = Vec::new();
        for item in inner.split(',') {
            values.push(parse_string(item.trim(), line)?);
        }
        return Ok(Value::Array(values));
    }
    raw.parse::<u8>()
        .map(Value::Integer)
        .map_err(|_| format!("line {line}: unsupported value {raw}"))
}

fn parse_string(raw: &str, line: usize) -> Result<String, String> {
    if raw.len() < 2 || !raw.starts_with('"') || !raw.ends_with('"') {
        return Err(format!("line {line}: expected a quoted string, got {raw}"));
    }
    let value = &raw[1..raw.len() - 1];
    if value.contains(['"', '\\']) {
        return Err(format!("line {line}: escapes are not supported"));
    }
    Ok(value.to_owned())
}

fn build_registry(mut values: BTreeMap<String, Value>) -> Result<Registry, String> {
    let registry = Registry {
        profiles: remove_array(&mut values, "profiles", "registry")?,
        workstreams: remove_array(&mut values, "workstreams", "registry")?,
    };
    reject_unknown(&values, "registry")?;
    Ok(registry)
}

fn build_test(
    mut values: BTreeMap<String, Value>,
    record_number: usize,
) -> Result<TestRecord, String> {
    let context = format!("test record {record_number}");
    let test = TestRecord {
        id: remove_string(&mut values, "id", &context)?,
        state: remove_string(&mut values, "state", &context)?,
        paths: remove_optional_array(&mut values, "paths", &context)?,
        commands: remove_optional_array(&mut values, "commands", &context)?,
    };
    reject_unknown(&values, &context)?;
    Ok(test)
}

fn build_waiver(
    mut values: BTreeMap<String, Value>,
    record_number: usize,
) -> Result<Waiver, String> {
    let context = format!("waiver record {record_number}");
    let waiver = Waiver {
        id: remove_string(&mut values, "id", &context)?,
        feature: remove_string(&mut values, "feature", &context)?,
        category: remove_string(&mut values, "category", &context)?,
        reason: remove_string(&mut values, "reason", &context)?,
        issue: remove_string(&mut values, "issue", &context)?,
        approver: remove_string(&mut values, "approver", &context)?,
        expiry: remove_string(&mut values, "expiry", &context)?,
    };
    reject_unknown(&values, &context)?;
    Ok(waiver)
}

fn build_feature(
    mut values: BTreeMap<String, Value>,
    record_number: usize,
) -> Result<Feature, String> {
    let context = format!("feature record {record_number}");
    let feature = Feature {
        id: remove_string(&mut values, "id", &context)?,
        priority: remove_string(&mut values, "priority", &context)?,
        phase: remove_integer(&mut values, "phase", &context)?,
        owner: remove_string(&mut values, "owner", &context)?,
        dependencies: remove_array(&mut values, "dependencies", &context)?,
        capability_profiles: remove_array(&mut values, "capability_profiles", &context)?,
        tests: remove_array(&mut values, "tests", &context)?,
        status: remove_string(&mut values, "status", &context)?,
        substitution_policy: remove_string(&mut values, "substitution_policy", &context)?,
    };
    reject_unknown(&values, &context)?;
    Ok(feature)
}

fn remove_string(
    values: &mut BTreeMap<String, Value>,
    key: &str,
    context: &str,
) -> Result<String, String> {
    let value = values
        .remove(key)
        .ok_or_else(|| format!("{context}: missing {key}"))?;
    match value {
        Value::String(value) => Ok(value),
        _ => Err(format!("{context}: {key} must be a string")),
    }
}

fn remove_integer(
    values: &mut BTreeMap<String, Value>,
    key: &str,
    context: &str,
) -> Result<u8, String> {
    let value = values
        .remove(key)
        .ok_or_else(|| format!("{context}: missing {key}"))?;
    take_integer(&value, key).map_err(|error| format!("{context}: {error}"))
}

fn take_integer(value: &Value, key: &str) -> Result<u8, String> {
    match value {
        Value::Integer(value) => Ok(*value),
        _ => Err(format!("{key} must be an integer")),
    }
}

fn remove_array(
    values: &mut BTreeMap<String, Value>,
    key: &str,
    context: &str,
) -> Result<Vec<String>, String> {
    let value = values
        .remove(key)
        .ok_or_else(|| format!("{context}: missing {key}"))?;
    match value {
        Value::Array(value) => Ok(value),
        _ => Err(format!("{context}: {key} must be an array")),
    }
}

fn remove_optional_array(
    values: &mut BTreeMap<String, Value>,
    key: &str,
    context: &str,
) -> Result<Vec<String>, String> {
    match values.remove(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(value)) => Ok(value),
        Some(_) => Err(format!("{context}: {key} must be an array")),
    }
}

fn reject_unknown(values: &BTreeMap<String, Value>, context: &str) -> Result<(), String> {
    if let Some(key) = values.keys().next() {
        return Err(format!("{context}: unknown key {key}"));
    }
    Ok(())
}

fn validate_ledger(
    ledger: &Ledger,
    markdown: &str,
    root: &Path,
    phase_claim: Option<u8>,
) -> Result<(), String> {
    validate_registry(&ledger.registry)?;
    let markdown_features = markdown_features(markdown)?;
    if markdown_features.len() != EXPECTED_FEATURES {
        return Err(format!(
            "Markdown has {} feature IDs; expected {EXPECTED_FEATURES}",
            markdown_features.len()
        ));
    }
    if ledger.features.len() != EXPECTED_FEATURES {
        return Err(format!(
            "ledger has {} feature records; expected {EXPECTED_FEATURES}",
            ledger.features.len()
        ));
    }

    validate_priority_counts(&markdown_features, "Markdown")?;
    let profiles = unique_registry(&ledger.registry.profiles, "profile")?;
    let workstreams = unique_registry(&ledger.registry.workstreams, "workstream")?;
    let tests = validate_tests(&ledger.tests, root)?;

    let mut features = BTreeMap::new();
    let mut ledger_priorities = Vec::new();
    for feature in &ledger.features {
        if !valid_feature_id(&feature.id) {
            return Err(format!("invalid feature ID {}", feature.id));
        }
        if features.insert(feature.id.as_str(), feature).is_some() {
            return Err(format!("duplicate ledger feature ID {}", feature.id));
        }
        ledger_priorities.push((feature.id.as_str(), feature.priority.as_str()));
    }
    validate_priority_counts(&ledger_priorities, "ledger")?;

    let markdown_map: BTreeMap<_, _> = markdown_features.iter().copied().collect();
    for (id, priority) in &markdown_map {
        let feature = features
            .get(id)
            .ok_or_else(|| format!("Markdown feature {id} is missing from parity.toml"))?;
        if feature.priority != *priority {
            return Err(format!(
                "{id} priority mismatch: Markdown={priority}, ledger={}",
                feature.priority
            ));
        }
    }
    for id in features.keys() {
        if !markdown_map.contains_key(id) {
            return Err(format!("ledger feature {id} is missing from Markdown"));
        }
    }

    for feature in &ledger.features {
        validate_feature(feature, &features, &profiles, &workstreams, &tests)?;
    }
    validate_test_traceability(&ledger.features, &tests)?;
    validate_dependency_cycles(&features)?;
    let waivers = validate_waivers(&ledger.waivers, &features)?;
    if let Some(phase) = phase_claim {
        validate_phase_claim(phase, &ledger.features, &tests, &waivers)?;
    }
    Ok(())
}

fn validate_registry(registry: &Registry) -> Result<(), String> {
    if registry.profiles.is_empty() {
        return Err("profile registry is empty".to_owned());
    }
    if registry.workstreams.is_empty() {
        return Err("workstream registry is empty".to_owned());
    }
    Ok(())
}

fn validate_tests<'a>(
    records: &'a [TestRecord],
    root: &Path,
) -> Result<BTreeMap<&'a str, &'a TestRecord>, String> {
    if records.len() != EXPECTED_FEATURES {
        return Err(format!(
            "ledger has {} acceptance test records; expected {EXPECTED_FEATURES}",
            records.len()
        ));
    }
    let packages = workspace_packages(root)?;
    let mut tests = BTreeMap::new();
    for test in records {
        if !valid_acceptance_id(&test.id) {
            return Err(format!("invalid acceptance test ID {}", test.id));
        }
        if tests.insert(test.id.as_str(), test).is_some() {
            return Err(format!("duplicate acceptance test ID {}", test.id));
        }
        if !matches!(
            test.state.as_str(),
            TEST_PLANNED | TEST_PRESENT | TEST_VERIFIED
        ) {
            return Err(format!(
                "{} has invalid test state {}; expected planned, present, or verified",
                test.id, test.state
            ));
        }
        unique_registry(&test.paths, &format!("{} path", test.id))?;
        unique_registry(&test.commands, &format!("{} command", test.id))?;
        if matches!(test.state.as_str(), TEST_PRESENT | TEST_VERIFIED)
            && test.paths.is_empty()
            && test.commands.is_empty()
        {
            return Err(format!(
                "{} is {} but has no local evidence",
                test.id, test.state
            ));
        }
        if test.state == TEST_VERIFIED && (test.paths.is_empty() || test.commands.is_empty()) {
            return Err(format!(
                "{} is verified but must provide both local test paths and runnable commands",
                test.id
            ));
        }
        for path in &test.paths {
            resolve_test_path(root, &test.id, path)?;
        }
        for command in &test.commands {
            resolve_test_command(&packages, &test.id, command)?;
        }
    }
    Ok(tests)
}

fn valid_acceptance_id(id: &str) -> bool {
    let Some(feature) = id.strip_prefix("accept-") else {
        return false;
    };
    valid_feature_id(&feature.to_ascii_uppercase())
        && feature.bytes().all(|byte| !byte.is_ascii_uppercase())
}

fn resolve_test_path(root: &Path, test_id: &str, relative: &str) -> Result<(), String> {
    let path = Path::new(relative);
    if path.is_absolute()
        || relative.is_empty()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!("{test_id} has invalid local test path {relative}"));
    }
    let full_path = root.join(path);
    if !full_path.is_file() {
        return Err(format!(
            "{test_id} test path {relative} does not resolve to a local file"
        ));
    }
    Ok(())
}

fn workspace_packages(root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let manifest = read(&root.join("Cargo.toml"))?;
    let mut packages = BTreeMap::new();
    for relative in workspace_members(&manifest)? {
        let directory = root.join(&relative);
        let package_manifest = read(&directory.join("Cargo.toml"))?;
        let name = package_name(&package_manifest, &relative)?;
        if packages.insert(name.clone(), directory).is_some() {
            return Err(format!("duplicate workspace package name {name}"));
        }
    }
    Ok(packages)
}

fn resolve_test_command(
    packages: &BTreeMap<String, PathBuf>,
    test_id: &str,
    command: &str,
) -> Result<(), String> {
    let parts: Vec<_> = command.split_ascii_whitespace().collect();
    let ["cargo", "test", "-p", package, "--test", target] = parts.as_slice() else {
        return Err(format!(
            "{test_id} command must be exactly `cargo test -p <package> --test <target>`: {command}"
        ));
    };
    let directory = packages.get(*package).ok_or_else(|| {
        format!("{test_id} command references missing workspace package {package}")
    })?;
    if target.is_empty()
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!(
            "{test_id} command has invalid test target {target}"
        ));
    }
    let file_target = directory.join("tests").join(format!("{target}.rs"));
    let directory_target = directory.join("tests").join(target).join("main.rs");
    if !file_target.is_file() && !directory_target.is_file() {
        return Err(format!(
            "{test_id} command target {package} --test {target} does not resolve locally"
        ));
    }
    Ok(())
}

fn unique_registry<'a>(values: &'a [String], kind: &str) -> Result<BTreeSet<&'a str>, String> {
    let set: BTreeSet<_> = values.iter().map(String::as_str).collect();
    if set.len() != values.len() {
        return Err(format!("duplicate {kind} registry entry"));
    }
    if set.contains("") {
        return Err(format!("empty {kind} registry entry"));
    }
    Ok(set)
}

fn markdown_features(markdown: &str) -> Result<Vec<(&str, &str)>, String> {
    let mut features = Vec::new();
    let mut ids = BTreeSet::new();
    for line in markdown.lines() {
        let columns: Vec<_> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if columns.len() < 2 || !valid_feature_id(columns[0]) {
            continue;
        }
        if !ids.insert(columns[0]) {
            return Err(format!("duplicate Markdown feature ID {}", columns[0]));
        }
        features.push((columns[0], columns[1]));
    }
    Ok(features)
}

fn valid_feature_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 6
        && matches!(&bytes[..2], b"IN" | b"SW" | b"GX" | b"AU" | b"OR" | b"RC")
        && bytes[2] == b'-'
        && bytes[3..].iter().all(u8::is_ascii_digit)
}

fn validate_priority_counts(features: &[(&str, &str)], source: &str) -> Result<(), String> {
    for (_, priority) in features {
        if !matches!(*priority, "P0" | "P1" | "P2" | "P3") {
            return Err(format!("{source} contains invalid priority {priority}"));
        }
    }
    for (priority, expected) in EXPECTED_PRIORITIES {
        let actual = features
            .iter()
            .filter(|(_, candidate)| *candidate == priority)
            .count();
        if actual != expected {
            return Err(format!(
                "{source} has {actual} {priority} features; expected {expected}"
            ));
        }
    }
    Ok(())
}

fn validate_feature(
    feature: &Feature,
    features: &BTreeMap<&str, &Feature>,
    profiles: &BTreeSet<&str>,
    workstreams: &BTreeSet<&str>,
    tests: &BTreeMap<&str, &TestRecord>,
) -> Result<(), String> {
    if !matches!(feature.priority.as_str(), "P0" | "P1" | "P2" | "P3") {
        return Err(format!(
            "{} has invalid priority {}",
            feature.id, feature.priority
        ));
    }
    if !(1..=9).contains(&feature.phase) {
        return Err(format!(
            "{} has invalid phase {}",
            feature.id, feature.phase
        ));
    }
    if !workstreams.contains(feature.owner.as_str()) {
        return Err(format!(
            "{} has unknown owner {}",
            feature.id, feature.owner
        ));
    }
    if feature.capability_profiles.is_empty() {
        return Err(format!("{} has no capability profiles", feature.id));
    }
    validate_known_values(
        &feature.id,
        "capability profile",
        &feature.capability_profiles,
        profiles,
    )?;
    if feature.tests.is_empty() {
        return Err(format!("{} has no acceptance tests", feature.id));
    }
    let unique_tests: BTreeSet<_> = feature.tests.iter().map(String::as_str).collect();
    if unique_tests.len() != feature.tests.len() {
        return Err(format!("{} has duplicate test entries", feature.id));
    }
    for test in &feature.tests {
        if !tests.contains_key(test.as_str()) {
            return Err(format!("{} has unknown test {test}", feature.id));
        }
    }
    if !matches!(
        feature.status.as_str(),
        "planned" | "implemented" | "verified" | "blocked"
    ) {
        return Err(format!(
            "{} has invalid status {}",
            feature.id, feature.status
        ));
    }
    if feature.status == "verified" {
        for test_id in &feature.tests {
            let test_state = &tests[test_id.as_str()].state;
            if test_state != TEST_VERIFIED {
                return Err(format!(
                    "{} is verified but acceptance test {test_id} has state {test_state}; expected verified evidence",
                    feature.id,
                ));
            }
        }
    }
    if !matches!(
        feature.substitution_policy.as_str(),
        EXACT_INTEGRATION | GENERIC_EQUIVALENT
    ) {
        return Err(format!(
            "{} has invalid substitution policy {}",
            feature.id, feature.substitution_policy
        ));
    }
    if NAMED_INTEGRATIONS.contains(&feature.id.as_str())
        && feature.substitution_policy != EXACT_INTEGRATION
    {
        return Err(format!(
            "{} is a named integration and cannot use a generic substitute",
            feature.id
        ));
    }

    let unique_dependencies: BTreeSet<_> = feature.dependencies.iter().collect();
    if unique_dependencies.len() != feature.dependencies.len() {
        return Err(format!("{} has duplicate dependencies", feature.id));
    }
    for dependency_id in &feature.dependencies {
        let dependency = features
            .get(dependency_id.as_str())
            .ok_or_else(|| format!("{} has unknown dependency {dependency_id}", feature.id))?;
        if dependency.phase > feature.phase {
            return Err(format!(
                "{} phase {} depends on {dependency_id} phase {}",
                feature.id, feature.phase, dependency.phase
            ));
        }
    }
    Ok(())
}

fn validate_test_traceability(
    features: &[Feature],
    tests: &BTreeMap<&str, &TestRecord>,
) -> Result<(), String> {
    let mut referenced = BTreeSet::new();
    for feature in features {
        for test in &feature.tests {
            referenced.insert(test.as_str());
        }
    }
    for test in tests.keys() {
        if !referenced.contains(test) {
            return Err(format!(
                "acceptance test {test} is not referenced by a feature"
            ));
        }
    }
    Ok(())
}

fn validate_waivers<'a>(
    records: &'a [Waiver],
    features: &BTreeMap<&str, &Feature>,
) -> Result<BTreeMap<&'a str, &'a Waiver>, String> {
    let mut ids = BTreeSet::new();
    let mut by_feature = BTreeMap::new();
    let today = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?
        .as_secs()
        / 86_400;
    for waiver in records {
        if waiver.id.is_empty() || !waiver.id.bytes().all(valid_record_byte) {
            return Err(format!("invalid waiver ID {}", waiver.id));
        }
        if !ids.insert(waiver.id.as_str()) {
            return Err(format!("duplicate waiver ID {}", waiver.id));
        }
        let feature = features.get(waiver.feature.as_str()).ok_or_else(|| {
            format!(
                "{} references unknown feature {}",
                waiver.id, waiver.feature
            )
        })?;
        if feature.status != "blocked" {
            return Err(format!(
                "{} cannot waive {} because it is not blocked",
                waiver.id, waiver.feature
            ));
        }
        if !matches!(
            waiver.category.as_str(),
            "external-dependency"
                | "hardware-unavailable"
                | "legal-approval"
                | "security-exception"
        ) {
            return Err(format!(
                "{} has invalid waiver category {}; unfinished implementation is not waivable",
                waiver.id, waiver.category
            ));
        }
        if waiver.reason.trim().is_empty() {
            return Err(format!("{} has an empty waiver reason", waiver.id));
        }
        if !waiver.issue.starts_with("https://") || !waiver.issue.contains('/') {
            return Err(format!("{} must reference an https issue", waiver.id));
        }
        if waiver.approver.trim().is_empty() {
            return Err(format!("{} has an empty waiver approver", waiver.id));
        }
        let expiry = date_as_epoch_day(&waiver.expiry)
            .ok_or_else(|| format!("{} has invalid expiry {}", waiver.id, waiver.expiry))?;
        if expiry < today {
            return Err(format!("{} expired on {}", waiver.id, waiver.expiry));
        }
        if by_feature.insert(waiver.feature.as_str(), waiver).is_some() {
            return Err(format!("{} has multiple waivers", waiver.feature));
        }
    }
    Ok(by_feature)
}

const fn valid_record_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

fn date_as_epoch_day(date: &str) -> Option<u64> {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = date[..4].parse::<u64>().ok()?;
    let month = date[5..7].parse::<u64>().ok()?;
    let day = date[8..].parse::<u64>().ok()?;
    if year < 1970 || !(1..=12).contains(&month) {
        return None;
    }
    let leap = is_leap_year(year);
    let month_lengths = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let maximum = month_lengths[usize::try_from(month - 1).ok()?];
    if !(1..=maximum).contains(&day) {
        return None;
    }
    let years = year - 1970;
    let leap_days = leap_years_through(year - 1) - leap_years_through(1969);
    let prior_month_days: u64 = month_lengths[..usize::try_from(month - 1).ok()?]
        .iter()
        .sum();
    Some(years * 365 + leap_days + prior_month_days + day - 1)
}

const fn is_leap_year(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

const fn leap_years_through(year: u64) -> u64 {
    year / 4 - year / 100 + year / 400
}

fn validate_phase_claim(
    phase: u8,
    features: &[Feature],
    tests: &BTreeMap<&str, &TestRecord>,
    waivers: &BTreeMap<&str, &Waiver>,
) -> Result<(), String> {
    for feature in features.iter().filter(|feature| feature.phase <= phase) {
        match feature.status.as_str() {
            "verified" => {
                if let Some(test) = feature
                    .tests
                    .iter()
                    .map(|id| tests[id.as_str()])
                    .find(|test| test.state != TEST_VERIFIED)
                {
                    return Err(format!(
                        "phase {phase} claim: {} acceptance test {} has state {}; expected verified evidence",
                        feature.id, test.id, test.state
                    ));
                }
            }
            "blocked" if waivers.contains_key(feature.id.as_str()) => {}
            "blocked" => {
                return Err(format!(
                    "phase {phase} claim: {} is blocked without a valid waiver",
                    feature.id
                ));
            }
            _ => {
                return Err(format!(
                    "phase {phase} claim: {} is {}; expected verified or blocked with a valid waiver",
                    feature.id, feature.status
                ));
            }
        }
    }
    Ok(())
}

fn validate_known_values(
    feature_id: &str,
    kind: &str,
    values: &[String],
    registry: &BTreeSet<&str>,
) -> Result<(), String> {
    let unique: BTreeSet<_> = values.iter().map(String::as_str).collect();
    if unique.len() != values.len() {
        return Err(format!("{feature_id} has duplicate {kind} entries"));
    }
    for value in values {
        if !registry.contains(value.as_str()) {
            return Err(format!("{feature_id} has unknown {kind} {value}"));
        }
    }
    Ok(())
}

fn validate_dependency_cycles(features: &BTreeMap<&str, &Feature>) -> Result<(), String> {
    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    for id in features.keys() {
        visit_dependency(id, features, &mut states, &mut stack)?;
    }
    Ok(())
}

fn visit_dependency<'a>(
    id: &'a str,
    features: &BTreeMap<&'a str, &'a Feature>,
    states: &mut BTreeMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
) -> Result<(), String> {
    match states.get(id) {
        Some(2) => return Ok(()),
        Some(1) => {
            stack.push(id);
            return Err(format!("feature dependency cycle: {}", stack.join(" -> ")));
        }
        _ => {}
    }
    states.insert(id, 1);
    stack.push(id);
    for dependency in &features[id].dependencies {
        visit_dependency(dependency, features, states, stack)?;
    }
    stack.pop();
    states.insert(id, 2);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Layer {
    Foundation,
    Media,
    Gpu,
    Io,
    Feature,
    Service,
    Ui,
    App,
    Tool,
    SimulationComposition,
    ProtocolClient,
}

#[derive(Debug)]
struct Package {
    name: String,
    relative_path: String,
    directory: PathBuf,
    layer: Layer,
}

fn check_dependencies(root: &Path) -> Result<usize, String> {
    let root_manifest = read(&root.join("Cargo.toml"))?;
    let members = workspace_members(&root_manifest)?;
    let mut packages = Vec::new();
    for relative_path in members {
        let directory = fs::canonicalize(root.join(&relative_path))
            .map_err(|error| format!("invalid workspace member {relative_path}: {error}"))?;
        let manifest = read(&directory.join("Cargo.toml"))?;
        let name = package_name(&manifest, &relative_path)?;
        let layer = classify_layer(&relative_path, &name)?;
        packages.push(Package {
            name,
            relative_path,
            directory,
            layer,
        });
    }
    let by_directory: BTreeMap<_, _> = packages
        .iter()
        .map(|package| (package.directory.clone(), package))
        .collect();
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("failed to resolve {}: {error}", root.display()))?;
    let mut count = 0;
    for source in &packages {
        let manifest = read(&source.directory.join("Cargo.toml"))?;
        for (dependency_name, dependency_path) in path_dependencies(&manifest)? {
            count += 1;
            let target_directory = fs::canonicalize(source.directory.join(&dependency_path))
                .map_err(|error| {
                    format!(
                        "{} dependency {dependency_name} has invalid path {dependency_path}: {error}",
                        source.name
                    )
                })?;
            let Some(target) = by_directory.get(&target_directory) else {
                if target_directory.starts_with(&canonical_root) {
                    return Err(format!(
                        "{} dependency {dependency_name} points to a non-member workspace path",
                        source.name
                    ));
                }
                continue;
            };
            if !dependency_allowed(source.layer, target.layer) {
                return Err(format!(
                    "dependency layer inversion: {} ({}) -> {} ({})",
                    source.name, source.relative_path, target.name, target.relative_path
                ));
            }
        }
    }
    Ok(count)
}

fn workspace_members(manifest: &str) -> Result<Vec<String>, String> {
    let mut in_workspace = false;
    let mut collecting = false;
    let mut members = Vec::new();
    for source_line in manifest.lines() {
        let line = source_line.trim();
        if line.starts_with('[') {
            in_workspace = line == "[workspace]";
        }
        if !in_workspace {
            continue;
        }
        if collecting {
            members.extend(quoted_strings(line)?);
            if line.contains(']') {
                collecting = false;
            }
            continue;
        }
        if let Some(value) = line
            .strip_prefix("members")
            .and_then(|line| line.trim_start().strip_prefix('='))
        {
            let value = value.trim();
            if !value.starts_with('[') {
                return Err("workspace members must be an array".to_owned());
            }
            members.extend(quoted_strings(value)?);
            collecting = !value.contains(']');
        }
    }
    if collecting {
        return Err("unterminated workspace members array".to_owned());
    }
    if members.is_empty() {
        return Err("workspace has no members".to_owned());
    }
    Ok(members)
}

fn quoted_strings(source: &str) -> Result<Vec<String>, String> {
    let mut strings = Vec::new();
    let mut remaining = source;
    while let Some(start) = remaining.find('"') {
        remaining = &remaining[start + 1..];
        let end = remaining
            .find('"')
            .ok_or_else(|| "unterminated quoted string".to_owned())?;
        strings.push(remaining[..end].to_owned());
        remaining = &remaining[end + 1..];
    }
    Ok(strings)
}

fn package_name(manifest: &str, relative_path: &str) -> Result<String, String> {
    let mut in_package = false;
    for source_line in manifest.lines() {
        let line = source_line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package && let Some(raw) = assignment_value(line, "name") {
            return parse_manifest_string(raw).map_err(|error| {
                format!("workspace member {relative_path} has invalid name: {error}")
            });
        }
    }
    Err(format!(
        "workspace member {relative_path} has no package name"
    ))
}

fn path_dependencies(manifest: &str) -> Result<Vec<(String, String)>, String> {
    let mut in_dependencies = false;
    let mut dependencies = Vec::new();
    for source_line in manifest.lines() {
        let line = source_line.trim();
        if line.starts_with('[') {
            let section = line.trim_matches(['[', ']']);
            in_dependencies = section == "dependencies"
                || section == "dev-dependencies"
                || section == "build-dependencies"
                || section.ends_with(".dependencies")
                || section.ends_with(".dev-dependencies")
                || section.ends_with(".build-dependencies");
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("malformed dependency declaration: {line}"));
        };
        if let Some(path) = inline_path(value)? {
            dependencies.push((name.trim().to_owned(), path));
        }
    }
    Ok(dependencies)
}

fn inline_path(value: &str) -> Result<Option<String>, String> {
    let Some(path_position) = value.find("path") else {
        return Ok(None);
    };
    let after_path = &value[path_position + "path".len()..];
    let Some((_, raw)) = after_path.split_once('=') else {
        return Err(format!("malformed path dependency: {value}"));
    };
    let raw = raw.trim_start();
    let Some(end) = raw.strip_prefix('"').and_then(|value| value.find('"')) else {
        return Err(format!("path dependency must use a quoted path: {value}"));
    };
    Ok(Some(raw[1..=end].to_owned()))
}

fn assignment_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (candidate, value) = line.split_once('=')?;
    (candidate.trim() == key).then(|| value.trim())
}

fn parse_manifest_string(raw: &str) -> Result<String, String> {
    raw.strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .map(str::to_owned)
        .ok_or_else(|| format!("expected quoted string, got {raw}"))
}

fn classify_layer(relative_path: &str, package_name: &str) -> Result<Layer, String> {
    if package_name == "fm-sim" {
        return Ok(Layer::SimulationComposition);
    }
    if package_name == "fm-client" {
        return Ok(Layer::ProtocolClient);
    }
    let layer = if relative_path.starts_with("crates/foundation/") {
        Layer::Foundation
    } else if relative_path.starts_with("crates/media/") {
        Layer::Media
    } else if relative_path.starts_with("crates/gpu/") {
        Layer::Gpu
    } else if relative_path.starts_with("crates/io/") {
        Layer::Io
    } else if relative_path.starts_with("crates/features/") {
        Layer::Feature
    } else if relative_path.starts_with("crates/services/") {
        Layer::Service
    } else if relative_path.starts_with("crates/ui/") {
        Layer::Ui
    } else if relative_path.starts_with("apps/") {
        Layer::App
    } else if relative_path.starts_with("tools/") {
        Layer::Tool
    } else {
        return Err(format!(
            "workspace member {relative_path} has no documented dependency layer"
        ));
    };
    Ok(layer)
}

const fn dependency_allowed(source: Layer, target: Layer) -> bool {
    match source {
        Layer::Foundation => matches!(target, Layer::Foundation),
        Layer::Media => matches!(target, Layer::Foundation | Layer::Media),
        Layer::Gpu => matches!(target, Layer::Foundation | Layer::Media | Layer::Gpu),
        Layer::Io => matches!(target, Layer::Foundation | Layer::Media | Layer::Io),
        Layer::Feature => matches!(target, Layer::Foundation | Layer::Media | Layer::Feature),
        Layer::Service => matches!(
            target,
            Layer::Foundation
                | Layer::Media
                | Layer::Gpu
                | Layer::Io
                | Layer::Feature
                | Layer::Service
        ),
        Layer::Ui => matches!(target, Layer::Foundation | Layer::Ui),
        Layer::App => !matches!(target, Layer::App | Layer::Tool),
        Layer::Tool => matches!(target, Layer::Tool),
        Layer::SimulationComposition => {
            matches!(target, Layer::Foundation | Layer::Media | Layer::Feature)
        }
        Layer::ProtocolClient => matches!(target, Layer::Foundation | Layer::Ui),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded() -> (Ledger, String) {
        let root = workspace_root();
        let ledger = parse_ledger(&read(&root.join("parity.toml")).unwrap()).unwrap();
        let markdown = read(&root.join("docs/spec/feature-parity.md")).unwrap();
        (ledger, markdown)
    }

    fn assert_invalid(mutator: impl FnOnce(&mut Ledger), expected: &str) {
        let (mut ledger, markdown) = loaded();
        mutator(&mut ledger);
        let error = validate_ledger(&ledger, &markdown, &workspace_root(), None).unwrap_err();
        assert!(
            error.contains(expected),
            "expected error containing {expected:?}, got {error:?}"
        );
    }

    #[test]
    fn checked_in_ledger_is_valid() {
        let (ledger, markdown) = loaded();
        validate_ledger(&ledger, &markdown, &workspace_root(), None).unwrap();
    }

    #[test]
    fn parser_rejects_missing_feature_field() {
        let source = r#"
            schema_version = 3
            [registry]
            profiles = ["portable"]
            workstreams = ["media"]
            [[features]]
            id = "IN-001"
        "#;
        assert!(
            parse_ledger(source)
                .unwrap_err()
                .contains("missing priority")
        );
    }

    #[test]
    fn parser_rejects_duplicate_keys() {
        let source = r#"
            schema_version = 3
            [registry]
            profiles = ["portable"]
            profiles = ["duplicate"]
            workstreams = ["media"]
        "#;
        assert!(
            parse_ledger(source)
                .unwrap_err()
                .contains("duplicate key profiles")
        );
    }

    #[test]
    fn rejects_duplicate_feature_ids() {
        assert_invalid(
            |ledger| {
                let duplicate = ledger.features[0].id.clone();
                ledger.features[1].id = duplicate;
            },
            "duplicate ledger feature ID",
        );
    }

    #[test]
    fn rejects_unknown_profile() {
        assert_invalid(
            |ledger| ledger.features[0].capability_profiles = vec!["unknown".to_owned()],
            "unknown capability profile",
        );
    }

    #[test]
    fn rejects_unknown_test() {
        assert_invalid(
            |ledger| ledger.features[0].tests = vec!["unknown".to_owned()],
            "unknown test",
        );
    }

    #[test]
    fn rejects_phase_inversion() {
        assert_invalid(
            |ledger| {
                let feature = ledger
                    .features
                    .iter_mut()
                    .find(|feature| feature.id == "IN-002")
                    .unwrap();
                feature.phase = 2;
            },
            "phase 2 depends on IN-001 phase 3",
        );
    }

    #[test]
    fn rejects_dependency_cycle() {
        assert_invalid(
            |ledger| {
                let feature = ledger
                    .features
                    .iter_mut()
                    .find(|feature| feature.id == "IN-023")
                    .unwrap();
                feature.dependencies.push("IN-024".to_owned());
            },
            "feature dependency cycle",
        );
    }

    #[test]
    fn rejects_generic_named_integration() {
        assert_invalid(
            |ledger| {
                let feature = ledger
                    .features
                    .iter_mut()
                    .find(|feature| feature.id == "IN-029")
                    .unwrap();
                feature.substitution_policy = GENERIC_EQUIVALENT.to_owned();
            },
            "cannot use a generic substitute",
        );
    }

    #[test]
    fn rejects_verified_feature_with_planned_test() {
        assert_invalid(
            |ledger| ledger.features[0].status = "verified".to_owned(),
            "IN-001 is verified but acceptance test accept-in-001 has state planned; expected verified evidence",
        );
    }

    #[test]
    fn rejects_verified_feature_with_present_test() {
        assert_invalid(
            |ledger| {
                ledger
                    .features
                    .iter_mut()
                    .find(|feature| feature.id == "RC-008")
                    .unwrap()
                    .status = "verified".to_owned();
            },
            "RC-008 is verified but acceptance test accept-rc-008 has state present; expected verified evidence",
        );
    }

    #[test]
    fn rejects_unknown_test_state() {
        assert_invalid(
            |ledger| {
                ledger
                    .tests
                    .iter_mut()
                    .find(|test| test.id == "accept-rc-008")
                    .unwrap()
                    .state = "complete".to_owned();
            },
            "accept-rc-008 has invalid test state complete; expected planned, present, or verified",
        );
    }

    #[test]
    fn rejects_missing_present_test_path() {
        assert_invalid(
            |ledger| {
                let test = ledger
                    .tests
                    .iter_mut()
                    .find(|test| test.id == "accept-rc-008")
                    .unwrap();
                test.paths = vec!["missing/process.rs".to_owned()];
            },
            "does not resolve to a local file",
        );
    }

    #[test]
    fn rejects_missing_command_target() {
        assert_invalid(
            |ledger| {
                let test = ledger
                    .tests
                    .iter_mut()
                    .find(|test| test.id == "accept-rc-008")
                    .unwrap();
                test.commands = vec!["cargo test -p fm-client --test absent".to_owned()];
            },
            "command target fm-client --test absent does not resolve locally",
        );
    }

    #[test]
    fn phase_claim_rejects_unfinished_applicable_feature() {
        let (ledger, markdown) = loaded();
        let error = validate_ledger(&ledger, &markdown, &workspace_root(), Some(2)).unwrap_err();
        assert!(
            error.contains("phase 2 claim: IN-003 is planned"),
            "{error}"
        );
    }

    #[test]
    fn rejects_invalid_waiver_for_unfinished_implementation() {
        assert_invalid(
            |ledger| {
                ledger.waivers.push(Waiver {
                    id: "waiver-in-002".to_owned(),
                    feature: "IN-002".to_owned(),
                    category: "unfinished-implementation".to_owned(),
                    reason: "Implementation is incomplete".to_owned(),
                    issue: "https://example.test/issues/2".to_owned(),
                    approver: "release-owner".to_owned(),
                    expiry: "2999-12-31".to_owned(),
                });
            },
            "unfinished implementation is not waivable",
        );
    }

    #[test]
    fn planned_feature_with_present_local_tests_is_accepted() {
        let (ledger, markdown) = loaded();
        let feature = ledger
            .features
            .iter()
            .find(|feature| feature.id == "RC-008")
            .unwrap();
        assert_eq!(feature.status, "planned");
        let test = ledger
            .tests
            .iter()
            .find(|test| test.id == "accept-rc-008")
            .unwrap();
        assert_eq!(test.state, "present");
        validate_ledger(&ledger, &markdown, &workspace_root(), None).unwrap();
    }

    #[test]
    fn verified_feature_with_verified_local_tests_is_accepted() {
        let (mut ledger, markdown) = loaded();
        ledger
            .features
            .iter_mut()
            .find(|feature| feature.id == "RC-008")
            .unwrap()
            .status = "verified".to_owned();
        ledger
            .tests
            .iter_mut()
            .find(|test| test.id == "accept-rc-008")
            .unwrap()
            .state = "verified".to_owned();
        validate_ledger(&ledger, &markdown, &workspace_root(), Some(1)).unwrap();
    }

    #[test]
    fn rejects_verified_test_without_complete_local_evidence() {
        assert_invalid(
            |ledger| {
                let test = ledger
                    .tests
                    .iter_mut()
                    .find(|test| test.id == "accept-rc-008")
                    .unwrap();
                test.state = "verified".to_owned();
                test.commands.clear();
            },
            "accept-rc-008 is verified but must provide both local test paths and runnable commands",
        );
    }

    #[test]
    fn dependency_direction_rejects_upward_edges() {
        assert!(!dependency_allowed(Layer::Foundation, Layer::Media));
        assert!(!dependency_allowed(Layer::Media, Layer::Service));
        assert!(!dependency_allowed(Layer::Ui, Layer::Service));
        assert!(dependency_allowed(Layer::Feature, Layer::Media));
    }
}
