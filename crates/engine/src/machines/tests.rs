use std::fs;

use cookie_agent_protocol::{
    MachineAssignment, MachineName, RuntimeId, runtime_protocol::RuntimeMachine,
};

use super::{
    Assignment, MachineError, MachineRegistry, ObservedRuntime, REGISTRY_FILE, RecordKind,
    host_machine_name,
};

fn name(value: &str) -> MachineName {
    MachineName::new(value).unwrap()
}

fn runtime(value: &str) -> RuntimeId {
    RuntimeId::new(value).unwrap()
}

fn reported(id: &str, host: &str) -> RuntimeMachine {
    RuntimeMachine {
        id: id.to_owned(),
        host_name: Some(host.to_owned()),
    }
}

fn assigned(machine: &str, by: MachineAssignment) -> Assignment {
    Assignment {
        machine: name(machine),
        by,
    }
}

fn names(registry: &MachineRegistry) -> Vec<String> {
    registry
        .machines()
        .into_iter()
        .map(|machine| machine.name.to_string())
        .collect()
}

fn memory_file(registry: &MachineRegistry, machine: &str, file: &str) -> std::path::PathBuf {
    let path = registry.memory().machine_dir(&name(machine)).join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "---\ndescription: x\n---\n").unwrap();
    path
}

#[test]
fn host_names_become_machine_names() {
    assert_eq!(host_machine_name("labpc"), "labpc");
    assert_eq!(host_machine_name("LabPC.example.com"), "labpc");
    assert_eq!(host_machine_name("my laptop (2)"), "my-laptop-2");
    assert_eq!(host_machine_name("lab_pc-1"), "lab_pc-1");
    assert_eq!(host_machine_name("--edge--"), "edge");
    assert_eq!(host_machine_name("7box"), "host-7box");
    assert_eq!(host_machine_name(""), "machine");
    assert_eq!(host_machine_name("...."), "machine");
    assert_eq!(host_machine_name(&"a".repeat(40)), "a".repeat(32));
    assert_eq!(
        host_machine_name(&format!("{}-b", "a".repeat(31))),
        "a".repeat(31)
    );
}

#[test]
fn a_runtime_is_assigned_by_pin_then_machine_id_then_a_new_machine() {
    let data = tempfile::tempdir().unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    // Rule 3: an unknown machine ID gets a new machine named after the host.
    assert_eq!(
        registry
            .resolve(&runtime("local"), Some(&reported("id-laptop", "laptop")))
            .unwrap(),
        assigned("laptop", MachineAssignment::MachineId)
    );
    // Rule 2: another runtime reporting the same ID joins it.
    assert_eq!(
        registry
            .resolve(&runtime("global"), Some(&reported("id-laptop", "renamed")))
            .unwrap(),
        assigned("laptop", MachineAssignment::MachineId)
    );
    // Rule 4: no ID pins the runtime to a new machine named after it.
    assert_eq!(
        registry.resolve(&runtime("thirdparty"), None).unwrap(),
        assigned("thirdparty", MachineAssignment::Pinned)
    );
    // Rule 1: a pin wins over the reported ID.
    registry
        .assign(&runtime("local"), &name("thirdparty"))
        .unwrap();
    assert_eq!(
        registry
            .resolve(&runtime("local"), Some(&reported("id-laptop", "laptop")))
            .unwrap(),
        assigned("thirdparty", MachineAssignment::Pinned)
    );
    assert_eq!(names(&registry), ["laptop", "thirdparty"]);

    // New machines were written at once and survive a restart.
    let reopened = MachineRegistry::open(data.path()).unwrap();
    assert_eq!(reopened.machines(), registry.machines());
    assert_eq!(
        reopened.lookup(&runtime("global"), Some(&reported("id-laptop", "x"))),
        Some(assigned("laptop", MachineAssignment::MachineId))
    );
    assert_eq!(reopened.lookup(&runtime("other"), None), None);
}

