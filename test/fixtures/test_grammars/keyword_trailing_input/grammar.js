export default grammar({
  name: "keyword_trailing_input",
  word: $ => $.word,
  rules: {
    program: $ => choice($.word, $.pair),
    pair: $ => seq($._keyword, $._keyword),
    _keyword: _ => token.immediate(prec(2, "mat")),
    word: _ => /[a-z]+/,
  },
});