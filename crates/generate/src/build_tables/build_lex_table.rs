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
    nfa::{CharacterSet, NfaCursor},
    rules::{Symbol, TokenSet},
    tables::{AdvanceAction, LexState, LexStateId, LexTable, ParseStateId, ParseTable},
};

pub const LARGE_CHARACTER_RANGE_COUNT: usize = 8;
const MAX_KEYWORD_BUCKET_LENGTH: u32 = 16;
const TAIL_DEPTH: u32 = MAX_KEYWORD_BUCKET_LENGTH + 1;
const TAIL_BIT: u32 = 1 << TAIL_DEPTH;
const DEPTH_MASK: u32 = (TAIL_BIT << 1) - 1;

pub struct LexTables {
    pub main_lex_table: LexTable,
    pub keyword_lex_table: LexTable,
    pub large_character_sets: Vec<(Option<Symbol>, CharacterSet)>,
    pub keyword_buckets: KeywordBuckets,
}

#[derive(Default)]
pub struct KeywordBuckets {
    /// Maximum accepted code-point length; zero disables the runtime bound.
    pub max_length: u32,
    /// Sorted exact code-point lengths with their specialized DFAs.
    pub tables: Vec<(u16, LexTable)>,
    /// The resolved DFA restricted to lengths not handled by a bucket.
    pub residual: LexTable,
    /// Word-start characters that require the full separator-aware DFA.
    pub peek: CharacterSet,
    /// Minimum accepted code-point length in the residual, or zero if empty.
    pub residual_min: u32,
}

fn word_body_start(lexical_grammar: &LexicalGrammar, word_token: Symbol) -> CharacterSet {
    let mut cursor = NfaCursor::new(&lexical_grammar.nfa, Vec::new());
    let mut result = CharacterSet::empty();
    let start = lexical_grammar.variables[word_token.index as usize].start_state;
    let mut stack = vec![start];
    let mut seen = FxHashSet::default();
    while let Some(state) = stack.pop() {
        if !seen.insert(state) {
            continue;
        }
        cursor.reset(vec![state]);
        for transition in cursor.transitions() {
            if transition.is_separator {
                stack.extend(transition.states);
            } else {
                result = result.add(&transition.characters);
            }
        }
    }
    result
}

/// Depths 0..=16 are exact. Depth 17 is absorbing and represents every longer
/// path. This finite product graph preserves tail reachability through cycles
/// without enumerating their unbounded lengths.
const fn advance_depths(depths: u32) -> u32 {
    ((depths << 1) & DEPTH_MASK) | (depths & TAIL_BIT)
}

/// Derive a specialized DFA without rebuilding token subsets: lexical
/// precedence has already removed losing transitions from `full`.
/// Projecting live depth pairs back onto states preserves compact loops;
/// the parser still checks that the accepted source end equals the word end.
fn prune_keyword_dfa(
    full: &LexTable,
    reachable: &[u32],
    predecessors: &[Vec<usize>],
    accept_depths: u32,
) -> LexTable {
    let mut live = vec![0; full.states.len()];
    let mut queue = VecDeque::new();
    for (state_id, state) in full.states.iter().enumerate() {
        if state.accept_action.is_some() {
            live[state_id] = reachable[state_id] & accept_depths;
            if live[state_id] != 0 {
                queue.push_back(state_id);
            }
        }
    }
    while let Some(state_id) = queue.pop_front() {
        let preceding_depths = (live[state_id] >> 1) | (live[state_id] & TAIL_BIT);
        for &predecessor in &predecessors[state_id] {
            let added = preceding_depths & reachable[predecessor] & !live[predecessor];
            if added != 0 {
                live[predecessor] |= added;
                queue.push_back(predecessor);
            }
        }
    }
    if live[0] == 0 {
        return LexTable::default();
    }
    let mut result = LexTable::default();
    let mut new_ids = vec![u32::MAX; full.states.len()];
    for (state_id, &depths) in live.iter().enumerate() {
        if depths != 0 {
            new_ids[state_id] = result.states.len() as u32;
            result.states.push(LexState::default());
        }
    }
    for (state_id, state) in full.states.iter().enumerate() {
        if live[state_id] == 0 {
            continue;
        }
        let output = &mut result.states[new_ids[state_id] as usize];
        if live[state_id] & accept_depths != 0 {
            output.accept_action = state.accept_action;
        }
        for (characters, action) in &state.advance_actions {
            let destination = action.state as usize;
            if action.in_main_token && advance_depths(live[state_id]) & live[destination] != 0 {
                output.advance_actions.push((
                    characters.clone(),
                    AdvanceAction {
                        state: new_ids[destination],
                        in_main_token: true,
                    },
                ));
            }
        }
    }
    result
}

