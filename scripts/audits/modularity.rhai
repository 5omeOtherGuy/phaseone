// modularity.rhai — the modularity audit as a p1 workflow (ADR-0053, live check 5).
//
// Inputs are computed by scripts/audits/modularity-prep.py and arrive as `args`:
//   sha, facts, units [{label, lens, files, brief}], findings_schema, verdict_schema,
//   refuter {rule, code, code-second}, refute_preamble.
// Stages, as in scripts/audits/modularity.py: Find (one worker per unit) → dedupe by the
// first evidence location → Verify (two refuters per finding, a third vote when a
// high/medium finding splits) → confirmed / discarded.
//
// Script functions (`fn`) cannot see `args`, so the helpers are `fn`s taking everything as
// an argument. That is also what the thunk threads need: `parallel` thunks and `pipeline`
// stages run on other threads, where calling a closure held in a variable borrows it
// mutably for the whole call (two thunks doing that to one closure is a data race), and
// writing a captured variable is one too. So a thunk is `Fn("refute_step").curry(..)` —
// curried arguments are copied into the function pointer — and the votes are counted
// after `parallel` has joined.
let a = args;
let facts = a.facts;
let refuter = a.refuter;
let audit_tools = ["read", "grep", "shell"];

// One refuter vote: the prompt is rebuilt per lens and every input arrives as an argument.
fn refute_step(preamble, lens_text, facts, schema, f, lens) {
    let shown = #{
        id: f.id, claim: f.claim, rule: f.rule, evidence: f.evidence,
        repro: f.repro, severity: f.severity, fix: f.fix,
    };
    agent(
        preamble + "\n" + lens_text + "\n\n## Finding\n```json\n" + json(shown)
            + "\n```\n\n## Facts\n" + facts,
        #{
            role: "verifier",
            label: "verify-" + f.unit + "-" + f.id + "-" + lens,
            schema: schema,
            tools: ["read", "grep", "shell"],
        },
    )
}

fn upheld(v) {
    v.status == "done" && v.value.verdict == "upheld"
}

phase("find");
let found = pipeline(a.units, |u| agent(u.brief, #{
    role: "worker",
    label: "find-" + u.label,
    schema: a.findings_schema,
    tools: audit_tools,
}));

// One finding per evidence location: a second finder quoting the same lines (±3) is a
// duplicate, whichever unit it came from.
let findings = [];
let seen = [];
let index = 0;
for r in found {
    let unit = a.units[index];
    index += 1;
    if r.status != "done" { continue; }
    for f in r.value.findings {
        let head = f.evidence[0];
        let duplicate = false;
        for s in seen {
            let distance = s.line - head.line;
            if distance < 0 { distance = -distance; }
            if s.file == head.file && distance <= 3 { duplicate = true; break; }
        }
        if duplicate { continue; }
        seen += [#{ file: head.file, line: head.line }];
        f.unit = unit.label;
        f.lens = unit.lens;
        findings += [f];
    }
}
log("findings to verify: " + findings.len());

phase("verify");
let verified = pipeline(findings, |f| {
    let votes = parallel([
        Fn("refute_step").curry(
            a.refute_preamble, refuter["rule"], facts, a.verdict_schema, f, "rule"
        ),
        Fn("refute_step").curry(
            a.refute_preamble, refuter["code"], facts, a.verdict_schema, f, "code"
        ),
    ]);
    let yes = 0;
    for v in votes { if upheld(v) { yes += 1; } }
    let keep = yes == 2;
    // A split vote on anything above low gets a third look (run2 of the Python audit: a
    // split medium finding hid a live defect). It runs here, on this stage's thread, so a
    // plain call of the `fn` is enough.
    if yes == 1 && (f.severity == "high" || f.severity == "medium") {
        let third = refute_step(
            a.refute_preamble, refuter["code-second"], facts, a.verdict_schema, f, "code-second"
        );
        votes += [third];
        keep = upheld(third);
    }
    #{ finding: f, votes: votes, confirmed: keep }
});

let confirmed = [];
let discarded = [];
for v in verified {
    if v.confirmed { confirmed += [v]; } else { discarded += [v]; }
}
log("confirmed: " + confirmed.len() + ", discarded: " + discarded.len());

#{
    commit: a.sha,
    units: a.units.map(|u| #{ label: u.label, lens: u.lens, files: u.files }),
    confirmed: confirmed,
    discarded: discarded,
}
