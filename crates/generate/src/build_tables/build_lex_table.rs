use std::{
    collections::{VecDeque, hash_map::Entry},
    mem,
};

use rustc_hash::{FxHashMap, FxHashSet};

use log::debug;

use super::{
    KeywordInfo, coincident_tokens::CoincidentTokenIndex, token_conflicts::TokenConflictMap,
};
use crate::{
    dedup::{SplitCriterion, split_state_id_groups},
    grammars::{LexicalGrammar, SyntaxGrammar},
    nfa::{CharacterSet, NfaCursor, NfaState},
    rules::{Symbol, SymbolView, TerminalIndex, TokenSet},
    strpool::StrPool,
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

pub(super) fn build_lex_table(
    parse_table: &mut ParseTable,
    syntax_grammar: &SyntaxGrammar,
    lexical_grammar: &LexicalGrammar,
    keyword_info: &KeywordInfo,
    coincident_token_index: &CoincidentTokenIndex,
    token_conflict_map: &TokenConflictMap,
    str_pool: &StrPool,
) -> LexTables {
    let KeywordInfo {
        tokens: keywords,
        unsafe_pairs: unsafe_keyword_pairs,
        immediate_tokens: immediate_keywords,
    } = keyword_info;
    let keyword_lex_table = if syntax_grammar.word_token.is_some() {
        let mut builder = LexTableBuilder::new(lexical_grammar);
        builder.add_state_for_tokens(keywords, str_pool);
        builder.table
    } else {
        LexTable::default()
    };

    let mut parse_state_ids_by_token_set = Vec::<(TokenSet, TokenSet, Vec<ParseStateId>)>::new();
    let starting_chars = token_conflict_map.starting_chars();
    // Only an internal terminal word token has a lexical rule; an external word
    // token contributes no keyword behavior (see `identify_keywords`).
    let word_token_index = syntax_grammar
        .word_token
        .and_then(Symbol::terminal_index)
        .map(usize::from);
    let word_start_chars: CharacterSet = word_token_index
        .map(|index| starting_chars[index].clone())
        .unwrap_or_default();
    let mut continuation_cache: FxHashMap<usize, CharacterSet> = FxHashMap::default();
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
    let mut deferred_keywords: TokenSet = unsafe_keyword_pairs
        .iter()
        .map(|&(keyword, _)| Symbol::from(keyword))
        .collect();
    deferred_keywords.insert_all(immediate_keywords);
    for (i, state) in parse_table.states.iter().enumerate() {
        let mut retained = TokenSet::new();
        if let Some(word_token_index) = word_token_index {
            let word_token = syntax_grammar.word_token.unwrap();
            for &(pair_kw, other) in unsafe_keyword_pairs {
                if state.terminal_entries.contains_key(Symbol::from(pair_kw))
                    && state.terminal_entries.contains_key(Symbol::from(other))
                    && !state.terminal_entries.contains_key(word_token)
                {
                    retained.insert(Symbol::from(pair_kw));
                    if log::log_enabled!(log::Level::Debug) {
                        debug!(
                            "Keywords - exclude {} in state {} because of conflict with {} (retaining raw keyword)",
                            str_pool.resolve(lexical_grammar.variables[usize::from(pair_kw)].name),
                            i,
                            str_pool.resolve(lexical_grammar.variables[usize::from(other)].name),
                        );
                    }
                }
            }
            // Retain keywords that a follow-token can extend mid-word.
            for pair_kw in deferred_keywords.terminals() {
                let kw_index = usize::from(pair_kw);
                let keyword = Symbol::from(pair_kw);
                let Some(&entry_id) = state.terminal_entries.get(keyword) else {
                    continue;
                };
                if retained.contains(keyword)
                    || (state.terminal_entries.contains_key(word_token)
                        && token_precedence[kw_index] <= token_precedence[word_token_index])
                {
                    continue;
                }
                let mut forced = continuation_cache
                    .entry(kw_index)
                    .or_insert_with(|| {
                        word_continuation_chars(lexical_grammar, word_token_index, kw_index)
                    })
                    .clone();
                if !state.terminal_entries.contains_key(word_token) {
                    let mut word_starts = word_start_chars.clone();
                    forced.remove_intersection(&mut word_starts);
                }
                if forced.is_empty() {
                    continue;
                }
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
                        follow.terminal_index().is_some_and(|index| {
                            follow != word_token
                                && follow != Symbol::from(pair_kw)
                                && !keywords.contains(follow)
                                && starting_chars[usize::from(index)].does_intersect(&forced)
                        })
                    });
                if let Some(follow) = culprit {
                    retained.insert(keyword);
                    if log::log_enabled!(log::Level::Debug) {
                        debug!(
                            "Keywords - retain-follow {} in state {} (follow-token {} starts with a word-continuation char; retaining raw keyword)",
                            str_pool.resolve(lexical_grammar.variables[kw_index].name),
                            i,
                            str_pool.resolve(
                                lexical_grammar.variables
                                    [usize::from(follow.terminal_index().unwrap())]
                                .name
                            ),
                        );
                    }
                }
            }
        }

        let guarded_keywords: TokenSet = immediate_keywords
            .iter()
            .filter(|keyword| {
                state.terminal_entries.contains_key(*keyword) && !retained.contains(*keyword)
            })
            .collect();
        let tokens: TokenSet = state
            .terminal_entries
            .keys()
            .copied()
            .chain(state.reserved_words.iter())
            .filter_map(|token| match token.view() {
                SymbolView::Terminal(index) => {
                    // Reservation-only immediate tokens are recognized through
                    // their non-immediate counterparts, not as syntax lookahead.
                    if lexical_grammar.variables[usize::from(index)].is_immediate
                        && !state.terminal_entries.contains_key(token)
                    {
                        return None;
                    }
                    if keywords.contains(token) && !retained.contains(token) {
                        syntax_grammar.word_token
                    } else {
                        Some(token)
                    }
                }
                SymbolView::End => Some(token),
                SymbolView::External(_)
                | SymbolView::EndOfNonTerminalExtra
                | SymbolView::NonTerminal(_) => None,
            })
            .collect();

        let mut did_merge = false;
        for entry in &mut parse_state_ids_by_token_set {
            let group_has_terminals = entry.0.terminals().next().is_some();
            let terminal_free = !group_has_terminals || tokens.terminals().next().is_none();
            if (terminal_free || entry.1 == guarded_keywords)
                && merge_token_set(
                    &mut entry.0,
                    &tokens,
                    token_conflict_map,
                    coincident_token_index,
                )
            {
                if !group_has_terminals {
                    entry.1 = guarded_keywords.clone();
                }
                did_merge = true;
                entry.2.push(i as u32);
                break;
            }
        }

        if !did_merge {
            parse_state_ids_by_token_set.push((tokens, guarded_keywords, vec![i as u32]));
        }
    }

    let mut builder = LexTableBuilder::new(lexical_grammar);
    builder.word_token = syntax_grammar.word_token.and_then(Symbol::terminal_index);
    builder.immediate_keywords = immediate_keywords.clone();
    for (tokens, guarded_keywords, parse_state_ids) in parse_state_ids_by_token_set {
        let lex_state_id =
            builder.add_state_for_tokens_with_word_guard(&tokens, &guarded_keywords, str_pool);
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
        builder.add_state_for_tokens(&TokenSet::from_iter([symbol]), str_pool);
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

    while let Some((ws, ks)) = queue.pop() {
        if !visited.insert((ws.clone(), ks.clone())) {
            continue;
        }

        let k_complete = ks.iter().any(|&s| {
            matches!(
                grammar.nfa.states[s as usize],
                NfaState::Accept { variable_index, .. } if variable_index == k_index
            )
        });

        w_cursor.reset(ws);
        let w_transitions = w_cursor.transitions();

        if k_complete {
            for t in &w_transitions {
                result = result.add(&t.characters);
            }
        }

        k_cursor.reset(ks);
        let k_transitions = k_cursor.transitions();
        for wt in &w_transitions {
            for kt in &k_transitions {
                if !wt.characters.does_intersect(&kt.characters) {
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

struct QueueEntry {
    state_id: LexStateId,
    nfa_states: Vec<u32>,
    eof_valid: bool,
    guard_id: usize,
}

struct LexTableBuilder<'a> {
    lexical_grammar: &'a LexicalGrammar,
    word_token: Option<TerminalIndex>,
    immediate_keywords: TokenSet,
    word_guards: Vec<TokenSet>,
    cursor: NfaCursor<'a>,
    table: LexTable,
    state_queue: VecDeque<QueueEntry>,
    state_ids_by_nfa_state_set: FxHashMap<(Vec<u32>, bool, usize), LexStateId>,
}

impl<'a> LexTableBuilder<'a> {
    fn new(lexical_grammar: &'a LexicalGrammar) -> Self {
        Self {
            lexical_grammar,
            word_token: None,
            immediate_keywords: TokenSet::new(),
            word_guards: vec![TokenSet::new()],
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

    fn add_state_for_tokens(&mut self, tokens: &TokenSet, str_pool: &StrPool) -> LexStateId {
        self.add_state_for_tokens_with_word_guard(tokens, &TokenSet::new(), str_pool)
    }

    fn add_state_for_tokens_with_word_guard(
        &mut self,
        tokens: &TokenSet,
        guarded_keywords: &TokenSet,
        str_pool: &StrPool,
    ) -> LexStateId {
        let guard_id = self
            .word_guards
            .iter()
            .position(|guard| guard == guarded_keywords)
            .unwrap_or_else(|| {
                self.word_guards.push(guarded_keywords.clone());
                self.word_guards.len() - 1
            });
        let mut eof_valid = false;
        let mut nfa_states: Vec<u32> = tokens
            .iter()
            .filter_map(|token| match token.view() {
                SymbolView::Terminal(index) => {
                    Some(self.lexical_grammar.variables[usize::from(index)].start_state)
                }
                SymbolView::End => {
                    eof_valid = true;
                    None
                }
                SymbolView::External(_)
                | SymbolView::EndOfNonTerminalExtra
                | SymbolView::NonTerminal(_) => {
                    unreachable!()
                }
            })
            .collect();
        if guard_id != 0
            && let Some(word) = self.word_token
            && !tokens.contains(Symbol::from(word))
        {
            self.cursor.reset(vec![
                self.lexical_grammar.variables[usize::from(word)].start_state,
            ]);
            nfa_states.extend(self.cursor.state_ids.iter().copied().filter(|&state_id| {
                !matches!(
                    self.lexical_grammar.nfa.states[state_id as usize],
                    NfaState::Advance { is_sep: true, .. }
                )
            }));
        }
        let (state_id, is_new) = self.add_state(nfa_states, eof_valid, guard_id);

        if is_new {
            debug!(
                "entry point state: {state_id}, tokens: {:?}",
                tokens
                    .iter()
                    .map(|t| match t.view() {
                        SymbolView::Terminal(index) => str_pool
                            .resolve(self.lexical_grammar.variables[usize::from(index)].name),
                        SymbolView::End => "<EOF>",
                        SymbolView::External(_)
                        | SymbolView::EndOfNonTerminalExtra
                        | SymbolView::NonTerminal(_) => unreachable!(),
                    })
                    .collect::<Vec<_>>()
            );
        }

        while let Some(QueueEntry {
            state_id,
            nfa_states,
            eof_valid,
            guard_id,
        }) = self.state_queue.pop_front()
        {
            self.populate_state(state_id, nfa_states, eof_valid, guard_id);
        }
        state_id
    }

    fn add_state(
        &mut self,
        nfa_states: Vec<u32>,
        eof_valid: bool,
        guard_id: usize,
    ) -> (LexStateId, bool) {
        self.cursor.reset(nfa_states);
        match self.state_ids_by_nfa_state_set.entry((
            self.cursor.state_ids.clone(),
            eof_valid,
            guard_id,
        )) {
            Entry::Occupied(o) => (*o.get(), false),
            Entry::Vacant(v) => {
                let state_id = self.table.states.len() as u32;
                self.table.states.push(LexState::default());
                self.state_queue.push_back(QueueEntry {
                    state_id,
                    nfa_states: v.key().0.clone(),
                    eof_valid,
                    guard_id,
                });
                v.insert(state_id);
                (state_id, true)
            }
        }
    }

    fn populate_state(
        &mut self,
        state_id: LexStateId,
        nfa_states: Vec<u32>,
        eof_valid: bool,
        guard_id: usize,
    ) {
        self.cursor.force_reset(nfa_states);

        // The EOF state is represented as an empty list of NFA states.
        let immediate_complete = guard_id != 0
            && self
                .cursor
                .completions()
                .any(|(id, _)| self.immediate_keywords.contains(Symbol::terminal(id)));
        let mut completion = None;
        for (id, prec) in self.cursor.completions() {
            if immediate_complete && self.word_token.is_some_and(|word| usize::from(word) == id) {
                continue;
            }
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
        let retained_immediate_complete = guard_id != 0
            && completion.is_some_and(|(id, _)| {
                let token = Symbol::terminal(id);
                self.immediate_keywords.contains(token)
                    && !self.word_guards[guard_id].contains(token)
            });

        // If EOF is a valid lookahead token, add a transition predicated on the null
        // character that leads to the empty set of NFA states.
        if eof_valid {
            let (next_state_id, _) = self.add_state(Vec::new(), false, guard_id);
            self.table.states[state_id as usize].eof_action = Some(AdvanceAction {
                state: next_state_id,
                in_main_token: true,
            });
        }

        for mut transition in transitions {
            let guarded_keyword_pending = guard_id != 0
                && self
                    .lexical_grammar
                    .variable_indices_for_nfa_states(&transition.states)
                    .any(|index| self.word_guards[guard_id].contains(Symbol::terminal(index)));
            if retained_immediate_complete
                && !guarded_keyword_pending
                && let Some(word) = self.word_token
            {
                transition.states.retain(|&state_id| {
                    self.lexical_grammar.variable_index_for_nfa_state(state_id) != usize::from(word)
                });
                if transition.states.is_empty() {
                    continue;
                }
            }
            if let Some((completed_id, completed_precedence)) = completion
                && !(guard_id != 0
                    && (self.word_guards[guard_id].contains(Symbol::terminal(completed_id))
                        || (retained_immediate_complete && guarded_keyword_pending))
                    && self.word_token.is_some_and(|word| {
                        self.lexical_grammar
                            .variable_indices_for_nfa_states(&transition.states)
                            .any(|index| index == usize::from(word))
                    }))
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

            let (next_state_id, _) = self.add_state(
                transition.states,
                eof_valid && transition.is_separator,
                guard_id,
            );
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
            self.table.states[state_id as usize].accept_action = Some(Symbol::End);
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
        .map(usize::from)
        .filter(|&index| !other.contains_terminal(index))
        .any(|index| {
            check_token_conflicts(index, other, token_conflict_map, coincident_token_index)
        })
    {
        return false;
    }

    if other
        .terminals()
        .map(usize::from)
        .filter(|&index| !tokens.contains_terminal(index))
        .any(|index| {
            check_token_conflicts(index, tokens, token_conflict_map, coincident_token_index)
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
        &mut LexStateSplit,
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

/// Separates lex states whose advance actions lead to different groups.
struct LexStateSplit;

impl SplitCriterion<LexState> for LexStateSplit {
    fn should_split(
        &mut self,
        left: &LexState,
        right: &LexState,
        group_ids_by_state_id: &[LexStateId],
    ) -> bool {
        lex_states_differ(left, right, group_ids_by_state_id)
    }

    /// The states of a group have the same advance actions apart from the states they lead
    /// to, so states that don't need to be split lead to the same groups. Every kept state
    /// leads to the same groups as the first one, and `state` was just found to as well.
    fn compatible_with_all(
        &mut self,
        _state: &LexState,
        _kept: &[u32],
        _group_ids_by_state_id: &[LexStateId],
    ) -> bool {
        true
    }
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

#[cfg(test)]
mod keyword_continuation_tests {
    use super::*;
    use crate::{grammars::LexicalVariable, nfa::Nfa, strpool::StrId};

    #[test]
    fn continuation_with_cyclic_word_and_keyword() {
        let advance = |state_id| NfaState::Advance {
            chars: CharacterSet::from_char('a'),
            state_id,
            is_sep: false,
            precedence: 0,
        };
        let grammar = LexicalGrammar {
            nfa: Nfa {
                states: vec![
                    NfaState::Accept {
                        variable_index: 0,
                        precedence: 0,
                    },
                    NfaState::Split(0, 2),
                    advance(1),
                    NfaState::Accept {
                        variable_index: 1,
                        precedence: 0,
                    },
                    NfaState::Split(3, 5),
                    advance(4),
                ],
            },
            variables: [2, 5]
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

        assert_eq!(
            word_continuation_chars(&grammar, 0, 1),
            CharacterSet::from_char('a')
        );
    }

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
