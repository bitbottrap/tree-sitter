export default grammar({
  name: "keyword_vim_colors",
  word: $ => $.word,
  extras: () => [],
  rules: {
    program: $ => choice(seq("hi", "=", $.color), $.word),
    color: _ => choice(
      token.immediate("bg"), token.immediate("background"),
      token.immediate("fg"), token.immediate("foreground"),
      /#[0-9a-fA-F]{6}/, token.immediate(/[a-zA-Z]+/),
    ),
    word: _ => /[a-zA-Z]+/,
  },
});