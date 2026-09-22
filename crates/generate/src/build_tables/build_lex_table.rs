use std::{
    collections::{BTreeSet, VecDeque, hash_map::Entry},
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

pub struct LexTables {
    pub main_lex_table: LexTable,
    pub keyword_lex_table: LexTable,
    pub large_character_sets: Vec<(Option<Symbol>, CharacterSet)>,
    /// Keyword length buckets: the byte-length of the longest keyword match in
    /// `keyword_lex_table`, or 0 when no length bound is sound (a peek
    /// separator or a variable-length keyword). The runtime uses it to skip
    /// the keyword consultation for longer words. See `compute_keyword_depths`.
    pub max_word_length: u16,
    /// Keyword length buckets: one DFA per keyword byte-length,
    /// `(length, table)`, sorted by length. Empty only when the keyword DFA
    /// admits no byte-length analysis (see `compute_keyword_depths`).
    /// The runtime dispatches a consultation to the bucket for the word's
    /// length, so a walk can only accept at exactly the word's byte length.
    /// See `build_keyword_buckets`.
    pub keyword_bucket_tables: Vec<(u16, LexTable)>,
    /// Keyword length buckets: DFA over the keywords not covered by any
    /// bucket. The dispatcher routes gap lengths and out-of-range dispatch
    /// here; the complete full DFA stays the `state == 0` legacy anchor.
    /// See `build_keyword_buckets`.
    pub keyword_residual_table: LexTable,
    /// Keyword length buckets: first characters on
    /// which the root separator can fire (separator-start ∩ word-start). Empty
    /// for disjoint grammars. Non-empty => the dispatcher peeks the first
    /// character and routes such words to the full DFA (the separator may
    /// consume leading bytes, so the word's length no longer equals a literal
    /// length). See `compute_keyword_depths`.
    pub keyword_peek_set: CharacterSet,
    /// Keyword length buckets: minimum byte length at which the
    /// residual (reduced) DFA can accept; 0 = no known bound (empty residual
    /// or dropped pathological symbols). The dispatcher answers gap entries
    /// below this length with a bare `return false` instead of the reduced
    /// DFA -- a word shorter than every residual keyword cannot match it.
    /// See `build_keyword_buckets`.
    pub keyword_residual_min: u16,
}

/// First characters that can begin the *body* of `word_token` -- i.e. the
/// characters that can appear at the runtime consultation's start position.
/// The word token's NFA carries the same leading separator as the keyword DFA
/// (prepended in `expand_tokens`), so the body's first characters are those on
/// non-separator transitions reachable through zero or more separator
/// transitions. Used to decide whether the keyword DFA's root separator edge
/// can ever fire during a consultation (see `compute_keyword_depths`).
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

/// Result of `compute_keyword_depths`.
/// - `max_len`: maximum literal byte length over keyword accept states.
/// - `lengths[s]`: the SET of byte lengths at which state `s` is reachable.
/// - `peek`: the (possibly empty) set of first characters on which the root
///   separator can fire (separator-start ∩ word-start intersection).
/// - `variable[s]`: state `s` is a consultation-reachable keyword state on or
///   downstream of a main-token cycle -- a keyword that can pump (verilog's
///   `PATHPULSE$<ident>$<ident>`), hence PROVABLY unbounded: pumping the cycle
///   yields matches at arbitrarily many lengths. A bounded variable-length
///   expression (`[a-c]{2,4}`) has an acyclic DFA and is NOT flagged.
struct KeywordDepths {
    max_len: u16,
    lengths: Vec<Vec<usize>>,
    peek: CharacterSet,
    variable: Vec<bool>,
}

/// Byte width of a main-token edge: every character the edge accepts must
/// have the same UTF-8 length (a keyword literal advances one codepoint per
/// step; a mixed-width set would make the byte cost path-dependent). `None`
/// when the edge is mixed-width (the caller bails out of bucketing).
fn edge_byte_width(chars: &CharacterSet) -> Option<usize> {
    let mut w = None;
    for r in chars.ranges() {
        // len_utf8 is monotonic in the codepoint, so a range has
        // uniform width iff its endpoints agree.
        let width = r.start().len_utf8();
        if width != r.end().len_utf8() {
            return None;
        }
        match w {
            None => w = Some(width),
            Some(b) if b == width => {}
            Some(_) => return None,
        }
    }
    w
}

/// Returns `None` (the grammar is not bucketable -- the full keyword DFA stays
/// the sole keyword function) when a mid-token separator or a
/// mixed-width edge makes byte-length bucketing unsound; otherwise a
/// `KeywordDepths` (see the struct): `max_len` is the maximum literal byte
/// length over BOUNDED keyword accept states, `lengths[s]` is the EXACT SET
/// of byte lengths at which state `s` is reachable from the root over
/// main-token edges (bounded DFS, gaps included), and `peek` is the (possibly
/// empty) set of first characters on which the root separator can fire -- the
/// separator-start ∩ word-start intersection.
/// When `peek` is non-empty the dispatcher must route a word beginning with one
/// of those characters to the full DFA (the separator can consume leading bytes,
/// so the word's length no longer equals a literal length); when empty, every
/// consultation matches exactly its literal and a bucket miss is final.
///
/// `lengths` is a set per state, not a single minimum, because DFA minimization
/// merges accept states of different-length literals that share a suffix:
/// fsharp's `bool` (4 bytes) and `false` (5 bytes) accept `sym_bool` in ONE
/// merged state. A minimum-distance BFS would place `sym_bool` only in bucket 4,
/// and a 5-byte `false` would miss bucket 5 (the runtime's byte-equality check
/// makes over-length accepts harmless, but a missing accept is a real bug).
/// Every keyword symbol must land in EVERY bucket where it is acceptable.
///
/// Returns a `KeywordDepths`.
fn compute_keyword_depths(
    table: &LexTable,
    word_start: &CharacterSet,
) -> Option<KeywordDepths> {
    let n = table.states.len();
    if n == 0 {
        return None;
    }

    // Leading-separator soundness. The keyword DFA's root carries the grammar's
    // separator as a SKIP edge (prepended in `expand_tokens`). A runtime
    // consultation restarts the lexer at the word token's first byte, so that
    // edge can only fire if the word's first character is also a separator
    // character. When the separator's start charset is DISJOINT from the word
    // token's start charset, no consultation can consume separator bytes, so
    // every keyword match is exactly its literal and exact-length bucketing is
    // sound -- this admits grammars whose separator is not pure whitespace
    // (tcl, fsharp, purescript) as long as it can never fire at a word start.
    // When the two sets DO intersect (agda's `\`), bucketing is still sound
    // for words that don't begin with an intersection character; those that do
    // are routed to the full DFA by a first-character peek in the dispatcher.
    let mut sep_start = CharacterSet::empty();
    for (chars, action) in &table.states[0].advance_actions {
        if !action.in_main_token {
            sep_start = sep_start.add(chars);
        }
    }
    let peek = {
        let (mut sep, mut word) = (sep_start, word_start.clone());
        sep.remove_intersection(&mut word)
    };

    // Mid-token separator soundness (computed
    // order-independently). 0-1 BFS for the minimum BYTE cost to each state
    // (separator edge = 0, main-token edge = its UTF-8 byte length). A
    // consultation on a non-peek word restarts at the word's first byte, so the
    // root's own separator edge cannot fire; a separator edge on a state whose
    // minimum cost is >= 1 (i.e. reachable only after consuming keyword bytes)
    // means a match could consume separator bytes mid-word and exceed its
    // literal's length, which exact-length bucketing cannot represent -- bail.
    // A mixed-width main edge (a set of characters of differing UTF-8 length)
    // makes the byte cost path-dependent -- also bail. Running the BFS to
    // completion before checking keeps this independent of traversal order.
    let mut dist = vec![usize::MAX; n];
    dist[0] = 0;
    let mut deque: VecDeque<usize> = VecDeque::new();
    deque.push_back(0);
    while let Some(u) = deque.pop_front() {
        let du = dist[u];
        for (chars, action) in &table.states[u].advance_actions {
            let v = action.state as usize;
            if v >= n {
                continue;
            }
            let w = if action.in_main_token {
                edge_byte_width(chars)?
            } else {
                0
            };
            if du + w < dist[v] {
                dist[v] = du + w;
                if w == 0 {
                    deque.push_front(v);
                } else {
                    deque.push_back(v);
                }
            }
        }
    }
    for (u, state) in table.states.iter().enumerate() {
        if dist[u] >= 1
            && dist[u] != usize::MAX
            && state
                .advance_actions
                .iter()
                .any(|(_, action)| !action.in_main_token)
        {
            return None;
        }
    }

    // Set-of-byte-lengths reachability over MAIN-TOKEN edges only, from the
    // root. Bucketing keys on byte length because the runtime's byte-equality
    // check compares bytes, and a keyword literal may contain multi-byte UTF-8
    // characters (e.g. fennel's `λ`): a codepoint-depth bound would be too
    // small and would wrongly skip the consultation. The result is a SET per
    // state, not a minimum: minimization merges same-suffix literals of
    // different length into one accept state (fsharp's `true`/`false` share
    // `sym_bool`), and the symbol must be bucketed at EVERY length it can
    // match.
    //
    // Which states a consultation can actually occupy: the root plus states
    // reachable from it through main-token edges. (A non-peek consultation
    // starts at the root and, by the gate above, can never take a separator
    // edge, so it stays inside this set.) Lengths are propagated only over
    // these AND only over states from which an accept is reachable via
    // main-token edges ("live"): the keyword DFA can carry cycles that never
    // accept (verilog has a live-but-separator-only-reachable self-loop), and
    // a length-set walk over a cycle would add a new length per iteration
    // until the u16 bail. States outside this intersection are unreachable by
    // a consultation, so their lengths are irrelevant; any keyword whose only
    // accept path leaves this set falls to the residual DFA.
    let mut mt_reach = vec![false; n];
    mt_reach[0] = true;
    let mut stack: Vec<usize> = vec![0];
    while let Some(u) = stack.pop() {
        for (_, action) in &table.states[u].advance_actions {
            let v = action.state as usize;
            if v < n && action.in_main_token && !mt_reach[v] {
                mt_reach[v] = true;
                stack.push(v);
            }
        }
    }
    let mut live = vec![false; n];
    let mut rev: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (u, state) in table.states.iter().enumerate() {
        for (_, action) in &state.advance_actions {
            let v = action.state as usize;
            if v < n && action.in_main_token {
                rev[v].push(u);
            }
        }
    }
    for (u, state) in table.states.iter().enumerate() {
        if state.accept_action.is_some() {
            live[u] = true;
            stack.push(u);
        }
    }
    while let Some(u) = stack.pop() {
        for &p in &rev[u] {
            if !live[p] {
                live[p] = true;
                stack.push(p);
            }
        }
    }
    let prop: Vec<bool> = (0..n).map(|u| mt_reach[u] && live[u]).collect();

    // Kahn topological order over the propagation subgraph. If states remain
    // after peeling, a consultation-reachable state sits on a main-token cycle
    // that can still accept: some keyword regex is not a fixed literal (e.g.
    // `a+b`), so exact-length bucketing is unsound -- bail.
    let mut indeg = vec![0usize; n];
    for (u, state) in table.states.iter().enumerate() {
        if !prop[u] {
            continue;
        }
        for (_, action) in &state.advance_actions {
            let v = action.state as usize;
            if v < n && action.in_main_token && prop[v] {
                indeg[v] += 1;
            }
        }
    }
    let mut queue: VecDeque<usize> = (0..n).filter(|&u| prop[u] && indeg[u] == 0).collect();
    let mut topo: Vec<usize> = Vec::with_capacity(n);
    while let Some(u) = queue.pop_front() {
        topo.push(u);
        for (_, action) in &table.states[u].advance_actions {
            let v = action.state as usize;
            if v < n && action.in_main_token && prop[v] {
                indeg[v] -= 1;
                if indeg[v] == 0 {
                    queue.push_back(v);
                }
            }
        }
    }
    // Kahn's algorithm emits a topological order of exactly the states NOT
    // reachable from any cycle: a state on a cycle, or downstream of one, never
    // reaches indegree 0. So `topo` is the literal-keyword subgraph, and the
    // consultation-reachable live states it omits (`prop && !in_topo`) are
    // exactly the VARIABLE-length keyword states -- a keyword regex that can
    // pump (verilog's `PATHPULSE$<ident>$<ident>` self-loops on identifier
    // characters). Their match lengths are infinite, so they are returned as
    // `variable`: `build_keyword_buckets` lets them RIDE buckets (at exactly
    // the lengths the bounded DFS reached, gaps included) but never CREATE
    // one, keeps them in the residual for out-of-range dispatch, and their
    // presence disables the over-length skip (a match can be arbitrarily
    // long). No bail: the grammar still buckets its literal keywords.
    let in_topo: Vec<bool> = {
        let mut v = vec![false; n];
        for &u in &topo {
            v[u] = true;
        }
        v
    };
    let variable: Vec<bool> = (0..n).map(|u| prop[u] && !in_topo[u]).collect();

    // Exact set-of-byte-lengths reachability: a bounded DEPTH-FIRST search of
    // the consultation subgraph over its (state, byte-length) PAIRS, from the
    // root. The subgraph already IS the graph -- `advance_actions` is its
    // adjacency list -- so no separate graph structure is needed. The visited
    // set is keyed on the PAIR, not the state: re-reaching a state at a
    // DIFFERENT length is a new pair and IS re-explored -- that is how cycles
    // are pumped, so `(?:[a-c]{2})+` revisits its accept state at 2, 4, 6,
    // ... and never at 3 or 5, and the resulting sets are EXACT, gaps and
    // all -- no period/residue approximation is needed anywhere. A pair is
    // skipped only when that exact (state, length) was already expanded:
    // everything downstream of a pair depends only on the pair, not the path
    // taken, so skipping is a pure diamond shortcut. (Every edge adds >= 1
    // byte, so the length strictly increases along any path: the pair graph
    // is acyclic up to the cap and the search terminates regardless.)
    //
    // The DFS is bounded at `cap` bytes. Every BOUNDED accept state sits on
    // acyclic paths only (a path repeating a state would put a cycle upstream
    // of it, making it `variable`), so its lengths are at most
    // (states on a simple path) x (max edge width) = 4n, and every bucket
    // length is a bounded accept length -- so cap = 4n covers EVERY bucket
    // length EXACTLY for every symbol, variable or not. Lengths beyond cap
    // are reachable only by pumping a cycle, which only `variable` symbols
    // do; those keep the residual DFA for out-of-range dispatch, so no
    // coverage beyond cap is ever consulted.
    let cap = 4 * n;
    let mut lengths: Vec<Vec<usize>> = vec![Vec::new(); n];
    if prop[0] {
        lengths[0].push(0);
        let mut seen = vec![vec![0u64; cap / 64 + 1]; n];
        seen[0][0] = 1;
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        while let Some((u, l)) = stack.pop() {
            for (chars, action) in &table.states[u].advance_actions {
                let v = action.state as usize;
                if v >= n || !action.in_main_token || !prop[v] {
                    continue;
                }
                // Edge weight = UTF-8 byte length, valid only when every
                // character the edge accepts has the same length (a keyword
                // literal advances one codepoint per step; a mixed-width set
                // would make the byte cost path-dependent, so bail).
                let w = edge_byte_width(chars)?;
                let nl = l + w;
                if nl > cap {
                    continue;
                }
                let (word, bit) = (nl / 64, nl % 64);
                if seen[v][word] & (1u64 << bit) == 0 {
                    seen[v][word] |= 1u64 << bit;
                    lengths[v].push(nl);
                    stack.push((v, nl));
                }
            }
        }
    }

    // Longest BOUNDED keyword literal (bytes) = max byte length over
    // non-variable accept states. A variable symbol's DFS lengths run to
    // `cap`, which is not a real bound -- but any variable symbol disables
    // the over-length skip anyway. Accept states reachable only through
    // separator edges have no main-token length: they are unreachable for a
    // consultation on a non-peek word, so excluding them from the bound is
    // sound (peek grammars disable the bound entirely anyway).
    let mut max_len = 0usize;
    for (u, state) in table.states.iter().enumerate() {
        if state.accept_action.is_some()
            && !variable[u]
            && let Some(&m) = lengths[u].iter().max()
        {
            max_len = max_len.max(m);
        }
    }

    u16::try_from(max_len)
        .ok()
        .map(|len| KeywordDepths {
            max_len: len,
            lengths,
            peek,
            variable,
        })
}

/// Keyword length buckets: partition the keyword
/// tokens by the byte-length of the literals they accept, and build one DFA per
/// length. Each bucket DFA is the token subset's DFA PRUNED to the words of
/// exactly its length: `build_bucket_dfa` builds the subset DFA (cyclic
/// keywords keep their compact loops), then a (state, byte-length) pair DFS
/// keeps only the states, edges, and accepts that lie on a path reaching an
/// accept at EXACTLY that length. So bucket L accepts precisely the length-L
/// words the full keyword DFA accepts, and carries NO dead branches: fsharp's
/// `bool` (minimized `true`|`false` into one accept state) contributes the
/// `true` path to bucket 4 and the `false` path to bucket 5, and NEITHER
/// bucket carries the other's chain -- the unpruned subset DFA re-inflates
/// each riding symbol's whole regex and leaves the cross-length chain as
/// unreachable code. A word of length L is walked through bucket L alone and
/// only reported as a keyword when it is exactly one of the bucket's literals.
///
/// Placement is ONE coverage rule for every keyword: each symbol's accept
/// states yield the SET of byte lengths at which it can match, and the symbol
/// rides in every bucket whose length lies in that set. The DFS in
/// `compute_keyword_depths` computes these sets EXACTLY, gaps and all, for
/// EVERY symbol up to its search cap -- which covers every bucket length:
/// `[a-c]{2}(?:[a-c]{2})?` matches 2 and 4 but never 3, so it rides buckets 2
/// and 4 only (a plain literal contributes one length; minimization merging
/// contributes several; a bounded variable-length expression contributes its
/// span, possibly with holes). A symbol on or downstream of a main-token
/// cycle (`variable`) has INFINITE coverage, but the DFS still computes it
/// exactly up to its search cap, and every bucket length is at or below that
/// search cap -- `(?:[a-c]{2})+` rides even buckets only, verilog's
/// `PATHPULSE$…` rides every length >= 13 up to the bucket length cap.
/// A symbol with NO computable coverage (an accept state a
/// consultation can never reach) is not bucketable, and a symbol matchable
/// only above the bucket length cap is not bucketable AT those lengths; both
/// live in the residual (see `MAX_KEYWORD_BUCKET_LENGTH`). Because each bucket
/// is pruned to accept-reaching paths of its exact length, a symbol rides
/// bucket L exactly when its accept is reachable at L -- the pruned bucket's
/// language is EXACT, never over- or under-approximating the length-L slice of
/// the full keyword DFA.
///
/// Buckets exist only for keyword byte lengths up to this cap. Keywords
/// matchable at longer lengths live in the residual DFA instead, which the
/// dispatcher reaches through its out-of-range tail (and through gap lengths
/// at or above the residual's minimum). This is the design's worst-case
/// bound: at most `MAX_KEYWORD_BUCKET_LENGTH` bucket DFAs and a dispatcher
/// switch with at most `MAX_KEYWORD_BUCKET_LENGTH + 1` case groups for ANY
/// grammar, so pathologically long or densely-lengthed keyword sets cannot
/// inflate the generated code per distinct length. The longest keywords in
/// known grammars are 19 bytes (verilog), so only verilog's two 18/19-byte
/// literals move to the residual.
const MAX_KEYWORD_BUCKET_LENGTH: usize = 16;

/// Returns `(buckets, residual, residual_min)` where `buckets` is sorted by
/// length, `residual` is a DFA over every keyword token NOT fully covered by
/// the buckets, and `residual_min` is the minimum byte length at which
/// `residual` can accept (0 = no known bound). The residual is empty unless
/// some keyword is matchable above the bucket cap or is variable-length:
/// every keyword matchable only at lengths within the cap has a non-empty
/// coverage and lands in a bucket for each of its lengths.
/// Building it structurally rather than assuming emptiness keeps the
/// conservative drop cases honest -- a keyword dropped from the buckets
/// (unreachable in `lengths`, zero-length, or longer than `u16`) still matches
/// via the residual, which is the operative fallback for gap lengths and
/// out-of-range dispatch. The complete full DFA remains the `state == 0`
/// legacy anchor for callers that predate bucketing.
///
/// `residual_min` lets the dispatcher answer gap lengths BELOW the residual's
/// minimum match length with a bare `return false`: a word shorter than every
/// residual keyword cannot match it. The bound is the minimum coverage-minimum
/// over the residual's symbols; a dropped pathological symbol has no
/// computable minimum, so the bound degrades to 0 (no bound -- every gap still
/// dispatches to the reduced DFA).
fn build_keyword_buckets(
    lexical_grammar: &LexicalGrammar,
    keywords: &TokenSet,
    table: &LexTable,
    lengths: &[Vec<usize>],
    variable: &[bool],
) -> (Vec<(u16, LexTable)>, LexTable, u16) {
    // Per-symbol match-length COVERAGE, unioned over the symbol's accept
    // states, in two parts (both EXACT up to the DFS cap, which covers every
    // bucket length):
    // - `exact`: lengths contributed by BOUNDED accept states. These CREATE
    //   bucket lengths, and their symbols ride those buckets.
    // - `cyclic`: lengths contributed by VARIABLE accept states (on or
    //   downstream of a main-token cycle, hence infinite coverage). These
    //   never create a bucket -- an unbounded keyword like m68k's `l[0-9]+`
    //   would otherwise create one bucket per length up to the cap -- they
    //   only RIDE buckets created by bounded keywords, at exactly the
    //   lengths their DFS reached: `(?:[a-c]{2})+` rides even buckets only,
    //   a weight-1 self-loop rides every length >= its minimum.
    // A symbol may have both (minimization can merge a literal accept state
    // and a cyclic one for the same token); its coverage is the union.
    let mut exact: FxHashMap<Symbol, BTreeSet<usize>> = FxHashMap::default();
    let mut cyclic: FxHashMap<Symbol, BTreeSet<usize>> = FxHashMap::default();
    for (u, state) in table.states.iter().enumerate() {
        let Some(sym) = state.accept_action else { continue };
        if !keywords.contains(sym) {
            continue;
        }
        let target = if variable[u] { &mut cyclic } else { &mut exact };
        target.entry(sym).or_default().extend(lengths[u].iter().copied());
    }

    // Bucket lengths: every length at which some BOUNDED keyword accept state
    // is reachable (an exact set), up to `MAX_KEYWORD_BUCKET_LENGTH`. Unbounded
    // (variable) keywords ride in existing buckets, they create none. Length 0
    // is reserved: the runtime passes 0 to mean "no length known" (the wasm
    // path and any caller without a word length), which must reach the full
    // DFA, so buckets start at 1. A zero-byte keyword is pathological and
    // unreachable anyway -- word tokens are at least one byte -- so it covers
    // no bucket length and stays in the residual. A bounded symbol matchable
    // ABOVE the cap (or at no bucketable length at all) is recorded in
    // `over_cap` and joins the residual even when it also rides a bucket, so
    // its long matches still find a DFA through the dispatcher's out-of-range
    // tail.
    let mut bucket_lens: BTreeSet<u16> = BTreeSet::new();
    let mut over_cap: TokenSet = TokenSet::new();
    for (sym, lens) in &exact {
        let mut over = false;
        for &len in lens {
            if len > MAX_KEYWORD_BUCKET_LENGTH {
                over = true;
                continue;
            }
            if let Ok(l) = u16::try_from(len)
                && l >= 1
            {
                bucket_lens.insert(l);
            }
        }
        if over {
            over_cap.insert(*sym);
        }
    }

    let mut bucketed: TokenSet = TokenSet::new();
    let mut buckets = Vec::new();
    for &len in &bucket_lens {
        // ONE rule: a symbol rides bucket `len` iff its (exact) coverage
        // contains `len` -- from a bounded accept state or a cyclic one. This
        // set drives only the residual bookkeeping (`bucketed`); the bucket's
        // DFA content comes from the pruned subset DFA below, whose accepts
        // are exactly these symbols (an accept is live at `len` iff its
        // coverage holds `len`).
        let l = len as usize;
        let token_set: TokenSet = keywords
            .iter()
            .filter(|&sym| {
                exact.get(&sym).is_some_and(|s| s.contains(&l))
                    || cyclic.get(&sym).is_some_and(|s| s.contains(&l))
            })
            .collect();
        for sym in token_set.iter() {
            bucketed.insert(sym);
        }
        let bucket_table = build_bucket_dfa(lexical_grammar, &token_set, l);
        // The pruned bucket's accepts must equal the placement set: the
        // placement DFS and the bucket liveness DFS walk the same
        // (state, length) pair graph, so a symbol is in `token_set` exactly
        // when an accept is reachable at `l`.
        debug_assert!(
            token_set.iter().all(|sym| bucket_table
                .states
                .iter()
                .any(|s| s.accept_action == Some(sym)))
                && bucket_table.states.iter().all(|s| {
                    s.accept_action.is_none_or(|sym| token_set.contains(sym))
                }),
            "bucket {len} accepts diverge from the placement set"
        );
        buckets.push((len, bucket_table));
    }

    // Residual = every keyword not covered by a bucket, PLUS every keyword
    // with a cyclic accept state (its matches can exceed the longest bucket --
    // pumping the cycle -- so gap-length and out-of-range dispatch must still
    // find it), PLUS every bounded keyword matchable above the bucket cap (the
    // buckets cannot carry those lengths, so the dispatcher's out-of-range tail
    // must reach it). An empty residual must NOT go through the subset
    // construction: running it over an EMPTY token set yields a spurious state
    // that accepts the end-of-input token, which would make the dispatcher
    // report a (bogus) keyword match for gap lengths. Returning the default
    // (empty) LexTable instead is the intended immediate-failure signal -- the
    // renderer sees an empty table and answers false outright for every length
    // that would consult it.
    let residual_set: TokenSet = keywords
        .iter()
        .filter(|s| !bucketed.contains(*s) || cyclic.contains_key(s) || over_cap.contains(*s))
        .collect();
    if residual_set.iter().next().is_none() {
        return (buckets, LexTable::default(), 0);
    }
    // Minimum byte length at which the residual can accept: the minimum of its
    // symbols' coverage minima (exact-set minimum or cyclic-set minimum -- a
    // cyclic state's SHORTEST path is simple, hence within the DFS cap, so the
    // cyclic minimum is its true minimum match length); a symbol
    // with no computable coverage at all (a dropped pathological keyword)
    // degrades the bound to 0 (no bound). The dispatcher uses this to answer
    // gap lengths below the minimum with a bare `return false` -- a word
    // shorter than every residual keyword can only fail there.
    let mut residual_min: usize = usize::MAX;
    for sym in residual_set.iter() {
        let sym_min = [exact.get(&sym), cyclic.get(&sym)]
            .into_iter()
            .flatten()
            .filter_map(|s| s.iter().next().copied())
            .min();
        let Some(sym_min) = sym_min else {
            residual_min = 0;
            break;
        };
        residual_min = residual_min.min(sym_min);
    }
    let residual_min = if residual_min == usize::MAX {
        0
    } else {
        residual_min.min(u16::MAX as usize) as u16
    };
    // Prune the residual to the lengths the dispatcher can actually consult it
    // at: the out-of-range tail (every length above the longest bucket) and the
    // gap lengths at or above `residual_min` (gap lengths BELOW the minimum
    // already answer false outright, and bucket lengths dispatch to
    // their buckets). With a known minimum, the same (state, byte-length) pair
    // liveness analysis used for the buckets removes every state, edge, and
    // accept that lies only on paths of non-consultable lengths -- e.g. a
    // bounded residual keyword's short prefix chain, or a cyclic keyword's
    // accept flag at a gap depth that no consultable word reaches. Without a
    // bound (`residual_min == 0`: a dropped pathological keyword put an
    // unbounded symbol in the residual) every gap keeps the reduced DFA and no
    // depth is provably dead, so the raw DFA is kept. An empty bucket set (the
    // residual is the only DFA, reachable from every length) likewise keeps it
    // raw.
    let residual_table = if residual_min == 0 || bucket_lens.is_empty() {
        let mut residual_builder = LexTableBuilder::new(lexical_grammar);
        residual_builder.add_state_for_tokens(&residual_set);
        mem::take(&mut residual_builder.table)
    } else {
        let pruned = prune_residual_dfa(lexical_grammar, &residual_set, &bucket_lens, residual_min as usize);
        // The prune removes only provably dead paths, so every residual symbol
        // must still have an accept state: each one has at least one
        // consultable length by construction (a gap at or above its minimum,
        // or the tail).
        debug_assert!(
            residual_set.iter().all(|sym| pruned
                .states
                .iter()
                .any(|s| s.accept_action == Some(sym))),
            "residual prune dropped a keyword accept"
        );
        pruned
    };
    (buckets, residual_table, residual_min)
}

/// Keyword length buckets: build the residual ("reduced") DFA --
/// the DFA over the keywords the buckets do not fully cover -- and PRUNE it to
/// the byte lengths the dispatcher can actually consult it at: every length
/// above the longest bucket (the dispatcher's out-of-range tail) and every GAP
/// length in `residual_min..=longest_bucket` -- a length in that range that is
/// itself a bucket length dispatches to its bucket, and a shorter gap answers
/// false outright, so neither consults this DFA. This is
/// the same generalized liveness analysis as `build_bucket_dfa`, with the
/// single seed layer `len` replaced by the CONSULTABLE depth set: a forward DFS
/// over (state, byte-length) pairs (main-token edges only, bounded at `cap`)
/// finds every pair a consultation can occupy, and a pair is LIVE if an accept
/// is reachable from it at any consultable depth (backward pass from the accept
/// pairs at those depths). The emitted DFA keeps a state iff some pair of it is
/// live, an edge iff it connects live pairs at matching depths, and an accept
/// iff the state is live at some consultable depth.
///
/// The DFS cap must exceed every depth at which the residual can be consulted
/// AND every depth at which one of its accepts is reachable at all, or the
/// prune would drop live paths. Bounded accepts sit on simple paths only, at
/// most `4n` (the `compute_keyword_depths` bound, `n` = residual states, edge
/// width <= 4 UTF-8 bytes), so every bounded over-cap accept is seeded within
/// the cap. A cyclic accept is reachable at `min`, `min + w`, `min + 2w`, ...
/// (one lap = `w <= 4n` bytes); its smallest TAIL depth (above the longest
/// bucket, hence consultable) is at most `16 + 4n`, also within the cap, and
/// seeds the whole pumping path: every pair on a path to a seeded pair is
/// live, so the states and edges carrying arbitrarily long tail words all
/// survive the prune. `cap = 8n + 32` covers both bounds with margin.
///
/// Separator edges are dropped exactly as in `build_bucket_dfa` (the peek guard
/// and the mid-token gate make them unreachable), and `edge_byte_width` cannot
/// fail for the same reason.
fn prune_residual_dfa(
    lexical_grammar: &LexicalGrammar,
    token_set: &TokenSet,
    bucket_lens: &BTreeSet<u16>,
    residual_min: usize,
) -> LexTable {
    let mut builder = LexTableBuilder::new(lexical_grammar);
    builder.add_state_for_tokens(token_set);
    let raw = mem::take(&mut builder.table);
    let n = raw.states.len();
    debug_assert!(raw.states.iter().all(|s| s.eof_action.is_none()));

    let cap = 8 * n + 32;
    let longest_bucket = *bucket_lens.iter().next_back().unwrap() as usize;
    // Consultable depths: the tail above the longest bucket, and gaps at or
    // above the residual minimum. Bucket depths are NOT consultable -- the
    // dispatcher routes those lengths to the buckets.
    let consultable =
        |d: usize| d > longest_bucket || (d >= residual_min && !bucket_lens.contains(&(d as u16)));

    // Pair sets as per-state bitsets (one bit per byte depth, like the
    // `compute_keyword_depths` visited set): `reach[u]` bit d = state u
    // reachable at d bytes over main-token edges.
    let words = cap / 64 + 1;
    let test = |set: &[u64], d: usize| set[d / 64] >> (d % 64) & 1 != 0;
    let set_bit = |set: &mut [u64], d: usize| set[d / 64] |= 1u64 << (d % 64);

    // Forward pair DFS: every (state, depth) pair a consultation can occupy.
    let mut reach = vec![vec![0u64; words]; n];
    set_bit(&mut reach[0], 0);
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    while let Some((u, d)) = stack.pop() {
        if d == cap {
            continue;
        }
        for (chars, action) in &raw.states[u].advance_actions {
            if !action.in_main_token {
                continue;
            }
            let v = action.state as usize;
            if v >= n {
                continue;
            }
            let ew = edge_byte_width(chars)
                .expect("mixed-width edge rejected by the compute_keyword_depths gate");
            let nd = d + ew;
            if nd <= cap && !test(&reach[v], nd) {
                set_bit(&mut reach[v], nd);
                stack.push((v, nd));
            }
        }
    }

    // Backward liveness over the pair graph, seeded from every accept pair at
    // a consultable depth. Reverse adjacency: (predecessor, edge width).
    let mut rev: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (u, state) in raw.states.iter().enumerate() {
        for (chars, action) in &state.advance_actions {
            if !action.in_main_token {
                continue;
            }
            let v = action.state as usize;
            if v >= n {
                continue;
            }
            let ew = edge_byte_width(chars)
                .expect("mixed-width edge rejected by the compute_keyword_depths gate");
            rev[v].push((u, ew));
        }
    }
    let mut live = vec![vec![0u64; words]; n];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for u in 0..n {
        if raw.states[u].accept_action.is_some() {
            for d in 1..=cap {
                if consultable(d) && test(&reach[u], d) && !test(&live[u], d) {
                    set_bit(&mut live[u], d);
                    stack.push((u, d));
                }
            }
        }
    }
    while let Some((v, nd)) = stack.pop() {
        for &(u, ew) in &rev[v] {
            // Reverse edge: (u, nd - ew) -> (v, nd).
            if nd >= ew && test(&reach[u], nd - ew) && !test(&live[u], nd - ew) {
                set_bit(&mut live[u], nd - ew);
                stack.push((u, nd - ew));
            }
        }
    }

    // Emit: states with any live pair, edges between live pairs, accepts only
    // at consultable depths.
    let mut new_id = vec![u32::MAX; n];
    let mut out = LexTable::default();
    for u in 0..n {
        if live[u].iter().any(|&word| word != 0) {
            new_id[u] = out.states.len() as u32;
            out.states.push(LexState::default());
        }
    }
    for u in 0..n {
        if new_id[u] == u32::MAX {
            continue;
        }
        let st = &mut out.states[new_id[u] as usize];
        if (1..=cap).any(|d| test(&live[u], d) && consultable(d)) {
            st.accept_action = raw.states[u].accept_action;
        }
        // Separator edges are dropped exactly as in `build_bucket_dfa`: the
        // consultation restarts at the word's first byte, so the root's
        // separator SKIP can never fire (a word beginning with a separator
        // character goes to the full DFA via the peek guard), and mid-token
        // separators are excluded by the gate.
        for (chars, action) in raw.states[u]
            .advance_actions
            .iter()
            .filter(|(_, action)| action.in_main_token)
        {
            let v = action.state as usize;
            if v >= n || new_id[v] == u32::MAX {
                continue;
            }
            let ew = edge_byte_width(chars)
                .expect("mixed-width edge rejected by the compute_keyword_depths gate");
            // Keep the edge iff some live pair of u reaches a live pair of v.
            let keeps =
                ew <= cap && (0..=cap - ew).any(|d| test(&live[u], d) && test(&live[v], d + ew));
            if keeps {
                st.advance_actions.push((
                    chars.clone(),
                    AdvanceAction { state: new_id[v], in_main_token: true },
                ));
            }
        }
    }
    out
}

/// Build one length bucket's DFA: the token subset's DFA (built exactly like
/// the full keyword DFA, so cyclic keywords keep their compact loops), then
/// PRUNED to the words of exactly `len` bytes. A forward DFS over the bucket
/// DFA's (state, byte-length) pairs (main-token edges only, bounded at `len`)
/// finds every pair a consultation can occupy; a pair is LIVE if an accept is
/// reachable from it at EXACTLY `len` (backward pass from the accept pairs at
/// layer `len`). The emitted DFA keeps a state iff some pair of it is live, an
/// edge iff it connects live pairs at matching depths, and an accept iff the
/// state is live at layer `len`. Dead branches vanish: fsharp's minimized
/// `bool` accept (`true`|`false`) keeps the `true` chain in bucket 4 and the
/// `false` chain in bucket 5, and NEITHER bucket carries the other's chain;
/// a short accept on a live path (bucket 3's `in` state inside the `int`
/// path) is suppressed because it is not live at layer 3. Loops stay compact:
/// m68k's `l[0-9]+` in bucket 11 keeps its digit self-loop (every loop depth
/// lies on an accept-at-11 path), which a full unroll to layer 11 would
/// materialize as a 11-state chain.
///
/// Separator edges are dropped: the dispatcher's peek guard routes any word
/// whose first character could fire the root separator to the full DFA, and
/// the mid-token gate in `compute_keyword_depths` bails the whole feature when
/// a consultation could consume separator bytes after keyword bytes, so a
/// bucket walk never needs them. Mixed-width edges are excluded by the same
/// gate, which is why `edge_byte_width` below cannot fail.
fn build_bucket_dfa(
    lexical_grammar: &LexicalGrammar,
    token_set: &TokenSet,
    len: usize,
) -> LexTable {
    let mut builder = LexTableBuilder::new(lexical_grammar);
    builder.add_state_for_tokens(token_set);
    let raw = mem::take(&mut builder.table);
    let n = raw.states.len();
    // Keyword token sets are all terminals, so the subset construction never
    // marks EOF valid and no state carries an eof_action.
    debug_assert!(raw.states.iter().all(|s| s.eof_action.is_none()));

    // Forward pair DFS: `reach[u][d]` = state u reachable at d bytes.
    let w = len + 1;
    let mut reach = vec![vec![false; w]; n];
    reach[0][0] = true;
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    while let Some((u, d)) = stack.pop() {
        if d == len {
            continue;
        }
        for (chars, action) in &raw.states[u].advance_actions {
            if !action.in_main_token {
                continue;
            }
            let v = action.state as usize;
            if v >= n {
                continue;
            }
            let ew = edge_byte_width(chars)
                .expect("mixed-width edge rejected by the compute_keyword_depths gate");
            let nd = d + ew;
            if nd <= len && !reach[v][nd] {
                reach[v][nd] = true;
                stack.push((v, nd));
            }
        }
    }

    // Backward liveness over the pair graph: live from an accept at layer len.
    // Reverse adjacency: (predecessor, edge width) for each state.
    let mut rev: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for (u, state) in raw.states.iter().enumerate() {
        for (chars, action) in &state.advance_actions {
            if !action.in_main_token {
                continue;
            }
            let v = action.state as usize;
            if v >= n {
                continue;
            }
            let ew = edge_byte_width(chars)
                .expect("mixed-width edge rejected by the compute_keyword_depths gate");
            rev[v].push((u, ew));
        }
    }
    let mut live = vec![vec![false; w]; n];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for u in 0..n {
        if raw.states[u].accept_action.is_some() && reach[u][len] {
            live[u][len] = true;
            stack.push((u, len));
        }
    }
    while let Some((v, nd)) = stack.pop() {
        for &(u, ew) in &rev[v] {
            // Reverse edge: (u, nd - ew) -> (v, nd).
            if nd >= ew && reach[u][nd - ew] && !live[u][nd - ew] {
                live[u][nd - ew] = true;
                stack.push((u, nd - ew));
            }
        }
    }

    // Emit: states with any live pair, edges between live pairs, accepts only
    // at layer `len`. Reverse the raw edges to walk them forward again.
    let mut new_id = vec![u32::MAX; n];
    let mut out = LexTable::default();
    for u in 0..n {
        if live[u].iter().any(|&l| l) {
            new_id[u] = out.states.len() as u32;
            out.states.push(LexState::default());
        }
    }
    for u in 0..n {
        if new_id[u] == u32::MAX {
            continue;
        }
        let st = &mut out.states[new_id[u] as usize];
        if live[u][len] {
            st.accept_action = raw.states[u].accept_action;
        }
        for (chars, action) in &raw.states[u].advance_actions {
            if !action.in_main_token {
                continue;
            }
            let v = action.state as usize;
            if v >= n || new_id[v] == u32::MAX {
                continue;
            }
            let ew = edge_byte_width(chars)
                .expect("mixed-width edge rejected by the compute_keyword_depths gate");
            // Keep the edge iff some live pair of u reaches a live pair of v.
            let keeps = ew <= len && (0..=len - ew).any(|d| live[u][d] && live[v][d + ew]);
            if keeps {
                st.advance_actions.push((
                    chars.clone(),
                    AdvanceAction { state: new_id[v], in_main_token: true },
                ));
            }
        }
    }
    out
}

pub fn build_lex_table(
    parse_table: &mut ParseTable,
    syntax_grammar: &SyntaxGrammar,
    lexical_grammar: &LexicalGrammar,
    keywords: &TokenSet,
    coincident_token_index: &CoincidentTokenIndex,
    token_conflict_map: &TokenConflictMap,
) -> LexTables {
    // Build option (keyword-create-trace): when TREE_SITTER_KEYWORD_CREATE_TRACE=1,
    // time the keyword-DFA creation phases and print them to stderr as
    // `KWCREATE <phase> <ns> <detail>` lines, so generation cost can be
    // attributed to its inputs. Unset => zero overhead, and generation stays
    // byte-identical.
    let kw_create_trace = std::env::var("TREE_SITTER_KEYWORD_CREATE_TRACE")
        .is_ok_and(|v| v == "1");
    let kw_now = || std::time::Instant::now();
    // A macro so `$detail` is only evaluated when tracing is on (a closure
    // cannot take `impl FnOnce()` as a parameter).
    macro_rules! kw_report {
        ($phase:expr, $t0:expr, $detail:expr) => {
            if kw_create_trace {
                eprintln!(
                    "KWCREATE {} {} {}",
                    $phase,
                    $t0.elapsed().as_nanos(),
                    $detail
                );
            }
        };
    }
    let kw_t0 = kw_now();
    let keyword_lex_table = if syntax_grammar.word_token.is_some() {
        let mut builder = LexTableBuilder::new(lexical_grammar);
        builder.add_state_for_tokens(keywords);
        builder.table
    } else {
        LexTable::default()
    };
    kw_report!(
        "full_dfa",
        kw_t0,
        format!(
            "states={} keywords={}",
            keyword_lex_table.states.len(),
            keywords.len()
        )
    );

    // Keyword length buckets: the length bound and the per-length bucket DFAs
    // are computed for every grammar with a word token. The only way to get no
    // buckets is a keyword DFA that the byte-length analysis declines
    // (compute_keyword_depths -> None), which leaves the full DFA as the sole
    // keyword function.
    let kw_t1 = kw_now();
    let (max_word_length, keyword_bucket_tables, keyword_residual_table, keyword_peek_set,
         keyword_residual_min) = {
        let word_start = syntax_grammar
            .word_token
            .map(|w| word_body_start(lexical_grammar, w))
            .unwrap_or_default();
        match compute_keyword_depths(&keyword_lex_table, &word_start) {
            Some(d) => {
                let KeywordDepths {
                    max_len,
                    lengths,
                    peek,
                    variable,
                } = d;
                let (buckets, residual, residual_min) = build_keyword_buckets(
                    lexical_grammar,
                    keywords,
                    &keyword_lex_table,
                    &lengths,
                    &variable,
                );
                // The over-length skip is unsound when either (a) the
                // separator can fire for words beginning with a peek
                // character (a match may consume separator bytes, so a
                // match can exceed its literal), or (b) any keyword is
                // variable-length (its matches have no length bound at
                // all). Disable the bound in either case while keeping the
                // buckets, which remain sound: peek words go to the full
                // DFA before any bucket dispatch, and variable keywords
                // ride exactly the buckets their DFS coverage reached,
                // plus the residual.
                let has_variable = variable.iter().any(|&v| v);
                let bound = if peek.range_count() == 0 && !has_variable {
                    max_len
                } else {
                    0
                };
                (bound, buckets, residual, peek, residual_min)
            }
            None => (0, Vec::new(), LexTable::default(), CharacterSet::empty(), 0),
        }
    };
    kw_report!(
        "buckets",
        kw_t1,
        format!(
            "buckets={} residual_states={} max_word_length={}",
            keyword_bucket_tables.len(),
            keyword_residual_table.states.len(),
            max_word_length,
        )
    );

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
        max_word_length,
        keyword_bucket_tables,
        keyword_residual_table,
        keyword_peek_set,
        keyword_residual_min,
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
