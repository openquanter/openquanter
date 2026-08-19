# Hooks

## `pre-push` — refuse to publish what belongs to an overlay

This repository is public. An overlay repository carries a strategy that
is not, and the two are developed side by side.

The failure this guards against is not copy-paste. It is a comment
**here** motivating framework work by quoting the overlay's numbers —
a window length, an entry threshold — in prose, with no identifier and
no digit for a reviewer to notice. That is what happened, and reading
the diff did not catch it.

**The check itself is not in this repository, deliberately.** A denylist
of private identifiers, kept here so the hook could read it, would name
every parameter it was protecting. The overlay owns the check, derives
its patterns from its own source, and publishes none of them. This hook
knows only where to ask.

```sh
git config oq.overlay /path/to/the/overlay
cp scripts/hooks/pre-push .git/hooks/pre-push
chmod +x .git/hooks/pre-push
```

With no `oq.overlay` configured it exits silently, so a machine without
an overlay is unaffected.

### What it cannot do

- **It is local.** Hooks are not committed with a clone and
  `--no-verify` skips them. This is a guard for the person who has both
  repositories open, which is the only person who can leak from one into
  the other.
- **It cannot catch paraphrase.** A comment describing a strategy's
  mechanism in fresh English passes. The rule that covers that is not
  mechanical:

  > A comment here may describe a problem the framework has. It may not
  > describe a strategy's parameters, in figures or in words. If
  > motivation needs a number, use one about the framework or the venue
  > — a latency, a tick count, a feed's quality — never one about a
  > strategy.

## Configured and unable to check

With no `oq.overlay` set the hook exits silently: a machine without an
overlay has nothing to leak from one.

With `oq.overlay` set and the checker missing it **refuses**, and says
which path it looked at. That case is not the same as the first one. An
operator who set the config asked for this protection, and passing
quietly would hand them a push that looks checked and was not — at the
moment it is likeliest to matter, since the overlay moving or renaming
the script is the likely cause.

```
pre-push: oq.overlay is set to /path/that/moved
          but /path/that/moved/tools/check_public_leak.py is not there,
          so nothing was checked.
          Fix the path, or unset it with:
              git config --unset oq.overlay
```
