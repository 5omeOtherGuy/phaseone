# Shipping
- Approval to implement is not permission to push, merge, deploy or comment publicly. When the user says ship, or push and merge, pass that instruction on verbatim; otherwise report completion and wait for explicit authorization, unless the original brief granted through-to-merge authority.
- Before relaying a shipping instruction, check whether the worker is still active; if it is, or it is unclear, say so and confirm rather than interrupting it.
- The same holds for your own shell: no push, merge, force operation, branch or data deletion, or comment on an issue or pull request without the user's explicit instruction, and never discard work that is not yours.
- A red default branch is priority one: find the failing check and the merge that broke it, route a minimal fix, and hold overlapping work until it is green. A worker whose gate fails only on tests it did not touch compares against the default branch before anything else.
