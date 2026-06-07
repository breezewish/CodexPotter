---
name: loop
description: Use only when user explicitly invokes $loop.
---

# CodexPotter Loop

This is a control protocol for running subagents in a loop pattern to "reconcile" repo to fulfill the
user request, which may be a complex task or target state.

When working in this pattern, subagents own all task execution, you are the orchestrator, you must:

- Do only the control actions this skill explicitly requires.
- Do NOT implement, review, fix issues, run checks, inspect repository.

Control parameters:

- rounds=N (default 6): maximum rounds to run

## 1. Prepare handoff

Build a `initial_prompt` for the subagent. Include user's original message (i.e. the message that
invokes this $loop skill) and helpful context from previous turns.

Next agent knows nothing about the current conversation - not even what user said or you previously
said. Thus, make sure your initial_prompt is self-contained (by supplying in context and critical
data sections), including all necessary context, including what you have previously replied and what user
previously talked, related to working on the task.

Structure `initial_prompt` with these sections:

```markdown
## Original User Request

<The user's original message text. MUST KEEP TEXT UNCHANGED. MUST copy the user's original
message text verbatim from the conversation. MUST NOT summarize, rewrite, normalize, translate,
merge, or omit any part of the user's message.>

## Important Context, Constraints, and User Preferences

<Supply from previous turns, in order to make this handoff self-contained.>

## Critical Data, Examples, and References

<Supply from previous turns, in order to make this handoff self-contained.>
```

For the "Important Context" and "Critical Data" sections:

- Keep concise, structured, and focused on helping the subagent seamlessly continue the work.
- Do not repeat infomation that is already in the user's original message.
- Do not repeat infomation comes from AGENTS.md, as subagents will read AGENTS.md by itself.
- Do not make up context or details.
- Do not enhance with your own analysis, assumptions or deductions.
- Do not inspect the repository to enrich it.

`$loop` control flow instructions MUST BE EXCLUDED in `initial_prompt`, otherwise subagent will loop again. Other skills like $xxx should be kept as is if they are part of the user message.

## 2. Create handoff file

2.1. Resolve the current git commit with `git rev-parse HEAD`, skip if not in a git repository.

2.2. Create handoff file:

```text
.codexpotter/projects_v2/{yyyy}_{mm}_{dd}_{slug}.md
```

where:
- `{yyyy} {mm} {dd}` is current local date
- `{slug}` is a short descriptive name generated from the user message (e.g., "add_login_feature", "fix_bug_123").

Do not overwrite an existing project directory.

2.3. Write handoff file in this shape:

```markdown
---
status: initial
finite_incantatem: false
git_commit: <current_git_commit>   <-- leave empty if not in a git repository
---

# Overall Goal

<initial_prompt>

## In Progress

## Todo

## Done
```

## 3. Run the Rounds

Run at most 6 rounds by default (user can change via --rounds N). In each round, do the following:

1. Start one subagent using the `potter_worker` agent.
2. Prompt the subagent with handoff file path only:

   ```md
   Work according to [this handoff file](<path to handoff md file>)
   ```

3. Wait for the subagent to finish. Subagent may take long time (e.g. 12 hours). Wait patiently, do not timeout or interrupt it.
4. Close the subagent after it finishes.
5. Report last subagent message, keep it unchanged.
6. Read handoff file, stop if the frontmatter contains the literal value `finite_incantatem: true`

If potter_worker agent is not available:
  This is a critical issue. Fail fast, do not use other alternative agents.
  Ask user to follow https://github.com/breezewish/CodexPotter to setup.

If handoff file cannot be read:
  Stop immediately and report the control-state problem.
  Do not repair it in the parent agent.

If `finite_incantatem` is not true after a round and there are more rounds left:
  Start the next round (even if subagent reports all tasks done).

## 4. Final Report

After the loop stops, report these info to user:

- Total rounds run.
- State (one of):
  - round limit reached
  - finished (when finite_incantatem==true)
- git hash change (if available) (xxxxxx -> xxxxxx).
- Last subagent messages of each round, which is the work summary. Keep details.
- Overall summary (according to last subagent messages in all rounds).

Do not add implementation analysis, code review, extra verification, or recommendations beyond the loop outcome.

## How to resume

If what user passed in is an existing handoff file to resume (iterate more rounds), you should:

1. Reset handoff file status to `open`, finite_incantatem to `false`.
2. Follow the same steps in "3. Run the Rounds" section.

## How to continue

If current loop was paused / interrupted and user wants to continue, you could simply send `continue` prompt to the subagent.

## Error Handling

When subagent fails or encounters an error, you should automatically resume it by sending a `continue` prompt.
Keep retrying several times if it keeps failing. This resumes the subagent with previous working state,
ensures maximum continuity.

Only as the last resort, close the subagent and start a new one.

## Feedback / Report Principles

- Keep concise, structured, readable.
- Before start the loop, tell user how many rounds to run, handoff file path.
- Do not mention this control flow details like "finite_incantatem", etc. User only know "rounds", "handoff file".
- Feedback using the same language as user message.
