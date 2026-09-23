use std::{
    collections::{VecDeque, hash_map::Entry},
    mem,
};

use rustc_hash::{FxHashMap, FxHashSet};

use log::debug;

use super::{coincident_tokens::CoincidentTokenIndex, token_conflicts::TokenConflictMap};
use crate::{
    dedup::split_state_id_groups,
    grammars::{LexicalGrammar, SyntaxGrammar},
    nfa::{CharacterSet, NfaCursor, NfaState},
    rules::{Symbol, TokenSet},
    tables::{
        AdvanceAction, LexState, LexStateId, LexTable, ParseAction, ParseStateId, ParseTable,
    },
};

pub const LARGE_CHARACTER_RANGE_COUNT: usize = 8;

pub struct LexTables {
    pub main_lex_table: LexTable,
    pub keyword_lex_table: LexTable,
    pub large_character_sets: Vec<(Option<Symbol>, CharacterSet)>,
}

#[expect(
    clippy::too_many_arguments,
    reason = "all parameters are required for lex table building"
)]
pub fn build_lex_table(
    parse_table: &mut ParseTable,
    syntax_grammar: &SyntaxGrammar,
    lexical_grammar: &LexicalGrammar,
    keywords: &TokenSet,
    coincident_token_index: &CoincidentTokenIndex,
    token_conflict_map: &TokenConflictMap,
    unsafe_keyword_pairs: &[(Symbol, Symbol)],
    str_pool: &crate::strpool::StrPool,
) -> LexTables {
    let keyword_lex_table = if syntax_grammar.word_token.is_some() {
        let mut builder = LexTableBuilder::new(lexical_grammar);
        builder.add_state_for_tokens(keywords);
        builder.table
    } else {
        LexTable::default()
    };

    let mut parse_state_ids_by_token_set = Vec::<(TokenSet, Vec<ParseStateId>)>::new();
    // Follow-token retention (see the loop below): per-token starting-character
    // sets (the chars each token's NFA can begin with) and per-keyword
    // word-continuation sets.
    let starting_chars = token_conflict_map.starting_chars();
    // The characters the word token can START a word with. A follow-token whose
    // first character is also a word-start character can lex its own word, so the
    // consultation's whole-word match already handles it and retention is not
    // needed. Only a culprit character that continues the word but cannot start one
    // (vim's `#`: `[a-zA-Z_](\w|#)*` extends `is` with `#`, but no word begins with
    // `#`) forces the lexer to stop mid-keyword.
    let word_start_chars: CharacterSet = syntax_grammar
        .word_token
        .map(|w| starting_chars[w.index as usize].clone())
        .unwrap_or_default();
    let mut continuation_cache: FxHashMap<usize, CharacterSet> = FxHashMap::default();
    // Precedence-aware retention (see the loop below): the explicit precedence of
    // each token is the `precedence` carried by its NFA accept state - the value
    // `token(prec(N, ...))` compiles to (0 when no `prec` is declared).
    // hyprlang's word token `string` is `token(prec(-1, ...))`, so its accept
    // state carries -1 while the `rgb`/`true`/... literals carry 0.
    let token_precedence: Vec<i32> = {
        let mut prec = vec![0i32; lexical_grammar.variables.len()];
        for state in &lexical_grammar.nfa.states {
            if let NfaState::Accept {
                variable_index,
                precedence,
            } = state
            {
                prec[*variable_index] = *precedence;
            }
        }
        prec
    };
    // State-sensitive keyword substitution. `unsafe_keyword_pairs` (from
    // identify_keywords) lists (keyword, conflicting token) pairs whose global
    // exclusion is replaced by a per-state decision. In any parse state where
    // BOTH members of a pair are valid but the word token is NOT, retain the
    // raw keyword in the main lexer instead of the word surrogate. The keyword
    // still lives in the global keyword DFA, so states where substitution IS
    // safe still get the word surrogate.
    // (Indices are avoided deliberately: minimize_parse_table has already run, so the
    //  state-local predicate is the robust way to express "unsafe in this state".)
    let unsafe_pairs: &[(Symbol, Symbol)] = unsafe_keyword_pairs;
    let mut seen_keywords = FxHashSet::default();
    let deferred_keywords: Vec<Symbol> = unsafe_pairs
        .iter()
        .filter_map(|&(keyword, _)| seen_keywords.insert(keyword).then_some(keyword))
        .collect();
    for (i, state) in parse_table.states.iter().enumerate() {
        // Skip substitution for a keyword in this state iff the state has the
        // unsafe structure for one of its recorded conflict pairs: both the
        // keyword and the conflicting token are valid, but `word` is absent.
        let mut retained: Vec<Symbol> = Vec::new();
        if let Some(word_token) = syntax_grammar.word_token {
            for &(pair_kw, other) in unsafe_pairs {
                if state.terminal_entries.contains_key(&pair_kw)
                    && state.terminal_entries.contains_key(&other)
                    && !state.terminal_entries.contains_key(&word_token)
                {
                    retained.push(pair_kw);
                    if log::log_enabled!(log::Level::Debug) {
                        debug!(
                            "Keywords - exclude {} in state {} because of conflict with {} (retaining raw keyword)",
                            str_pool
                                .resolve(lexical_grammar.variables[pair_kw.index as usize].name),
                            i,
                            str_pool.resolve(lexical_grammar.variables[other.index as usize].name),
                        );
                    }
                }
            }
            // Follow-token retention. The rule above keys on the recorded
            // conflict token being valid in this state, but vim's `is#` breaks in
            // the expression state, where the conflict token (`identifier_token2`)
            // is NOT valid - only `match_case` (`#`) is - so the rule never fires there.
            // The general structure: `word` is absent from the parse state (the
            // parser never wants a whole word here, so the lexer is free to
            // stop mid-keyword and lex `is` + `#` separately), yet after
            // shifting the keyword, some NON-KEYWORD follow-token can START with a
            // character the word regex would use to extend the keyword into a
            // longer word (after `is`, `match_case` starts with `#`, and the word
            // regex `[a-zA-Z_](\w|#)*` extends `is` with `#`). Retain the raw
            // keyword so the main lexer still matches it directly and the
            // follow-token can lex separately. Keyword follow-tokens are excluded:
            // they are themselves substituted into the word regex, so the
            // consultation's whole-word match handles them and the parser never
            // needs the lexer to stop mid-word for one (bash's `do` after `in` must
            // NOT trigger retention, or `infoo` would split and the bash fix is
            // lost). v's cast state is unaffected: `identifier` (the word token) IS
            // valid there, so the rule cannot fire and the v cast fix is preserved.
            for &pair_kw in &deferred_keywords {
                if retained.contains(&pair_kw)
                    || !state.terminal_entries.contains_key(&pair_kw)
                    || state.terminal_entries.contains_key(&word_token)
                {
                    continue;
                }
                let kw_index = pair_kw.index as usize;
                let forced = continuation_cache.entry(kw_index).or_insert_with(|| {
                    let mut forced = word_continuation_chars(
                        lexical_grammar,
                        word_token.index as usize,
                        kw_index,
                    );
                    let mut word_starts = word_start_chars.clone();
                    forced.remove_intersection(&mut word_starts);
                    forced
                });
                // Only characters that continue the word but cannot START one
                // force the lexer to stop mid-keyword; a character that can
                // start a word lets the follow-token lex its own word and the
                // consultation handles it (bash's space/comma after `in`).
                if forced.is_empty() {
                    continue;
                }
                // Shift successors give state-local followers. A reduction
                // needs its lookahead before the stack can reach that shift, so
                // the grammar follow set is used conservatively there.
                let Some(&entry_id) = state.terminal_entries.get(&pair_kw) else {
                    continue;
                };
                let actions = parse_table.action_lists.get(entry_id);
                let reduction_follows = actions
                    .iter()
                    .any(|action| matches!(action, ParseAction::Reduce { .. }))
                    .then(|| token_conflict_map.following_tokens(kw_index));
                let culprit = actions
                    .iter()
                    .filter_map(|action| match action {
                        ParseAction::Shift { state: after, .. } => Some(*after),
                        _ => None,
                    })
                    .flat_map(|after| {
                        parse_table.states[after as usize]
                            .terminal_entries
                            .keys()
                            .copied()
                    })
                    .chain(reduction_follows.into_iter().flat_map(TokenSet::iter))
                    .find(|&follow| {
                        follow.is_terminal()
                            && follow != word_token
                            && follow != pair_kw
                            && !keywords.contains(follow)
                            && !syntax_grammar.extra_symbols.contains(&follow)
                            && starting_chars[follow.index as usize]
                                .chars()
                                .any(|character| forced.contains(character))
                    });
                if let Some(follow) = culprit {
                    retained.push(pair_kw);
                    if log::log_enabled!(log::Level::Debug) {
                        debug!(
                            "Keywords - retain-follow {} in state {} (follow-token {} starts with a word-continuation char; retaining raw keyword)",
                            str_pool.resolve(lexical_grammar.variables[kw_index].name),
                            i,
                            str_pool.resolve(lexical_grammar.variables[follow.index as usize].name),
                        );
                    }
                }
            }
            // Precedence-aware retention. The follow-token rule above requires the
            // word token to be ABSENT from the state, which is exactly backwards for
            // hyprlang: its word token (`string` = prec -1) is VALID in the value
            // state after `=`, competing head-on with the keyword (`rgb` = prec 0).
            // The main lexer resolves that competition through precedence: `rgb`
            // is a raw literal there, and the DFA builder prunes the
            // lower-precedence `string` continuation at the `rgb` accept point
            // (TokenConflictMap::prefer_transition), so `rgb` wins over the
            // strictly longer `rgb(122,` word. Substituting the word surrogate
            // removes `rgb` from the state's token set, which removes the accept
            // point and therefore the pruning - the word then wins on length and
            // the consultation's whole-word guard rejects the `rgb` prefix.
            // Retaining the raw keyword restores the accept point, so the
            // existing builder applies the same precedence pruning with no
            // runtime change.
            //
            // Gate: the word token has STRICTLY NEGATIVE precedence (the author
            // declared it a last-resort, weaker-than-default match) AND the
            // keyword's precedence is strictly greater. The negative-precedence
            // test is what makes this hyprlang-only: vim's word `keyword` and v's word
            // `identifier` are prec 0, so the rule never fires for them and the vim
            // and v fixes above are untouched. It also excludes fsharp, whose word
            // `identifier` is prec 0 but which has many POSITIVE-precedence tokens
            // (`do` = 9, ...): a bare `prec(K) > prec(W)` gate would wrongly retain
            // those in the main lexer, where they belong only in the keyword DFA.
            {
                let word_prec = token_precedence[word_token.index as usize];
                if word_prec < 0 {
                    // Iterate the filter-3-deferred keywords, NOT the whole
                    // keyword DFA: those are the keywords whose global exclusion
                    // was deferred, so retaining them restores the main lexer's
                    // coverage of exactly the tokens that need it. The global
                    // `keywords` set also contains keywords that belong ONLY in
                    // the keyword DFA (hyprlang's `true`/`false`/`on`/...); retaining
                    // those would put a prec-0 literal into the main lexer where the
                    // DFA builder prunes the lower-precedence word continuation,
                    // wrongly splitting `truexyz` into `true` + `x`.
                    for &pair_kw in &deferred_keywords {
                        if retained.contains(&pair_kw)
                            || !state.terminal_entries.contains_key(&pair_kw)
                        {
                            continue;
                        }
                        if token_precedence[pair_kw.index as usize] > word_prec {
                            retained.push(pair_kw);
                            if log::log_enabled!(log::Level::Debug) {
                                debug!(
                                    "Keywords - retain-prec {} in state {} (word token {} is valid here with lower precedence {} < {}; retaining raw keyword to restore precedence pruning)",
                                    str_pool.resolve(
                                        lexical_grammar.variables[pair_kw.index as usize].name
                                    ),
                                    i,
                                    str_pool.resolve(
                                        lexical_grammar.variables[word_token.index as usize].name
                                    ),
                                    word_prec,
                                    token_precedence[pair_kw.index as usize],
                                );
                            }
                        }
                    }
                }
            }
        }
        let tokens = state
            .terminal_entries
            .keys()
            .copied()
            .chain(state.reserved_words.iter())
            .filter_map(|token| {
                if token.is_terminal() {
                    let immediate_syntax = lexical_grammar.variables[token.index as usize]
                        .is_immediate
                        && state.terminal_entries.contains_key(&token);
                    if keywords.contains(token) && !retained.contains(&token) && !immediate_syntax {
                        syntax_grammar.word_token
                    } else {
                        Some(token)
                    }
                } else if token.is_eof() {
                    Some(token)
                } else {
                    None
                }
            })
            .collect();

        let mut did_merge = false;
        for entry in &mut parse_state_ids_by_token_set {
            if merge_token_set(
                &mut entry.0,
                &tokens,
                token_conflict_map,
                coincident_token_index,
            ) {
                did_merge = true;
                entry.1.push(i as u32);
                break;
            }
        }

        if !did_merge {
            parse_state_ids_by_token_set.push((tokens, vec![i as u32]));
        }
    }

    let mut builder = LexTableBuilder::new(lexical_grammar);
    for (tokens, parse_state_ids) in parse_state_ids_by_token_set {
        let lex_state_id = builder.add_state_for_tokens(&tokens);
        for id in parse_state_ids {
            parse_table.states[id as usize].lex_state_id = lex_state_id;
        }
    }

    let mut main_lex_table = mem::take(&mut builder.table);
    minimize_lex_table(&mut main_lex_table, parse_table);
    sort_states(&mut main_lex_table, parse_table);

    let mut large_character_sets = Vec::new();
    for (variable_ix, _variable) in lexical_grammar.variables.iter().enumerate() {
        let symbol = Symbol::terminal(variable_ix);
        builder.reset();
        builder.add_state_for_tokens(&TokenSet::from_iter([symbol]));
        for state in &builder.table.states {
            let mut characters = CharacterSet::empty();
            for (chars, action) in &state.advance_actions {
                if action.in_main_token {
                    characters = characters.add(chars);
                    continue;
                }

                if chars.range_count() > LARGE_CHARACTER_RANGE_COUNT
                    && !large_character_sets.iter().any(|(_, set)| set == chars)
                {
                    large_character_sets.push((None, chars.clone()));
                }
            }

            if characters.range_count() > LARGE_CHARACTER_RANGE_COUNT
                && !large_character_sets
                    .iter()
                    .any(|(_, set)| *set == characters)
            {
                large_character_sets.push((Some(symbol), characters));
            }
        }
    }

    LexTables {
        main_lex_table,
        keyword_lex_table,
        large_character_sets,
    }
}

