#![allow(clippy::unwrap_used)] // Test fixture helpers only.

use atm_core::Money;
use atm_org::{
    department::{
        hire_check, live_counts, parse_department, roster_refusal, task_budget_default, validate,
        Department, DepartmentError, HireCheck, HireMode, ModelPattern,
    },
    AgentId, OrgEvent, OrgState, RoleSpec, TaskBudget,
};
use std::collections::BTreeSet;

const MINIMAL: &str = "title='A'\nprojects=['alpha']\n";

// Break caught: either real department or its defaulted/explicit limits stop loading.
#[test]
fn real_departments_parse() {
    let apps = parse_department("apps", include_str!("fixtures/departments/apps.toml")).unwrap();
    assert_eq!(apps.title, "Apps");
    assert_eq!(
        apps.projects,
        ["mansa".parse().unwrap(), "baliyo".parse().unwrap()]
    );
    assert_eq!(apps.hire_mode, HireMode::Report);
    assert_eq!(apps.roster.len(), 13);
    let tools = parse_department("tools", include_str!("fixtures/departments/tools.toml")).unwrap();
    assert_eq!(tools.title, "Agent tools");
    assert_eq!(
        tools.projects,
        [
            "agent-tmux-manager".parse().unwrap(),
            "harness".parse().unwrap()
        ]
    );
    assert_eq!(tools.hire_mode, HireMode::Report);
    assert_eq!(tools.roster.len(), 14);
    let entry = tools
        .roster
        .iter()
        .find(|entry| entry.seats.to_string() == "agent-tmux-manager/build-*")
        .unwrap();
    assert_eq!(entry.max.unwrap().get(), 9);
    assert!(tools.spend.unwrap().models.unwrap()[0].allows("openai-codex/gpt-6.2-sol:max"));
}

// Break caught: ignoring unknown TOML keys or accepting structurally unusable departments.
#[test]
fn strict_keys_and_required_values() {
    for trailer in [
        "typo=1\n",
        "[hire]\nmode='report'\ntypo=1\n",
        "[[roster]]\nseats='alpha/lead'\n[[roster]]\nseats='alpha/worker'\ntypo=1\n",
        "[spend]\nmodels=['opus']\ntypo=1\n",
        "[spend]\ntask_budget={turns=1,typo=1}\n",
        "[spend]\ntask_budget={turns=1,deadline=2026-11-21}\n",
        "[spend]\ntask_budget={cost={microdollars=1,typo=1}}\n",
        "[hire]\nmode='prevent'\n",
        "head='owner'\n",
        "[[roster]]\nseats='alpha/lead'\nmax=0\n",
        "[[roster]]\nseats='alpha/lead'\nmax=-1\n",
    ] {
        assert!(
            matches!(
                parse_department("one", &format!("{MINIMAL}{trailer}")),
                Err(DepartmentError::Toml { .. })
            ),
            "accepted or misclassified: {trailer}"
        );
    }
    for text in ["projects=['alpha']\n", "title='A'\n"] {
        assert!(matches!(
            parse_department("one", text),
            Err(DepartmentError::Toml { .. })
        ));
    }
    for (text, message) in [
        (
            "title='A'\nprojects=[]\n",
            "projects: projects must not be empty",
        ),
        (
            "title='A'\nprojects=['alpha','alpha']\n",
            "projects: duplicate project alpha",
        ),
    ] {
        assert_eq!(
            parse_department("one", text).unwrap_err().to_string(),
            message
        );
    }
    for id in ["Bad", "owner", "system", "-dash", "a/b"] {
        assert!(parse_department(id, MINIMAL)
            .unwrap_err()
            .to_string()
            .starts_with("id:"));
    }
    let dept = parse_department("one", MINIMAL).unwrap();
    assert_eq!(dept.id.as_str(), "one");
    assert_eq!(dept.hire_mode, HireMode::Report);
    assert!(
        dept.roster.is_empty()
            && dept.goal.is_none()
            && dept.head.is_none()
            && dept.spend.is_none()
    );
    assert_eq!(
        parse_department("one", &format!("{MINIMAL}[hire]\n"))
            .unwrap()
            .hire_mode,
        HireMode::Report
    );
}

