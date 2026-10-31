use std::fs;

use super::*;

const LIMIT: usize = 4096;

fn name(value: &str) -> MemoryName {
    MemoryName::new(value).unwrap()
}

fn root() -> (tempfile::TempDir, MemoryRoot) {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    assert!(memory.create_defaults_if_absent().unwrap());
    (data, memory)
}

#[test]
fn the_overview_is_made_once_and_keeps_its_body_and_keywords() {
    let (_data, memory) = root();
    let lock = memory.lock().unwrap();
    let mut changes = memory.subscribe();
    assert!(memory.ensure_overview(&lock, &name("web"), LIMIT).unwrap());
    assert!(changes.has_changed().unwrap());
    changes.mark_unchanged();
    let path = memory.project_dir(&name("web")).join("overview.md");
    let made = crate::file::MemoryDocument::parse(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        made.description(),
        "Overview of the `web` project, loaded in full into its sessions"
    );
    assert_eq!(made.body(), "");
    // A new overview is stamped as created.
    assert!(made.created().is_some() && made.created() == made.modified());
    // Nothing to do writes and signals nothing.
    assert!(!memory.ensure_overview(&lock, &name("web"), LIMIT).unwrap());
    assert!(!changes.has_changed().unwrap());

    // Another description is replaced; the keywords and the body stay.
    fs::write(
        &path,
        "---\ndescription: Mine\nkeywords: [shop]\n---\n\nA shop.\n",
    )
    .unwrap();
    assert!(memory.ensure_overview(&lock, &name("web"), LIMIT).unwrap());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "---\ndescription: Overview of the `web` project, loaded in full into its sessions\nkeywords: [shop]\n---\n\nA shop.\n"
    );

    // An invalid file is left as it is.
    fs::write(&path, "no frontmatter\n").unwrap();
    assert!(!memory.ensure_overview(&lock, &name("web"), LIMIT).unwrap());
    assert_eq!(fs::read_to_string(&path).unwrap(), "no frontmatter\n");
}

#[test]
fn nothing_is_made_while_the_memory_folder_does_not_exist() {
    let data = tempfile::tempdir().unwrap();
    let memory = MemoryRoot::new(data.path());
    let lock = memory.lock().unwrap();
    assert!(!memory.ensure_overview(&lock, &name("web"), LIMIT).unwrap());
    assert!(!data.path().join("memory").exists());
}

#[test]
fn project_folders_move_and_go_whole() {
    let (_data, memory) = root();
    let lock = memory.lock().unwrap();
    let web = memory.project_dir(&name("web"));
    fs::create_dir_all(&web).unwrap();
    fs::write(web.join("a.md"), "x").unwrap();
    fs::write(web.join("notes.txt"), "x").unwrap();
    fs::create_dir_all(memory.project_dir(&name("taken"))).unwrap();
    assert_eq!(
        memory.project_folders().unwrap(),
        [name("taken"), name("web")]
    );
    assert_eq!(memory.project_memory_count(&name("web")), 1);
    assert!(matches!(
        memory.rename_project_folder(&lock, &name("web"), &name("taken")),
        Err(MemoryError::Exists(_))
    ));
    let moves = memory
        .rename_project_folder(&lock, &name("web"), &name("shop"))
        .unwrap();
    assert!(memory.project_dir(&name("shop")).join("a.md").is_file());
    moves.undo();
    assert!(web.join("a.md").is_file());
    // A project without a folder moves nothing.
    let moves = memory
        .rename_project_folder(&lock, &name("none"), &name("other"))
        .unwrap();
    assert!(moves.renamed.is_empty());

    assert_eq!(
        memory.remove_project_folder(&lock, &name("web")).unwrap(),
        1
    );
    assert!(!web.exists());
    assert_eq!(
        memory.remove_project_folder(&lock, &name("web")).unwrap(),
        0
    );
}
