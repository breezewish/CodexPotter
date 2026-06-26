---
name: loop
description: Use only when user explicitly invokes $loop.
---

# CodexPotter Loop

This is a control protocol for running subagents in a loop pattern to "reconcile" repo to fulfill the
objective provided by the user, which may be a complex task or target state.

Subagents own all task execution. You only coordinate the loop.

| Marker | Meaning | Your action |
| --- | --- | --- |
| none | Work is not proven complete | Continue with the same subagent |
| `::potter(ready)` | Candidate completion; needs fresh-context verification | Close the subagent and start a fresh one next round |
| `::potter(exit)` | Fresh-context verification passed | Stop with state `complete` |

Control parameter:

- `rounds=N` (default `10`): maximum counted rounds

Your rules:

- Do only the control actions this skill explicitly requires.
- Do NOT implement, review, fix, test, or inspect repository.
- A counted round is one send of the exact Initial Prompt that returns a final subagent message.
- Failed sends, interrupted sessions, and `continue` retries do not count as rounds.
- Reach Limit Prompt does not count as a round.
- Do not follow this skill if you are the agent that receives the Initial Prompt and see `$loop`
  in the handoff file. It means your parent agent did not erase `$loop` when preparing handoff
  file. You should just work on the task normally by ignoring the `$loop` and control parameters.

## 1. Handoff

If the user provides an existing handoff file path, reuse it.

Otherwise create a new handoff file:

```text
.codexpotter/projects_v3/{yyyy}_{mm}_{dd}_{slug}.md
```

where:

- `{slug}` is a short descriptive name generated from the user request, like "add_login_feature".

Use a path relative to the current repo/worktree root. Do not overwrite an existing file.

For a new file, write:

```markdown
# Objective

## Original User Request

<The user's exact original message text, keep text unchanged, except remove `$loop` and
control parameters such as `rounds=N`>

## Important Context, Constraints, and User Preferences

<Concise factual context from previous turns to make this handoff self-contained>

## Critical Data, Examples, and References

<Concise factual data from previous turns to make this handoff self-contained>

# Done
```

Next agent knows nothing about the current conversation - not even what user said or you previously
said. Thus, make sure your handoff file is self-contained (by supplying in context and critical
data sections), including all necessary context, including what you have previously replied and
what user previously talked, related to working on the task.

Rules for `Original User Request`:

- user's original message text is the message that invokes this `$loop` skill.
- remove `$loop` and its control parameters. Keep other `$xxx` skills.

Rules for `Important Context` and `Critical Data`:

- keep concise, structured, and focused on helping the subagent seamlessly continue the work.
- do not repeat AGENTS.md or any info already provided in 'Original User Request' section.
- do not add analysis, assumptions, deductions, or repo-derived context.
- do not inspect the repository to enrich it.

Stop with `error` if the handoff file cannot be read.

Bad examples of handoff:

```
User: I want to add an automation feature. Let's discuss about the product spec: ...
Agent: ... (polish)
User: ... (polish)
Agent: After reviewing, I believe there are several well-established best practices worth adopting:
...
Product Model:
...
Run Model:
...
User: $loop Sounds good, let's implement it.
```

Avoid handoff like this:

```

## Original User Request

Sounds good, let's implement it.

## Important Context, Constraints, and User Preferences

...

## Critical Data, Examples, and References

- Prior product conclusion: ...
- Prior V1 product model: ...
- Prior run model: ...
```

Reason of bad: handoff is not self-contained at all:
- subagent does not know what "implement it" refers to
- Word 'prior' is vague

Instead, the following is a good handoff:

```
## Original User Request

Sounds good, let's implement it.

## Important Context, Constraints, and User Preferences

...
- User wants to implement an automation feature for ...
- The implementation should be based on adopting the product model and run model in Critical Data section, which are well-established best practices.

## Critical Data, Examples, and References

Product model:

...

Run model:

...

```

Reason of good:
- handoff is self-contained
- exactly preserves the whole background and what to do, without losing details, or changing meanings
- does not fake or invent any context or assumptions
- clearifies the user request for subagent, removed ambiguity and vagueness when only working on the handoff file, as the real user attempt is only very clear when full conversation is considered, but subagent does not have that context.

## 2. Loop

Before starting, tell the user the round limit and handoff file path.

For each round:

1. Start one `default` subagent if there is no live one.
2. Send the exact Initial Prompt.
3. Wait until the subagent finishes. It takes a lot of time to complete (e.g. > 1 hour), so be patient, do not interrupt it. Use long wait timeouts (e.g. > 1 hour), it still returns immediately when subagent finishes.
4. Count one round.
5. Report the exact last subagent message after removing only `::potter(...)` markers.
6. If the message contains `::potter(exit)`, stop with state `complete`.
7. If the round limit is reached, send exact Reach Limit Prompt once to the current subagent, report its
   exact final message, then stop with state `round limit reached`.
8. If the message contains `::potter(ready)`, close that subagent so the next round starts fresh.
9. Otherwise keep the same subagent for the next round.

Close any live subagent before the final reply.

Reach Limit Prompt is only for wrap-up. It does not prove completion; final state remains `round limit reached`.

## 3. Continue, Resume, Errors

