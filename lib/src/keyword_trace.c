// Keyword-DFS consultation trace sink. See keyword_trace.h.
//
// Compiled into the core only when TREE_SITTER_KEYWORD_TRACE is defined; the
// call sites in parser.c / the CLI are guarded by the same macro, so an
// uninstrumented build contains none of this.

#ifdef TREE_SITTER_KEYWORD_TRACE

#include "keyword_trace.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static FILE *s_out = NULL;
static bool s_tried_open = false;

static void kwtrace_ensure_open(void) {
  if (s_tried_open) return;
  s_tried_open = true;
  const char *path = getenv("TREE_SITTER_KEYWORD_TRACE");
  if (path != NULL && path[0] != '\0') {
    s_out = fopen(path, "wb");
  }
}

int ts_keyword_trace_enabled(void) {
  kwtrace_ensure_open();
  return s_out != NULL;
}

void ts_keyword_trace_begin_file(const char *path, const char *grammar_name) {
  kwtrace_ensure_open();
  if (s_out == NULL) return;
  uint32_t path_len = (uint32_t)strlen(path);
  uint32_t name_len = grammar_name != NULL ? (uint32_t)strlen(grammar_name) : 0;
  uint8_t tag = KWTRACE_TAG_FILE;
  fwrite(&tag, 1, 1, s_out);
  fwrite(&path_len, sizeof(path_len), 1, s_out);
  fwrite(path, 1, path_len, s_out);
  fwrite(&name_len, sizeof(name_len), 1, s_out);
  if (name_len != 0) fwrite(grammar_name, 1, name_len, s_out);
}

void ts_keyword_trace_consult(
  uint32_t start_byte,
  uint32_t start_row,
  uint32_t start_col,
  uint32_t len,
  uint32_t state,
  int accepted,
  uint32_t result_symbol
) {
  if (s_out == NULL) return;  // only after a successful begin_file/open
  // Written field-by-field (no struct padding) so the Python collector and
  // the C replay harness can read the same bytes without matching the
  // compiler's struct layout. Native byte order; same-machine only.
  uint8_t tag = KWTRACE_TAG_CONSULT;
  uint8_t acc = (uint8_t)(accepted ? 1 : 0);
  fwrite(&tag, 1, 1, s_out);
  fwrite(&start_byte, sizeof(start_byte), 1, s_out);
  fwrite(&start_row, sizeof(start_row), 1, s_out);
  fwrite(&start_col, sizeof(start_col), 1, s_out);
  fwrite(&len, sizeof(len), 1, s_out);
  fwrite(&state, sizeof(state), 1, s_out);
  fwrite(&result_symbol, sizeof(result_symbol), 1, s_out);
  fwrite(&acc, 1, 1, s_out);
}

void ts_keyword_trace_skip(uint32_t start_byte, uint32_t len) {
  if (s_out == NULL) return;
  uint8_t tag = KWTRACE_TAG_SKIP;
  fwrite(&tag, 1, 1, s_out);
  fwrite(&start_byte, sizeof(start_byte), 1, s_out);
  fwrite(&len, sizeof(len), 1, s_out);
}

int ts_keyword_trace_eval_all(void) {
  static int cached = -1;
  if (cached < 0) {
    const char *v = getenv("TREE_SITTER_KEYWORD_EVAL_ALL");
    cached = (v != NULL && v[0] == '1') ? 1 : 0;
  }
  return cached;
}

void ts_keyword_trace_eval(
  uint32_t start_byte,
  uint32_t len,
  uint32_t state,
  uint8_t evaluated,
  uint8_t results,
  int accepted,
  int eval_all,
  uint64_t overhead,
  const uint64_t *test_cycles,
  uint64_t total_cycles
) {
  if (s_out == NULL) return;
  // Field-by-field like the consult record: no struct padding on the wire.
  uint8_t tag = KWTRACE_TAG_EVAL;
  uint8_t acc = (uint8_t)(accepted ? 1 : 0);
  uint8_t ea = (uint8_t)(eval_all ? 1 : 0);
  fwrite(&tag, 1, 1, s_out);
  fwrite(&start_byte, sizeof(start_byte), 1, s_out);
  fwrite(&len, sizeof(len), 1, s_out);
  fwrite(&state, sizeof(state), 1, s_out);
  fwrite(&evaluated, 1, 1, s_out);
  fwrite(&results, 1, 1, s_out);
  fwrite(&acc, 1, 1, s_out);
  fwrite(&ea, 1, 1, s_out);
  fwrite(&overhead, sizeof(overhead), 1, s_out);
  for (int i = 0; i < 6; i++) {
    fwrite(&test_cycles[i], sizeof(test_cycles[i]), 1, s_out);
  }
  fwrite(&total_cycles, sizeof(total_cycles), 1, s_out);
}

#endif  // TREE_SITTER_KEYWORD_TRACE
