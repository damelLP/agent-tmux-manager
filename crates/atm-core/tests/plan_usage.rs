use atm_core::{PlanProfileId, PlanUsageWindow};
use serde_json::json;

#[test]
fn profile_ids_are_validated_in_config_and_on_the_wire() {
    for id in [
        "a",
        "0",
        "team_1-backup",
        "abcdefghijklmnopqrstuvwxyz012345",
    ] {
        let profile = PlanProfileId::new(id).unwrap();
        assert_eq!(profile.to_string(), id);
        assert_eq!(serde_json::to_value(&profile).unwrap(), json!(id));
        assert_eq!(
            serde_json::from_value::<PlanProfileId>(json!(id)).unwrap(),
            profile
        );
    }
    for id in [
        "",
        "_a",
        "-a",
        "Team",
        "a b",
        "a/b",
        "é",
        "abcdefghijklmnopqrstuvwxyz0123456",
    ] {
        assert!(PlanProfileId::new(id).is_err(), "accepted {id:?}");
        assert!(serde_json::from_value::<PlanProfileId>(json!(id)).is_err());
    }
}

#[test]
fn out_of_contract_windows_are_rejected_at_construction() {
    assert!(PlanUsageWindow::from_epoch_seconds(0, 0.0, None).is_none());
    for value in [f64::NAN, f64::NEG_INFINITY, f64::INFINITY, -0.1, 100.1] {
        assert!(PlanUsageWindow::from_epoch_seconds(300, value, None).is_none());
    }
    assert!(PlanUsageWindow::from_epoch_seconds(300, 0.0, None).is_some());
    assert!(PlanUsageWindow::from_epoch_seconds(300, 100.0, Some(i64::MAX)).is_none());
    // Year 10000 is representable by chrono, but not an RFC 3339 year.
    assert!(PlanUsageWindow::from_epoch_seconds(300, 100.0, Some(253402300800)).is_none());
}
