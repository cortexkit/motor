use super::*;
use cortexkit_test_support::ScratchDir;
use motor_protocol::SiblingSnapshot;

fn root() -> CallerRoot {
    CallerRoot::parse("/home=/host/home").unwrap()
}
fn sibling(path: &str) -> SiblingSnapshot {
    SiblingSnapshot {
        canonical_path: path.into(),
        commit: "a".repeat(40),
        tree_hash: "b".repeat(64),
        base_commit: None,
    }
}
fn workspace(mapped: bool) -> Workspace {
    Workspace {
        repository: "/Users/u/repo".into(),
        siblings: if mapped {
            vec![sibling("/home/u/lib")]
        } else {
            vec![sibling("/Users/u/lib")]
        },
        ..Workspace::default()
    }
}
fn save(state: &Path, catalog: &Catalog) {
    Split::new(catalog, &[root()])
        .save(state, false, |_, _| Ok(()))
        .unwrap();
}

#[test]
fn every_mapped_collection_uses_record_and_mappings_and_round_trips() {
    let scratch = ScratchDir::new("caller-catalog-shapes");
    let mut catalog = Catalog::default();
    catalog
        .workspaces
        .insert("/home/u/proj".into(), workspace(false));
    catalog
        .workspaces
        .insert("/Users/u/mixed".into(), workspace(true));
    catalog
        .workspaces
        .insert("/Users/u/plain".into(), workspace(false));
    catalog.repositories.insert(
        "/Users/u/repo".into(),
        Repository {
            desired: Some(BaseRequest {
                tip: "tip".into(),
                workspace: "/Users/u/mixed".into(),
                siblings: vec![sibling("/home/u/lib")],
                git_dependencies: vec![],
                command: None,
            }),
            ..Repository::default()
        },
    );
    catalog.legacy_checkouts.insert(
        "/home/u/old".into(),
        LegacyCheckout {
            repository: "/Users/u/repo".into(),
            tip: None,
        },
    );
    catalog
        .sibling_commits
        .insert("/home/u/lib".into(), "commit".into());
    let id = Uuid::from_u128(1);
    catalog.transferred.insert(
        id,
        RetainedTransfer {
            repository: "/Users/u/repo".into(),
            snapshot: "snapshot".into(),
            tip: "tip".into(),
            siblings: vec![sibling("/home/u/lib")],
            finished_ms: 3,
        },
    );
    catalog.mirror_used_ms.insert("mirror".into(), 7);
    save(&scratch, &catalog);
    let raw: serde_json::Value = storage::read_json(&scratch.join("caller-catalog.json")).unwrap();
    assert_eq!(raw.as_object().unwrap().len(), 5);
    for collection in raw.as_object().unwrap().values() {
        assert!(!collection.as_object().unwrap().is_empty());
        for entry in collection.as_object().unwrap().values() {
            assert_eq!(entry.as_object().unwrap().len(), 2);
            assert_eq!(
                entry["mappings"],
                serde_json::json!([{"caller_prefix":"/home", "host_root":"/host/home"}])
            );
            assert!(entry.get("record").is_some());
        }
    }
    assert_eq!(raw["sibling_commits"]["/home/u/lib"]["record"], "commit");
    assert!(raw["transferred"].get(id.to_string()).is_some());
    let disk = Split::read(&scratch).unwrap();
    assert_eq!(disk.identity.workspaces.len(), 1);
    assert!(disk.identity.workspaces.contains_key("/Users/u/plain"));
    assert_eq!(disk.identity.mirror_used_ms["mirror"], 7);
    assert_eq!(
        serde_json::to_vec(&disk.loaded()).unwrap(),
        serde_json::to_vec(&catalog).unwrap()
    );
    assert_eq!(disk.caller.pins().len(), 6);
    fixed(&[root()], &disk.caller.pins()).unwrap();
}

#[test]
fn identity_catalog_bytes_and_pre_campaign_records_are_unchanged() {
    let scratch = ScratchDir::new("caller-catalog-identity");
    let mut catalog = Catalog::default();
    catalog
        .workspaces
        .insert("/Users/u/proj".into(), workspace(false));
    save(&scratch, &catalog);
    assert_eq!(
        fs::read(scratch.join("catalog.json")).unwrap(),
        serde_json::to_vec(&catalog).unwrap()
    );
    assert!(!scratch.join("caller-catalog.json").exists());
    assert!(Split::read(&scratch).unwrap().caller.pins().is_empty());
}

