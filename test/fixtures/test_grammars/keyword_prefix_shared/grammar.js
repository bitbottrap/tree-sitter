export default grammar({
  name: "keyword_prefix_shared",
  word: $ => $.identifier,
  rules: {
    program: $ => choice(
      $.identifier,
      seq(":", $.long_keyword),
      seq("!", $.short_keyword),
    ),
    short_keyword: _ => token(prec(1, "a")),
    long_keyword: _ => "alpha",
    identifier: _ => /[a-z]+/,
  },
});