#[test]
fn a_clashing_name_gets_the_next_free_suffix() {
    let data = tempfile::tempdir().unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    for (id, expected) in [("a", "box"), ("b", "box-2"), ("c", "box-3")] {
        assert_eq!(
            registry
                .resolve(&runtime(&format!("r{id}")), Some(&reported(id, "box")))
                .unwrap()
                .machine,
            name(expected)
        );
    }
    // A runtime ID that names a machine already gets a suffix too.
    registry.resolve(&runtime("labpc"), None).unwrap();
    assert_eq!(
        registry
            .resolve(&runtime("labpc_2"), Some(&reported("d", "labpc")))
            .unwrap()
            .machine,
        name("labpc-2")
    );
    // A long name is shortened to keep its suffix within 32 bytes.
    let long = "h".repeat(32);
    registry
        .resolve(&runtime("x1"), Some(&reported("e", &long)))
        .unwrap();
    assert_eq!(
        registry
            .resolve(&runtime("x2"), Some(&reported("f", &long)))
            .unwrap()
            .machine,
        name(&format!("{}-2", "h".repeat(30)))
    );
}

#[test]
fn renaming_moves_the_memory_folder_and_refuses_a_taken_name() {
    let data = tempfile::tempdir().unwrap();
    fs::create_dir(data.path().join("memory")).unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    registry
        .resolve(&runtime("a"), Some(&reported("id-a", "laptop")))
        .unwrap();
    registry
        .resolve(&runtime("b"), Some(&reported("id-b", "labpc")))
        .unwrap();
    memory_file(&registry, "laptop", "gpu.md");
    let mut changes = registry.subscribe();
    changes.mark_unchanged();

    assert!(matches!(
        registry.rename(&name("laptop"), &name("labpc")),
        Err(MachineError::NameTaken(taken)) if taken == name("labpc")
    ));
    assert!(matches!(
        registry.rename(&name("nope"), &name("other")),
        Err(MachineError::NotFound(_))
    ));
    // A memory folder that belongs to no machine takes the name too.
    memory_file(&registry, "stray", "x.md");
    assert!(matches!(
        registry.rename(&name("laptop"), &name("stray")),
        Err(MachineError::NameTaken(_))
    ));
    assert!(!changes.has_changed().unwrap());
    assert_eq!(names(&registry), ["labpc", "laptop"]);

    registry.rename(&name("laptop"), &name("desk")).unwrap();
    assert!(changes.has_changed().unwrap());
    assert_eq!(names(&registry), ["desk", "labpc"]);
    assert!(
        registry
            .memory()
            .machine_dir(&name("desk"))
            .join("gpu.md")
            .is_file()
    );
    assert!(!registry.memory().machine_dir(&name("laptop")).exists());
    // The runtime reporting the renamed machine's ID follows it.
    assert_eq!(
        registry.lookup(&runtime("a"), Some(&reported("id-a", "laptop"))),
        Some(assigned("desk", MachineAssignment::MachineId))
    );
}

