export default grammar({
  name: "keyword_reduction_boundary",
  word: $ => $.word,
  rules: {
    program: $ => choice($.comparison, $.collision, $.word),
    comparison: $ => seq("if", $.operand, token(prec(1, "is")), optional("#"), $.word),
    operand: $ => choice(seq("(", $.word, ")"), seq("a:", $.immediate_identifier)),
    collision: $ => seq("choose", choice(token(prec(1, "is")), $.immediate_identifier), ";"),
    immediate_identifier: _ => token.immediate(/[a-zA-Z_0-9#]+/),
    word: _ => /[a-zA-Z_][a-zA-Z_0-9#]*/,
  },
});