// Break caught: forcing a date or both limits, accepting negative cost, or losing precision.
#[test]
fn budget_without_date() {
    for (budget, turns, cost) in [
        (
            "{ turns=300, cost={microdollars=5000} }",
            Some(300),
            Some(Money::from_microdollars(5000)),
        ),
        ("{ turns=100 }", Some(100), None),
        ("{ cost={microdollars=0} }", None, Some(Money::zero())),
    ] {
        let spend = parse_department("one", &format!("{MINIMAL}[spend]\ntask_budget={budget}\n"))
            .unwrap()
            .spend
            .unwrap();
        assert!(spend.models.is_none());
        assert_eq!(
            spend.task_budget,
            Some(TaskBudget {
                turns,
                cost,
                deadline: None
            })
        );
    }
    let bad = format!("{MINIMAL}[spend]\ntask_budget={{cost={{microdollars=-1}}}}\n");
    assert!(parse_department("one", &bad)
        .unwrap_err()
        .to_string()
        .starts_with("spend.task_budget.cost.microdollars:"));
}

fn budget_department(id: &str, project: &str, mode: &str, budget: &str) -> Department {
    parse_department(id, &format!("title='A'\nprojects=['{project}']\n[hire]\nmode='{mode}'\n[spend]\ntask_budget={budget}\n")).unwrap()
}

// Break caught: choosing a single department, ignoring report mode, or
// taking a smaller budget from an unrelated project.
#[test]
fn department_task_budget_takes_independent_minima() {
    let project = "alpha".parse().unwrap();
    let departments = [
        budget_department(
            "tools",
            "alpha",
            "report",
            "{turns=8,cost={microdollars=500000}}",
        ),
        budget_department(
            "apps",
            "alpha",
            "enforce",
            "{turns=20,cost={microdollars=120000}}",
        ),
        budget_department(
            "other",
            "beta",
            "enforce",
            "{turns=0,cost={microdollars=0}}",
        ),
    ];
    let (budget, from) = task_budget_default(&departments, &project).unwrap();
    assert_eq!(
        budget,
        TaskBudget {
            turns: Some(8),
            cost: Some(Money::from_microdollars(120000)),
            deadline: None
        }
    );
    assert_eq!(
        serde_json::to_value(from).unwrap(),
        serde_json::json!({"turns":"tools","cost":"apps"})
    );
    assert!(task_budget_default(&departments, &"gamma".parse().unwrap()).is_none());
}

// Break caught: treating absence as zero or returning an empty default, losing
// exact zeros, or resolving equal minima by input/scan order rather than ID.
#[test]
fn department_task_budget_keeps_missing_fields_and_breaks_ties_by_id() {
    let project = "alpha".parse().unwrap();
    let empty = budget_department("empty", "alpha", "report", "{}");
    assert!(task_budget_default(std::slice::from_ref(&empty), &project).is_none());
    for (rule, field, expected) in [
        (
            "{turns=0}",
            "turns",
            TaskBudget {
                turns: Some(0),
                cost: None,
                deadline: None,
            },
        ),
        (
            "{cost={microdollars=0}}",
            "cost",
            TaskBudget {
                turns: None,
                cost: Some(Money::zero()),
                deadline: None,
            },
        ),
    ] {
        let mut departments = [
            budget_department("zeta", "alpha", "report", rule),
            empty.clone(),
            budget_department("first", "alpha", "enforce", rule),
        ];
        for _ in 0..2 {
            let (budget, from) = task_budget_default(&departments, &project).unwrap();
            assert_eq!(budget, expected);
            assert_eq!(
                serde_json::to_value(from).unwrap(),
                serde_json::json!({(field):"first"})
            );
            departments.reverse();
        }
    }
}

