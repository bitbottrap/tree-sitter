export default grammar({
  name: "keyword_extraction_minimal",

  word: $ => $.word,

  rules: {
    source_file: $ => choice(
      // A bare word parses fine on its own: "b" -> (word).
      $.word,

      // Makes the keyword `a` valid in the initial lex state, alongside `word`.
      seq($.kw, $.word),

      // Introduces the anonymous literal "a-". This is the key: `word`
      // (/ [a-z]+ /) can match a string that conflicts with "a-", but the
      // keyword `a` cannot. That asymmetry marks the (`a`, "a-") pair as
      // unsafe for keyword substitution, so `a` is retained in the main
      // lexer only in the states where both are valid and `word` is not;
      // everywhere else `a` is substituted by the word surrogate and "ab"
      // lexes as a single (word). See README.md.
      seq($.word, choice($.kw, "a-")),
    ),

    kw: $ => token(prec(1, "a")),

    word: $ => /[a-z]+/,
  },
});
