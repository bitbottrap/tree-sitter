# Keyword extraction with a conflicting literal: per-state retention

The smallest grammar that exercises the keyword-exclusion machinery in
`identify_keywords` / `build_lex_table`.

## What the grammar sets up

With a `word` token declared, tree-sitter is supposed to lex a keyword-prefixed
word as a single word token. Docs
(`docs/src/creating-parsers/3-writing-the-grammar.md`, "Keyword Extraction"):

> scan for an `identifier` first, and find `instanceofSomething`. It would then
> correctly recognize the code as invalid.

Two features interact here:

1. **Lexical precedence outranks match length.** Per the "Conflicting Tokens"
   docs, precedence (#2) is applied *before* longest-match (#3). `kw` is
   `token(prec(1, "a"))`, `word` is `/[a-z]+/` (prec 0). Wherever both are
   valid, `kw` wins on `ab` even though `word` matches longer.
2. **Keyword extraction rescues exactly this case.** When `kw` is substituted
   by the word surrogate in a parse state, the main lexer scans `ab` as one
   `word`, and the keyword consultation's whole-word check (the walked keyword
   must end exactly at the word's end) rejects the `a` prefix — so `ab` stays a
   single `word`.

The rescue only works while `kw` is substituted. The anonymous literal `"a-"`
creates a state where substitution is *not* safe: there `word` is absent and
both `kw` and `"a-"` are valid, and `kw`'s and `word`'s conflict profiles
against `"a-"` differ (`kw` matches `"a"` only; `word` can continue past it).
Substituting `kw` there would change how `"a-"` lexes.

## How the generator handles it

`identify_keywords` (`crates/generate/src/build_tables/build_tables.rs`) does
not drop `kw` from the keyword table over that conflict. It keeps `kw` in the
global keyword DFA and records the `(kw, "a-")` pair as an *unsafe pair*.
`build_lex_table` then decides per parse state: in any state where both members
of a pair are valid but `word` is absent, the raw keyword is retained in the
main lexer instead of the word surrogate. `tree-sitter generate --log` shows
both halves:

```
Keywords - add candidate kw
Keywords - defer kw (conflict with a- deferred to per-state substitution)
Keywords - include kw
Keywords - exclude kw in state 2 because of conflict with a- (retaining raw keyword)
```

The result: `ab` lexes as one `word` (the initial state substitutes `kw`, so
the consultation applies), and `"a-"` still lexes correctly in the state that
expects it (the raw `kw` is retained there). Both corpus cases pass. Run them
with:

```sh
TREE_SITTER_LANGUAGE=keyword-extraction-minimal \
  cargo test -p tree-sitter-cli -- tests::corpus_test::test_feature_corpus_files --exact --nocapture
```

## What each grammar feature contributes (all required)

- `word: $ => $.word` — enables keyword extraction at all.
- `token(prec(1, "a"))` — precedence is what would let `a` beat the longer
  `word` if `kw` were ever consulted directly in the initial state.
- `seq($.kw, $.word)` — makes `kw` valid in the *initial* state alongside
  `word`, so the substitution and the consultation apply to `ab`.
- `seq($.word, choice($.kw, "a-"))` — introduces `"a-"`, the non-keyword token
  whose asymmetric conflict with `word` marks the pair unsafe, and creates the
  state where the raw `kw` must be retained.
