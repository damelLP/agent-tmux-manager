//! Nonempty task rendering keeps extensible JSON and safe inspect pointers.
use super::*;
fn tasks() -> Value {
    serde_json::from_str(include_str!("../fixtures/atm_me/context-tasks.json")).unwrap()
}
// Break caught: nonempty tasks lack an inspect command, or unknown JSON fields are dropped.
#[tokio::test]
async fn me_nonempty_tasks_have_actionable_text_and_preserve_complete_json() {
    let output = exchange(&["me"], json!({"result":tasks()}), true, false).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("../fixtures/atm_me/context-tasks.txt")
    );
    let mut value = tasks();
    value["open_tasks"][0]["future"] = json!({"keep":"é"});
    let output = exchange(
        &["me", "--format", "json"],
        json!({"result":value}),
        true,
        false,
    )
    .await;
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        value
    );
}
// Break caught: terminal controls escape the task line or untrusted task IDs form shell commands.
#[tokio::test]
async fn task_titles_are_one_line_and_inspect_pointers_require_canonical_ids() {
    let mut value = tasks();
    value["open_tasks"][0]["title"] = json!("é🙂\nspoof\u{1b}[31m");
    value["open_tasks"][0]["task_id"] = json!("t7;false");
    let output = exchange(&["me"], json!({"result":value}), true, false).await;
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("é🙂"));
    assert!(!text.contains("\nspoof"));
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains("inspect: atm task get t7;false"));
    assert!(text.contains("inspect: atm task get t9"));
    assert_eq!(text.matches("inspect: atm task get ").count(), 1);
    let output = exchange(&["me"], json!({"result":tasks()}), true, true).await;
    assert_eq!(output.status.code(), Some(141));
    assert!(output.stderr.is_empty(), "{output:?}");
}
