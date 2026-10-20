export default grammar({
  name: "keyword_prefix_deferred_long",
  word: $ => $.identifier,
  rules: {
    program: $ => choice(
      $.identifier,
      seq(":", $.long_keyword),
      seq("!", choice($.long_keyword, "alpha-")),
      seq("?", $.short_keyword),
    ),
    short_keyword: _ => token(prec(2, "a")),
    long_keyword: _ => token(prec(1, "alpha")),
    identifier: _ => /[a-z]+/,
  },
});
