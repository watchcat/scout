# The prompt cache: what MiniMax caches

The board card said the per-user profile facts sit inside the system
prompt and break the prefix cache for every user, and asked for them to
be moved to the end. Two things turned out to be true instead.

## The facts are already last

`agent::preamble_with_profile` writes the static rules first, then the
user's facts, their search languages and shops, and the date — so the
system prompt already has the longest shared prefix it can: every run on
every account begins with the same several thousand tokens of rules (the
same deployment's, since rules for tools it has no key for are left out).

## MiniMax does not cache a prefix

`scripts/prompt-cache-probe.py` sends seven requests of about 6,000
tokens — three thousand of fixed rules, five tool definitions, a question
— varying only where and how the per-user text differs, and reads
`cached_tokens` back. On 2026-10-17 against `minimax-m3`:

| request                                            | prompt | cached |
|----------------------------------------------------|-------:|-------:|
| facts at the end of the system prompt, user 1      |  6001  |   147  |
| same, user 2 — shared prefix, different facts      |  6001  |   128  |
| user 2 again — exact repeat                        |  6001  |  6000  |
| facts in the user message, user 3                  |  6001  |   128  |
| same, user 4 — shared prefix, different facts      |  6002  |   128  |
| user 1 with one sentence appended at the very end  |  6005  |   128  |
| that again — exact repeat                          |  6005  |  6004  |

A hit only on an exact repeat; one sentence added at the end of an
otherwise identical prompt, and the whole prefix counts as new. The 128
is a constant that every request reports. Within a run, each model turn
re-sends the previous prompt with a tool result appended — exactly the
shape that misses — so on MiniMax the cache pays nothing at all, and no
arrangement of the system prompt can change that.

## What follows

- The prompt stays as it is. Its layout is the right one for a provider
  that does cache prefixes: static first, per-user last. Where the tool
  definitions land in such a provider's rendering is theirs to decide,
  and the probe, pointed at it through `LLM_BASE_URL`, will say.
- `cached_tokens` is now written on every run (`runs.cached_tokens`) and
  counted on `/metrics`, so a provider that does cache shows up as a
  number rather than a hope.
- The card closes as measured: nothing to move, and nothing to gain
  here until the provider changes.