/// Keyword exclusion: the set of characters the word token `w_index` can continue
/// with immediately after matching a complete instance of the keyword token
/// `k_index`.
///
/// A keyword candidate's language is a subset of the word token's (that is what
/// makes it a candidate), so every string the keyword matches, the word regex also
/// matches. This walks the two NFAs in lock-step over their shared prefixes; at
/// each joint state where the keyword has just completed, it records the characters
/// the word NFA can advance on from there - i.e. the characters that would extend
/// the keyword into a strictly longer word. For vim's `is` against the word regex
/// `[a-zA-Z_](\w|#)*` this yields the word-continuation set including `#`, which is
/// what lets the caller see that `match_case` (`#`) collides with the word scan.
///
/// Returns an empty set when the keyword can never be extended (no live word
/// continuation), so the caller's follow-token test is a no-op for such keywords.
fn word_continuation_chars(
    grammar: &LexicalGrammar,
    w_index: usize,
    k_index: usize,
) -> CharacterSet {
    let mut w_cursor = NfaCursor::new(&grammar.nfa, Vec::new());
    let mut k_cursor = NfaCursor::new(&grammar.nfa, Vec::new());

    let expand = |cursor: &mut NfaCursor, states: Vec<u32>| -> Vec<u32> {
        cursor.reset(states);
        cursor.state_ids.clone()
    };

    let mut result = CharacterSet::empty();
    let mut visited: FxHashSet<(Vec<u32>, Vec<u32>)> = FxHashSet::default();
    let mut queue: Vec<(Vec<u32>, Vec<u32>)> = Vec::with_capacity(8);
    queue.push((
        expand(&mut w_cursor, vec![grammar.variables[w_index].start_state]),
        expand(&mut k_cursor, vec![grammar.variables[k_index].start_state]),
    ));

    // Safety cap: keyword tokens are literal-ish, so the joint product is tiny;
    // the bound only guards against a pathological regex keyword.
    let mut budget = 10_000usize;
    while let Some((ws, ks)) = queue.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        if !visited.insert((ws.clone(), ks.clone())) {
            continue;
        }

        // Has the keyword completed at this joint state?
        let k_complete = ks.iter().any(|&s| {
            matches!(
                grammar.nfa.states[s as usize],
                NfaState::Accept { variable_index, .. } if variable_index == k_index
            )
        });

        w_cursor.reset(ws.clone());
        let w_transitions = w_cursor.transitions();

        if k_complete {
            // Record the continuation for this spelling, then keep walking in
            // case the keyword also accepts a longer spelling.
            for t in &w_transitions {
                result = result.add(&t.characters);
            }
        }

        // Keyword still consuming: advance both NFAs on a shared character.
        k_cursor.reset(ks);
        let k_transitions = k_cursor.transitions();
        for wt in &w_transitions {
            for kt in &k_transitions {
                let mut wc = wt.characters.clone();
                let mut kc = kt.characters.clone();
                let shared = wc.remove_intersection(&mut kc);
                if shared.is_empty() {
                    continue;
                }
                queue.push((
                    expand(&mut w_cursor, wt.states.clone()),
                    expand(&mut k_cursor, kt.states.clone()),
                ));
            }
        }
    }

    result
}

