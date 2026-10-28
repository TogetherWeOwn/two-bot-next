# Docs map

Browse the linked sections below, in order. Add each new guide's purpose/link to
the relevant section, not this entry point. Add a strict link here only when
creating a new section. README and each linked `index-*.md` must stay <12,000
bytes; other docs stay <=65,536 except LARGE_DOCS.

Rows use the full line `- [Purpose](relative.md)` with destinations relative to
`docs/`. Only direct README links to sibling `index-*.md` files are assembled;
section links are not recursive, and unlinked sections are orphans.

Run `python3 scripts/check_docs_index.py --json`. After a union merge/rebase, run
`python3 scripts/check_docs_index.py --dedupe --json`: it repairs exact duplicate
lines within README and each linked section, without reordering or guessing
between different labels or cross-section duplicate targets. Repair remaining
findings. Check GitHub mergeability too: local union success is not proof that
GitHub can merge. Resolve reported conflicts locally and re-run the checker.

## References

- [Find a doc](README.md)
- [References](index-references.md)

## Operations

- [Operations](index-operations.md)

## Storage and internal actions

- [Storage and internal actions](index-storage-and-internal-actions.md)

## Commands, moderation and community

- [Commands, moderation and community](index-commands-moderation-and-community.md)

## Voice

- [Voice](index-voice.md)

## Staging acceptance

- [Staging acceptance](index-staging-acceptance.md)

## Cutover and watch

- [Cutover and watch](index-cutover-and-watch.md)

## Engineering and security

- [Engineering and security](index-engineering-and-security.md)
