#ifndef TREE_SITTER_KEYWORD_TRACE_H_
#define TREE_SITTER_KEYWORD_TRACE_H_

// Build option: keyword-DFS consultation tracing.
//
// Compile the core with -DTREE_SITTER_KEYWORD_TRACE to record every keyword
// DFA consultation (the ts_parser__call_keyword_lex_fn walk) to a binary
// trace file. Activation is at runtime: set TREE_SITTER_KEYWORD_TRACE=<path>
// in the environment and the sink opens lazily on the first traced file. When
// the feature is compiled out (the default) every hook is a no-op and the
// consultation path is byte-identical to an uninstrumented build.
//
// Record layout (native byte order, tightly packed via fwrite of the structs
// below, each prefixed by a one-byte tag):
//
//   tag 1  KWTraceFileRecord   -- one per parsed file, carries its path AND
//                                 the grammar (language) name that was loaded
//                                 to parse it, so every consultation that
//                                 follows is attributable to one grammar.
//   tag 2  KWTraceConsultRecord -- one per keyword DFA consultation
//   tag 3  KWTraceSkipRecord   -- one per keyword candidate the length bound
//                                 rejected, i.e. a word that reached the
//                                 consultation gate but got NO DFA analysis
//   tag 4  KWTraceEvalRecord   -- one per keyword candidate: the full
//                                 promotion-test matrix (which tests were
//                                 evaluated, their results) with per-test
//                                 cycle counts. See the consultation-test
//                                 ordering section at the bottom of this
//                                 header.
//
// tag 2 + tag 3 together are every word the parser considered a keyword
// candidate, so `ns / (tag2 + tag3)` is the keyword-DFS cost amortized over
// all consultation attempts rather than only the ones that ran the DFA.
//
// The consult record stores the word's start byte + byte length and the
// `state` argument the parser passed to keyword_lex_fn (0 = full DFA, or the
// word's byte length under the bucket dispatch), plus the DFA's raw return
// and result_symbol as ground truth for replay validation. A replay harness
// re-slices the word from the file at [start_byte, start_byte+len) and calls
// the same keyword_lex_fn with the recorded state.

#include <stdint.h>
#include <stddef.h>
#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

#define KWTRACE_TAG_FILE 1
#define KWTRACE_TAG_CONSULT 2
#define KWTRACE_TAG_SKIP 3
#define KWTRACE_TAG_EVAL 4

// Monotonic cycle counter for per-test consultation timing. On x86/x86-64 this
// is TSC (constant-rate on every modern part); on aarch64 the virtual count
// register; elsewhere it degrades to nanoseconds from CLOCK_MONOTONIC (the
// analysis treats the unit as opaque, so the ordering conclusion holds
// regardless). Read with a serializing fence so the measured work cannot be
// reordered across the counter read; the fence's own cost is calibrated once
// per consultation (the `overhead` field of the eval record) and subtracted
// from each test's cycles by the analysis.
static inline uint64_t ts_cycle_counter(void) {
#if defined(__x86_64__) || defined(__i386__)
  uint32_t lo, hi;
  __asm__ __volatile__("lfence\n\trdtsc" : "=a"(lo), "=d"(hi) : : "memory");
  return ((uint64_t)hi << 32) | lo;
#elif defined(__aarch64__)
  uint64_t v;
  __asm__ __volatile__("isb\n\tmrs %0, cntvct_el0" : "=r"(v) : : "memory");
  return v;
#else
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
#endif
}

// Cycles elapsed since `start`, minus the calibration `overhead` (the cost of
// one counter read, which inflates every bracketed measurement), clamped at 0
// so a test cheaper than the calibration cannot wrap to a huge value.
static inline uint64_t ts_cycle_elapsed(uint64_t start, uint64_t overhead) {
  uint64_t d = ts_cycle_counter() - start;
  return d > overhead ? d - overhead : 0;
}

typedef struct {
  uint8_t tag;        // KWTRACE_TAG_FILE
  uint32_t path_len;  // bytes that follow (no NUL terminator)
  // ... path_len bytes of path, then uint32_t name_len + name_len bytes of
  // the grammar (language) name active for this file, so the collector can
  // associate every consultation that follows with the grammar under test.
  // A zero name_len means the CLI could not resolve a name.
} KWTraceFileRecord;

typedef struct {
  uint8_t tag;           // KWTRACE_TAG_CONSULT
  uint32_t start_byte;   // word start offset in the file
  uint32_t start_row;    // word start point (for a faithful lexer reset)
  uint32_t start_col;
  uint32_t len;          // word byte length
  uint32_t state;        // state arg passed to keyword_lex_fn
  uint32_t result_symbol;// lexer->result_symbol after the walk
  uint8_t accepted;      // DFA raw return (matched a keyword)
} KWTraceConsultRecord;

// Begin tracing a new file: opens the sink (from $TREE_SITTER_KEYWORD_TRACE)
// on first use and writes a file record carrying the path and the grammar
// name (may be NULL). Safe to call unconditionally from the CLI; a no-op when
// the feature is compiled out or the env var is unset.
void ts_keyword_trace_begin_file(const char *path, const char *grammar_name);

