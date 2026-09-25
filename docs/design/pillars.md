# p1 / Phaseone — core pillars (rev 2 after owner feedback; NOT yet owner-approved)

**p1 is a lean, modular Rust coding harness that adapts itself to the model it runs.**
You type a prompt and the agent starts working.

Each pillar restates recorded owner direction; each *Boundary* is a scope
recommendation.

1. **A coding harness that holds up over long autonomous work — for the owner
   first.** Simple to use: prompt in, work out. Built so a run can go on for hours,
   mostly unattended, and end in verified, reviewable results.
   *Boundary:* no enterprise platform, marketplace, or workflow engine; hypothetical
   users do not drive v1 scope.

2. **The harness reshapes itself around the model.** An agent gets the prompt, tools,
   tool descriptions and provider behaviour suited to its model — and sees only
   those. The environment is assembled for that agent, not a generic toolbox with
   names hidden.
   *Boundary:* Claude and GPT are the first optimisation targets; other models stay
   usable through suitable modules. Tools may be shared, partly shared or
   model-specific as each case deserves.

3. **A small core with exchangeable modules.** A new project: one small Rust
   agent-loop API; everything else is a module around it — providers, tools,
   sessions, frontends, and optional capabilities such as delegation tools, routing
   and modes. Tools are always their own modules; providers only translate wire
   behaviour. Iris donates parts (Nexus as raw material with a reduced interface),
   not structure.
   *Boundary (amended 2026-09-25 by the owner, ADR-0071):* modules are WebAssembly artifacts
   the native host loads by name; the core, host and terminal front ends stay native. No
   marketplace, auto-registration or fixed crate list. Adding a model through existing contracts should not require
   changing the core; a genuinely new capability may.

4. **Efficiency across the whole job.** Low memory and startup cost, lean contexts,
   and model-native behaviour where it earns its complexity — small gains compound
   over hours.
   *Boundary:* measure the whole job (retries, verification, repair), not only
   prompt size. "Beats generic and native-headless harnesses" is an
   experience-backed hypothesis to be measured.

## Orchestration: supported and optimised for, never imposed  [owner, 2026-09-19]

Delegating to other agents is how capable models naturally work today, and p1 is
built so that this works well: agents can be given tools to start other agents
(on any configured model, each in its own model-appropriate environment), are told
when those finish, and can inspect, verify and request repair. It is a capability
delivered by modules and used when the agent or the user sees fit. There is no
mandatory coordinating agent, no orchestrator mode the user must be in, and no
delegation requirement.

Modes (bundles of prompt + tools + policy, ampi-style) are something the owner wants
for himself later, added as modules. They are not a pillar of p1.

## Proposed first usable slice (scope to confirm)

A working coding harness: type a prompt; the agent works on a real repository in a
model-appropriate environment, with Claude and with GPT — each seeing its own tools
and prompt. It shows the actual model/route used and available token/cost
information, never reporting unknown cost as zero. A delegation tool module is
available: an agent can hand a bounded task to another agent on a different model
and is notified when it finishes. The owner's observed failures (needless stops and
questions, missed completion notices, invented time-boxes, blocked actions,
uncontrollable compaction point, forgotten decisions) serve as behaviour tests as the
harness matures; this slice demonstrates the architecture, not hours-long reliability.

## Deferred decisions

Technical mechanisms: session/log model, inbox and turn-end hooks, tool-declaration
kinds for native tools, how an agent's environment is described as data, prompt
composition (whole prompt files as a starting proposal).

Release scope to size later: initial UI, minimum context controls, restart recovery,
routing beyond explicit configuration, modes, mid-session model switching (not
ruled out).