#[cfg(unix)]
#[test]
fn a_failed_registry_write_puts_the_folder_back() {
    use std::os::unix::fs::PermissionsExt;

    let data = tempfile::tempdir().unwrap();
    fs::create_dir(data.path().join("memory")).unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    registry
        .resolve(&runtime("a"), Some(&reported("id-a", "laptop")))
        .unwrap();
    memory_file(&registry, "laptop", "gpu.md");
    // A data root that takes no new files fails the registry write after
    // the folder moved.
    fs::set_permissions(data.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let writable = fs::File::create(data.path().join("probe")).is_ok();
    let renamed = registry.rename(&name("laptop"), &name("desk"));
    fs::set_permissions(data.path(), fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        // Permissions do not bind this user (root); nothing to check.
        return;
    }
    assert!(
        matches!(renamed, Err(MachineError::Store(_))),
        "{renamed:?}"
    );
    assert!(
        registry
            .memory()
            .machine_dir(&name("laptop"))
            .join("gpu.md")
            .is_file()
    );
    assert!(!registry.memory().machine_dir(&name("desk")).exists());
    assert_eq!(names(&registry), ["laptop"]);
}

#[test]
fn assigning_moves_a_pin_and_creates_a_machine_for_a_new_name() {
    let data = tempfile::tempdir().unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    registry
        .assign(&runtime("box"), &name("container"))
        .unwrap();
    assert_eq!(
        registry.lookup(&runtime("box"), Some(&reported("x", "y"))),
        Some(assigned("container", MachineAssignment::Pinned))
    );
    registry.assign(&runtime("box"), &name("labpc")).unwrap();
    let machines = registry.machines();
    assert_eq!(machines[0].name, name("container"));
    assert!(machines[0].runtimes.is_empty());
    assert_eq!(machines[1].runtimes, [runtime("box")]);

    registry.unassign(&runtime("box")).unwrap();
    assert!(matches!(
        registry.unassign(&runtime("box")),
        Err(MachineError::NotPinned(_))
    ));
    // Unpinned, it falls back to the machine ID it reports.
    assert_eq!(
        registry.lookup(&runtime("box"), Some(&reported("x", "y"))),
        None
    );
}

#[test]
fn merging_moves_ids_pins_and_memories_unless_a_memory_name_is_in_both() {
    let data = tempfile::tempdir().unwrap();
    fs::create_dir(data.path().join("memory")).unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    registry
        .resolve(&runtime("box"), Some(&reported("id-box", "container")))
        .unwrap();
    registry
        .assign(&runtime("tool"), &name("container"))
        .unwrap();
    registry
        .resolve(&runtime("labpc"), Some(&reported("id-labpc", "labpc")))
        .unwrap();
    memory_file(&registry, "container", "gpu.md");
    memory_file(&registry, "container", "scratch.md");
    let clash = memory_file(&registry, "labpc", "gpu.md");

    assert!(matches!(
        registry.merge(&name("labpc"), &name("labpc")),
        Err(MachineError::MergeSelf(_))
    ));
    assert!(matches!(
        registry.merge(&name("container"), &name("nope")),
        Err(MachineError::NotFound(_))
    ));
    assert!(matches!(
        registry.merge(&name("container"), &name("labpc")),
        Err(MachineError::MemoryConflict { name: file, .. }) if file == "gpu.md"
    ));
    assert_eq!(names(&registry), ["container", "labpc"]);
    assert_eq!(
        registry.memory().machine_memory_count(&name("container")),
        2
    );

    fs::remove_file(clash).unwrap();
    registry.merge(&name("container"), &name("labpc")).unwrap();
    assert_eq!(names(&registry), ["labpc"]);
    let labpc = &registry.machines()[0];
    assert_eq!(labpc.ids, ["id-labpc", "id-box"]);
    assert_eq!(labpc.runtimes, [runtime("tool")]);
    assert_eq!(registry.memory().machine_memory_count(&name("labpc")), 2);
    assert!(!registry.memory().machine_dir(&name("container")).exists());
    assert_eq!(
        registry.lookup(&runtime("box"), Some(&reported("id-box", "container"))),
        Some(assigned("labpc", MachineAssignment::MachineId))
    );
}

#[test]
fn forgetting_refuses_a_machine_with_connected_runtimes_or_memories() {
    let data = tempfile::tempdir().unwrap();
    fs::create_dir(data.path().join("memory")).unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    let laptop = reported("id-laptop", "laptop");
    registry.resolve(&runtime("local"), Some(&laptop)).unwrap();
    let connected = [(runtime("local"), Some(laptop))];
    assert!(matches!(
        registry.forget(&name("laptop"), &connected),
        Err(MachineError::InUse(_))
    ));
    let memory = memory_file(&registry, "laptop", "gpu.md");
    assert!(matches!(
        registry.forget(&name("laptop"), &[]),
        Err(MachineError::InUse(_))
    ));
    fs::remove_file(memory).unwrap();
    registry.forget(&name("laptop"), &[]).unwrap();
    assert!(names(&registry).is_empty());
    assert!(!registry.memory().machine_dir(&name("laptop")).exists());
    assert!(matches!(
        registry.forget(&name("laptop"), &[]),
        Err(MachineError::NotFound(_))
    ));
}

#[test]
fn a_change_rereads_what_another_daemon_wrote() {
    let data = tempfile::tempdir().unwrap();
    let first = MachineRegistry::open(data.path()).unwrap();
    let second = MachineRegistry::open(data.path()).unwrap();
    first
        .resolve(&runtime("a"), Some(&reported("id-a", "laptop")))
        .unwrap();
    second
        .resolve(&runtime("b"), Some(&reported("id-b", "labpc")))
        .unwrap();
    assert_eq!(names(&second), ["labpc", "laptop"]);
    first.rename(&name("labpc"), &name("lab")).unwrap();
    assert_eq!(names(&first), ["lab", "laptop"]);
}

#[test]
fn the_registry_file_is_strict() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(REGISTRY_FILE);
    let open = |text: &str| {
        fs::write(&path, text).unwrap();
        MachineRegistry::open(data.path()).map(|registry| registry.machines())
    };
    assert_eq!(
        open(r#"{"version":1,"machines":[{"name":"labpc","ids":["x"],"runtimes":["labpc"]}]}"#)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(open(r#"{"version":2,"machines":[]}"#).unwrap().len(), 0);
    for broken in [
        r#"{"version":3,"machines":[]}"#,
        r#"{"version":1,"machines":[],"remembered":[{"runtime":"labpc","kind":"spawned","last_seen":"2026-10-21T00:00:00Z"}]}"#,
        r#"{"version":2,"machines":[],"remembered":[{"runtime":"labpc","kind":"spawned","last_seen":"2026-10-21T00:00:00Z"},{"runtime":"labpc","kind":"dial_in","last_seen":"2026-10-21T00:00:00Z"}]}"#,
        r#"{"version":2,"machines":[],"remembered":[{"runtime":"labpc","kind":"spawned","machine_id":"","last_seen":"2026-10-21T00:00:00Z"}]}"#,
        r#"{"version":2,"machines":[],"remembered":[{"runtime":"labpc","kind":"builtin","last_seen":"2026-10-21T00:00:00Z"}]}"#,
        r#"{"version":2,"machines":[],"remembered":[{"runtime":"labpc","kind":"spawned","last_seen":"2026-10-21T00:00:00Z","extra":1}]}"#,
        r#"{"version":1,"machines":[],"extra":true}"#,
        r#"{"version":1,"machines":[{"name":"Lab","ids":[],"runtimes":[]}]}"#,
        r#"{"version":1,"machines":[{"name":"a","ids":["x"],"runtimes":[]},{"name":"b","ids":["x"],"runtimes":[]}]}"#,
        r#"{"version":1,"machines":[{"name":"a","ids":[],"runtimes":["r"]},{"name":"b","ids":[],"runtimes":["r"]}]}"#,
        r#"{"version":1,"machines":[{"name":"a","ids":[],"runtimes":[]},{"name":"a","ids":[],"runtimes":[]}]}"#,
        r#"{"version":1,"machines":[{"name":"a","ids":[""],"runtimes":[]}]}"#,
    ] {
        assert!(
            matches!(open(broken), Err(MachineError::Store(_))),
            "{broken}"
        );
    }
}

fn observed(id: &str, connected: bool, reported: Option<RuntimeMachine>) -> ObservedRuntime {
    ObservedRuntime {
        runtime: runtime(id),
        kind: RecordKind::Spawned,
        connected,
        reported,
        platform: Some("linux".into()),
        home: Some("/home/u".into()),
    }
}

fn at(seconds: i64) -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_800_000_000 + seconds).unwrap()
}

