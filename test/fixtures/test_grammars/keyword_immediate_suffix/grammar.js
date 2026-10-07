export default grammar({
  name: "keyword_immediate_suffix",
  word: $ => $.word,
  rules: {
    program: $ => choice($.word, $.number, $.definition),
    number: $ => seq($.digits, choice($.suffix, seq("in", $.word))),
    definition: $ => seq("let", $.word, "=", $.word),
    suffix: _ => token.immediate("lf"),
    digits: _ => /[0-9]+/,
    word: _ => /[a-z]+/,
  },
});