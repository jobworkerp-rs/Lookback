use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const EXPECTED: &[(&str, &str, &[&str])] = &[
    ("lookback_issue_list_ready", "issue_tracker.rpc.v1.IssueGateway/ListReady", &["pageSize", "pageToken"]),
    ("lookback_issue_claim", "issue_tracker.rpc.v1.IssueGateway/Claim", &["issueId", "expectedRevision", "idempotencyKey"]),
    ("lookback_issue_heartbeat", "issue_tracker.rpc.v1.IssueGateway/Heartbeat", &["issueId", "capabilityId", "expectedRevision", "idempotencyKey"]),
    ("lookback_issue_release", "issue_tracker.rpc.v1.IssueGateway/Release", &["issueId", "capabilityId"]),
    ("lookback_issue_transition", "issue_tracker.rpc.v1.IssueGateway/Transition", &["issueId", "targetState", "capabilityId", "expectedRevision", "idempotencyKey", "completion", "cancellation"]),
];

fn mapping<'a>(value: &'a serde_yaml::Value, subject: &str) -> Result<&'a serde_yaml::Mapping> {
    value.as_mapping().with_context(|| format!("{subject} must be a mapping"))
}

fn string<'a>(map: &'a serde_yaml::Mapping, key: &str, subject: &str) -> Result<&'a str> {
    map.get(serde_yaml::Value::String(key.into()))
        .and_then(serde_yaml::Value::as_str)
        .with_context(|| format!("{subject}.{key} must be a string"))
}

fn keys(map: &serde_yaml::Mapping) -> Result<BTreeSet<&str>> {
    map.keys()
        .map(|key| key.as_str().context("mapping key must be a string"))
        .collect()
}

fn validate_worker(worker: &serde_yaml::Value, expected: (&str, &str, &[&str])) -> Result<()> {
    let (name, rpc, fields) = expected;
    let worker = mapping(worker, name)?;
    ensure!(string(worker, "name", name)? == name, "worker name mismatch");
    let settings = mapping(
        worker.get(serde_yaml::Value::String("settings".into())).context("worker.settings is missing")?,
        name,
    )?;
    ensure!(string(settings, "method", name)? == rpc, "{name}: settings.method differs from expected RPC");
    let description = string(worker, "description", name)?;
    let description: serde_yaml::Value = serde_yaml::from_str(description)
        .with_context(|| format!("{name}: structured description is invalid YAML"))?;
    let description = mapping(&description, name)?;
    ensure!(string(description, "format", name)? == "jobworkerp-description", "{name}: invalid format");
    ensure!(description.get(serde_yaml::Value::String("version".into())).and_then(serde_yaml::Value::as_i64) == Some(1), "{name}: version must be 1");
    ensure!(!string(description, "description", name)?.trim().is_empty(), "{name}: description is blank");
    ensure!(string(description, "rpc", name)? == rpc, "{name}: rpc does not match settings.method");
    let parameters = mapping(
        description.get(serde_yaml::Value::String("parameters".into())).context("parameters is missing")?,
        name,
    )?;
    let actual = keys(parameters)?;
    let expected: BTreeSet<_> = fields.iter().copied().collect();
    ensure!(actual == expected, "{name}: parameter keys differ; expected {expected:?}, got {actual:?}");
    for field in fields {
        let annotation = mapping(
            parameters.get(serde_yaml::Value::String((*field).into())).context("parameter is missing")?,
            &format!("{name}.{field}"),
        )?;
        ensure!(!string(annotation, "description", &format!("{name}.{field}"))?.trim().is_empty(), "{name}.{field}: description is blank");
        if *field == "expectedRevision" {
            let examples = annotation.get(serde_yaml::Value::String("examples".into()))
                .and_then(serde_yaml::Value::as_sequence)
                .context("expectedRevision.examples is missing")?;
            ensure!(examples.iter().any(|value| value.as_str() == Some("0")) && examples.iter().any(|value| value.as_str() == Some("123")), "{name}.expectedRevision: examples must contain quoted decimal strings 0 and 123");
        }
    }
    Ok(())
}

fn run(path: &Path) -> Result<()> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root: serde_yaml::Value = serde_yaml::from_str(&raw).context("outer worker YAML is invalid")?;
    let workers = mapping(&root, "root")
        .and_then(|root| root.get(serde_yaml::Value::String("workers".into())).context("workers is missing"))?
        .as_sequence()
        .context("workers must be a sequence")?;
    let indexed: BTreeMap<_, _> = workers.iter().map(|worker| {
        let map = mapping(worker, "worker")?;
        Ok((string(map, "name", "worker")?, worker))
    }).collect::<Result<_>>()?;
    ensure!(indexed.len() == EXPECTED.len(), "unexpected number of gateway workers");
    for expected in EXPECTED {
        validate_worker(*indexed.get(expected.0).with_context(|| format!("missing worker {}", expected.0))?, *expected)
            .with_context(|| format!("validate worker {}", expected.0))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let path = std::env::args().nth(1).context("usage: mcp-description-check <worker-yaml>")?;
    if std::env::args().nth(2).is_some() { bail!("usage: mcp-description-check <worker-yaml>"); }
    run(Path::new(&path))
}
