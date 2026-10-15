# Forgejo workflows

This directory intentionally contains no workflow files.

Forgejo Actions falls back to `.github/workflows/` only when `.forgejo/workflows/`
does not exist. Keeping this directory (with this non-YAML placeholder) stops the
Forgejo mirror from running the GitHub-only container publishing job, which
pushes to GHCR with GitHub's `GITHUB_TOKEN`.

Add `.yml` files here only for jobs meant to run on Forgejo.