#[test]
fn an_older_registry_reads_with_no_records_and_is_written_at_the_new_version() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(REGISTRY_FILE);
    fs::write(
        &path,
        r#"{"version":1,"machines":[{"name":"labpc","ids":["id-labpc"],"runtimes":[]}]}"#,
    )
    .unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    assert!(registry.registry().records.is_empty());
    registry
        .observe(
            &[observed("labpc", true, Some(reported("id-labpc", "labpc")))],
            at(0),
        )
        .unwrap();
    let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["version"], 2);
    assert_eq!(
        written["remembered"],
        serde_json::json!([{
            "runtime": "labpc",
            "kind": "spawned",
            "machine_id": "id-labpc",
            "platform": "linux",
            "home": "/home/u",
            "last_seen": "2027-01-15T08:00:00Z",
        }])
    );
}

#[test]
fn an_offline_runtime_stays_on_the_machine_it_was_last_seen_on() {
    let data = tempfile::tempdir().unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    let labpc = reported("id-labpc", "labpc");
    registry.resolve(&runtime("labpc"), Some(&labpc)).unwrap();
    // Never seen: no machine while it is offline.
    assert_eq!(registry.lookup_offline(&runtime("labpc"), None), None);
    registry
        .observe(&[observed("labpc", true, Some(labpc.clone()))], at(0))
        .unwrap();
    // A second look while it stays connected writes nothing new.
    registry
        .observe(&[observed("labpc", true, Some(labpc.clone()))], at(5))
        .unwrap();
    assert_eq!(registry.record(&runtime("labpc")).unwrap().last_seen, at(0));
    // Going offline stamps when it was last seen.
    registry
        .observe(&[observed("labpc", false, None)], at(10))
        .unwrap();
    assert_eq!(
        registry.record(&runtime("labpc")).unwrap().last_seen,
        at(10)
    );
    // Across a restart the record places it, as `last_seen`.
    let reopened = MachineRegistry::open(data.path()).unwrap();
    assert_eq!(
        reopened.lookup_offline(&runtime("labpc"), None),
        Some(assigned("labpc", MachineAssignment::LastSeen))
    );
    // A pin comes first.
    reopened.assign(&runtime("labpc"), &name("lab")).unwrap();
    assert_eq!(
        reopened.lookup_offline(&runtime("labpc"), None),
        Some(assigned("lab", MachineAssignment::Pinned))
    );
    // Reconnecting with another report rewrites the record.
    reopened
        .observe(
            &[ObservedRuntime {
                platform: Some("macos".into()),
                ..observed("labpc", true, Some(labpc))
            }],
            at(20),
        )
        .unwrap();
    let record = reopened.record(&runtime("labpc")).unwrap();
    assert_eq!(
        (record.platform.as_deref(), record.last_seen),
        (Some("macos"), at(20))
    );
}

