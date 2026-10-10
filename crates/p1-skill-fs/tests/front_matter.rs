//! Exercise the supported subset through discovery/load, not private parsing.
use p1_contracts::skill::SkillSource;
use p1_skill_fs::FilesystemSkills;

#[test]
fn supported_text_forms_crlf_and_ignored_nested_maps() {
    for (header, expected) in [
        (
            "name: renamed\ndescription: plain text # comment\n",
            "plain text",
        ),
        (
            "name: 'renamed'\ndescription: 'it''s quoted'\n",
            "it's quoted",
        ),
        (
            "name: \"renamed\"\ndescription: \"quote: \\\"x\\\"\\nnext\\tline\\\\end\"\n",
            "quote: \"x\"\nnext\tline\\end",
        ),
        (
            "name: >-\n  renamed\ndescription: >-\n  folded first\n  second line\n",
            "folded first second line",
        ),
        (
            "name: >\n  renamed\ndescription: >\n  clip first\n  next\n",
            "clip first next\n",
        ),
        (
            "name: |\n  renamed\ndescription: |\n  literal first\n  next\n",
            "literal first\nnext\n",
        ),
        (
            "name: |-\n  renamed\ndescription: |-\n  strip first\n  next\n",
            "strip first\nnext",
        ),
        (
            "description: >-\n  first paragraph\n  continued\n\n  second paragraph\n\n\n  third\n",
            "first paragraph continued\nsecond paragraph\n\nthird",
        ),
        (
            "metadata:\n  name: ignored\n  nested:\n    description: ignored\nother: [unparsed, data]\ndescription: final text\n",
            "final text",
        ),
    ] {
        for crlf in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let directory = temp.path().join("skills/example");
            std::fs::create_dir_all(&directory).unwrap();
            let text = format!("---\n{header}---\nBODY\n");
            std::fs::write(
                directory.join("SKILL.md"),
                if crlf {
                    text.replace('\n', "\r\n")
                } else {
                    text
                },
            )
            .unwrap();
            let source = FilesystemSkills::discover(temp.path(), None, &["skills".into()], &[]);
            let listing = source.list();
            assert!(
                listing.warnings.is_empty(),
                "{header}: {:?}",
                listing.warnings
            );
            assert_eq!(listing.skills.len(), 1);
            assert_eq!(listing.skills[0].description, expected);
            let expected_name = if header.starts_with("name:") {
                "renamed"
            } else {
                "example"
            };
            assert_eq!(listing.skills[0].name, expected_name);
            assert_eq!(
                source.load(expected_name).unwrap().body,
                if crlf { "BODY\r\n" } else { "BODY\n" }
            );
        }
    }
}

#[test]
fn invalid_or_unclosed_headers_warn_and_skip() {
    for text in [
        "description: no header\n",
        "---\ndescription: missing close\n",
        "---\nname: example\n---\nbody",
        "---\ndescription: ''\n---\nbody",
        "---\ndescription: [unclosed\n---\nbody",
        "---\ndescription: 'unclosed\n---\nbody",
        "---\nname: BAD\ndescription: text\n---\nbody",
        "---\ndescription: first\ndescription: duplicate\n---\nbody",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("skills/example");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("SKILL.md"), text).unwrap();
        let source = FilesystemSkills::discover(temp.path(), None, &["skills".into()], &[]);
        assert!(source.list().skills.is_empty(), "{text}");
        assert_eq!(source.list().warnings.len(), 1, "{text}");
        assert!(source.load("example").is_err());
    }
}
