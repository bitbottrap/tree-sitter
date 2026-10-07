export default grammar({
  name: "keyword_state_local_extraction",
  word: $ => $.word,
  rules: {
    program: $ => choice(
      $.word,
      seq($.keyword, $.word),
      seq($.word, choice($.keyword, "a-")),
      seq("!", $.keyword, $.word),
    ),
    keyword: _ => token(prec(1, "a")),
    word: _ => /[a-z]+/,
  },
});