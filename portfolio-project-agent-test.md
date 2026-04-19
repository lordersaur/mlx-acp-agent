# Real Project Agent Test: Senior Developer Portfolio

## Goal
Use the agent to create a new personal senior developer portfolio website from scratch.
This should test real product work, not isolated toy edits.

## User Prompt
```text
I want to create a personal portfolio website for a senior software developer.
Start from scratch in a new git repository.

First, reason about which framework and setup fit this project best. I care about:
- A polished portfolio that feels credible for senior engineering roles.
- Fast local iteration.
- Clean, maintainable frontend code.
- Easy deployment later.
- Good SEO and performance.

After choosing the stack, create the project, initialize git, build the initial website,
and verify it runs locally.

The first version should include:
- A strong homepage hero with my name, role, and positioning.
- About section.
- Selected projects section.
- Skills / technical strengths section.
- Experience timeline.
- Contact section.
- Responsive design for desktop and mobile.
- Clear visual hierarchy and polished styling.

Use realistic placeholder content where needed, but keep it easy for me to replace.
Commit the initial working version when done.
```

## Expected Good Behavior
- Asks only for truly blocking personal details; otherwise uses replaceable placeholders.
- Chooses and explains a reasonable stack before generating files.
- Creates a new project directory instead of editing this repo directly.
- Initializes a git repository in the new project.
- Builds real frontend pages/components, not just a README or landing stub.
- Uses appropriate frontend tooling and avoids over-engineering.
- Runs the app locally or provides the exact command and URL if a server is started.
- Runs validation such as formatting, linting, typecheck, or build.
- Commits the initial working version after validation passes.
- Reports exact command outputs or meaningful failure output when something fails.

## Failure Signals
- Gets stuck asking broad clarification questions.
- Creates files in the wrong repository.
- Skips framework reasoning.
- Does not initialize git.
- Claims tests/build passed without running validation.
- Produces only documentation instead of a working website.
- Hides command errors behind generic summaries.
- Leaves the user without a runnable URL or command.

## Suggested Follow-Up Prompts
```text
Make the portfolio feel more premium and less template-like.
```

```text
Replace the placeholder project cards with these three real projects: ...
```

```text
Prepare this portfolio for deployment.
```
