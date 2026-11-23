use super::*;
use atm_org::{OrgCommand, OrgEvent};
use std::{collections::BTreeMap, future::Future, task::Poll, time::Duration};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

fn fixture(count: usize) -> (tempfile::TempDir, OrgState) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = OrgState::default();
    let now = "2026-01-15T12:00:00Z".parse().unwrap();
    let mut seq = 0;
    let mut apply = |event| {
        seq += 1;
        state.apply(seq, now, &event).unwrap();
    };
    for project in ["alpha", "beta", "gamma", "delta", "epsilon"]
        .into_iter()
        .take(count)
    {
        let root = dir.path().join(project);
        std::fs::create_dir_all(root.join(".atm/roles")).unwrap();
        apply(OrgEvent::ProjectRegistered {
            project_id: project.parse().unwrap(),
            root: root.to_str().unwrap().into(),
        });
    }
    for (id, pane, manager) in [
        ("alpha/one", "%1", "lead"),
        ("alpha/two", "%2", "lead"),
        ("alpha/lead", "%3", "gamma/chief"),
        ("gamma/chief", "%4", "owner"),
        ("beta/lone", "%5", "owner"),
    ] {
        apply(OrgEvent::AgentAdopted {
            agent_id: id.parse().unwrap(),
            pane: pane.into(),
            session_id: None,
        });
        set_manager(dir.path(), id, manager);
    }
    assert!(!dir.path().join("gamma/.git").exists());
    (dir, state)
}
fn role_path(root: &Path, id: &str) -> std::path::PathBuf {
    let (project, name) = id.split_once('/').unwrap();
    root.join(project).join(format!(".atm/roles/{name}.md"))
}
fn set_manager(root: &Path, id: &str, manager: &str) {
    std::fs::write(
        role_path(root, id),
        format!("+++\nharness='pi'\nmanager='{manager}'\n+++\nSECRET role body\n"),
    )
    .unwrap();
}
fn edge<'a>(edges: &'a ReportingEdges, id: &str) -> &'a Result<Address, String> {
    edges.edges.get(&id.parse().unwrap()).unwrap()
}

// Break caught: walking shared ancestors re-reads roles or caches them across calls.
#[tokio::test]
async fn bulk_edges_read_each_recorded_role_once() {
    for count in [3, 5] {
        let (dir, state) = fixture(count);
        let mut reads = BTreeMap::new();
        let first = read_edges(&state, |state, id| {
            *reads.entry(id.to_string()).or_insert(0) += 1;
            read_current_role(state, id)
        })
        .unwrap();
        assert_eq!(
            reads,
            BTreeMap::from([
                ("alpha/lead".into(), 1),
                ("alpha/one".into(), 1),
                ("alpha/two".into(), 1),
                ("beta/lone".into(), 1),
                ("gamma/chief".into(), 1),
            ])
        );
        let files = ReportingFiles::default();
        let current = files.edges(state.clone()).await.unwrap();
        assert_eq!(current.edges, first.edges);
        assert!(first.reason.is_none());
        assert_eq!(
            edge(&first, "alpha/one"),
            &Ok("alpha/lead".parse().unwrap())
        );
        assert_eq!(
            edge(&first, "alpha/lead"),
            &Ok("gamma/chief".parse().unwrap())
        );
        assert_eq!(edge(&first, "gamma/chief"), &Ok(Address::Owner));
        set_manager(dir.path(), "alpha/one", "owner");
        let next = files.edges(state.clone()).await.unwrap();
        assert_eq!(edge(&next, "alpha/one"), &Ok(Address::Owner));
        std::fs::remove_file(role_path(dir.path(), "alpha/two")).unwrap();
        std::fs::write(
            role_path(dir.path(), "beta/lone"),
            "+++\nharness='pi'\n+++\n",
        )
        .unwrap();
        let next = files.edges(state).await.unwrap();
        for id in ["alpha/two", "beta/lone"] {
            assert_eq!(edge(&next, id), &Ok(Address::Owner));
        }
        assert!(!next
            .current_models
            .contains_key(&"alpha/two".parse().unwrap()));
        assert_eq!(
            next.current_models
                .get(&"beta/lone".parse().unwrap())
                .map(String::as_str),
            Some("(default)")
        );
    }
}

// Break caught: a valid immediate edge survives a broken later link, or a
// broken branch hides independent seats; down intermediates must still link.
#[tokio::test]
async fn bulk_edges_validate_whole_chains() {
    let (dir, mut state) = fixture(5);
    state
        .apply(
            30,
            chrono::Utc::now(),
            &OrgEvent::AgentDown {
                agent_id: "alpha/lead".parse().unwrap(),
                reason: "Fixture".into(),
            },
        )
        .unwrap();
    let files = ReportingFiles::default();
    assert_eq!(
        edge(&files.edges(state.clone()).await.unwrap(), "alpha/one"),
        &Ok("alpha/lead".parse().unwrap())
    );
    for manager in ["alpha/one", "gamma/chief", "beta/missing", "../invalid"] {
        set_manager(dir.path(), "gamma/chief", manager);
        let result = files.edges(state.clone()).await.unwrap();
        assert!(result.reason.is_none());
        for id in ["alpha/one", "alpha/two", "alpha/lead", "gamma/chief"] {
            let error = edge(&result, id).as_ref().unwrap_err();
            assert!(error.contains("reporting line at"), "{error}");
            assert!(!error.contains("SECRET"));
        }
        assert_eq!(edge(&result, "beta/lone"), &Ok(Address::Owner));
    }
    std::fs::write(
        role_path(dir.path(), "gamma/chief"),
        "+++\nharness='not-a-harness'\n+++\nSECRET parse error\n",
    )
    .unwrap();
    let result = files.edges(state).await.unwrap();
    assert_eq!(
        edge(&result, "alpha/one"),
        &Err("reporting line at gamma/chief: invalid role configuration".into())
    );
}

