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
