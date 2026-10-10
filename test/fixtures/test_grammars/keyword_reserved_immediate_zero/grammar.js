import base from "../keyword_reserved_immediate/grammar.js";

export default grammar(base, {
  name: "keyword_reserved_immediate_zero",
  rules: {
    dot_keyword: $ => seq($.identifier, ".", choice(
      seq(token.immediate("keyword"), $.identifier),
      seq(token.immediate("other"), "(", $.identifier, ")"),
    )),
  },
});
