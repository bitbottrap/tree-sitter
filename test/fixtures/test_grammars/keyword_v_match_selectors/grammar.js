export default grammar({
  name: "keyword_v_match_selectors",
  word: $ => $.identifier,
  rules: {
    program: $ => choice(
      $.selector_expression,
      $.as_type_cast_expression,
      $.membership_expression,
      $.type_test_expression,
      seq("!", $.identifier, choice($._as, "as-", $._in, "in-", $._is, "is-")),
    ),
    selector_expression: $ => seq($.identifier, ".", $.identifier),
    as_type_cast_expression: $ => seq($._as, $.selector_expression),
    membership_expression: $ => seq($._in, $.selector_expression),
    type_test_expression: $ => seq($._is, $.selector_expression),
    _as: _ => token(prec(1, "as")),
    _in: _ => token(prec(1, "in")),
    _is: _ => token(prec(1, "is")),
    identifier: _ => /[a-z_][a-z_0-9]*/,
  },
});