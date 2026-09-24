use p1_tui::render::workers::{BlockState, WorkerBlock, WorkersHeader, WorkersPane, render};

fn worker() -> WorkerBlock {
    WorkerBlock {
        id: "w1".into(),
        task: "task".into(),
        route: "openrouter-free/deepseek/deepseek-v4.1-flash:free".into(),
        model: Some("deepseek-v4.1-flash".into()),
        state: BlockState::Done,
        elapsed: Some("4m12s".into()),
        cost_micro_usd: None,
        tokens: Some(48_213),
        context_window: Some(128_000),
        grants: "read".into(),
        activity: "done".into(),
    }
}

fn pane(worker: WorkerBlock) -> WorkersPane {
    WorkersPane {
        header: WorkersHeader::default(),
        workers: vec![worker],
        focused: None,
    }
}

#[test]
fn long_model_is_whole_and_its_metrics_survive_at_both_widths() {
    for (width, compact) in [(38, true), (56, false)] {
        let lines = render(&pane(worker()), width, compact);
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        let row = &text[3];
        assert!(row.contains("deepseek-v4.1-flash"), "{row}");
        assert!(!row.contains("openrouter-free"), "{row}");
        if compact {
            assert!(text[4].contains("48.2k/128k"), "{}", text[4]);
        } else {
            assert!(row.contains("48.2k/128k"), "{row}");
        }
    }
}

#[test]
fn overlong_model_is_cut_and_never_partially_shows_the_route() {
    let mut worker = worker();
    worker.model = Some("gpt-6-astra-reasoning-preview-2026-09".into());
    let lines = render(&pane(worker), 38, true);
    let row = lines[3].to_string();
    assert!(row.contains("gpt-6-astra-reasonin"), "{row}");
    assert!(row.contains('…'));
    assert!(!row.contains("openrouter-free"));
}

#[test]
fn a_short_model_gets_a_dim_route_suffix_when_it_fits() {
    let mut worker = worker();
    worker.model = Some("glm-5.3".into());
    worker.route = "glm/5.3-pass".into();
    let row = render(&pane(worker), 56, false)[3].to_string();
    assert!(row.starts_with("      glm-5.3 · glm/5.3-pass"), "{row}");
}

#[test]
fn finished_elapsed_and_unknown_metrics_are_explicit() {
    for (width, compact) in [(38, true), (56, false)] {
        let lines = render(&pane(worker()), width, compact);
        let text: String = lines.iter().map(ToString::to_string).collect();
        assert!(text.contains("4m12s"), "{text}");
        assert!(!text.contains("$0"), "{text}");
    }
    let mut worker = worker();
    worker.tokens = None;
    worker.context_window = None;
    worker.elapsed = Some("4m12s".into());
    let text: String = render(&pane(worker), 56, false)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(text.contains("—/— · 4m12s · —"), "{text}");
}

#[test]
fn empty_optional_rows_collapse_only_in_the_wide_form() {
    let mut worker = worker();
    worker.grants.clear();
    worker.activity.clear();
    assert_eq!(render(&pane(worker.clone()), 56, false).len(), 6);
    assert_eq!(render(&pane(worker.clone()), 38, true).len(), 7);
    worker.grants = "read".into();
    worker.activity = "done".into();
    assert_eq!(render(&pane(worker.clone()), 56, false).len(), 8);
    assert_eq!(render(&pane(worker), 38, true).len(), 7);
}

#[test]
fn an_unknown_model_keeps_the_route_as_the_lead() {
    let mut worker = worker();
    worker.model = None;
    worker.route = "route".into();
    let row = render(&pane(worker), 56, false)[3].to_string();
    assert!(row.starts_with("      route"), "{row}");
}