#[cfg(test)]
mod keyword_continuation_tests {
    use super::*;
    use crate::{grammars::LexicalVariable, nfa::Nfa, strpool::StrId};

    #[test]
    fn continuation_after_longer_optional_keyword_spelling() {
        let advance = |character, state_id| NfaState::Advance {
            chars: CharacterSet::from_char(character),
            state_id,
            is_sep: false,
            precedence: 0,
        };
        let accept = |variable_index| NfaState::Accept {
            variable_index,
            precedence: 0,
        };
        let grammar = LexicalGrammar {
            nfa: Nfa {
                states: vec![
                    advance('a', 1),
                    NfaState::Split(2, 3),
                    accept(0),
                    advance('b', 4),
                    NfaState::Split(5, 6),
                    accept(0),
                    advance('#', 7),
                    accept(0),
                    advance('a', 9),
                    NfaState::Split(10, 11),
                    accept(1),
                    advance('b', 12),
                    accept(1),
                ],
            },
            variables: [0, 8]
                .into_iter()
                .map(|start_state| LexicalVariable {
                    name: StrId::default(),
                    kind: crate::grammars::VariableType::Named,
                    implicit_precedence: 0,
                    start_state,
                    is_immediate: false,
                })
                .collect(),
        };

        let chars = word_continuation_chars(&grammar, 0, 1);
        assert!(
            chars.contains('#'),
            "the word can extend the keyword 'ab' with '#'"
        );
    }
}

