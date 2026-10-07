const offset = 1;

export default grammar({
  name: "keyword_precedence_offset_zero",
  word: $ => $.word,
  extras: _ => [token(prec(offset, /\s/))],
  rules: {
    program: $ => choice(
      $.word,
      seq($.keyword, $.word),
      seq($.word, choice($.keyword, token(prec(offset, "a-")))),
      seq(token(prec(offset, "!")), $.keyword, token(prec(offset, "b"))),
    ),
    keyword: _ => token(prec(1 + offset, "a")),
    word: _ => token(prec(-1 + offset, /[a-z]+/)),
  },
});