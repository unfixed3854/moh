use moh::tools::{
    BashArgs, EditArgs, JobCancelArgs, JobStatusArgs, JobWaitArgs, ReadArgs, UpdatePlanArgs,
    WriteArgs,
};
use schemars::schema_for;
use serde_json::{Value, json};

fn required(schema: &Value) -> Value {
    schema.get("required").cloned().unwrap_or_else(|| json!([]))
}

fn assert_contract_basics(
    name: &str,
    schema: &Value,
    expected_required: Value,
    fields: &[(&str, &str)],
) {
    assert_eq!(
        schema["additionalProperties"], false,
        "{name} must be strict"
    );
    assert_eq!(
        required(schema),
        expected_required,
        "{name} required fields"
    );
    for (field, description) in fields {
        assert_eq!(
            schema["properties"][field]["description"], *description,
            "{name}.{field} description"
        );
    }
}

#[test]
fn derived_tool_schemas_expose_each_argument_contract() {
    let schemas = [
        (
            "read",
            serde_json::to_value(schema_for!(ReadArgs)).unwrap(),
            json!(["path"]),
            vec![
                ("path", "Cwd-relative or absolute file or directory path."),
                ("offset", "One-indexed first logical line to display."),
                ("limit", "Maximum number of logical lines to display."),
            ],
        ),
        (
            "write",
            serde_json::to_value(schema_for!(WriteArgs)).unwrap(),
            json!(["path", "content"]),
            vec![
                ("path", "Cwd-relative or absolute path to write."),
                (
                    "content",
                    "Complete contents that the target file should contain.",
                ),
            ],
        ),
        (
            "edit",
            serde_json::to_value(schema_for!(EditArgs)).unwrap(),
            json!(["path", "remove_from", "remove_to", "replacement_lines"]),
            vec![
                ("path", "Cwd-relative or absolute path to edit."),
                (
                    "remove_from",
                    "First three-character line anchor to remove, inclusive.",
                ),
                (
                    "remove_to",
                    "Last three-character line anchor to remove, inclusive.",
                ),
                (
                    "replacement_lines",
                    "Replacement content with exactly one logical line per element.",
                ),
            ],
        ),
        (
            "bash",
            serde_json::to_value(schema_for!(BashArgs)).unwrap(),
            json!(["command"]),
            vec![
                ("command", "Command interpreted by Bash with `-lc`."),
                (
                    "background",
                    "Return when the command is running instead of waiting for completion.",
                ),
                ("timeout_ms", "Optional command timeout in milliseconds."),
            ],
        ),
        (
            "job_status",
            serde_json::to_value(schema_for!(JobStatusArgs)).unwrap(),
            json!([]),
            vec![(
                "job_id",
                "Optional canonical job identifier; omitting it lists all retained jobs.",
            )],
        ),
        (
            "job_wait",
            serde_json::to_value(schema_for!(JobWaitArgs)).unwrap(),
            json!(["job_ids"]),
            vec![
                (
                    "job_ids",
                    "One or more canonical job identifiers to wait for.",
                ),
                (
                    "timeout_ms",
                    "Optional bounded wait deadline in milliseconds.",
                ),
            ],
        ),
        (
            "job_cancel",
            serde_json::to_value(schema_for!(JobCancelArgs)).unwrap(),
            json!(["job_id"]),
            vec![("job_id", "The canonical identifier of the job to cancel.")],
        ),
        (
            "update_plan",
            serde_json::to_value(schema_for!(UpdatePlanArgs)).unwrap(),
            json!(["plan"]),
            vec![
                ("explanation", "Optional reason for changing the plan."),
                ("plan", "Complete ordered replacement plan."),
            ],
        ),
    ];

    for (name, schema, expected_required, fields) in schemas {
        assert_contract_basics(name, &schema, expected_required, &fields);
    }
}

#[test]
fn derived_tool_schemas_expose_structural_constraints() {
    let read = serde_json::to_value(schema_for!(ReadArgs)).unwrap();
    assert_eq!(read["properties"]["path"]["minLength"], 1);
    assert_eq!(read["properties"]["offset"]["minimum"], 1);
    assert_eq!(read["properties"]["limit"]["minimum"], 1);
    assert!(read["properties"].get("file_path").is_none());

    let write = serde_json::to_value(schema_for!(WriteArgs)).unwrap();
    assert_eq!(write["properties"]["path"]["minLength"], 1);

    let edit = serde_json::to_value(schema_for!(EditArgs)).unwrap();
    assert_eq!(edit["properties"]["path"]["minLength"], 1);
    assert_eq!(
        edit["properties"]["remove_from"]["pattern"],
        "^[A-Za-z0-9]{3}$"
    );
    assert_eq!(
        edit["properties"]["remove_to"]["pattern"],
        "^[A-Za-z0-9]{3}$"
    );
    assert_eq!(
        edit["properties"]["replacement_lines"]["items"]["pattern"],
        "^[^\\r\\n]*$"
    );

    let bash = serde_json::to_value(schema_for!(BashArgs)).unwrap();
    assert_eq!(bash["properties"]["command"]["minLength"], 1);
    assert_eq!(bash["properties"]["background"]["default"], false);
    assert_eq!(bash["properties"]["timeout_ms"]["minimum"], 1);
    assert_eq!(bash["properties"]["timeout_ms"]["maximum"], 3_600_000);

    let job_wait = serde_json::to_value(schema_for!(JobWaitArgs)).unwrap();
    assert_eq!(job_wait["properties"]["job_ids"]["minItems"], 1);
    assert_eq!(job_wait["properties"]["timeout_ms"]["minimum"], 0);
    assert_eq!(job_wait["properties"]["timeout_ms"]["maximum"], 300_000);

    let update_plan = serde_json::to_value(schema_for!(UpdatePlanArgs)).unwrap();
    assert_eq!(update_plan["properties"]["plan"]["maxItems"], 32);
    let definitions = &update_plan["$defs"];
    let item = &definitions["PlanItem"];
    assert_eq!(item["additionalProperties"], false);
    assert_eq!(
        item["properties"]["step"]["description"],
        "One ordered plan step."
    );
    let status = &definitions["PlanStatus"];
    assert_eq!(
        Value::Array(
            status["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .map(|variant| variant["const"].clone())
                .collect(),
        ),
        json!([
            "pending",
            "in_progress",
            "completed",
            "blocked",
            "cancelled"
        ])
    );
}

#[test]
fn job_service_entry_points_preserve_derived_validation_guards() {
    let source = include_str!("../src/tools/job.rs");
    for (entry_point, signature) in [
        ("status", "pub async fn status(&self, args: JobStatusArgs)"),
        ("cancel", "pub async fn cancel(&self, args: JobCancelArgs)"),
    ] {
        let start = source
            .find(signature)
            .expect("JobService entry point must exist");
        let entry = &source[start..];
        let body = &entry[..entry
            .find("\n    ///")
            .or_else(|| entry.find("\n    fn "))
            .unwrap_or(entry.len())];
        assert!(
            body.contains("args.validate()"),
            "JobService::{entry_point} must guard future derived constraints"
        );
    }
}
