// Bash-shaped synthetic (no external scanner) for the keyword word-boundary
// fix. It reproduces the real tree-sitter-bash misparse in miniature:
//
//   - `word` may contain `-`, so `in-foo` is one word (as in bash).
//   - The `$` rule models bash's `$`-expansion states: `in` and `-` are both
//     valid there while `word` is absent. Stock's generator sees that the
//     keyword `in` and the literal `-` get different conflict status when the
//     word token is substituted in those states, so it excludes `in` from
//     keyword extraction GLOBALLY ("Keywords - exclude in because of conflict
//     with -"). The raw `in` literal then beats the longer word by lexical
//     precedence everywhere, silently splitting `in-foo` -> `in` + `-foo` and
//     accepting malformed input such as `case x in-foo) ;; esac`.
//   - The fix defers that decision per state: `in` stays in the keyword DFA
//     (whole-word semantics, so the malformed inputs ERROR) while the two
//     `$`-expansion states keep the raw literal (`$-`, `$in` still lex).
//
// The corpus below pins the fixed behavior; under the pre-fix generator the
// four `:error` cases silently parse as `in` + suffix instead.
module.exports = grammar({
  name: "keyword_bash_word_boundaries",

  word: $ => $.word,

  rules: {
    program: $ => choice(
      seq("for", $.variable_name, "in", repeat($.word), ";", "do", $.cmd, ";", "done"),
      seq("select", $.variable_name, "in", repeat($.word), ";", "do", $.cmd, ";", "done"),
      seq("case", $.word, "in", $.word, ")", optional($.cmd), ";;", "esac"),
      // Models bash's `$`-expansion: `in` and `-` valid while `word` is
      // absent. This is the state pair that makes stock exclude `in` globally.
      seq("$", choice("-", "in", /[a-zA-Z_][a-zA-Z0-9_]*/))
    ),

    variable_name: $ => /[a-zA-Z_][a-zA-Z0-9_]*/,
    cmd: $ => $.word,
    word: _ => /[a-zA-Z_-][a-zA-Z0-9_-]*/,
  },
});
