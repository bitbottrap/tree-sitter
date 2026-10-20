export default grammar({
  name: "keyword_retained_merge",
  word: $ => $.word,
  rules: {
    program: $ => choice(
      $.word,
      seq(":", $.holder),
      seq("!", $.holder, $.word),
    ),
    holder: $ => seq("#", optional(choice($.keyword, "a-"))),
    keyword: _ => token(prec(1, "a")),
    word: _ => /[a-z]+/,
  },
});
