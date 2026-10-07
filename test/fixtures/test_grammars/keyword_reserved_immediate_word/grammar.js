export default grammar({
  name: "keyword_reserved_immediate_word",
  word: $ => $.word,
  reserved: {
    global: _ => [token.immediate("match")],
  },
  rules: {
    program: $ => choice($.keyword_member, $.word_member, $.ordinary_keyword, $.ordinary_word),
    keyword_member: _ => seq(".", token.immediate("match"), "!"),
    word_member: $ => seq(".", $.word, "?"),
    ordinary_keyword: $ => seq("(", $.word, ")", "match", "!"),
    ordinary_word: $ => seq("(", $.word, ")", $.word, "?"),
    word: _ => /[a-z]+/,
  },
});
