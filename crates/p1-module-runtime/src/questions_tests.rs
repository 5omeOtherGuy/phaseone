use super::*;

fn question() -> Question {
    Question {
        question: "Choose storage".into(),
        header: "Storage".into(),
        options: vec![
            QuestionOption {
                label: "SSD".into(),
                description: "Working set".into(),
                preview: None,
            },
            QuestionOption {
                label: "HDD".into(),
                description: "Bulk data".into(),
                preview: None,
            },
        ],
        multi_select: false,
    }
}

#[test]
fn nested_bounds_and_duplicates_are_authoritative() {
    let valid = question();
    assert!(validate(std::slice::from_ref(&valid)).is_ok());
    let mut bad = Vec::new();
    bad.push(vec![]);
    bad.push(vec![valid.clone(); 5]);
    bad.push(vec![valid.clone(), valid.clone()]);
    for change in 0..14 {
        let mut q = valid.clone();
        match change {
            0 => q.question.clear(),
            1 => q.question = "q".repeat(2001),
            2 => q.header.clear(),
            3 => q.header = "h".repeat(13),
            4 => q.options.truncate(1),
            5 => q.options = vec![q.options[0].clone(); 5],
            6 => q.options[0].label.clear(),
            7 => q.options[0].label = q.options[1].label.clone(),
            8 => q.options[0].label = "Other".into(),
            9 => q.options[0].description.clear(),
            10 => q.options[0].description = "é".repeat(1001),
            11 => q.options[0].preview = Some("p".repeat(8001)),
            12 => {
                q.multi_select = true;
                q.options[0].preview = Some(String::new());
            }
            13 => q.options[0].label = "l".repeat(2001),
            _ => unreachable!(),
        }
        bad.push(vec![q]);
    }
    for questions in bad {
        assert!(validate(&questions).is_err(), "{questions:?}");
    }
}

#[test]
fn the_interface_accepts_questions_never_answers() {
    let wit = include_str!("../../../modules/wit/interaction.wit");
    assert!(wit.contains("ask: func(questions: list<question>) -> result<asked, question-error>"));
    assert_eq!(wit.matches("func(").count(), 1);
    assert!(!wit.contains("submit"));
}