fn build_keyword_buckets(full: &LexTable, word_start: &CharacterSet) -> KeywordBuckets {
    let state_count = full.states.len();
    if state_count == 0 || full.states.iter().any(|state| state.eof_action.is_some()) {
        return KeywordBuckets::default();
    }
    let mut reachable = vec![0u32; state_count];
    let mut tail_minimum = vec![0u32; state_count];
    let mut queue = VecDeque::from([(0usize, 0u32)]);
    reachable[0] = 1;
    while let Some((state_id, depth)) = queue.pop_front() {
        for (_, action) in &full.states[state_id].advance_actions {
            if !action.in_main_token {
                continue;
            }
            let destination = action.state as usize;
            let next_depth = depth + 1;
            let bit = 1 << next_depth.min(TAIL_DEPTH);
            if reachable[destination] & bit == 0 {
                reachable[destination] |= bit;
                if bit == TAIL_BIT {
                    tail_minimum[destination] = next_depth;
                }
                queue.push_back((destination, next_depth));
            }
        }
    }
    if full.states.iter().enumerate().any(|(state_id, state)| {
        reachable[state_id] & !1 != 0
            && state
                .advance_actions
                .iter()
                .any(|(_, action)| !action.in_main_token)
    }) {
        return KeywordBuckets::default();
    }
    let mut separator_start = CharacterSet::empty();
    for (characters, action) in &full.states[0].advance_actions {
        if !action.in_main_token {
            separator_start = separator_start.add(characters);
        }
    }
    let peek = separator_start.remove_intersection(&mut word_start.clone());
    let mut predecessors = vec![Vec::new(); state_count];
    for (state_id, state) in full.states.iter().enumerate() {
        for (_, action) in &state.advance_actions {
            if action.in_main_token {
                predecessors[action.state as usize].push(state_id);
            }
        }
    }
    let mut live = vec![false; state_count];
    let mut pending = Vec::new();
    for (state_id, state) in full.states.iter().enumerate() {
        if reachable[state_id] != 0 && state.accept_action.is_some() {
            live[state_id] = true;
            pending.push(state_id);
        }
    }
    while let Some(state_id) = pending.pop() {
        for &predecessor in &predecessors[state_id] {
            if reachable[predecessor] != 0 && !live[predecessor] {
                live[predecessor] = true;
                pending.push(predecessor);
            }
        }
    }
    let mut indegree = vec![0; state_count];
    for (state_id, state) in full.states.iter().enumerate() {
        if live[state_id] {
            for (_, action) in &state.advance_actions {
                if action.in_main_token && live[action.state as usize] {
                    indegree[action.state as usize] += 1;
                }
            }
        }
    }
    let mut queue: VecDeque<_> = (0..state_count)
        .filter(|&state_id| live[state_id] && indegree[state_id] == 0)
        .collect();
    let mut bounded = vec![false; state_count];
    let mut longest = vec![0u32; state_count];
    while let Some(state_id) = queue.pop_front() {
        bounded[state_id] = true;
        for (_, action) in &full.states[state_id].advance_actions {
            let destination = action.state as usize;
            if action.in_main_token && live[destination] {
                longest[destination] = longest[destination].max(longest[state_id] + 1);
                indegree[destination] -= 1;
                if indegree[destination] == 0 {
                    queue.push_back(destination);
                }
            }
        }
    }
    let mut bucket_depths = 0;
    let mut max_length = 0;
    for (state_id, state) in full.states.iter().enumerate() {
        if bounded[state_id] && state.accept_action.is_some() {
            bucket_depths |= reachable[state_id] & (TAIL_BIT - 2);
            max_length = max_length.max(longest[state_id]);
        }
    }
    if !peek.is_empty()
        || live
            .iter()
            .zip(&bounded)
            .any(|(&live, &bounded)| live && !bounded)
    {
        max_length = 0;
    }
    let tables = (1..=MAX_KEYWORD_BUCKET_LENGTH)
        .filter(|depth| bucket_depths & (1 << depth) != 0)
        .map(|depth| {
            (
                depth as u16,
                prune_keyword_dfa(full, &reachable, &predecessors, 1 << depth),
            )
        })
        .collect();
    let residual_depths = DEPTH_MASK & !(bucket_depths | 1);
    let residual = prune_keyword_dfa(full, &reachable, &predecessors, residual_depths);
    let residual_min = full
        .states
        .iter()
        .enumerate()
        .filter(|(_, state)| state.accept_action.is_some())
        .filter_map(|(state_id, _)| {
            let depths = reachable[state_id] & residual_depths;
            if depths == 0 {
                None
            } else if depths & !TAIL_BIT != 0 {
                Some(depths.trailing_zeros())
            } else {
                Some(tail_minimum[state_id])
            }
        })
        .min()
        .unwrap_or(0);
    KeywordBuckets {
        max_length,
        tables,
        residual,
        peek,
        residual_min,
    }
}

pub fn build_lex_table(
    parse_table: &mut ParseTable,
    syntax_grammar: &SyntaxGrammar,
    lexical_grammar: &LexicalGrammar,
    keywords: &TokenSet,
    coincident_token_index: &CoincidentTokenIndex,
    token_conflict_map: &TokenConflictMap,
) -> LexTables {
    let keyword_lex_table = if syntax_grammar.word_token.is_some() {
        let mut builder = LexTableBuilder::new(lexical_grammar);
        builder.add_state_for_tokens(keywords);
        builder.table
    } else {
        LexTable::default()
    };
    let keyword_buckets = syntax_grammar
        .word_token
        .map_or_else(KeywordBuckets::default, |word| {
            build_keyword_buckets(&keyword_lex_table, &word_body_start(lexical_grammar, word))
        });
    let mut parse_state_ids_by_token_set = Vec::<(TokenSet, Vec<ParseStateId>)>::new();
    for (i, state) in parse_table.states.iter().enumerate() {
        let tokens = state
            .terminal_entries
            .keys()
            .copied()
            .chain(state.reserved_words.iter())
            .filter_map(|token| {
                if token.is_terminal() {
                    if keywords.contains(token) {
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
        keyword_buckets,
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
