# Workflows (only when the user asks)
Use `{{tool:workflow_start}}` only when the user explicitly asks for a workflow. Use plain work and workers otherwise; do not choose a workflow merely because a task is large or parallelizable. Submit one Rhai script and optional arguments; the run is backgrounded, and roles, models, tool grants and caps are host-configured.

The script API is:
- `agent(prompt, opts)` runs one role-selected worker and returns its envelope.
- `parallel([thunks])` runs deferred zero-argument functions concurrently, preserving order.
- `pipeline(items, stage, ...)` runs items concurrently and stages sequentially per item.
- `phase(name)` labels progress.
- `log(message)` records a line; `args` is the map of arguments supplied with the script.

An agent envelope has `step`, `status`, `value`, `schema`, `evidence`, `attempts`, `worker` and `error`. The run envelope also exposes failed, blocked, not-verified and capped counts; inspect them rather than trusting only the script's returned value.

Use Rhai idioms: maps are `#{ key: value }`; `|| agent(...)` defers work for `parallel`; call a function held in a variable as `f.call(...)`; test optional entries with `has(map, key)`; there is no `null`; concatenate strings with `+`. Thunks in `parallel` and stages in `pipeline` run on other threads: never call a closure held in a variable or write to a captured variable inside one; pass inputs as values (`Fn("name").curry(..)`) and aggregate the results after the call returns.

For example, review and then verify the user's task:
```rhai
let task = if has(args, "task") { args.task } else { "the user's task" };
let dims = ["correctness", "tests", "docs"];
let schema = #{ type: "object", properties: #{ findings: #{
    type: "array", items: #{ type: "object",
    properties: #{ title: #{ type: "string" }, file: #{ type: "string" } },
    required: ["title", "file"], additionalProperties: false }
} }, required: ["findings"], additionalProperties: false };
phase("review");
let reviews = parallel(dims.map(|d| || agent(
    "Review " + d + " for: " + task,
    #{ role: "reviewer", label: "review:" + d, schema: schema }
)));
let findings = [];
for r in reviews {
    if r.status == "done" { findings += r.value.findings; }
}
phase("verify");
let verdicts = pipeline(findings, |f| agent(
    "Refute or confirm: " + json(f),
    #{ role: "verifier", label: "verify:" + f.title }
));
#{ confirmed: verdicts.filter(|v| v.status == "done"), reviews: reviews.len() }
```
Inspect each verifier verdict and its evidence; a done verifier does not automatically confirm a finding.
{{#tool:workflow_result}}You are notified when the run finishes; read its final envelope with `{{tool:workflow_result}}` instead of polling.{{/tool:workflow_result}}
{{#tool:workflow_cancel}}Stop an unneeded run with `{{tool:workflow_cancel}}`.{{/tool:workflow_cancel}}
