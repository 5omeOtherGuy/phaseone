//! The role rule (ADR-0065): the environment states the route's CAPACITY, the role
//! sets the working window, and the threshold is the environment's own value or the
//! user's percentage of that window. Specification: `docs/design/context.md` §2.
//!
//! Pure table tests: no files, no provider, no network.

use p1_assembly::{ContextSettings, Role, RoleWindows};

/// A table with the shipped shape (32k output headroom, a long tail).
fn settings(window_tokens: u64, summarize_at_tokens: Option<u64>) -> ContextSettings {
    ContextSettings {
        window_tokens,
        output_headroom_tokens: 32_000,
        summarize_at_tokens,
        keep_recent_tokens: 60_000,
        user_verbatim_tokens: 8_000,
        tool_result_excerpt_chars: 2_000,
        summary_output_tokens: 12_000,
    }
}

/// `(window_tokens, summarize_at_tokens)` of one agent's effective context.
fn effective(settings: &ContextSettings, role: Role, windows: RoleWindows) -> (u64, u64) {
    let context = settings
        .effective("test-env", role, windows)
        .unwrap_or_else(|error| panic!("the table must be valid: {error}"));
    assert_eq!(context.role, role);
    (context.window_tokens, context.summarize_at_tokens)
}

// (a) the role table.
#[test]
fn the_workable_window_is_the_capacity_clamped_by_the_role() {
    let windows = RoleWindows::default();
    // The owner's defaults: lead 500k / worker 300k, summarizing at 80%.
    assert_eq!(windows.lead_window_tokens, 500_000);
    assert_eq!(windows.worker_window_tokens, 300_000);
    assert_eq!(windows.summarize_at_percent, 80);

    // A 1M route: the role window decides, not the route.
    let million = settings(1_000_000, None);
    assert_eq!(effective(&million, Role::Lead, windows), (500_000, 400_000));
    assert_eq!(
        effective(&million, Role::Worker, windows),
        (300_000, 240_000)
    );

    // A 260k route: the capacity clamps the 300k worker window.
    let small = settings(260_000, None);
    assert_eq!(effective(&small, Role::Worker, windows), (260_000, 208_000));
    assert_eq!(effective(&small, Role::Lead, windows), (260_000, 208_000));

    // A 200k route: the capacity clamps the lead window too (Claude's capacity in
    // this revision).
    let claude = settings(200_000, None);
    assert_eq!(effective(&claude, Role::Lead, windows), (200_000, 160_000));
    assert_eq!(effective(&claude, Role::Worker, windows), (200_000, 160_000));
}

// (a2) the user's own table replaces the defaults.
#[test]
fn the_user_table_sets_the_role_windows_and_the_percentage() {
    let windows = RoleWindows {
        lead_window_tokens: 400_000,
        worker_window_tokens: 200_000,
        summarize_at_percent: 50,
    };
    let million = settings(1_000_000, None);
    assert_eq!(effective(&million, Role::Lead, windows), (400_000, 200_000));
    assert_eq!(effective(&million, Role::Worker, windows), (200_000, 100_000));
}

// (b) an environment that pins the threshold keeps it, for every role.
#[test]
fn an_explicit_environment_threshold_is_honoured() {
    let windows = RoleWindows::default();

    // A pinned value below both walls is used as it is (`glm`'s shape).
    let glm = settings(260_000, Some(150_000));
    assert_eq!(effective(&glm, Role::Lead, windows), (260_000, 150_000));
    assert_eq!(effective(&glm, Role::Worker, windows), (260_000, 150_000));

    // The 1M route with a measured pin: the role still sets the window.
    let deepseek = settings(1_000_000, Some(150_000));
    assert_eq!(effective(&deepseek, Role::Lead, windows), (500_000, 150_000));
    assert_eq!(
        effective(&deepseek, Role::Worker, windows),
        (300_000, 150_000)
    );
}

