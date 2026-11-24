; Kotlin highlights for tree-sitter-kotlin-ng, which ships no queries.
; Written against its node-types.json (1.1); scope names follow
; HIGHLIGHT_NAMES in src/highlight.rs. Later patterns win ties, so the
; specific captures come last.

(identifier) @variable

[
  (line_comment)
  (block_comment)
  (shebang)
] @comment

[
  (string_literal)
  (multiline_string_literal)
  (character_literal)
] @string

(escape_sequence) @string.special
(interpolation) @punctuation.delimiter

[
  (number_literal)
  (float_literal)
] @number

[
  "package" "import" "class" "interface" "object" "fun" "val" "var"
  "typealias" "constructor" "init" "companion" "enum" "data" "sealed"
  "if" "else" "when" "for" "while" "do" "try" "catch" "finally"
  "return" "throw" "in" "!in" "is" "!is" "as" "as?" "by" "where"
  "this" "super" "get" "set" "out" "vararg" "suspend" "inline"
  "operator" "override" "open" "abstract" "final" "const" "lateinit"
  "private" "protected" "public" "internal" "inner" "value"
  "noinline" "crossinline" "tailrec" "external" "infix" "expect"
  "actual" "dynamic"
] @keyword
(reification_modifier) @keyword

[
  "=" "+=" "-=" "*=" "/=" "%=" "==" "!=" "===" "!==" "<" ">" "<=" ">="
  "&&" "||" "!" "!!" "+" "-" "*" "/" "%" "++" "--" ".." "..<" "?:"
  "->" "?." "::"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." ";" ":"] @punctuation.delimiter

(user_type (identifier) @type)
(annotation) @attribute
(label) @label

(class_declaration name: (identifier) @type)
(object_declaration name: (identifier) @type)
(function_declaration name: (identifier) @function)
(call_expression (identifier) @function)
(call_expression (navigation_expression (identifier) @function .))
(parameter (identifier) @variable.parameter)
