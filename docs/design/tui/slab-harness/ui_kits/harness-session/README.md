# Harness session

Full 120×40 p1 screen: transcript (W=76) · 2-col gutter · right pane (38) · statusline, all inset by one unit of GROUND.

Flow: the session opens blocked on an **edit approval**. Press `y` (allow once), `a` (session) or `n` (deny).
Approving runs `cargo test` (running block with ▪▪▪ → fails with a folded body), then the agent delegates to two workers;
the pane's ledger, workers and folds update and the statusline diff goes `+1 −1`. After that, type in the composer and
press Enter to append an operator input + a shell block.

Files: `Session.jsx`. Uses Block, Decision, Worker, OperatorInput, Prose, Band, Composer, Statusline, Pane.