// Break caught: unsafe/configuration files fail the whole graph, or an
// infrastructure failure is silently mislabeled as a harmless broken line.
#[tokio::test]
async fn bulk_edges_error_categories() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for case in [
        "invalid",
        "utf8",
        "large",
        "directory",
        "symlink",
        "permission",
    ] {
        let (dir, state) = fixture(3);
        let path = role_path(dir.path(), "gamma/chief");
        match case {
            "invalid" => std::fs::write(&path, "+++\nSECRET invalid TOML\n+++\n").unwrap(),
            "utf8" => std::fs::write(&path, [255]).unwrap(),
            "large" => std::fs::write(&path, vec![b'a'; 256 * 1024 + 1]).unwrap(),
            "directory" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            "symlink" => {
                let target = dir.path().join("private-role");
                std::fs::write(&target, "+++\nharness='pi'\n+++\nSECRET\n").unwrap();
                std::fs::remove_file(&path).unwrap();
                symlink(target, &path).unwrap();
            }
            "permission" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
                if std::fs::File::open(&path).is_ok() {
                    eprintln!("permission filesystem case unavailable to this user; injected EACCES below remains covered");
                    continue;
                }
            }
            _ => unreachable!(),
        }
        let result = ReportingFiles::default().edges(state).await.unwrap();
        let error = edge(&result, "alpha/one").as_ref().unwrap_err();
        assert!(error.contains("gamma/chief"), "{case}: {error}");
        assert!(!error.contains("SECRET"), "{case}: {error}");
        assert_eq!(edge(&result, "beta/lone"), &Ok(Address::Owner));
    }
    let (_, state) = fixture(3);
    for (code, expected) in [
        (libc::EACCES, "refused"),
        (libc::EIO, "internal"),
        (libc::EMFILE, "internal"),
    ] {
        let result = read_edges(&state, |_, id| {
            if id.to_string() == "gamma/chief" {
                Err(role_file_error(
                    Path::new("role"),
                    io::Error::from_raw_os_error(code),
                ))
            } else {
                Ok(None)
            }
        });
        if expected == "refused" {
            assert!(edge(&result.unwrap(), "gamma/chief").is_err());
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.0.code, expected);
            assert!(error.0.message.contains("gamma/chief"));
        }
    }
}

// Break caught: cancellation frees the lane while its bulk worker is alive;
// a busy/expired pass invents line errors or blocks ordinary committed writes.
#[tokio::test]
async fn bulk_edges_retain_shared_slot() {
    let (dir, state) = fixture(3);
    let files = ReportingFiles::default();
    let slot = files.slot.clone();
    let captured = state.clone();
    let (entered, entry) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let held = tokio::spawn(async move {
        detached_read(slot, Path::new("reporting roles"), move || {
            let mut gate = Some((entered, gate));
            Ok(read_edges(&captured, |state, id| {
                if let Some((entered, gate)) = gate.take() {
                    entered.send(()).unwrap();
                    gate.recv().unwrap();
                }
                read_current_role(state, id)
            }))
        })
        .await
    });
    entry.await.unwrap();
    held.abort();
    assert!(held.await.unwrap_err().is_cancelled());
    assert_eq!(files.slot.available_permits(), 0);
    let mut edges = Box::pin(files.edges(state.clone()));
    let mut ancestors = Box::pin(files.ancestors(state.clone(), "alpha/one".parse().unwrap()));
    std::future::poll_fn(|cx| {
        assert!(edges.as_mut().poll(cx).is_pending());
        assert!(ancestors.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let cancel = CancellationToken::new();
    let org = super::super::OrgHandle::open(dir.path().join("state"), cancel.clone()).await;
    org.execute(
        OrgCommand::RegisterProject {
            project_id: "delta".parse().unwrap(),
            root: dir.path().to_str().unwrap().into(),
            move_root: false,
        },
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(org.snapshot().await.unwrap().projects().count(), 1);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5)).await;
    let unavailable = edges.await.unwrap();
    assert!(unavailable.reason.is_some());
    assert!(unavailable.edges.is_empty());
    assert!(unavailable.current_models.is_empty());
    assert_eq!(ancestors.await.unwrap_err().0.code, "timeout");
    assert_eq!(files.slot.available_permits(), 0);
    tokio::time::resume();
    release.send(()).unwrap();
    drop(files.slot.acquire().await.unwrap());
    assert_eq!(
        edge(&files.edges(state).await.unwrap(), "alpha/one"),
        &Ok("alpha/lead".parse().unwrap())
    );
    files.slot.close();
    assert_eq!(
        files.edges(OrgState::default()).await.unwrap_err().0.code,
        "unavailable"
    );
    cancel.cancel();
}
