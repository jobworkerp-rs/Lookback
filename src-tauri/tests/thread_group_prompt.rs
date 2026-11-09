use std::{collections::BTreeMap, sync::Arc};

use command_utils::util::jq::execute_jq;
use serde_json::json;

#[test]
fn group_prompt_uses_member_summaries_not_the_output_title() {
    let workflow: serde_yaml::Value = serde_yaml::from_str(include_str!(
        "../../workers/lang-workers/workers/thread-group-summary/thread-group-summary-single.yaml"
    ))
    .unwrap();
    let tasks = workflow["do"].as_sequence().unwrap();
    assert!(
        tasks
            .iter()
            .find_map(|task| task.get("initOutputVars"))
            .unwrap()["set"]
            .get("group_title")
            .is_none()
    );
    let generation_tasks = tasks
        .iter()
        .find_map(|task| task.get("generateSummaryWithRetry"))
        .expect("generateSummaryWithRetry workflow task")["try"]
        .as_sequence()
        .expect("retry-wrapped generation tasks");
    let prompt = generation_tasks
        .iter()
        .find_map(|task| task.get("generateSummary"))
        .expect("generateSummary workflow task")["run"]["function"]["arguments"]["prompt"]
        .as_str()
        .unwrap();
    assert!(!prompt.contains("group.title"));
    assert!(!prompt.contains("group_title"));
    let body = prompt.strip_prefix("$${").unwrap().trim_end();
    let body = body.strip_suffix('}').unwrap();
    let parser = liquid::ParserBuilder::with_stdlib().build().unwrap();
    let template = parser.parse(body).unwrap();
    for group in [
        json!({"groupCanonicalKey":"root-key"}),
        json!({"groupCanonicalKey":"root-key","title":"Readable title"}),
        json!({"groupCanonicalKey":"root-key","title":""}),
    ] {
        let rendered = template
            .render(
                &liquid::to_object(&json!({
                    "group": group,
                    "member_summaries": ["Summary A", "Summary B"],
                    "thread_group_summary_user_tail": "Merge the summaries above."
                }))
                .unwrap(),
            )
            .unwrap();
        assert!(rendered.contains("Summary A"));
        assert!(rendered.contains("Summary B"));
        assert!(rendered.contains("- canonical key: root-key"));
        assert!(rendered.trim_end().ends_with("Merge the summaries above."));
        assert!(!rendered.contains("- title:"));
        assert!(!rendered.contains("Readable title"));
    }

    let description = tasks
        .iter()
        .find_map(|task| task.get("createSummaryThread"))
        .unwrap()["run"]["function"]["arguments"]["json_body"]
        .as_str()
        .unwrap();
    assert!(!description.contains("$group.title"));
    let expression = description
        .strip_prefix("${")
        .unwrap()
        .strip_suffix('}')
        .unwrap();
    let context = BTreeMap::from([
        (
            "group".into(),
            Arc::new(json!({"id":{"value":42},"groupCanonicalKey":"root-key"})),
        ),
        (
            "workflow".into(),
            Arc::new(json!({"input":{"group_owner_user_id":1}})),
        ),
    ]);
    let request = execute_jq(json!({}), expression, &context).unwrap();
    let request: serde_json::Value = serde_json::from_str(request.as_str().unwrap()).unwrap();
    assert_eq!(request["description"], "root-key");
}
