export default grammar({
  name: "keyword_retained_immediate_prefix",
  word: $ => $.word,
  rules: {
    program: $ => seq($.word, ".", choice($.long_statement, $.short_statement)),
    long_statement: $ => seq(token.immediate("othermatch"), $.word),
    short_statement: $ => seq(token.immediate("other"), "(", $.word, ")"),
    word: _ => /[a-z][a-z(]*/,
  },
});