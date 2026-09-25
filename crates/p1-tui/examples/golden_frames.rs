#[path = "support/block.rs"]
mod fixture;
fn main() {
    for name in [
        "read", "shell", "write", "edit", "skill", "search", "send", "stop", "delegate", "ask",
        "notify", "compact",
    ] {
        let s = fixture::screen(name);
        let text = p1_tui::render::block::lines(&s.transcript, 76, None, 0, true)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(
            format!("{}/tests/golden/{name}.txt", env!("CARGO_MANIFEST_DIR")),
            text,
        )
        .unwrap();
    }
}
