---
description: Assess git state, create branch if needed, stage changes, and commit with AppSignal formatting
allowed-tools: Bash, Read, Grep, Glob
---

## Your task

Assess git state, create properly named branches if needed, stage changes, and commit with consistent AppSignal formatting.

## Branch creation

- Analyze changes to determine: `feat` or `fix`
- Identify subject area: mcp, k8s, api, frontend, backend, monitoring
- Format: `/{feat|fix}/subject/kebab-case-title`
- Examples: `/feat/mcp/add-incident-tools`, `/fix/frontend/chart-rendering`

## Commit format

```
<Title summarizing change>

<Brief description>

* Major change 1
* Major change 2

Fixes: <AppSignal incident URL or GitHub issue>

[changelog]
Customer-facing summary line 1
Optional context line 2
```

## Process

1. Run `git status` to assess current state
2. Create branch if on main/develop: `git checkout -b /{feat|fix}/subject/title`
3. Stage relevant files: `git add`
4. Commit with formatted message
5. Never auto-push (allow for amends)
6. Do not add "Generated with" or mention yourself
7. Don't use words like comprehensive to describe tests. If the feature/fix was tested, don't mention it at all, we assume it was tested.