#[test]
fn one_file_insertions_have_one_atomic_write_and_deletions_are_not_restored() {
    for mapped in [false, true] {
        let scratch = ScratchDir::new("caller-catalog-one-file");
        let mut catalog = Catalog::default();
        save(&scratch, &catalog);
        catalog
            .workspaces
            .insert("/Users/u/proj".into(), workspace(mapped));
        let before = Split::read(&scratch).unwrap().loaded();
        assert!(before.workspaces.is_empty());
        let mut writes = 0;
        Split::new(&catalog, &[root()])
            .save(&scratch, false, |_, last| {
                writes += 1;
                assert!(last);
                Ok(())
            })
            .unwrap();
        assert_eq!(writes, 1);
        assert_eq!(Split::read(&scratch).unwrap().loaded().workspaces.len(), 1);
        let tree = scratch.join("tree");
        fs::create_dir(&tree).unwrap();
        fs::rename(&tree, scratch.join("trash")).unwrap();
        // A crash after detaching the tree still leaves the old record.
        assert!(Split::read(&scratch)
            .unwrap()
            .loaded()
            .workspaces
            .contains_key("/Users/u/proj"));
        catalog.workspaces.clear();
        save(&scratch, &catalog);
        assert!(Split::read(&scratch)
            .unwrap()
            .loaded()
            .workspaces
            .is_empty());
    }
}

#[test]
fn crashes_after_each_phase_keep_old_keys_and_identity_wins_dual_keys() {
    // Case 0 adds one record to each file; cases 1 and 2 move a record to the
    // caller and identity files respectively; case 3 moves one in each direction.
    for case in 0..4 {
        for crash_after in 1..=4 {
            let scratch = ScratchDir::new("caller-catalog-crash");
            let mut before = Catalog::default();
            before
                .workspaces
                .insert("/Users/u/a".into(), workspace(case == 2 || case == 3));
            before
                .workspaces
                .insert("/Users/u/b".into(), workspace(false));
            for record in before.workspaces.values_mut() {
                record.sibling_freshness.insert(
                    "/Users/u/lib".into(),
                    serde_json::from_value(serde_json::json!([{"tree":"old"}])).unwrap(),
                );
            }
            save(&scratch, &before);
            let mut after = before.clone();
            match case {
                0 => {
                    after
                        .workspaces
                        .insert("/home/u/new".into(), workspace(false));
                    after
                        .repositories
                        .insert("/Users/u/new-repo".into(), Repository::default());
                }
                1 => {
                    after
                        .workspaces
                        .insert("/Users/u/a".into(), workspace(true));
                }
                2 => {
                    after
                        .workspaces
                        .insert("/Users/u/a".into(), workspace(false));
                }
                3 => {
                    after
                        .workspaces
                        .insert("/Users/u/a".into(), workspace(false));
                    after
                        .workspaces
                        .insert("/Users/u/b".into(), workspace(true));
                }
                _ => unreachable!(),
            }
            let mut writes = 0;
            let result = Split::new(&after, &[root()]).save(&scratch, false, |_, _| {
                writes += 1;
                if writes == crash_after {
                    Err(io::Error::other("simulated process death after rename"))
                } else {
                    Ok(())
                }
            });
            if crash_after <= writes {
                assert!(result.is_err());
            }
            let disk = Split::read(&scratch).unwrap();
            let loaded = disk.loaded();
            for key in before.workspaces.keys() {
                assert!(loaded.workspaces.contains_key(key));
            }
            assert!(Split::new(&disk.identity, &[root()]).caller.is_empty());
            for key in disk
                .identity
                .workspaces
                .keys()
                .filter(|key| disk.caller.workspaces.contains_key(*key))
            {
                assert_eq!(
                    loaded.workspaces[key].siblings,
                    disk.identity.workspaces[key].siblings
                );
                assert!(loaded.workspaces[key].sibling_freshness.is_empty());
            }
            if case == 0 && crash_after == 1 {
                assert!(!loaded.workspaces.contains_key("/home/u/new"));
                assert!(loaded.repositories.contains_key("/Users/u/new-repo"));
            }
            // A subsequent setup/save can record any unpublished new key.
            save(&scratch, &after);
            assert_eq!(
                serde_json::to_vec(&Split::read(&scratch).unwrap().loaded()).unwrap(),
                serde_json::to_vec(&after).unwrap()
            );
        }
    }
}

#[test]
fn fixed_mapping_refusals_name_prefix_and_both_remedies() {
    for configured in [
        vec![],
        vec![CallerRoot::parse("/home=/other").unwrap()],
        vec![CallerRoot::parse("/data=/host/home").unwrap()],
    ] {
        let error = fixed(&configured, &[root()]).unwrap_err().to_string();
        assert!(
            error.contains("/home")
                && error.contains("restore --caller-root")
                && error.contains("motor-runner forget-caller-root"),
            "{error}"
        );
    }
    for spelling in ["/host//home", "/host/./home", "/host/home/"] {
        let mut recorded = root();
        recorded.host_root = spelling.into();
        assert!(fixed(&[root()], &[recorded]).is_err());
    }
}
