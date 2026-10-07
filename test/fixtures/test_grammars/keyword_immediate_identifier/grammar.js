export default grammar({
  name: "keyword_immediate_identifier",
  word: $ => $.word,
  rules: {
    program: $ => choice($.word, $.variable),
    variable: $ => seq("$", $.id),
    id: $ => seq(
      optional(token.immediate("::")),
      $._identifier,
      repeat(seq(token.immediate("::"), $._identifier)),
    ),
    _identifier: _ => token.immediate(/[a-zA-Z_][a-zA-Z_0-9]*/),
    word: _ => /[a-zA-Z_0-9:]+/,
  },
});