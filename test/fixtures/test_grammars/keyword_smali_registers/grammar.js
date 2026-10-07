export default grammar({
  name: "keyword_smali_registers",
  word: $ => $.identifier,
  rules: {
    program: $ => choice($.instruction, $.register_list, $.identifier),
    instruction: $ => seq("move", $.register, ",", $.register),
    register_list: $ => seq("{", $.register, ",", $.register, "}"),
    register: $ => choice($.variable, $.parameter),
    variable: _ => token.immediate(/v\d+/),
    parameter: _ => token.immediate(/p\d+/),
    identifier: _ => /[a-z][a-z0-9]*/,
  },
});