// Break caught: only the long form works, or the inline table ignores unknown keys.
#[test]
fn roster_strings_and_tables_normalize() {
    let short = "roster=['alpha/lead','alpha/build-*',{seats='alpha/build-hard-*',max=9}]\n";
    let long = "[[roster]]\nseats='alpha/lead'\n[[roster]]\nseats='alpha/build-*'\n[[roster]]\nseats='alpha/build-hard-*'\nmax=9\n";
    let dept = |suffix: &str| parse_department("one", &format!("{MINIMAL}{suffix}"));
    let mixed = dept(short).unwrap().roster;
    assert_eq!(mixed, dept(long).unwrap().roster);
    assert_eq!(
        mixed
            .iter()
            .map(|e| (e.seats.to_string(), e.max.map(|n| n.get())))
            .collect::<Vec<_>>(),
        [
            ("alpha/lead".into(), None),
            ("alpha/build-*".into(), None),
            ("alpha/build-hard-*".into(), Some(9)),
        ]
    );
    assert_eq!(dept("roster=['alpha/lead']\n").unwrap().roster, mixed[..1]);
    for invalid in [
        "roster=[{seats='alpha/lead',typo=1}]\n",
        "roster=[7]\n",
        "roster=[{max=1}]\n",
        "roster=[{seats='alpha/lead',max=0}]\n",
    ] {
        assert!(
            matches!(dept(invalid), Err(DepartmentError::Toml { .. })),
            "{invalid}"
        );
    }
    assert!(dept("roster=['alpha/bu*ild']\n")
        .unwrap_err()
        .to_string()
        .starts_with("roster.seats:"));
}

fn assert_roster_typo_at(roster: &str, expected_line: usize) {
    let Err(error) = parse_department("one", &format!("{MINIMAL}{roster}")) else {
        panic!("expected a roster table error on line {expected_line}");
    };
    assert!(
        matches!(error, DepartmentError::Toml { line: Some(line), .. } if line == expected_line)
            && error.to_string().contains("unknown field `typo`"),
        "expected `typo` on line {expected_line}, got {error:?}"
    );
}

// Break caught: a table's deserializer error is replaced by a generic roster error.
#[test]
fn roster_array_of_tables_typo_names_key_and_line() {
    assert_roster_typo_at(
        "[[roster]]\nseats='alpha/lead'\n[[roster]]\nseats='alpha/worker'\nmax=2\ntypo=1\n",
        8,
    );
}

#[test]
fn roster_inline_table_typo_names_key_and_line() {
    assert_roster_typo_at(
        "roster=[\n 'alpha/lead',\n {seats='alpha/worker',max=2},\n {seats='alpha/build-*',typo=1},\n]\n",
        6,
    );
}

// Break caught: offsets mistaken for line numbers or source snippets disclosed.
#[test]
fn toml_diagnostic_points_to_original_line() {
    let error = parse_department(
        "one",
        "title='Équipe'\nprojects=['alpha']\ntypo=1 # PRIVATE-SENTINEL\n",
    )
    .unwrap_err();
    assert!(
        matches!(error, DepartmentError::Toml { line: Some(3), .. }),
        "{error:?}"
    );
    let detail = error.to_string();
    assert!(detail.contains("unknown field `typo`"), "{detail}");
    assert!(!detail.contains("PRIVATE-SENTINEL") && !detail.contains('\n'));
}