// Whether the sink is (or can be) open — i.e. the feature is compiled in and
// $TREE_SITTER_KEYWORD_TRACE names a file. Callers use this to gate the
// instrumentation bookkeeping (counter reads, bitmask assembly) so a run
// without the env var pays nothing beyond one cached check per consultation.
int ts_keyword_trace_enabled(void);

// Record one consultation. Called from parser.c under the ifdef.
void ts_keyword_trace_consult(
  uint32_t start_byte,
  uint32_t start_row,
  uint32_t start_col,
  uint32_t len,
  uint32_t state,
  int accepted,
  uint32_t result_symbol
);

// Record a keyword candidate that the `max_word_length` bound rejected before
// any DFA analysis. Only ever emitted by a bucketed language with a nonzero
// bound; stock emits none, so its attempt count equals its consult count.
// Called from parser.c under the ifdef.
void ts_keyword_trace_skip(uint32_t start_byte, uint32_t len);

// ---------------------------------------------------------------------------
// Consultation-test ordering instrumentation (tag 4).
//
// A keyword candidate becomes a keyword leaf only if EVERY test in the
// promotion chain passes. In `ts_parser__lex` the chain runs in a fixed order
// and SHORT-CIRCUITS at the first failure, so a normal trace only ever records
// the tests a candidate actually reached. To reason about the OPTIMAL order we
// need the cost AND outcome of every test on every candidate, including the
// ones the current order skips. Two switches, both runtime-gated so an
// uninstrumented or non-eval run is byte-identical to stock:
//
//   TREE_SITTER_KEYWORD_TRACE=<path>  -- open the sink (as before).
//   TREE_SITTER_KEYWORD_EVAL_ALL=1    -- evaluate EVERY test for EVERY
//                                       candidate (no short-circuit): run the
//                                       DFA walk even when the bound rejects,
//                                       and run the post-accept parse-table
//                                       lookups even when the walk rejects.
//                                       The promotion DECISION is unchanged
//                                       (a skipped-by-policy test still cannot
//                                       promote), only the observation is
//                                       widened. This yields the exact
//                                       counterfactual matrix: for any test
//                                       order, the analysis can compute its
//                                       cost by summing the tests a candidate
//                                       reaches before its first failure.
//
// The eval record captures, per candidate: which tests were evaluated
// (`evaluated` bitmask), each evaluated test's boolean result (`results`
// bitmask, bit set = test passed), and the cycles spent in each test. Tests
// not evaluated have neither a result bit nor a nonzero cycle count.
//
// Test bits (the promotion chain, in the CURRENT source order):
enum {
  KWTEST_BOUND    = 1u << 0,  // word length within max_word_length (over-length skip)
  KWTEST_RESET    = 1u << 1,  // lexer reset + start to the word's first byte
  KWTEST_WALK     = 1u << 2,  // keyword DFA walk (dispatcher + bucket/full DFA)
  KWTEST_BYTE_EQ  = 1u << 3,  // walked token ends exactly at the word's end
  KWTEST_ACTIONS  = 1u << 4,  // result symbol has parse actions in this state
  KWTEST_RESERVED = 1u << 5,  // result symbol is a reserved word in this state
};

typedef struct {
  uint8_t tag;          // KWTRACE_TAG_EVAL
  uint32_t start_byte;  // word start offset (identifies the candidate)
  uint32_t len;         // word byte length
  uint32_t state;       // dispatch state the walk used (0 = full DFA)
  uint8_t evaluated;    // bitmask of KWTEST_* actually run
  uint8_t results;      // bitmask of KWTEST_* that passed (subset of evaluated)
  uint8_t accepted;     // the DFA walk's raw result (== results & KWTEST_WALK)
  uint8_t eval_all;     // 1 when TREE_SITTER_KEYWORD_EVAL_ALL was on
  uint64_t overhead;    // cycles to read the counter twice (calibration)
  uint64_t cyc_bound;
  uint64_t cyc_reset;
  uint64_t cyc_walk;
  uint64_t cyc_byte_eq;
  uint64_t cyc_actions;
  uint64_t cyc_reserved;
  uint64_t cyc_total;   // whole candidate, first counter read to last
} KWTraceEvalRecord;

// Record one candidate's full test matrix. Called from parser.c under the
// ifdef, once per keyword candidate, after the promotion decision.
void ts_keyword_trace_eval(
  uint32_t start_byte,
  uint32_t len,
  uint32_t state,
  uint8_t evaluated,
  uint8_t results,
  int accepted,
  int eval_all,
  uint64_t overhead,
  const uint64_t *test_cycles,  // 6 entries: bound,reset,walk,byte_eq,actions,reserved
  uint64_t total_cycles
);

// Whether TREE_SITTER_KEYWORD_EVAL_ALL=1 is set. Cached after first read.
// Returns 0 when the trace feature is compiled out.
int ts_keyword_trace_eval_all(void);

#ifdef __cplusplus
}
#endif

#endif  // TREE_SITTER_KEYWORD_TRACE_H_
