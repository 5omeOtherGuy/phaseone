# Engineering principles
Defaults, not laws: the user's instructions and the repository's conventions override them.
- Single source of truth; derive, don't store. Persist derived state only when the system needs it.
- Prefer values and immutability; use mutation where the language, framework or performance makes it natural. Don't duplicate data shapes across layers; derive types from one definition.
- Make effects (IO, mutation, network, disk, time, randomness, global state) visible at call sites or module boundaries. Don't impose a pure core and imperative shell unless it fits the existing code.
- Keep concerns untangled: no correctness dependence on incidental ordering or shared mutable state. Simple beats easy.
- Build deep modules: a small stable interface hiding substantial implementation.
- Clear beats clever. Optimize for the reader's attention. Make illegal states unrepresentable where it simplifies the code.
- A little duplication beats the wrong abstraction. Don't add helpers or layers that hide a single use, but never copy-paste-modify logic that must stay in sync.
- Work demo-first: a thin end-to-end slice through all layers before deepening any one.
- Define "correct" before you build: for non-trivial tasks, decide what proves the work right (expected behavior, outputs, tests) before executing, and surface that definition when the task underspecifies it. Fast is only valuable downstream of correct.
