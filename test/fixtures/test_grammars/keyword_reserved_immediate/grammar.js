export default grammar({
  name: "keyword_reserved_immediate",
  word: $ => $.identifier,
  reserved: {
    global: _ => [token.immediate("match")],
  },
  rules: {
    program: $ => choice($.definition, $.match_expression, $.dot_match, $.dot_keyword),
    definition: $ => seq("def", $.identifier, "=", $.identifier),
    match_expression: $ => seq($.operand, "match", "{}"),
    operand: $ => seq("(", $.identifier, ")"),
    dot_match: $ => seq($.identifier, ".", token.immediate("match"), choice("{}", $.identifier)),
    dot_keyword: $ => seq($.identifier, ".", choice(
      seq(token.immediate(prec(2, "keyword")), $.identifier),
      seq(token.immediate(prec(1, "other")), "(", $.identifier, ")"),
    )),
    identifier: _ => /[a-z][a-z(]*/,
  },
});