module.exports = grammar({
  name: "keyword_m68k_conditionals",
  word: $ => $._symbol_chars,
  extras: () => [],
  inline: $ => [$._address],
  conflicts: $ => [[$.element_list], [$._start_line, $._label_colon, $.external_label]],
  rules: {
    source_file: $ => prec.left(seq(listSep($._element, $._nl), optional($._nl))),
    element_list: $ => listSep($._element, $._nl),
    _element: $ => choice($._definition, $._statement, $._standalone_label, $._block),
    _standalone_label: $ => alias($._name, $.label),
    _definition: $ => choice($.macro_definition, $.symbol_definition),
    _statement: $ => seq($._start_line, choice($.instruction, $.macro_call)),
    _block: $ => choice($.repeat, $.conditional, $.end),
    _start_line: $ => choice($._label, $._ws),
    _label: $ => $.external_label,
    _name: $ => field("name", $._identifier),
    _label_definition: $ => choice($._label_colon, seq($._name, $._ws)),
    _label_colon: $ => seq($._ws, $._name, ":"),
    external_label: $ => seq($._ws, $._name, "::"),
    _end_line: $ => seq($._ws, alias(/[^\r\n]+/, $.comment)),
    _instruction_mnemonic_op: $ => alias(
      choice(...["move", "moveq", "lea", "dbf"].map(caseInsensitive)), $.instruction_mnemonic,
    ),
    _instruction_mnemonic_noop: $ => alias(caseInsensitive("nop"), $.instruction_mnemonic),
    _symbol_definition_mnemonic: $ => alias(caseInsensitive("fequ"), $.directive_mnemonic),
    _conditional_mnemonic_exp: $ => alias(caseInsensitive("if"), $.control_mnemonic),
    _else_mnemonic: $ => alias(caseInsensitive("else"), $.control_mnemonic),
    _endif_mnemonic: $ => alias(caseInsensitive("endif"), $.control_mnemonic),
    _macro_mnemonic: $ => alias(caseInsensitive("macro"), $.control_mnemonic),
    _endm_mnemonic: $ => alias(caseInsensitive("endm"), $.control_mnemonic),
    _rept_mnemonic: $ => alias(caseInsensitive("rept"), $.control_mnemonic),
    _endr_mnemonic: $ => alias(caseInsensitive("endr"), $.control_mnemonic),
    _end_mnemonic: $ => alias(caseInsensitive("end"), $.control_mnemonic),
    instruction: $ => prec.right(choice(
      seq(
        field("mnemonic", $._instruction_mnemonic_op),
        optional(seq(".", field("size", $._size))),
        $._ws, field("operands", $.operand_list),
      ),
      field("mnemonic", $._instruction_mnemonic_noop),
    )),
    operand_list: $ => listSep($._operand, $._sep),
    _operand: $ => choice($._effective_address, $._expression),
    argument_list: $ => listSep($._operand, $._sep),
    _size: $ => $.size,
    size: () => /[bwlsdxqBWLSDXQ]/,
    macro_call: $ => prec.right(seq(
      field("name", $._identifier_nodot), optional(seq($._ws, field("arguments", $.argument_list))),
    )),
    repeat: $ => seq(
      $._start_line, $._rept_mnemonic, $._ws, field("count", $._expression), $._nl,
      field("body", $.element_list), $._nl, $._start_line, $._endr_mnemonic,
    ),
    conditional: $ => seq(
      $._start_line, $._conditional_block_start,
      optional(seq(
        field("consequent", $.element_list), $._nl,
        optional(seq($._conditional_block_else, field("alternate", $.element_list), $._nl)),
      )),
      $._conditional_block_end,
    ),
    _conditional_block_else: $ => prec.dynamic(1, seq($._start_line, $._else_mnemonic, $._nl)),
    _conditional_block_start: $ => seq($._conditional_expression, $._nl),
    _conditional_block_end: $ => seq($._start_line, $._endif_mnemonic),
    _conditional_expression: $ => seq(
      field("mnemonic", $._conditional_mnemonic_exp), $._ws, field("test", $._expression),
    ),
    end: $ => prec.right(seq($._start_line, $._end_mnemonic, listSep(/.*/, $._nl))),
    macro_definition: $ => prec(2, seq(
      $._label_definition, $._macro_mnemonic, $._end_line, $._nl,
      optional(seq(field("body", $.element_list), $._nl)), $._start_line, $._endm_mnemonic,
    )),
    symbol_definition: $ => prec(2, seq(
      $._name, field("mnemonic", $._symbol_definition_mnemonic), $._ws, field("value", $._expression),
    )),
    _effective_address: $ => choice(
      $._register, $.immediate_value, $.absolute_value, $.indirect_address_postinc,
    ),
    immediate_value: $ => seq("#", field("value", $._expression)),
    absolute_value: $ => seq("(", field("value", $._expression), ").", field("size", $._size)),
    _register: $ => choice($.data_register, $.address_register),
    data_register: () => /[dD][0-7]/,
    address_register: () => /[aA][0-7]/,
    _address: $ => field("register", $.address_register),
    indirect_address_postinc: $ => seq("(", $._address, ")+"),
    _expression: $ => choice(
      $._numeric_literal, $.string_literal, $.binary_expression, $.parenthesized_expression, $._identifier,
    ),
    parenthesized_expression: $ => seq("(", $._expression, ")"),
    binary_expression: $ => choice(...[["=", 2], [">", 3], ["&", 8]].map(([operator, precedence]) => prec.left(
      precedence, seq(
        field("left", $._expression), field("operator", alias(operator, $.operator)),
        field("right", $._expression),
      ),
    ))),
    _numeric_literal: $ => choice($.hexadecimal_literal, $.decimal_literal),
    hexadecimal_literal: () => /\$[0-9A-Fa-z]+/,
    decimal_literal: () => prec.left(/\d+/),
    string_literal: () => seq('"', repeat(prec(1, /[^"\n\\]+/)), '"'),
    _identifier: $ => choice($.symbol, $.interpolated),
    _identifier_nodot: $ => alias($.symbol_no_dot, $.symbol),
    macro_arg: () => /\\\.?[a-zA-Z][a-zA-Z0-9_]*/,
    interpolated: $ => prec.right(1, repeat1(prec.right(-1, $.macro_arg))),
    symbol_no_dot: $ => prec.right($._symbol_chars),
    symbol: $ => prec.right(seq(repeat1(seq(optional("."), $._symbol_chars)), optional("$"))),
    _symbol_chars: () => /([a-zA-Z0-9_%]|(\\?\\\.))+/,
    _sep: () => /\s*,\s*/,
    _ws: () => /[ \t]+/,
    _nl: () => /([ \t]*(\r\n|\n|\r))+/,
  },
});

function listSep(rule, separator) {
  return seq(repeat(seq(rule, separator)), rule);
}

function caseInsensitive(keyword) {
  return new RegExp(keyword.split("").map(character => `[${character}${character.toUpperCase()}]`).join(""));
}
