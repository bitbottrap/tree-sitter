#include "tree_sitter/parser.h"

void *tree_sitter_keyword_quoted_atom_external_scanner_create(void) { return NULL; }
void tree_sitter_keyword_quoted_atom_external_scanner_destroy(void *payload) {}
unsigned tree_sitter_keyword_quoted_atom_external_scanner_serialize(void *payload, char *buffer) { return 0; }
void tree_sitter_keyword_quoted_atom_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {}

bool tree_sitter_keyword_quoted_atom_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid_symbols) {
  while (lexer->lookahead == ' ' || lexer->lookahead == '\t') lexer->advance(lexer, true);
  if (!valid_symbols[0] || lexer->lookahead != ':') return false;
  lexer->advance(lexer, false);
  lexer->mark_end(lexer);
  lexer->result_symbol = 0;
  return lexer->lookahead == '"' || lexer->lookahead == '\'';
}
