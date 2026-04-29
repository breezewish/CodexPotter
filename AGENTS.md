# Repository Guidelines

## Workflow Principles

- **NEVER** use git to discard or revert any changes not made by you - they are proposely changed by user and you should live with that.

## Engineering Rules

This project requires extremely high code quality and maintainability. Best engineering practices must be followed at all times.

The rules below are some typical principles. They are not exhaustive, and you must always use your best judgment to **write the cleanest code possible**. You must always clean up and refactor immediately when you see opportunities or any violations of these engineering rules.

### Core Principles: Simplicity & Readability

- Boring Code - Obvious, self-explanatory > clever, minimize cognitive load
- Single Responsibility - One function, one job
- Explicit over Implicit - Clear is better than concise
- Meaningful Abstractions - Only when they reduce cognitive load
- Keep DRY - Only if it does not conflict with the above principles

### Better Maintainability

- Keep the public API surface minimal; prefer reusing existing methods
- Don't treat "looks similar" as "equivalent"
- Abstractions must be meaningful
- Keep certainty, single source of truth - e.g. don't introduce "optional" unless absolutely necessary
- Always add clear, concise and explicit doc comments for public APIs, complex logic, and non-obvious code

### Naming Symbols

- Choosing the name that needs the least explanation, consider: verb clarity, noun specificity, context
