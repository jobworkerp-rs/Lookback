//! Static checks over the `${ ... }` jq expressions in the bundled workflow
//! YAML, run with the engine's own jaq build.
//!
//! The workflow runner evaluates jq through `command_utils::util::jq`, whose
//! jaq dialect and error reporting differ from the `jq` CLI used by the
//! vitest fixtures. Three unbalanced-parenthesis expressions in the Issue
//! resolution workflows passed every CLI-based check and only failed once a
//! production run reached them, so the first test runs each expression
//! through the real engine with every `$variable` bound to null and fails on
//! any load or compile error. Runtime errors (null inputs) are expected and
//! ignored.
//!
//! Binding every variable hides a second class of failure: jaq rejects a
//! reference to a variable the context never defined (`//` does not help),
//! so a `set` guarded by `if:` followed by a step that reads the variable
//! fails at runtime whenever the guard was false. The second test walks the
//! task tree and requires every reference to be preceded by a definition
//! that is reached unconditionally relative to the reference.
//!
//! The engine's `fromjson` also differs from jq on an empty or blank string
//! (jobworkerp-rs #357): it yields no value instead of failing, so a
//! `catch` default never applies and the runner turns the empty result into
//! `[]`. The remaining tests pin that behaviour and require the fallback
//! form that also covers it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use command_utils::util::jq::execute_jq;

use crate::data::paths::tests::{expand_env_defaults, walkdir};

fn workers_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../workers")
}

fn workflow_documents() -> Vec<(PathBuf, serde_yaml::Value)> {
    let mut files = walkdir(&workers_dir())
        .into_iter()
        // `schema/` holds the DSL JSON schema, whose examples are prose.
        .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        .filter(|path| !path.components().any(|part| part.as_os_str() == "schema"))
        .collect::<Vec<_>>();
    files.sort();
    files
        .into_iter()
        .map(|file| {
            let text = std::fs::read_to_string(&file).expect("workflow YAML is readable");
            // The runtime expands `%{VAR:-default}` before parsing, so an
            // unexpanded placeholder must not hide a file from these checks.
            let document = serde_yaml::from_str::<serde_yaml::Value>(&expand_env_defaults(&text))
                .unwrap_or_else(|error| panic!("{} does not parse: {error}", file.display()));
            (file, document)
        })
        .collect()
}

fn relative(file: &Path) -> String {
    file.strip_prefix(workers_dir())
        .unwrap_or(file)
        .display()
        .to_string()
}

