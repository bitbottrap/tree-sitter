export default grammar({
  name: "keyword_comment_boundary",
  word: $ => $.word,
  extras: $ => [/\s/, $.comment],
  rules: {
    program: $ => choice($.word, seq("!", $.keyword, $.word), seq($.word, choice($.keyword, "a-"))),
    keyword: $ => token(prec(1, "a")),
    word: $ => /[a-z][a-z#]*/,
    comment: $ => token(prec(2, /#[^\n]*/)),
  },
});