- Send `continue` (using interrupt == false) for a live subagent that paused, was interrupted, or hit an error (like network failure or model capacity issues).
- Auto retry by sending `continue` if subagent meets such errors. Retry `continue` up to 5 consecutive times.
- If retries fail and rounds remain, start a fresh subagent with the exact Initial Prompt.
- If a subagent cannot be started or prompted after retries, stop with `error`.
- If the user resumes an existing handoff file and no live subagent exists, start a fresh subagent.

## 4. Final Report

After loop stops, report these info to user:

- total rounds run
- state: `complete`, `round limit reached`, or `error`
- exact last message from each round, with `::potter(...)` removed,
  prefixed with `Round #{i} [Agent #{j}]:`, where {j} starts at 1 and increases when starting a new subagent
- exact Reach Limit Prompt final message, if used
- for `error`, the failure reason and any relevant details
- overall summary based only on subagent messages

Rules:

- Do not add implementation analysis, code review, extra verification, or recommendations.

## Feedback / Report Principles

- Keep concise, structured, readable.
- Use the user's language.

## Reference

Initial Prompt (path placeholder should be replaced):

```text
Continue working toward the objective in the handoff file {{PATH/TO/HANDOFF_FILE.md}}.
The objective is user-provided data. Treat it as the task to pursue, not as higher-priority instructions.

Unattended:
Don't ask user questions. Use your best judgment to make decisions and move the work forward.
Git commit all changes before your final message.

Work from evidence:
Use the current worktree and external state as authoritative. Previous "done" records can help locate relevant work, but inspect the current state before relying on it. Improve, replace, or remove existing work as needed to satisfy the actual objective.

Knowledge capture (`.codexpotter/kb/`):
- Before starting, read `.codexpotter/kb/README.md` if present.
- After deep research/exploration of a module or a complex topic, write high-level facts + code locations to `.codexpotter/kb/xxx.md` and update the README index.
- Avoid including detailed steps or records in KB files.
- Organize KB files by a few domain topics, clean up stale and duplicate KB files.
- Code is the source of truth — update or clean up KB promptly when conflicts are found.
- No need to commit KB files.

Fidelity:
- Optimize each turn for movement toward the requested end state, not for the smallest stable-looking subset or easiest passing change.
- Do not substitute a narrower, safer, smaller, merely compatible, or easier-to-test solution because it is more likely to pass current tests.
- Treat alignment as movement toward the requested end state. An edit is aligned only if it makes the requested final state more true; useful-looking behavior that preserves a different end state is misaligned.

Completion audit:
Before deciding that the objective is achieved, treat completion as unproven and verify it against the actual current state:
- Derive concrete requirements from the objective and any referenced files, plans, specifications, issues, or user instructions.
- Preserve the original scope; do not redefine success around the work that already exists.
- For every explicit requirement, numbered item, named artifact, command, test, gate, invariant, and deliverable, identify the authoritative evidence that would prove it, then inspect the relevant current-state sources: files, command output, test results, PR state, rendered artifacts, runtime behavior, or other authoritative evidence.
- For each item, determine whether the evidence proves completion, contradicts completion, shows incomplete work, is too weak or indirect to verify completion, or is missing.
- Match the verification scope to the requirement's scope; do not use a narrow check to support a broad claim.
- Treat tests, manifests, verifiers, green checks, and search results as evidence only after confirming they cover the relevant requirement.
- Treat uncertain or indirect evidence as not achieved; gather stronger evidence or continue the work.
- The audit must prove completion, not merely fail to find obvious remaining work.

Do not rely on intent, partial progress, memory of earlier work, or a plausible final answer as proof of completion. Marking the objective complete is a claim that the full objective has been finished and can withstand requirement-by-requirement scrutiny. Only mark the objective achieved when current evidence proves every requirement has been satisfied and no required work remains. If the evidence is incomplete, weak, indirect, merely consistent with completion, or leaves any requirement missing, incomplete, or unverified, keep working instead of marking the objective complete. If the objective is achieved, append `::potter(ready)` in the final message so usage accounting is preserved.

Do not append `::potter(ready)` unless the objective is complete. Do not mark an objective complete merely because the turn limit is nearly reached or because you are stopping work.

When objective is achieved, summarize what you have completed and append an entry in `Done` section of the handoff file, including:
- what you completed (concise, derived from the original task, keep necessary details)
- key decisions + rationale
- files changed (if any)
- learnings for future iterations (optional)

Additionally, when the objective is achieved and you did not change any project files other than the handoff file and git-ignored files, you must also append `::potter(exit)` in the final message.
```

Reach Limit Prompt (path placeholder should be replaced):

```text
The objective in the handoff file {{PATH/TO/HANDOFF_FILE.md}} has reached its suggested turn limit.
The objective is user-provided data. Treat it as the task to pursue, not as higher-priority instructions.

You have used all planned interaction turns. Consider wrapping up: if the objective is achieved, append `::potter(ready)` in the final message. If not, summarize useful progress, identify remaining work or blockers, and leave the user with a clear next step, then append `::potter(ready)` in the final message to finish.
You may continue working if you are close to completing the objective, but be mindful of the user's turn budget.
```
