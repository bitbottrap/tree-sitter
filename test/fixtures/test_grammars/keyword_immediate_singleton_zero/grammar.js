export default grammar({
  name: "keyword_immediate_singleton_zero",
  word: $ => $.word,
  rules: {
    program: $ => choice($.literal, $.fixed_regex, $.singleton_class, $.grouped, $.exact_repeat),
    literal: $ => seq("literal:", choice($.word, $.literal_statement)),
    literal_statement: $ => seq(token.immediate(prec(0, "match")), $.word),
    fixed_regex: $ => seq("regex:", choice($.word, $.regex_statement)),
    regex_statement: $ => seq(token.immediate(prec(0, /match/)), $.word),
    singleton_class: $ => seq("class:", choice($.word, $.class_statement)),
    class_statement: $ => seq(token.immediate(prec(0, /m[a]tch/)), $.word),
    grouped: $ => seq("grouped:", choice($.word, $.grouped_statement)),
    grouped_statement: $ => seq(token.immediate(prec(0, /(?:match|match)/)), $.word),
    exact_repeat: $ => seq("repeat:", choice($.word, $.repeat_statement)),
    repeat_statement: $ => seq(token.immediate(prec(0, /mat{1}ch/)), $.word),
    word: _ => /[a-z]+/,
  },
});