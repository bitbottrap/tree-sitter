module.exports = grammar({
  name: "keyword_quoted_atom",
  externals: $ => [$._quoted_atom_start],
  extras: $ => [/\r?\n/, /[ \t]|\r?\n|\\\r?\n/, $.comment],
  rules: {
    source: $ => sep1($._expression, $._terminator),
    _terminator: () => prec.right(repeat1(/\r?\n/)),
    _expression: $ => choice(
      $.identifier, $.atom, $.quoted_atom, $.string,
      $.list, $.binary_operator, $.call,
    ),
    identifier: () => /[a-z_][a-zA-Z_0-9]*[?!]?/,
    atom: () => token(seq(":", /[a-zA-Z_][a-zA-Z_0-9]*[?!]?/)),
    quoted_atom: $ => seq(alias($._quoted_atom_start, ":"), choice($._double, $._single)),
    _double: $ => seq(
      '"', alias($._double_content, $.quoted_content), repeat($.interpolation), '"',
    ),
    _single: $ => seq("'", alias($._single_content, $.quoted_content), "'"),
    _double_content: () => token.immediate(/[^"\\#]+/),
    _single_content: () => token.immediate(/[^'\\#]+/),
    string: $ => $._double,
    interpolation: $ => seq("#{", $._expression, "}"),
    list: $ => seq("[", choice(sep1($._expression, ","), sep1($.pair, ",")), "]"),
    pair: $ => seq($.quoted_keyword, $._expression),
    quoted_keyword: $ => seq(choice($._double, $._single), token.immediate(/:\s/)),
    binary_operator: $ => prec.right(70, seq(
      field("left", $._expression), "=", field("right", $._expression),
    )),
    call: $ => choice(
      prec.left(seq(field("target", $.identifier), alias($._arguments, $.arguments))),
      prec.left(seq(field("target", alias($._dot, $.dot)), alias($._arguments, $.arguments))),
    ),
    _dot: $ => prec(210, seq(field("left", $._expression), ".", field("right", $.identifier))),
    _arguments: $ => seq(token.immediate("("), optional(sep1($._expression, ",")), ")"),
    comment: () => token(prec(-1, seq("#", /.*/))),
  },
});

function sep1(rule, separator) {
  return seq(rule, repeat(seq(separator, rule)));
}
