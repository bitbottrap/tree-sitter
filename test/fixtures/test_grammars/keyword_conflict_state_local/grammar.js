export default grammar({
  name: "keyword_conflict_state_local",
  word: $ => $.word,
  rules: {
    program: $ => choice(
      $.word,
      seq($.keyword, $.word),
      // The conflict choice includes $.word, so the "a-" literal competes
      // with a word only in states where a word is actually reachable.
      seq($.word, choice($.keyword, "a-", $.word)),
      seq(":", choice($.keyword, "a-")),
    ),
    keyword: _ => token(prec(1, "a")),
    word: _ => /[a-z]+/,
  },
});
