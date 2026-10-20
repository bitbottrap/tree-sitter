export default grammar({
  name: "keyword_prefix_repro",
  word: $ => $.identifier,
  rules: {
    program: $ => choice(
      $.identifier,
      seq(":", $.long_keyword),
      seq("!", choice($.short_keyword, "a-")),
    ),
    short_keyword: _ => token(prec(1, "a")),
    long_keyword: _ => "alpha",
    identifier: _ => /[a-z]+/,
  },
});