// (c) an invalid combination is a load error naming the environment, the role and
// the numbers — never a silent clamp.
#[test]
fn a_threshold_that_does_not_fit_the_effective_window_names_environment_role_and_numbers() {
    let windows = RoleWindows::default();

    // The environment pins a threshold equal to the window the worker gets: the
    // wall is 300000 - 32000 = 268000.
    let error = settings(300_000, Some(300_000))
        .effective("deepseek2", Role::Worker, windows)
        .unwrap_err();
    assert!(error.contains("`deepseek2`"), "{error}");
    assert!(error.contains("worker"), "{error}");
    assert!(error.contains("300000"), "{error}");
    assert!(error.contains("268000"), "{error}");

    // The same pin is fine for the lead on a 1M route...
    let deepseek = settings(1_000_000, Some(300_000));
    assert_eq!(effective(&deepseek, Role::Lead, windows), (500_000, 300_000));
    // ...and refused for the worker on it: the clamp, not the pin, decides.
    let error = deepseek
        .effective("deepseek", Role::Worker, windows)
        .unwrap_err();
    assert!(error.contains("`deepseek`"), "{error}");
    assert!(error.contains("worker"), "{error}");
    assert!(error.contains("268000"), "{error}");
    assert!(error.contains("explicit summarize_at_tokens"), "{error}");
}

#[test]
fn a_summary_cap_that_does_not_fit_the_effective_window_is_a_load_error() {
    // 280000 fits the CAPACITY's wall (1000000 - 32000) so the environment file
    // loads, but not the worker's (300000 - 32000).
    let mut table = settings(1_000_000, None);
    table.summary_output_tokens = 280_000;
    let error = table
        .effective("big", Role::Worker, RoleWindows::default())
        .unwrap_err();
    assert!(error.contains("`big`"), "{error}");
    assert!(error.contains("worker"), "{error}");
    assert!(error.contains("summary_output_tokens"), "{error}");
    assert!(error.contains("280000"), "{error}");
    assert!(error.contains("268000"), "{error}");
    // The lead's 500k window leaves room, so the same table is fine for it.
    assert_eq!(
        effective(&table, Role::Lead, RoleWindows::default()),
        (500_000, 400_000)
    );
}

#[test]
fn a_derived_threshold_at_the_wall_names_the_percentage() {
    // W = 300000 and a 100000 headroom leave a 200000 wall; 80% of 300000 is 240000.
    let table = ContextSettings {
        output_headroom_tokens: 100_000,
        ..settings(300_000, None)
    };
    let error = table
        .effective("tight", Role::Lead, RoleWindows::default())
        .unwrap_err();
    assert!(error.contains("`tight`"), "{error}");
    assert!(error.contains("lead"), "{error}");
    assert!(error.contains("summarize_at_percent 80"), "{error}");
    assert!(error.contains("240000"), "{error}");
    assert!(error.contains("200000"), "{error}");
}

#[test]
fn the_user_table_itself_is_validated() {
    let bad = |windows: RoleWindows| windows.validate().unwrap_err();
    assert!(
        bad(RoleWindows {
            lead_window_tokens: 0,
            ..RoleWindows::default()
        })
        .contains("lead_window_tokens")
    );
    assert!(
        bad(RoleWindows {
            worker_window_tokens: 0,
            ..RoleWindows::default()
        })
        .contains("worker_window_tokens")
    );
    assert!(
        bad(RoleWindows {
            summarize_at_percent: 0,
            ..RoleWindows::default()
        })
        .contains("summarize_at_percent (0)")
    );
    assert!(
        bad(RoleWindows {
            summarize_at_percent: 96,
            ..RoleWindows::default()
        })
        .contains("summarize_at_percent (96)")
    );
    // The whole accepted range is accepted.
    for percent in [1, 95] {
        RoleWindows {
            summarize_at_percent: percent,
            ..RoleWindows::default()
        }
        .validate()
        .unwrap_or_else(|error| panic!("{percent} must be accepted: {error}"));
    }
}

#[test]
fn the_role_spellings_are_lead_and_worker() {
    assert_eq!(Role::parse("lead").unwrap(), Role::Lead);
    assert_eq!(Role::parse("worker").unwrap(), Role::Worker);
    assert_eq!(Role::Lead.name(), "lead");
    assert_eq!(Role::Worker.name(), "worker");
    let error = Role::parse("boss").unwrap_err();
    assert!(error.contains("boss"), "{error}");
    assert!(error.contains("lead, worker"), "{error}");
}
