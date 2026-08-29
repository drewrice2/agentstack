# References

Three role prompts, run in three separate sessions, in this order:

1. [`hunter-prompt.md`](hunter-prompt.md) — finds candidate bugs aggressively.
2. [`skeptic-prompt.md`](skeptic-prompt.md) — disputes Hunter's list under a
   2x penalty for wrongly dismissing real bugs.
3. [`referee-prompt.md`](referee-prompt.md) — renders the final verdict given
   both prior outputs.

The scoring rules in each prompt are load-bearing — they shape the agent's
behavior. Do not edit them when adapting the skill to a new codebase; instead,
adjust only the framing of the target ("the provided database" → "the Rust
crate at `src/`", etc.).