// Break caught: '*' matches a bare prefix, a non-ASCII digit, or an unnumbered job.
#[test]
fn numbered_and_exact_seats() {
    let dept = parse_department(
        "one",
        &format!("{MINIMAL}roster=['alpha/build-*','alpha/lead']\n"),
    )
    .unwrap();
    let (numbered, exact) = (&dept.roster[0], &dept.roster[1]);
    for (name, expected) in [
        ("build-1", true),
        ("build-12", true),
        ("build-hard-1", false),
        ("build-", false),
        ("build-x", false),
        ("build-١", false),
    ] {
        let seat = AgentId {
            project: "alpha".parse().unwrap(),
            name: name.into(),
        };
        assert_eq!(numbered.matches(&seat), expected, "{name}");
    }
    assert!(!numbered.matches(&"beta/build-1".parse().unwrap()));
    assert!(exact.matches(&"alpha/lead".parse().unwrap()));
    assert!(!exact.matches(&"alpha/lead1".parse().unwrap()));
    for invalid in [
        "build-*",
        "alpha/*",
        "alpha/bu*ild",
        "alpha/a**",
        "alpha/abcdefghijklmnopqrstuvwxyz012345*",
    ] {
        let error =
            parse_department("one", &format!("{MINIMAL}roster=['{invalid}']\n")).unwrap_err();
        assert!(error.to_string().starts_with("roster.seats:"), "{error}");
    }
}

// Break caught: treating a model as a numbered seat pattern or a substring search.
#[test]
fn model_patterns_are_literal_or_prefix() {
    for (pattern, name, expect) in [
        ("opus", "opus", true),
        ("opus", "opus:max", false),
        (
            "openai-codex/gpt-6.2-sol*",
            "openai-codex/gpt-6.2-sol",
            true,
        ),
        (
            "openai-codex/gpt-6.2-sol*",
            "openai-codex/gpt-6.2-sol:max",
            true,
        ),
        (
            "openai-codex/gpt-6.2-sol*",
            "openai-codex/gpt-6.2-solid",
            true,
        ),
        (
            "openai-codex/gpt-6.2-sol*",
            "openai-codex/gpt-6.3-sol",
            false,
        ),
        ("*", "opus", true),
    ] {
        assert_eq!(
            pattern.parse::<ModelPattern>().unwrap().allows(name),
            expect,
            "{pattern} vs {name}"
        );
    }
    for bad in ["op*us", "opus**", ""] {
        assert!(bad
            .parse::<ModelPattern>()
            .unwrap_err()
            .to_string()
            .starts_with("spend.models:"));
    }
}

// Break caught: requiring a head to be a member project, or missing validation of the other references.
#[test]
fn validation_checks_keyed_project_references() {
    let parse = |project| {
        parse_department("one", &format!("title='A'\nprojects=['alpha','missing']\nhead='{project}/lead'\nroster=['beta/build-*']\n")).unwrap()
    };
    let registered = BTreeSet::from(["alpha".parse().unwrap(), "beta".parse().unwrap()]);
    let problems = validate(&[parse("unregistered")], &registered);
    assert!(problems
        .iter()
        .all(|problem| problem.department_id().as_str() == "one"));
    assert_eq!(
        problems.iter().map(ToString::to_string).collect::<Vec<_>>(),
        [
            "projects: project missing is not registered",
            "roster.seats: beta/build-* is outside the department's projects",
            "head: unregistered/lead is not in a registered project",
        ]
    );
    assert_eq!(validate(&[parse("beta")], &registered), problems[..2]);
}

// Break caught: counting down seats, nonmatching names, registry-only facts or capping live at max.
#[test]
fn live_counts_non_down_seats_independently_of_max() {
    let mut state = OrgState::default();
    let now = "2026-11-21T12:00:00Z".parse().unwrap();
    state
        .apply(
            1,
            now,
            &OrgEvent::ProjectRegistered {
                project_id: "alpha".parse().unwrap(),
                root: "/alpha".into(),
            },
        )
        .unwrap();
    for (seq, id, pane) in [
        (2, "alpha/build-1", "%1"),
        (3, "alpha/build-12", "%2"),
        (4, "alpha/build-hard-1", "%3"),
    ] {
        state
            .apply(
                seq,
                now,
                &OrgEvent::AgentAdopted {
                    agent_id: id.parse().unwrap(),
                    pane: pane.into(),
                    session_id: None,
                },
            )
            .unwrap();
    }
    let dept = parse_department(
        "one",
        &format!("{MINIMAL}roster=['alpha/build-*', {{seats='alpha/build-*',max=1}}]\n"),
    )
    .unwrap();
    assert!(dept.roster.iter().all(|entry| entry.live(&state) == 2));
    state
        .apply(
            5,
            now,
            &OrgEvent::AgentDown {
                agent_id: "alpha/build-12".parse().unwrap(),
                reason: "fixture".into(),
            },
        )
        .unwrap();
    assert!(dept.roster.iter().all(|entry| entry.live(&state) == 1));
}