struct QueueEntry {
    state_id: LexStateId,
    nfa_states: Vec<u32>,
    eof_valid: bool,
}

struct LexTableBuilder<'a> {
    lexical_grammar: &'a LexicalGrammar,
    cursor: NfaCursor<'a>,
    table: LexTable,
    state_queue: VecDeque<QueueEntry>,
    state_ids_by_nfa_state_set: FxHashMap<(Vec<u32>, bool), LexStateId>,
}

impl<'a> LexTableBuilder<'a> {
    fn new(lexical_grammar: &'a LexicalGrammar) -> Self {
        Self {
            lexical_grammar,
            cursor: NfaCursor::new(&lexical_grammar.nfa, vec![]),
            table: LexTable::default(),
            state_queue: VecDeque::new(),
            state_ids_by_nfa_state_set: FxHashMap::default(),
        }
    }

    fn reset(&mut self) {
        self.table = LexTable::default();
        self.state_queue.clear();
        self.state_ids_by_nfa_state_set.clear();
    }

    fn add_state_for_tokens(&mut self, tokens: &TokenSet) -> LexStateId {
        let mut eof_valid = false;
        let nfa_states = tokens
            .iter()
            .filter_map(|token| {
                if token.is_terminal() {
                    Some(self.lexical_grammar.variables[token.index as usize].start_state)
                } else {
                    eof_valid = true;
                    None
                }
            })
            .collect();
        let (state_id, is_new) = self.add_state(nfa_states, eof_valid);

        if is_new {
            debug!(
                "entry point state: {state_id}, tokens: {:?}",
                tokens
                    .iter()
                    // EOF and external tokens have no lexical-grammar
                    // variable, so only terminals can be named (yaml's entry
                    // state, for example, holds only external tokens).
                    .filter(|t| t.is_terminal())
                    .map(|t| &self.lexical_grammar.variables[t.index as usize].name)
                    .collect::<Vec<_>>()
            );
        }

        while let Some(QueueEntry {
            state_id,
            nfa_states,
            eof_valid,
        }) = self.state_queue.pop_front()
        {
            self.populate_state(state_id, nfa_states, eof_valid);
        }
        state_id
    }

