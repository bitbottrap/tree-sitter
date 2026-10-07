export default grammar({
  name: "keyword_nul_input",
  word: $ => $.identifier,
  rules: {
    program: $ => $.identifier,
    identifier: _ => /[^ \t\r\n]+/,
  },
});