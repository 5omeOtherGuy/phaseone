//! `[[mcp_servers]]` (ADR-0181): checked with the rest of the environment file, read on its own
//! by `load_mcp_servers`, and an alias uses the servers of the environment it names.

mod common;

use common::*;
use p1_assembly::{AssemblyError, McpServerTransport, load_environment, load_mcp_servers};

const BASE: &str = "family = \"test\"\nprovider = \"test-provider\"\nmodel = \"m\"\n";
const SERVER: &str = "\n[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\n";

#[test]
fn an_environment_without_the_table_names_no_server() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(dir.path(), "plain", BASE, "hi");
    assert!(load_environment("plain", &[dir.path().to_path_buf()]).is_ok());
    assert!(
        load_mcp_servers("plain", &[dir.path().to_path_buf()])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_table_loads_with_the_environment_and_reads_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(dir.path(), "tools", &format!("{BASE}{SERVER}"), "hi");
    assert!(load_environment("tools", &[dir.path().to_path_buf()]).is_ok());
    let servers = load_mcp_servers("tools", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "docs");
    assert!(matches!(
        &servers[0].transport,
        McpServerTransport::Stdio { command, .. } if command == "docs-mcp"
    ));
}

#[test]
fn an_invalid_entry_fails_the_environment_load() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(
        dir.path(),
        "bad",
        &format!("{BASE}\n[[mcp_servers]]\nname = \"docs\"\n"),
        "hi",
    );
    let error = load_environment("bad", &[dir.path().to_path_buf()]).unwrap_err();
    assert!(
        matches!(&error, AssemblyError::InvalidEnvironmentFile { message, .. }
            if message.contains("exactly one of `command` and `url`")),
        "{error:?}"
    );
    assert!(load_mcp_servers("bad", &[dir.path().to_path_buf()]).is_err());
}

#[test]
fn an_alias_uses_the_servers_of_the_environment_it_names() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(
        dir.path(),
        "target",
        &format!("route = \"r\"\nprofile = \"p\"\n{SERVER}"),
        "hi",
    );
    let alias = dir.path().join("alias");
    std::fs::create_dir_all(&alias).unwrap();
    std::fs::write(
        alias.join("environment.toml"),
        "alias_of = \"target\"\naccount = \"two\"\n",
    )
    .unwrap();
    let servers = load_mcp_servers("alias", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "docs");
}

/// ADR-0201: a pack without `environments/`, or without this environment, is passed over;
/// the servers come from the layer that defines the environment.
#[test]
fn layers_without_the_environment_are_passed_over() {
    let dir = tempfile::tempdir().unwrap();
    let bare = dir.path().join("bare-pack/environments");
    write_environment(&bare, "other", BASE, "hi");
    let shipped = dir.path().join("shipped/environments");
    write_environment(&shipped, "tools", &format!("{BASE}{SERVER}"), "hi");
    let dirs = [dir.path().join("accounts-pack/environments"), bare, shipped];
    let servers = load_mcp_servers("tools", &dirs).unwrap();
    assert_eq!(servers[0].name, "docs");
}
