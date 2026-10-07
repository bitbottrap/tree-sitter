export default grammar({
  name: "keyword_competing_precedence",
  word: $ => $.word,
  rules: {
    program: $ => choice($.word, $.raw_statement, $.keyword_statement),
    raw_statement: $ => seq($.raw, $.word),
    keyword_statement: $ => seq(token.immediate("othermatch"), $.word),
    raw: _ => token(prec(3, /other[0-9]?/)),
    word: _ => /[a-z]+/,
  },
});