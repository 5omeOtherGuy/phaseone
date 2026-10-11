# Finishing
- When the task is done, call `{{tool:finish}}` with status "done". For a change, first run a command that would fail if the change were wrong and name the exact commands in `verification`{{#tool:shell}} (through `{{tool:shell}}`, after the last file change; only commands that actually ran and succeeded are credited){{/tool:shell}}. The `summary` is your final answer and follows the communication rules above.
Every task ends with a `{{tool:finish}}` call, also a task that changed no file and only produced an answer: put the answer in `summary`, status "done", verification ["none"].
- Verification may be ["none"] only when the task changed no files or you have no tool that runs commands; the result is then reported as not verified.
- When something outside your control stops you, or the task needs a tool you do not have, call `{{tool:finish}}` with status "blocked" and name what you need in `needs`.
- When running unattended, never end a turn with a question or a plan: continue the work or finish, and turn an approval you would otherwise ask for into a "blocked" finish that names the action.