// Break caught: an index counts down seats, loses duplicate rows, conflates
// exact seats with numbered prefixes, or omits prefixes ending in a digit.
#[test]
fn indexed_live_counts_match_exact_and_numbered_rows() {
    let mut state = OrgState::default();
    let now = "2026-11-21T12:00:00Z".parse().unwrap();
    for (seq, id) in [(1, "alpha"), (2, "beta")] {
        state
            .apply(
                seq,
                now,
                &OrgEvent::ProjectRegistered {
                    project_id: id.parse().unwrap(),
                    root: format!("/fixture/{id}"),
                },
            )
            .unwrap();
    }
    for (i, id) in [
        "alpha/build-",
        "alpha/build-1",
        "alpha/build-10",
        "alpha/build-12",
        "alpha/build-123",
        "alpha/build-hard-1",
        "beta/build-12",
    ]
    .into_iter()
    .enumerate()
    {
        state
            .apply(
                i as u64 + 3,
                now,
                &OrgEvent::AgentAdopted {
                    agent_id: id.parse().unwrap(),
                    pane: format!("%{i}"),
                    session_id: None,
                },
            )
            .unwrap();
    }
    state
        .apply(
            10,
            now,
            &OrgEvent::AgentDown {
                agent_id: "alpha/build-123".parse().unwrap(),
                reason: "fixture".into(),
            },
        )
        .unwrap();
    let dept = parse_department(
        "one",
        "title='A'\nprojects=['alpha','beta']\nroster=[
            'alpha/build-1',
            'alpha/build-*',
            'alpha/build-1*',
            {seats='alpha/build-*',max=1},
            'alpha/build-123',
            'beta/build-*',
        ]\n",
    )
    .unwrap();
    assert_eq!(
        dept.roster
            .iter()
            .map(live_counts(&state))
            .collect::<Vec<_>>(),
        [1, 3, 2, 3, 0, 1]
    );
}

// Break caught: report or a non-member roster grants admission, or overlapping
// enforcing departments must *all* allow a start rather than just one.
#[test]
fn hire_checks_only_enforcing_departments_for_a_named_caller() {
    let state = OrgState::default();
    let caller = "beta/manager".parse().unwrap();
    let seat = "alpha/build-2".parse().unwrap();
    let enforce = |id, rows| {
        parse_department(
            id,
            &format!("{MINIMAL}roster={rows}\n[hire]\nmode='enforce'\n"),
        )
        .unwrap()
    };
    let miss = enforce("first", "['alpha/build-1']");
    let report =
        parse_department("report", &format!("{MINIMAL}roster=['alpha/build-*']\n")).unwrap();
    let other_project = parse_department(
        "elsewhere",
        "title='B'\nprojects=['beta']\nroster=['alpha/build-*']\n[hire]\nmode='enforce'\n",
    )
    .unwrap();
    let role = model_role(None);
    assert_eq!(
        hire_check(std::slice::from_ref(&miss), None, &seat, &role, &state),
        HireCheck::Owner
    );
    for departments in [vec![], vec![report.clone(), other_project]] {
        assert_eq!(
            hire_check(&departments, Some(&caller), &seat, &role, &state),
            HireCheck::Uncovered
        );
    }
    let departments = [miss.clone(), report, enforce("second", "[]")];
    assert_eq!(
        hire_check(&departments, Some(&caller), &seat, &role, &state),
        HireCheck::NeedsApproval {
            reason: "department first: not on roster".into()
        }
    );
    for roster in ["['alpha/build-2']", "['alpha/build-*']"] {
        let departments = [miss.clone(), enforce("allow", roster)];
        assert_eq!(
            hire_check(&departments, Some(&caller), &seat, &role, &state),
            HireCheck::Allowed
        );
    }
}