    fn add_state(&mut self, nfa_states: Vec<u32>, eof_valid: bool) -> (LexStateId, bool) {
        self.cursor.reset(nfa_states);
        match self
            .state_ids_by_nfa_state_set
            .entry((self.cursor.state_ids.clone(), eof_valid))
        {
            Entry::Occupied(o) => (*o.get(), false),
            Entry::Vacant(v) => {
                let state_id = self.table.states.len() as u32;
                self.table.states.push(LexState::default());
                self.state_queue.push_back(QueueEntry {
                    state_id,
                    nfa_states: v.key().0.clone(),
                    eof_valid,
                });
                v.insert(state_id);
                (state_id, true)
            }
        }
    }

    fn populate_state(&mut self, state_id: LexStateId, nfa_states: Vec<u32>, eof_valid: bool) {
        self.cursor.force_reset(nfa_states);

        // The EOF state is represented as an empty list of NFA states.
        let mut completion = None;
        for (id, prec) in self.cursor.completions() {
            if let Some((prev_id, prev_precedence)) = completion
                && TokenConflictMap::prefer_token(
                    self.lexical_grammar,
                    (prev_precedence, prev_id),
                    (prec, id),
                )
            {
                continue;
            }
            completion = Some((id, prec));
        }

        let (transitions, has_sep) = self.cursor.transitions_and_any_sep();

        // If EOF is a valid lookahead token, add a transition predicated on the null
        // character that leads to the empty set of NFA states.
        if eof_valid {
            let (next_state_id, _) = self.add_state(Vec::new(), false);
            self.table.states[state_id as usize].eof_action = Some(AdvanceAction {
                state: next_state_id,
                in_main_token: true,
            });
        }

        for transition in transitions {
            if let Some((completed_id, completed_precedence)) = completion
                && !TokenConflictMap::prefer_transition(
                    self.lexical_grammar,
                    &transition,
                    completed_id,
                    completed_precedence,
                    has_sep,
                )
            {
                continue;
            }

            let (next_state_id, _) =
                self.add_state(transition.states, eof_valid && transition.is_separator);
            self.table.states[state_id as usize].advance_actions.push((
                transition.characters,
                AdvanceAction {
                    state: next_state_id,
                    in_main_token: !transition.is_separator,
                },
            ));
        }

        if let Some((complete_id, _)) = completion {
            self.table.states[state_id as usize].accept_action =
                Some(Symbol::terminal(complete_id));
        } else if self.cursor.state_ids.is_empty() {
            self.table.states[state_id as usize].accept_action = Some(Symbol::end());
        }
    }
}

