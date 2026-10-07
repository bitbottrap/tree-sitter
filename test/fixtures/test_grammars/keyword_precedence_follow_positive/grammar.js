import base from "../keyword_precedence_follow/grammar.js";

export default grammar(base, {
  name: "keyword_precedence_follow_positive",
  rules: {
    word: _ => token(prec(1, /[a-z0-9(,]+/)),
  },
});
