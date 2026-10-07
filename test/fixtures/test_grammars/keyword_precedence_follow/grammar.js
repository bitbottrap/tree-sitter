export default grammar({
  name: "keyword_precedence_follow",
  word: $ => $.word,
  rules: {
    program: $ => choice(
      $.word,
      seq($.rgb, "(", "1", ")"),
      seq($.word, choice($.rgb, "rgb-")),
    ),
    rgb: _ => token(prec(2, "rgb")),
    word: _ => /[a-z0-9(,]+/,
  },
});