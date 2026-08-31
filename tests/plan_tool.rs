use std::str::FromStr;

use moh::{
    session::{PlanItem, PlanStatus},
    tools::{PlanToolError, PlanUpdateOutcome, UpdatePlanArgs, plan_update_channel},
};

#[test]
fn plan_statuses_parse_the_five_canonical_names() {
    for (text, expected) in [
        ("pending", PlanStatus::Pending),
        ("in_progress", PlanStatus::InProgress),
        ("completed", PlanStatus::Completed),
        ("blocked", PlanStatus::Blocked),
        ("cancelled", PlanStatus::Cancelled),
    ] {
        assert_eq!(PlanStatus::from_str(text).unwrap(), expected, "{text}");
        assert_eq!(expected.as_str(), text, "{text}");
    }
}

#[test]
fn plan_item_rejects_invalid_step_text() {
    for step in [
        "",
        " leading",
        "trailing ",
        "\ttrimmed",
        "contains\ncontrol",
        &"x".repeat(257),
    ] {
        assert!(
            PlanItem::parse(step, PlanStatus::Pending).is_err(),
            "{step:?} must be rejected"
        );
    }
}

#[test]
fn update_plan_rejects_excess_items_and_multiple_active_items() {
    let too_many = UpdatePlanArgs {
        explanation: None,
        plan: (0..33)
            .map(|index| PlanItem::parse(format!("Step {index}"), PlanStatus::Pending).unwrap())
            .collect(),
    };
    assert!(too_many.validate().is_err());

    let multiple_active = UpdatePlanArgs {
        explanation: None,
        plan: vec![
            PlanItem::parse("First", PlanStatus::InProgress).unwrap(),
            PlanItem::parse("Second", PlanStatus::InProgress).unwrap(),
        ],
    };
    assert!(multiple_active.validate().is_err());
}

#[test]
fn update_plan_allows_duplicate_steps_and_an_empty_clear() {
    let duplicates = UpdatePlanArgs {
        explanation: None,
        plan: vec![
            PlanItem::parse("Verify", PlanStatus::Pending).unwrap(),
            PlanItem::parse("Verify", PlanStatus::Completed).unwrap(),
        ],
    };
    assert!(duplicates.validate().is_ok());

    assert!(
        UpdatePlanArgs {
            explanation: None,
            plan: vec![],
        }
        .validate()
        .is_ok()
    );
}

#[tokio::test]
async fn update_waits_for_the_authoritative_receiver() {
    let (client, mut receiver) = plan_update_channel();
    let call = tokio::spawn(async move {
        client
            .replace(UpdatePlanArgs {
                explanation: Some("Start verification".into()),
                plan: vec![PlanItem::parse("Run tests", PlanStatus::InProgress).unwrap()],
            })
            .await
    });
    let request = receiver.recv().await.unwrap();
    let outcome = PlanUpdateOutcome::durable(
        request.plan().to_vec(),
        request.explanation().map(str::to_owned),
    );
    request.succeed(outcome);
    assert_eq!(call.await.unwrap().unwrap().plan()[0].step(), "Run tests");
}

#[tokio::test]
async fn closed_request_channel_returns_a_runtime_error() {
    let (client, receiver) = plan_update_channel();
    drop(receiver);

    let error = client
        .replace(UpdatePlanArgs {
            explanation: None,
            plan: vec![],
        })
        .await
        .unwrap_err();

    assert!(matches!(error, PlanToolError::Runtime));
    assert_eq!(
        error.to_string(),
        "[E_RUNTIME] plan tool state is unavailable"
    );
}

#[test]
fn plan_update_outcome_renders_canonical_counts_and_pending_durability() {
    let outcome = PlanUpdateOutcome::new(
        vec![
            PlanItem::parse("Inspect the code", PlanStatus::Completed).unwrap(),
            PlanItem::parse("Run tests", PlanStatus::InProgress).unwrap(),
            PlanItem::parse("Review", PlanStatus::Pending).unwrap(),
            PlanItem::parse("Document", PlanStatus::Pending).unwrap(),
        ],
        Some("Start verification".into()),
        false,
    );

    assert_eq!(
        outcome.render(),
        "Plan updated: 1 completed, 1 in progress, 2 pending, 0 blocked, 0 cancelled.\n\
         Explanation: Start verification\n\
         1. [completed] Inspect the code\n\
         2. [in_progress] Run tests\n\
         3. [pending] Review\n\
         4. [pending] Document\n\
         Plan persistence is pending; the live session retains this update."
    );
}
