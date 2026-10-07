export default grammar({
  name: "keyword_reserved_immediate_regex",
  word: $ => $.word,
  reserved: {
    global: $ => [$.keyword, $.general_keyword],
    unreserved: _ => [],
  },
  rules: {
    program: $ => choice(
      seq(".", choice(seq($.keyword, "!"), seq($.word, "?"))),
      seq(";", choice(seq($.general_keyword, "!"), seq($.word, "?"))),
      seq(":", reserved("unreserved", $.word), "?"),
    ),
    keyword: _ => token.immediate(prec(2, /match/)),
    general_keyword: _ => token.immediate(prec(2, /stop(?:s)?/)),
    word: _ => /[a-z]+/,
  },
});
