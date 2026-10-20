export default grammar({
  name: "keyword_precedence_adjacent_follow",
  word: $ => $.word,
  rules: {
    program: $ => choice(
      $.word,
      seq($.word, choice(seq($.keyword, "("), "a-", $.word)),
      // An unrelated state where the keyword and "a-" compete without the word
      // token, which defers the keyword's substitution to each parse state.
      seq(":", choice($.keyword, "a-")),
    ),
    keyword: _ => token(prec(1, "a")),
    word: _ => /[a-z(]+/,
  },
});
