# Repository guidance

## Pull request context

- Link the primary Linear ticket in the PR description. Read its requirements, acceptance criteria and relevant decisions before implementing or reviewing the change; state when ticket context is unavailable.
- For changes to shared APIs, queue messages, schemas or libraries, identify affected producer/consumer repositories. Reference open companion PRs as `owner/repo#123` or use an explicit branch reference `owner/repo@branch-name` when the companion code is unmerged. Links only select context for repositories already available to the reviewer.
- State which dependency revision or contract was validated. Do not assume a linked repository's default branch matches the deployed version.
- Keep the description short: what changed and why, ticket, relevant companion references, and validation performed. Include deployment order and rollback constraints when the change depends on coordinated releases. Say when there is no cross-repository impact; do not invent companion changes.
