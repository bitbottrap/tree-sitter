export default grammar({
  name: "keyword_reduction_pattern_boundary",
  word: $ => $.word,
  rules: {
    program: $ => choice($.comparison, $.collision, $.word),
    comparison: $ => seq("if", $.operand, token(prec(1, /is(?:b)?/)), optional("#"), $.word),
    operand: $ => choice(seq("(", $.word, ")"), seq("a:", $.immediate_identifier)),
    collision: $ => seq("choose", choice(token(prec(1, /is(?:b)?/)), $.immediate_identifier), ";"),
    immediate_identifier: _ => token.immediate(/[a-zA-Z_0-9#]+/),
    word: _ => /[a-zA-Z_][a-zA-Z_0-9#]*/,
  },
});