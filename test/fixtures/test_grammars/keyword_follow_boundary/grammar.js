export default grammar({
  name: "keyword_follow_boundary",
  word: $ => $.word,
  rules: {
    program: $ => choice(
      $.word,
      seq("!", $.keyword, "#", $.word),
      seq("?", $.keyword, "do", $.word),
      seq($.word, choice($.keyword, "is-")),
    ),
    keyword: _ => token(prec(1, "is")),
    word: _ => /[a-z][a-z#]*/,
  },
});