fn check_token_conflicts(
    i: usize,
    set_without_terminal: &TokenSet,
    token_conflict_map: &TokenConflictMap,
    coincident_token_index: &CoincidentTokenIndex,
) -> bool {
    let wpr = token_conflict_map.row_words;
    let row_start = i * wpr;
    let set_bits = set_without_terminal.terminal_bits_words();

    // Does terminal i conflict with or match-prefix any terminal in the set?
    let conflict_row = &token_conflict_map.conflict_or_prefix_bits[row_start..row_start + wpr];
    for (&c, &s) in conflict_row.iter().zip(set_bits) {
        if c & s != 0 {
            return true;
        }
    }

    // Does terminal i overlap (in either direction) with any non-coincident terminal in the set?
    let overlap_row = &token_conflict_map.overlap_either_bits[row_start..row_start + wpr];
    let coincident_row = &coincident_token_index.row_bits[row_start..row_start + wpr];
    for ((&o, &s), &c) in overlap_row.iter().zip(set_bits).zip(coincident_row) {
        if o & s & !c != 0 {
            return true;
        }
    }

    false
}

fn merge_token_set(
    tokens: &mut TokenSet,
    other: &TokenSet,
    token_conflict_map: &TokenConflictMap,
    coincident_token_index: &CoincidentTokenIndex,
) -> bool {
    if tokens
        .terminals()
        .filter(|terminal| !other.contains_terminal(terminal.index as usize))
        .any(|terminal| {
            check_token_conflicts(
                terminal.index as usize,
                other,
                token_conflict_map,
                coincident_token_index,
            )
        })
    {
        return false;
    }

    if other
        .terminals()
        .filter(|terminal| !tokens.contains_terminal(terminal.index as usize))
        .any(|terminal| {
            check_token_conflicts(
                terminal.index as usize,
                tokens,
                token_conflict_map,
                coincident_token_index,
            )
        })
    {
        return false;
    }

    tokens.insert_all(other);
    true
}