#[test]
fn renaming_and_merging_move_remembered_runtimes() {
    let data = tempfile::tempdir().unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    let boxed = reported("id-box", "container");
    registry.resolve(&runtime("box"), Some(&boxed)).unwrap();
    registry
        .resolve(&runtime("labpc"), Some(&reported("id-labpc", "labpc")))
        .unwrap();
    registry
        .observe(&[observed("box", true, Some(boxed))], at(0))
        .unwrap();
    registry
        .observe(&[observed("box", false, None)], at(1))
        .unwrap();
    registry.rename(&name("container"), &name("cage")).unwrap();
    assert_eq!(
        registry.lookup_offline(&runtime("box"), None),
        Some(assigned("cage", MachineAssignment::LastSeen))
    );
    registry.merge(&name("cage"), &name("labpc")).unwrap();
    assert_eq!(
        registry.lookup_offline(&runtime("box"), None),
        Some(assigned("labpc", MachineAssignment::LastSeen))
    );
}

#[test]
fn forgetting_a_machine_drops_the_runtimes_it_remembers() {
    let data = tempfile::tempdir().unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    let labpc = reported("id-labpc", "labpc");
    registry.resolve(&runtime("labpc"), Some(&labpc)).unwrap();
    registry
        .observe(&[observed("labpc", true, Some(labpc))], at(0))
        .unwrap();
    registry
        .observe(&[observed("labpc", false, None)], at(1))
        .unwrap();
    // An offline runtime does not keep the machine.
    registry.forget(&name("labpc"), &[]).unwrap();
    assert!(names(&registry).is_empty());
    assert_eq!(registry.record(&runtime("labpc")), None);
}

#[test]
fn forgetting_a_runtime_drops_only_an_offline_runtime_s_record() {
    let data = tempfile::tempdir().unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    let labpc = reported("id-labpc", "labpc");
    registry.resolve(&runtime("labpc"), Some(&labpc)).unwrap();
    registry
        .observe(&[observed("labpc", true, Some(labpc))], at(0))
        .unwrap();
    assert!(matches!(
        registry.forget_runtime(&runtime("labpc"), true),
        Err(MachineError::StillConnected(_))
    ));
    registry.forget_runtime(&runtime("labpc"), false).unwrap();
    assert_eq!(registry.lookup_offline(&runtime("labpc"), None), None);
    assert!(matches!(
        registry.forget_runtime(&runtime("labpc"), false),
        Err(MachineError::NotRemembered(_))
    ));
    // The machine stays.
    assert_eq!(names(&registry), ["labpc"]);
}

#[test]
fn dropping_records_keeps_the_others() {
    let data = tempfile::tempdir().unwrap();
    let registry = MachineRegistry::open(data.path()).unwrap();
    registry
        .observe(
            &[
                observed("ssh", true, None),
                ObservedRuntime {
                    kind: RecordKind::DialIn,
                    ..observed("roamer", true, None)
                },
            ],
            at(0),
        )
        .unwrap();
    registry
        .drop_records(|record| record.kind == RecordKind::Spawned)
        .unwrap();
    let kept = registry
        .registry()
        .records
        .into_iter()
        .map(|record| record.runtime.to_string())
        .collect::<Vec<_>>();
    assert_eq!(kept, ["roamer"]);
}
