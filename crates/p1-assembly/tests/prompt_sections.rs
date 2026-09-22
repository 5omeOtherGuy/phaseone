//! Conditional prompt sections: `{{#tool:<module>}}` … `{{/tool:<module>}}`
//! (ADR-0050 item 4). Rendered through the public `render_prompt`.

use p1_assembly::{AssemblyError, Substitutions, render_prompt};

fn substitutions() -> Substitutions {
    Substitutions {
        workspace: "/work".into(),
        date: "2026-01-01".into(),
        os: "linux".into(),
    }
}

/// Render with `(module, module)` pairs: no per-agent name override, the common case.
fn render(template: &str, modules: &[&str]) -> Result<String, AssemblyError> {
    let pairs: Vec<(String, String)> = modules
        .iter()
        .map(|module| ((*module).to_string(), (*module).to_string()))
        .collect();
    render_prompt(template, &pairs, &substitutions())
}

fn render_ok(template: &str, modules: &[&str]) -> String {
    render(template, modules).expect("the template renders")
}

// ------------------------------------------------------------------ kept/dropped

#[test]
fn a_kept_section_keeps_its_text_and_its_placeholders() {
    let template =
        "tools: {{tool_names}}\n{{#tool:read}}\n`{{tool:read}}` works\n{{/tool:read}}\nend\n";
    assert_eq!(
        render_ok(template, &["read", "finish"]),
        "tools: read, finish\n\n`read` works\n\nend\n"
    );
}

#[test]
fn a_dropped_section_goes_entirely() {
    let template =
        "tools: {{tool_names}}\n{{#tool:read}}\n`{{tool:read}}` works\n{{/tool:read}}\nend\n";
    assert_eq!(render_ok(template, &["finish"]), "tools: finish\nend\n");
}

#[test]
fn a_section_for_an_unassembled_module_is_dropped_not_an_error() {
    assert_eq!(
        render_ok("a{{#tool:shell}}s{{/tool:shell}}b", &["read"]),
        "ab"
    );
}

// ------------------------------------------------------------------ nesting

#[test]
fn nested_sections_need_every_module_they_name() {
    let template = "a{{#tool:read}}R{{#tool:edit}}E{{/tool:edit}}{{/tool:read}}z";
    assert_eq!(render_ok(template, &["read", "edit"]), "aREz");
    assert_eq!(render_ok(template, &["read"]), "aRz");
    assert_eq!(render_ok(template, &["edit"]), "az");
    assert_eq!(render_ok(template, &[]), "az");
}

#[test]
fn a_dropped_section_still_tracks_the_sections_it_contains() {
    assert_eq!(
        render_ok(
            "A{{#tool:shell}}{{#tool:read}}R{{/tool:read}}S{{/tool:shell}}B",
            &[]
        ),
        "AB"
    );
    // The inner open is still open when the outer close arrives: still an error.
    let error = render("A{{#tool:shell}}{{#tool:read}}R{{/tool:shell}}B", &[]).unwrap_err();
    match error {
        AssemblyError::ToolSectionMismatch { module } => assert_eq!(module, "shell"),
        other => panic!("expected ToolSectionMismatch, got {other:?}"),
    }
}

// ------------------------------------------------------------------ newlines

#[test]
fn exactly_one_newline_after_a_dropped_section_is_dropped() {
    // A section on its own lines leaves no blank line behind.
    assert_eq!(
        render_ok("A\n{{#tool:m}}\nB\n{{/tool:m}}\nC\n", &[]),
        "A\nC\n"
    );
    // Kept, the tags' own line breaks stay: a blank line around the section.
    assert_eq!(
        render_ok("A\n{{#tool:m}}\nB\n{{/tool:m}}\nC\n", &["m"]),
        "A\n\nB\n\nC\n"
    );
    // Only ONE newline, and only when there is one: a blank line survives…
    assert_eq!(render_ok("A{{#tool:m}}B{{/tool:m}}\n\nC", &[]), "A\nC");
    // …and a close tag that is not at a line end drops no newline at all.
    assert_eq!(render_ok("A {{#tool:m}}B{{/tool:m}} C", &[]), "A  C");
}