/// Extracts jq expressions from one scalar. `$${ ... }` is a Liquid
/// template and is skipped; `${` inside a string stays part of the
/// enclosing expression because braces are matched, not split.
fn jq_expressions(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = text[search_from..].find("${") {
        let start = search_from + rel;
        if start > 0 && bytes[start - 1] == b'$' {
            search_from = start + 2;
            continue;
        }
        let mut depth = 0usize;
        let mut end = None;
        for (offset, ch) in text[start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        out.push(text[start + 2..end].trim().to_owned());
        search_from = end + 1;
    }
    out
}

fn collect(value: &serde_yaml::Value, out: &mut Vec<String>) {
    match value {
        serde_yaml::Value::String(text) => out.extend(jq_expressions(text)),
        serde_yaml::Value::Sequence(items) => items.iter().for_each(|item| collect(item, out)),
        serde_yaml::Value::Mapping(map) => map.values().for_each(|item| collect(item, out)),
        _ => {}
    }
}

fn variable_names(expr: &str) -> BTreeSet<String> {
    expr.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .filter(|token| token.len() > 1 && token.starts_with('$'))
        .map(|token| token.trim_start_matches('$').to_owned())
        .collect()
}

/// Variables an expression reads from the workflow context: every `$name`
/// minus the ones the expression binds itself (`. as $x`, `reduce .. as $e`,
/// destructuring `as [$a, {b: $c}]`, and `def f($a; $b)` parameters).
fn free_variables(expr: &str) -> BTreeSet<String> {
    let mut bound = BTreeSet::new();
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    for (index, token) in tokens.iter().enumerate() {
        if *token == "as" {
            // The pattern is one `$name` or a bracketed destructuring that may
            // span tokens; it ends when brackets balance (or at the pipe).
            let mut depth = 0i32;
            for pattern in &tokens[index + 1..] {
                bound.extend(variable_names(pattern));
                depth += pattern.matches(['[', '{']).count() as i32;
                depth -= pattern.matches([']', '}']).count() as i32;
                if pattern.contains('|') || (depth <= 0 && !pattern.ends_with([',', ':'])) {
                    break;
                }
            }
        }
        if *token == "def" {
            let rest = tokens[index + 1..].join(" ");
            if let Some(end) = rest.find(':') {
                bound.extend(variable_names(&rest[..end]));
            }
        }
    }
    variable_names(expr).difference(&bound).cloned().collect()
}

/// Every `${ ... }` expression in the bundled workflows with its file.
fn workflow_expressions() -> Vec<(String, String)> {
    workflow_documents()
        .iter()
        .flat_map(|(file, document)| {
            let mut expressions = Vec::new();
            collect(document, &mut expressions);
            expressions.into_iter().map(|expr| (relative(file), expr))
        })
        .collect()
}

#[test]
fn every_workflow_jq_expression_compiles_in_the_engine() {
    let mut checked = 0usize;
    let mut failures = Vec::new();
    for (file, expr) in workflow_expressions() {
        checked += 1;
        let params: BTreeMap<String, Arc<serde_json::Value>> = variable_names(&expr)
            .into_iter()
            .map(|name| (name, Arc::new(serde_json::Value::Null)))
            .collect();
        if let Err(error) = execute_jq(serde_json::Value::Null, &expr, &params) {
            let message = format!("{error:#}");
            if !message.starts_with("jq runtime error") {
                failures.push(format!(
                    "{file}: {message}\n    {}",
                    expr.lines().collect::<Vec<_>>().join("\n    ")
                ));
            }
        }
    }
    assert!(
        checked > 500,
        "expected hundreds of expressions, found {checked}"
    );
    assert!(
        failures.is_empty(),
        "{} jq expression(s) do not compile in the workflow engine:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// A `catch` default never applies to an empty or blank string, so a
/// fallback must be written `(try (X | fromjson)) // D`, which covers a
/// failure and the empty result alike. Only `try … catch` fallbacks are
/// checked; a bare `fromjson` on a gRPC/HTTP body is not.
fn fromjson_catch_fallbacks(expr: &str) -> Vec<String> {
    expr.match_indices("fromjson) catch")
        .map(|(at, _)| {
            let start = expr.floor_char_boundary(at.saturating_sub(40));
            expr[start..].chars().take(120).collect()
        })
        .collect()
}

#[test]
fn engine_fromjson_yields_nothing_for_a_blank_string() {
    // Fails once jobworkerp-rs #357 is fixed; the `// D` fallbacks and the
    // check below can then be relaxed.
    for blank in ["", "  "] {
        assert_eq!(
            execute_jq(serde_json::json!(blank), "fromjson", &BTreeMap::new()).unwrap(),
            serde_json::json!([]),
            "{blank:?}"
        );
    }
}

#[test]
fn every_fromjson_fallback_also_covers_an_empty_result() {
    let failures = workflow_expressions()
        .into_iter()
        .flat_map(|(file, expr)| {
            fromjson_catch_fallbacks(&expr)
                .into_iter()
                .map(move |site| format!("{file}: {site}"))
        })
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "{} fromjson fallback(s) must be written `(try (X | fromjson)) // D`:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(fromjson_catch_fallbacks("(try ($t | fromjson)) // null").is_empty());
    assert_eq!(
        fromjson_catch_fallbacks("try ($t | fromjson) catch null").len(),
        1
    );
}

#[test]
fn expression_extraction_skips_liquid_and_matches_nested_braces() {
    let text = r#"a: ${ { x: ($y // {}) } } b: $${ liquid } c: "${ .z }""#;
    assert_eq!(jq_expressions(text), vec!["{ x: ($y // {}) }", ".z"]);
    assert_eq!(
        variable_names("$a + ($b.c // $a) | $__x"),
        BTreeSet::from(["a".to_owned(), "b".to_owned(), "__x".to_owned()])
    );
    assert_eq!(
        free_variables(
            "$rows | map(. as $row | $row.id) | reduce .[] as [$k, {v: $v}] ({}; . + {($k): $v + $base}) | def pick($p; $q): $p + $q + $other; pick(1; 2)"
        ),
        BTreeSet::from(["rows".to_owned(), "base".to_owned(), "other".to_owned()])
    );
}

/// One ancestor task on the way to a definition or reference. `scope` marks
/// bodies (for / try / catch / fork) whose definitions are only visible
/// inside that body.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Frame {
    id: usize,
    /// Normalized `if:` text when the task is guarded.
    guard: Option<String>,
    scope: bool,
}

#[derive(Debug)]
struct Binding {
    var: String,
    pos: usize,
    path: Vec<Frame>,
    task: String,
    /// The reference sits in the task's own `if:`, which runs before the
    /// guard can protect it.
    in_guard: bool,
}

const GLOBAL_VARIABLES: [&str; 3] = ["workflow", "ENV", "__loc__"];

#[derive(Default)]
struct Walker {
    next_id: usize,
    pos: usize,
    defs: Vec<Binding>,
    refs: Vec<Binding>,
    /// `(task, reader, sibling)` for every `set` key reading another key the
    /// same `set` replaces (see `INTENDED_PREVIOUS_VALUE_READS`).
    sibling_reads: Vec<(String, String, String)>,
}

impl Walker {
    fn frame(&mut self, guard: Option<String>, scope: bool) -> Frame {
        self.next_id += 1;
        Frame {
            id: self.next_id,
            guard,
            scope,
        }
    }

    fn define(&mut self, var: &str, path: &[Frame], task: &str) {
        self.defs.push(Binding {
            var: var.to_owned(),
            pos: self.pos,
            path: path.to_vec(),
            task: task.to_owned(),
            in_guard: false,
        });
    }

    fn references(
        &mut self,
        value: &serde_yaml::Value,
        path: &[Frame],
        task: &str,
        in_guard: bool,
    ) {
        let mut expressions = Vec::new();
        collect(value, &mut expressions);
        for expr in expressions {
            for name in free_variables(&expr) {
                if GLOBAL_VARIABLES.contains(&name.as_str()) {
                    continue;
                }
                self.refs.push(Binding {
                    var: name,
                    pos: self.pos,
                    path: path.to_vec(),
                    task: task.to_owned(),
                    in_guard,
                });
            }
        }
    }

    fn task_list(&mut self, list: &serde_yaml::Value, path: &mut Vec<Frame>) {
        let Some(items) = list.as_sequence() else {
            return;
        };
        for item in items {
            let Some(map) = item.as_mapping() else {
                continue;
            };
            for (name, def) in map {
                let name = name.as_str().unwrap_or("?").to_owned();
                self.task(&name, def, path);
            }
        }
    }

    fn body(
        &mut self,
        list: Option<&serde_yaml::Value>,
        path: &mut Vec<Frame>,
        defines: &[(&str, &str)],
    ) {
        let frame = self.frame(None, true);
        path.push(frame);
        for (var, task) in defines {
            let p = path.clone();
            self.define(var, &p, task);
        }
        if let Some(list) = list {
            self.task_list(list, path);
        }
        path.pop();
    }

    fn task(&mut self, name: &str, def: &serde_yaml::Value, path: &mut Vec<Frame>) {
        self.pos += 1;
        let Some(map) = def.as_mapping() else { return };
        let guard = map
            .get("if")
            .and_then(serde_yaml::Value::as_str)
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "));
        let frame = self.frame(guard, false);
        path.push(frame);

        // Expressions evaluated at this task's position: everything except
        // nested bodies. `for.in` belongs here, its `do` does not.
        for (key, value) in map {
            match key.as_str().unwrap_or("") {
                // `export` runs after the task body, see below.
                "do" | "try" | "catch" | "branches" | "export" => {}
                "if" => self.references(value, path, name, true),
                "for" => {
                    if let Some(inner) = value.get("in") {
                        self.references(inner, path, name, false);
                    }
                }
                _ => self.references(value, path, name, false),
            }
        }
        let mapping_keys = |value: Option<&serde_yaml::Value>| -> Vec<String> {
            value
                .and_then(serde_yaml::Value::as_mapping)
                .map(|m| {
                    m.keys()
                        .filter_map(|k| k.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        if let Some(set) = map.get("set").and_then(serde_yaml::Value::as_mapping) {
            self.sibling_reads.extend(set_sibling_reads(name, set));
        }
        // A `set` task writes its keys into the workflow context before the
        // task's own `export.as` is evaluated, so the export may read them.
        for key in mapping_keys(map.get("set")) {
            let p = path.clone();
            self.define(&key, &p, name);
        }
        if let Some(export) = map.get("export") {
            self.pos += 1;
            self.references(export, path, name, false);
        }
        for key in mapping_keys(map.get("export").and_then(|e| e.get("as"))) {
            let p = path.clone();
            self.define(&key, &p, name);
        }

        if let Some(for_def) = map.get("for") {
            // `each` / `at` are visible only inside the loop body; the engine
            // names them `item` / `index` when they are omitted.
            let loop_vars: Vec<(&str, &str)> = [("each", "item"), ("at", "index")]
                .iter()
                .map(|(key, default)| {
                    let var = for_def
                        .get(*key)
                        .and_then(serde_yaml::Value::as_str)
                        .filter(|var| !var.is_empty())
                        .unwrap_or(default);
                    (var, name)
                })
                .collect();
            self.body(map.get("do"), path, &loop_vars);
        } else if map.contains_key("do") {
            self.body(map.get("do"), path, &[]);
        }
        if map.contains_key("try") {
            self.body(map.get("try"), path, &[]);
        }
        if let Some(catch) = map.get("catch") {
            let error_var: Vec<(&str, &str)> = catch
                .get("as")
                .and_then(serde_yaml::Value::as_str)
                .map(|var| vec![(var, name)])
                .unwrap_or_default();
            self.body(catch.get("do"), path, &error_var);
        }
        if map.contains_key("branches") {
            self.body(map.get("branches"), path, &[]);
        }
        path.pop();
    }
}

/// A definition reaches a reference when it comes first in document order,
/// every ancestor it does not share with the reference is unguarded (or
/// guarded by the very same `if:` text that also guards the reference), and
/// it is not inside a body the reference sits outside of.
fn dominates(def: &Binding, reference: &Binding) -> bool {
    if def.var != reference.var || def.pos >= reference.pos {
        return false;
    }
    let shared = def
        .path
        .iter()
        .zip(reference.path.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut reference_guards: Vec<&String> = reference.path[shared..]
        .iter()
        .filter_map(|frame| frame.guard.as_ref())
        .collect();
    if reference.in_guard
        && let Some(own) = reference.path.last().and_then(|frame| frame.guard.as_ref())
        && let Some(index) = reference_guards.iter().position(|g| *g == own)
    {
        // The task's own guard is what is being evaluated; it protects nothing.
        reference_guards.remove(index);
    }
    def.path[shared..].iter().all(|frame| {
        !frame.scope
            && match &frame.guard {
                None => true,
                Some(guard) => reference_guards.contains(&guard),
            }
    })
}

fn undefined_references(document: &serde_yaml::Value) -> Vec<Binding> {
    let mut walker = Walker::default();
    let mut path = Vec::new();
    if let Some(list) = document.get("do") {
        walker.task_list(list, &mut path);
    }
    if let Some(output) = document.get("output") {
        walker.pos += 1;
        walker.references(output, &path, "output", false);
    }
    let Walker { defs, refs, .. } = walker;
    refs.into_iter()
        .filter(|reference| !defs.iter().any(|def| dominates(def, reference)))
        .collect()
}

/// References whose definition is guarded by a *different* condition that
/// implies the reference's guard (flag variables, task-kind checks). They
/// are safe today but the analysis cannot prove it; triage is tracked in
/// Lookback-app #204. Add here only with a reason, never for Issue workflows.
const KNOWN_IMPLIED_GUARDS: &[(&str, &str)] = &[
    (
        "workflows/daily-work-summary/daily-work-summary-batch.yaml",
        "daily_path_plan",
    ),
    (
        "workflows/weekly-work-summary/weekly-work-summary-batch.yaml",
        "weekly_path_plan",
    ),
    (
        "workflows/monthly-work-summary/monthly-work-summary-batch.yaml",
        "monthly_path_plan",
    ),
    (
        "workflows/lookback-periodic-run.yaml",
        "regular_import_common_args",
    ),
    (
        "workflows/lookback-periodic-run.yaml",
        "regular_import_start_date",
    ),
    (
        "lang-workers/workers/thread-reflection/thread-reflection-single.yaml",
        "fact_unresolved_count",
    ),
    (
        "lang-workers/workers/thread-reflection/thread-reflection-single.yaml",
        "heuristic_score",
    ),
    (
        "lang-workers/workers/thread-reflection/thread-reflection-single.yaml",
        "parsed_output",
    ),
    // Guarded by the `source_labels_ready` flag set together with the payload.
    (
        "workflows/issue-extract/issue-extract.yaml",
        "source_apply_payload",
    ),
];

#[test]
fn every_variable_reference_is_preceded_by_an_unconditional_definition() {
    let mut failures = Vec::new();
    for (file, document) in &workflow_documents() {
        if document.get("do").is_none() {
            continue;
        }
        let file_name = relative(file);
        let mut seen = BTreeSet::new();
        for reference in undefined_references(document) {
            // Language workers receive their prompt text through the worker's
            // `workflow_context` at registration, not through a task.
            if reference.var.ends_with("_system_prompt") || reference.var.ends_with("_user_tail") {
                continue;
            }
            if KNOWN_IMPLIED_GUARDS
                .iter()
                .any(|(known_file, var)| *known_file == file_name && *var == reference.var)
            {
                continue;
            }
            if seen.insert((reference.var.clone(), reference.task.clone())) {
                failures.push(format!(
                    "{}: `${}` read in `{}` has no unconditional earlier definition",
                    relative(file),
                    reference.var,
                    reference.task
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} variable reference(s) can hit `undefined variable` in the engine:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn dominance_requires_order_and_unguarded_path() {
    let document: serde_yaml::Value = serde_yaml::from_str(
        r#"
do:
  - init:
      set:
        ready: "${ false }"
  - guarded:
      if: "${ $ready }"
      set:
        only_sometimes: "${ true }"
  - reader:
      if: "${ ($only_sometimes // false) | not }"
      set:
        x: "${ $ready }"
  - same_guard_reader:
      if: "${ $ready }"
      set:
        z: "${ $only_sometimes }"
  - loop:
      for:
        each: item
        in: "${ [1] }"
      do:
        - inner:
            set:
              inside: "${ $item }"
  - after:
      set:
        y: "${ $inside + $item }"
  - sibling:
      export:
        as:
          a: "${ .x }"
          b: "${ $a }"
"#,
    )
    .expect("fixture parses");
    let mut undefined: Vec<(String, String)> = undefined_references(&document)
        .into_iter()
        .map(|b| (b.var, b.task))
        .collect();
    undefined.sort();
    undefined.dedup();
    assert_eq!(
        undefined,
        vec![
            ("a".to_owned(), "sibling".to_owned()),
            ("inside".to_owned(), "after".to_owned()),
            ("item".to_owned(), "after".to_owned()),
            ("only_sometimes".to_owned(), "reader".to_owned()),
        ]
    );
}

/// Keys of one `set` are evaluated against the context as it was before the
/// task, so a key reading another key of the same `set` sees that key's old
/// value. Such a read must either reset the sibling to a constant (reading
/// the old value is then the point) or be listed here as intended.
const INTENDED_PREVIOUS_VALUE_READS: &[(&str, &str, &str)] = &[
    // The page token being consumed is recorded before the next one replaces it.
    (
        "collectReviewCandidatePage",
        "review_page_seen_tokens",
        "review_page_token",
    ),
    (
        "collectOperationHistoryPage",
        "history_page_seen_tokens",
        "history_page_token",
    ),
];

fn is_constant_reset(value: &serde_yaml::Value) -> bool {
    let mut expressions = Vec::new();
    collect(value, &mut expressions);
    expressions.iter().all(|expr| {
        matches!(
            expr.trim(),
            "null" | "[]" | "{}" | "\"\"" | "false" | "true" | "0"
        )
    })
}

fn set_sibling_reads(task: &str, set: &serde_yaml::Mapping) -> Vec<(String, String, String)> {
    let mut reads = Vec::new();
    for (reader, value) in set {
        let reader = reader.as_str().unwrap_or_default();
        let mut expressions = Vec::new();
        collect(value, &mut expressions);
        let read: BTreeSet<String> = expressions
            .iter()
            .flat_map(|expr| free_variables(expr))
            .collect();
        for (sibling, sibling_value) in set {
            let sibling = sibling.as_str().unwrap_or_default();
            if sibling != reader
                && read.contains(sibling)
                && !is_constant_reset(sibling_value)
                && !INTENDED_PREVIOUS_VALUE_READS.contains(&(task, reader, sibling))
            {
                reads.push((task.to_owned(), reader.to_owned(), sibling.to_owned()));
            }
        }
    }
    reads
}

fn sibling_reads(document: &serde_yaml::Value) -> Vec<(String, String, String)> {
    let mut walker = Walker::default();
    if let Some(list) = document.get("do") {
        walker.task_list(list, &mut Vec::new());
    }
    walker.sibling_reads
}

#[test]
fn no_set_key_reads_a_sibling_it_expects_to_be_updated() {
    let mut failures = Vec::new();
    for (file, document) in workflow_documents() {
        failures.extend(sibling_reads(&document).into_iter().map(|(task, reader, sibling)| {
            format!(
                "{}: `{task}` computes `{reader}` from `${sibling}`, which the same set replaces",
                relative(&file)
            )
        }));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn sibling_reads_allow_constant_resets_only() {
    let document: serde_yaml::Value = serde_yaml::from_str(
        r#"
do:
  - shape:
      set:
        complete: "${ $count > 0 }"
        latest: "${ if $complete then 1 else null end }"
  - consume:
      set:
        result: "${ $pending.value }"
        pending: "${ null }"
"#,
    )
    .expect("fixture parses");
    assert_eq!(
        sibling_reads(&document),
        vec![(
            "shape".to_owned(),
            "latest".to_owned(),
            "complete".to_owned()
        )]
    );
}
