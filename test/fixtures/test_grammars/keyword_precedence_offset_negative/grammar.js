const offset = 0;

export default grammar({
  name: "keyword_precedence_offset_negative",
  word: $ => $.word,
  extras: _ => [token(prec(offset, /\s/))],
  rules: {
    program: $ => choice(
      $.word,
      seq($.keyword, $.word),
      seq($.word, choice($.keyword, "a-")),
      seq("!", $.keyword, "b"),
    ),
    keyword: _ => "a",
    word: _ => token(prec(-1 + offset, /[a-z]+/)),
  },
});