// ------------------------------------------------------------------ errors

#[test]
fn an_unclosed_section_names_its_module() {
    let error = render("a{{#tool:read}}b", &["read"]).unwrap_err();
    match &error {
        AssemblyError::ToolSectionMismatch { module } => assert_eq!(module, "read"),
        other => panic!("expected ToolSectionMismatch, got {other:?}"),
    }
    assert!(error.to_string().contains("read"), "{error}");
    // The innermost open section is the one named.
    let error = render("{{#tool:read}}{{#tool:edit}}x", &["read", "edit"]).unwrap_err();
    match &error {
        AssemblyError::ToolSectionMismatch { module } => assert_eq!(module, "edit"),
        other => panic!("expected ToolSectionMismatch, got {other:?}"),
    }
}

#[test]
fn a_close_without_an_open_section_names_its_module() {
    let error = render("a{{/tool:read}}b", &["read"]).unwrap_err();
    match &error {
        AssemblyError::ToolSectionMismatch { module } => assert_eq!(module, "read"),
        other => panic!("expected ToolSectionMismatch, got {other:?}"),
    }
    assert!(error.to_string().contains("read"), "{error}");
}

#[test]
fn a_close_naming_another_module_than_the_innermost_open_one_is_an_error() {
    let error = render(
        "{{#tool:read}}{{#tool:edit}}x{{/tool:read}}",
        &["read", "edit"],
    )
    .unwrap_err();
    match &error {
        AssemblyError::ToolSectionMismatch { module } => assert_eq!(module, "read"),
        other => panic!("expected ToolSectionMismatch, got {other:?}"),
    }

    let error = render(
        "{{#tool:read}}{{#tool:edit}}x{{/tool:edit}}{{/tool:edit}}",
        &["read", "edit"],
    )
    .unwrap_err();
    match &error {
        AssemblyError::ToolSectionMismatch { module } => assert_eq!(module, "edit"),
        other => panic!("expected ToolSectionMismatch, got {other:?}"),
    }
}

#[test]
fn a_tool_outside_any_section_for_an_unassembled_module_is_still_an_error() {
    let error = render("a {{tool:shell}} b", &["read"]).unwrap_err();
    match &error {
        AssemblyError::ToolNotInEnvironment { module } => assert_eq!(module, "shell"),
        other => panic!("expected ToolNotInEnvironment, got {other:?}"),
    }
    assert!(error.to_string().contains("shell"), "{error}");
}

// ------------------------------------------------------------------ dropped contents

#[test]
fn placeholders_inside_a_dropped_section_are_never_looked_at() {
    // Neither an assembled module nor a known placeholder: dropped is dropped.
    assert_eq!(
        render_ok(
            "x{{#tool:shell}}use `{{tool:shell}}`, {{tool:nope}} and {{nope}}{{/tool:shell}}y",
            &["read"]
        ),
        "xy"
    );
}

#[test]
fn the_same_placeholders_error_once_the_section_is_kept() {
    let error = render("x{{#tool:shell}}{{nope}}{{/tool:shell}}y", &["shell"]).unwrap_err();
    match &error {
        AssemblyError::UnknownPlaceholder { placeholder } => assert_eq!(placeholder, "nope"),
        other => panic!("expected UnknownPlaceholder, got {other:?}"),
    }

    let error = render("x{{#tool:shell}}{{tool:edit}}{{/tool:shell}}y", &["shell"]).unwrap_err();
    match &error {
        AssemblyError::ToolNotInEnvironment { module } => assert_eq!(module, "edit"),
        other => panic!("expected ToolNotInEnvironment, got {other:?}"),
    }
}