fn minimize_lex_table(table: &mut LexTable, parse_table: &mut ParseTable) {
    // Initially group the states by their accept action and their
    // valid lookahead characters.
    let mut state_ids_by_signature = FxHashMap::default();
    for (i, state) in table.states.iter().enumerate() {
        let signature = (
            i == 0,
            state.accept_action,
            state.eof_action.is_some(),
            state
                .advance_actions
                .iter()
                .map(|(characters, action)| (characters.clone(), action.in_main_token))
                .collect::<Vec<_>>(),
        );
        state_ids_by_signature
            .entry(signature)
            .or_insert(Vec::new())
            .push(i as u32);
    }
    let mut state_ids_by_group_id = state_ids_by_signature
        .into_iter()
        .map(|e| e.1)
        .collect::<Vec<_>>();
    state_ids_by_group_id.sort();
    let error_group_index = state_ids_by_group_id
        .iter()
        .position(|g| g.contains(&0))
        .unwrap();
    state_ids_by_group_id.swap(error_group_index, 0);

    let mut group_ids_by_state_id = vec![0u32; table.states.len()];
    for (group_id, state_ids) in state_ids_by_group_id.iter().enumerate() {
        for state_id in state_ids {
            group_ids_by_state_id[*state_id as usize] = group_id as u32;
        }
    }

    while split_state_id_groups(
        &table.states,
        &mut state_ids_by_group_id,
        &mut group_ids_by_state_id,
        1,
        lex_states_differ,
    ) {}

    let mut new_states = Vec::with_capacity(state_ids_by_group_id.len());
    for state_ids in &state_ids_by_group_id {
        let mut new_state = LexState::default();
        mem::swap(&mut new_state, &mut table.states[state_ids[0] as usize]);

        for (_, advance_action) in &mut new_state.advance_actions {
            advance_action.state = group_ids_by_state_id[advance_action.state as usize];
        }
        if let Some(eof_action) = &mut new_state.eof_action {
            eof_action.state = group_ids_by_state_id[eof_action.state as usize];
        }
        new_states.push(new_state);
    }

    for state in &mut parse_table.states {
        state.lex_state_id = group_ids_by_state_id[state.lex_state_id as usize];
    }

    table.states = new_states;
}

fn lex_states_differ(
    left: &LexState,
    right: &LexState,
    group_ids_by_state_id: &[LexStateId],
) -> bool {
    left.advance_actions
        .iter()
        .zip(right.advance_actions.iter())
        .any(|(left, right)| {
            group_ids_by_state_id[left.1.state as usize]
                != group_ids_by_state_id[right.1.state as usize]
        })
}

fn sort_states(table: &mut LexTable, parse_table: &mut ParseTable) {
    // Get a mapping of old state index -> new_state_index
    let mut old_ids_by_new_id = (0..table.states.len()).collect::<Vec<_>>();
    old_ids_by_new_id[1..].sort_by_key(|id| &table.states[*id]);

    // Get the inverse mapping
    let mut new_ids_by_old_id = vec![0u32; old_ids_by_new_id.len()];
    for (id, old_id) in old_ids_by_new_id.iter().enumerate() {
        new_ids_by_old_id[*old_id] = id as u32;
    }

    // Reorder the parse states and update their references to reflect
    // the new ordering.
    table.states = old_ids_by_new_id
        .iter()
        .map(|old_id| {
            let mut state = LexState::default();
            mem::swap(&mut state, &mut table.states[*old_id]);
            for (_, advance_action) in &mut state.advance_actions {
                advance_action.state = new_ids_by_old_id[advance_action.state as usize];
            }
            if let Some(eof_action) = &mut state.eof_action {
                eof_action.state = new_ids_by_old_id[eof_action.state as usize];
            }
            state
        })
        .collect();

    // Update the parse table's lex state references
    for state in &mut parse_table.states {
        state.lex_state_id = new_ids_by_old_id[state.lex_state_id as usize];
    }
}
