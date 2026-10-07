export default grammar({
  name: "keyword_rego_membership",
  word: $ => $.var,
  conflicts: $ => [[$.call, $.definition]],
  rules: {
    program: $ => choice($.set, repeat1(choice($.assignment, $.definition)), seq("!", $.var, choice($._in, "in-"))),
    set: $ => seq("{", $._expression, "}"),
    _expression: $ => choice($.ref, $.call, $.membership),
    membership: $ => prec.left(1, seq($._expression, $._in, $._expression)),
    _in: _ => token(prec(1, "in")),
    ref: $ => prec.left(2, seq($.var, repeat(seq(".", $.var)))),
    call: $ => seq($.var, "(", optional($.var), ")"),
    assignment: $ => seq($.var, ":=", $._expression, optional(seq("if", $.set))),
    definition: $ => seq($.var, "(", $.var, ")", "if", $.set),
    var: _ => /[A-Za-z_]+\w*/,
  },
});