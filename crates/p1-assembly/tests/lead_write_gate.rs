//! Issue #1: a parent and its workers are assembled from ONE catalog, and that is
//! what makes their file mutations serialized — observations stay per agent, the
//! write gate is shared. A different catalog (another host) shares nothing.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use p1_assembly::{Catalog, ToolServices, ToolSpec, assemble};
use p1_contracts::Tool;
use p1_testkit::FakeTool;
use p1_workspace::Workspace;

fn recording_catalog(seen: Arc<Mutex<Vec<Workspace>>>) -> Catalog {
    let mut catalog = Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");
    catalog.tool(
        "write",
        Box::new(move |_spec: &ToolSpec, services: &ToolServices| {
            seen.lock().unwrap().push(services.workspace.clone());
            Ok(Arc::new(FakeTool::new("write")) as Arc<dyn Tool>)
        }),
    );
    catalog
}

#[test]
fn agents_of_one_catalog_share_the_write_gate_and_nothing_else_does() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let host = recording_catalog(seen.clone());
    let other_host = recording_catalog(seen.clone());
    let environment = environment_file("e", "test-provider", &["write"], "tools: {{tool_names}}");
    let parent_dir = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();

    assemble(&host, &environment, parent_dir.path(), &substitutions()).unwrap();
    assemble(&host, &environment, parent_dir.path(), &substitutions()).unwrap();
    // A child given its own directory is still serialized with its parent: cheap,
    // and right whenever the two directories overlap.
    assemble(&host, &environment, child_dir.path(), &substitutions()).unwrap();
    assemble(
        &other_host,
        &environment,
        parent_dir.path(),
        &substitutions(),
    )
    .unwrap();

    let seen = seen.lock().unwrap();
    let gate = |index: usize| seen[index].write_gate();
    assert!(gate(0).is_shared_with(gate(1)), "parent and worker");
    assert!(
        gate(0).is_shared_with(gate(2)),
        "worker in its own directory"
    );
    assert!(!gate(0).is_shared_with(gate(3)), "another catalog");
}
