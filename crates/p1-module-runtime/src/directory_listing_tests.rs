use super::*;

#[tokio::test]
async fn ignore_globs_and_credential_refusals() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/generated")).unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    for name in [
        "src/lib.rs",
        "src/generated/schema.rs",
        "app.log",
        ".hidden",
        ".claude/.credentials.json",
    ] {
        std::fs::File::create(dir.path().join(name)).unwrap();
    }
    let credential = dir.path().join(".claude/.credentials.json");
    std::fs::hard_link(&credential, dir.path().join("alias")).unwrap();
    let workspace = Workspace::new(dir.path())
        .unwrap()
        .with_credential_paths(vec![credential.clone()]);
    let cap = DirectoryListingCapability::new(workspace, Some(dir.path().to_path_buf()));
    let request = || ListingRequest {
        path: ".".into(),
        depth: 3,
        limit: 500,
        ignore: vec!["*.log".into(), "**/generated".into()],
        continuation: None,
    };
    let page = cap
        .list_directory(request(), CancellationToken::new())
        .await
        .unwrap();
    let paths: Vec<_> = page.entries.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&".hidden"));
    assert!(paths.contains(&"src/lib.rs"));
    for forbidden in [
        "app.log",
        "src/generated",
        "src/generated/schema.rs",
        ".claude/.credentials.json",
        "alias",
    ] {
        assert!(!paths.contains(&forbidden), "{forbidden}: {paths:?}");
    }
    let mut direct = request();
    direct.path = ".claude/.credentials.json".into();
    assert!(matches!(
        cap.list_directory(direct, CancellationToken::new()).await,
        Err(FsError::Io(_))
    ));
    let mut invalid = request();
    invalid.ignore = vec!["[".into()];
    assert!(matches!(
        cap.list_directory(invalid, CancellationToken::new()).await,
        Err(FsError::InvalidPattern(_))
    ));
}