// Break caught: a starting seat is not counted, a down seat is, or a live
// target consumes a second slot. A full match is preferred to a roster miss.
#[test]
fn hire_roster_counts_other_live_seats() {
    let mut state = OrgState::default();
    let now = "2026-11-21T12:00:00Z".parse().unwrap();
    let live: AgentId = "alpha/build-1".parse().unwrap();
    let down: AgentId = "alpha/build-2".parse().unwrap();
    let role = atm_org::parse_role("build-1", "+++\nharness='pi'\n+++\nFixture\n").unwrap();
    for (i, event) in [
        OrgEvent::ProjectRegistered {
            project_id: live.project.clone(),
            root: "/alpha".into(),
        },
        OrgEvent::AgentUpRequested {
            agent_id: live.clone(),
            role: role.clone(),
        },
        OrgEvent::AgentUpRequested {
            agent_id: down.clone(),
            role: role.clone(),
        },
        OrgEvent::AgentDown {
            agent_id: down.clone(),
            reason: "fixture".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        state.apply(i as u64 + 1, now, &event).unwrap();
    }
    let make = |id, rows| {
        parse_department(
            id,
            &format!("{MINIMAL}roster={rows}\n[hire]\nmode='enforce'\n"),
        )
        .unwrap()
    };
    let departments = [
        make("miss", "[]"),
        make("full", "[{seats='alpha/build-*',max=1}]"),
        make("also-full", "[{seats='alpha/build-*',max=1}]"),
    ];
    let caller = "alpha/lead".parse().unwrap();
    assert_eq!(
        hire_check(&departments, Some(&caller), &down, &role, &state),
        HireCheck::NeedsApproval {
            reason: "department full: roster full: 1 of 1 for alpha/build-*".into()
        }
    );
    for alternatives in [
        vec![make(
            "same",
            "[{seats='alpha/build-*',max=1}, {seats='alpha/build-*',max=2}]",
        )],
        vec![
            departments[1].clone(),
            make("other", "[{seats='alpha/build-*',max=2}]"),
        ],
    ] {
        assert_eq!(
            hire_check(&alternatives, Some(&caller), &down, &role, &state),
            HireCheck::Allowed
        );
    }
    state
        .apply(
            5,
            now,
            &OrgEvent::AgentStuck {
                agent_id: live.clone(),
                reason: "fixture".into(),
            },
        )
        .unwrap();
    assert_eq!(
        hire_check(&departments, Some(&caller), &live, &role, &state),
        HireCheck::Allowed
    );
    assert!(roster_refusal(&departments[1], &live, true, &live_counts(&state)).is_none());
    state
        .apply(
            6,
            now,
            &OrgEvent::AgentAdopted {
                agent_id: down,
                pane: "%9".into(),
                session_id: None,
            },
        )
        .unwrap();
    assert_eq!(
        roster_refusal(&departments[1], &live, true, &live_counts(&state))
            .unwrap()
            .to_string(),
        "roster full: 1 of 1 for alpha/build-*"
    );
    let combined = model_department("limited", "[{seats='alpha/build-*',max=1}]", "models=[]");
    // build-2 is now live, so the other build is the one occupying its slot.
    assert_eq!(
        model_refusal(&[combined], &model_role(None), &state).as_deref(),
        Some("department limited: roster full: 1 of 1 for alpha/build-*; model (default) is not allowed")
    );
}

fn model_department(id: &str, roster: &str, rule: &str) -> Department {
    parse_department(
        id,
        &format!("{MINIMAL}roster={roster}\n[hire]\nmode='enforce'\n[spend]\n{rule}\n"),
    )
    .unwrap()
}

fn model_role(model: Option<&str>) -> RoleSpec {
    let mut role = atm_org::parse_role("build-2", "+++\nharness='pi'\n+++\nFixture\n").unwrap();
    role.config.model = model.map(str::to_owned);
    role
}

fn model_refusal(departments: &[Department], role: &RoleSpec, state: &OrgState) -> Option<String> {
    let caller = "alpha/lead".parse().unwrap();
    let seat = "alpha/build-2".parse().unwrap();
    match hire_check(departments, Some(&caller), &seat, role, state) {
        HireCheck::NeedsApproval { reason } => Some(reason),
        HireCheck::Allowed => None,
        check => panic!("expected an enforcing department: {check:?}"),
    }
}

// Break caught: a roster admission and a model admission in different
// departments (or a report-only admission) authorize a named start.
#[test]
fn hire_requires_roster_and_model_in_one_department() {
    let state = OrgState::default();
    let role = model_role(Some("test-pro:max"));
    let roster = "['alpha/build-*']";
    let mut departments = vec![
        model_department("roster", roster, "models=['test-lite*']"),
        model_department("model", "[]", "models=['test-pro*']"),
        model_department("neither", "[]", "models=[]"),
    ];
    let mut report = model_department("report", roster, "models=['*']");
    report.hire_mode = HireMode::Report;
    departments.push(report);
    assert_eq!(
        model_refusal(&departments, &role, &state).as_deref(),
        Some(concat!(
            "department roster: model \"test-pro:max\" is not allowed; ",
            "department model: not on roster; ",
            "department neither: not on roster; model \"test-pro:max\" is not allowed"
        ))
    );
    for rule in ["models=['test-pro*']", "models=['test-pro:max']", ""] {
        departments.push(model_department("both", roster, rule));
        assert_eq!(model_refusal(&departments, &role, &state), None, "{rule}");
        departments.pop();
    }
}

// Break caught: confusing a known default with an absent model rule or letting
// an empty list or a merely similar (case, prefix without *) model through.
#[test]
fn hire_resolves_literal_default_and_empty_model_lists() {
    for (model, rule, expected) in [
        (None, "models=['(default)']", None),
        (None, "models=['*']", None),
        (None, "", None),
        (
            None,
            "models=[]",
            Some("department one: model (default) is not allowed"),
        ),
        (
            None,
            "models=['test-lite']",
            Some("department one: model (default) is not allowed"),
        ),
        (
            Some("Test-Lite"),
            "models=['test-lite*']",
            Some("department one: model \"Test-Lite\" is not allowed"),
        ),
        (
            Some("test-lite:max"),
            "models=['test-lite']",
            Some("department one: model \"test-lite:max\" is not allowed"),
        ),
    ] {
        let dept = model_department("one", "['alpha/build-2']", rule);
        assert_eq!(
            model_refusal(&[dept], &model_role(model), &OrgState::default()).as_deref(),
            expected,
            "{model:?} {rule}"
        );
    }
}

// Break caught: match is run on display text, or refusal leaks controls / a
// partial escape after cutting at the byte bound.
#[test]
fn hire_matches_raw_models_and_bounds_refusal_text() {
    let model = format!("test-\"\n\u{202e}{}é-hidden", "x".repeat(174));
    let role = model_role(Some(&model));
    let state = OrgState::default();
    let mut allowed = model_department("one", "['alpha/build-2']", "models=[]");
    let denied = allowed.clone();
    allowed.spend.as_mut().unwrap().models = Some(vec![model.parse().unwrap()]);
    assert_eq!(model_refusal(&[allowed], &role, &state), None);
    let reason = model_refusal(&[denied], &role, &state).unwrap();
    assert_eq!(
        reason,
        format!(
            "department one: model \"test-\\\"\\n\\u{{202e}}{}...\" is not allowed",
            "x".repeat(174)
        )
    );
}
